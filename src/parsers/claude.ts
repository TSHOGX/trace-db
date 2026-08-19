/**
 * parsers/claude.ts — Claude Code sessions.
 *
 * Store: one JSONL per session at ~/.claude/projects/<enc-cwd>/<uuid>.jsonl.
 * Event stream dispatched on top-level `type`. Ground-truthed 2026-07.
 * Relevant record types:
 *   user / assistant       chat turns; message.content is str (user) or block list
 *   system                 subtype ∈ compact_boundary | local_command | away_summary |
 *                          informational | turn_duration | stop_hook_summary |
 *                          api_error | scheduled_task_fire | agents_killed → system event
 *   ai-title               carries aiTitle → native session title
 *   mode / permission-mode → system event (mode/permission change)
 *   attachment             → system event
 *   file-history-snapshot / last-prompt → skipped (pure engine bookkeeping)
 *
 * One assistant record's content list can mix text, thinking, and tool_use
 * blocks — we emit ONE EVENT PER BLOCK. A user record carrying toolUseResult is
 * a tool result (blocks are tool_result payloads, linked by tool_use_id).
 * cwd/gitBranch/sessionId/uuid/parentUuid come off records directly. Tree A =
 * parentUuid→uuid; tree B = forkedFrom{sessionId,messageUuid} (fork/resume) and
 * subagents.
 *
 * SUBAGENTS (tree B, resolved deterministically here — NOT deferred to fan-out).
 * A subagent transcript is a nested `bg`/isSidechain file at
 *   <enc-cwd>/<parent-uuid>/subagents/agent-<hash>.jsonl
 * with a sibling `agent-<hash>.meta.json` carrying `{agentType}`. The parent is
 * the DIRECTORY NAME, so parent→child needs no LLM. The subagent's own
 * `sessionId` field equals the parent uuid (would collide), so identity is the
 * PATH: id = `claude:<parent-uuid>/agent-<hash>`, parentSessionId =
 * `claude:<parent-uuid>`. That edge feeds the existing tree-B lineage collapse.
 */

import { readdirSync, statSync } from "fs";
import { join } from "path";
import { CLAUDE_PROJECTS } from "../paths.js";
import type { Event, Parser, UnifiedSession } from "../types.js";
import {
  asDict,
  asNum,
  asStr,
  compact,
  EventBuilder,
  fileSource,
  flattenContent,
  isoToEpochSec,
  readJson,
  readJsonl,
} from "../util.js";

const UUID_RE = /^[0-9a-fA-F-]{36}$/;

/** A discovered transcript: a top-level session file or a nested subagent file. */
interface FileEntry {
  path: string;
  /** For a subagent file: the parent session's native uuid (its dir name). */
  parentUuid: string | null;
}

/**
 * Enumerate Claude transcripts. Two shapes under each project dir:
 *   <proj>/<uuid>.jsonl                                  → top-level session
 *   <proj>/<uuid>/subagents/agent-<hash>.jsonl           → subagent (tree B)
 * The top-level scan is one level deep (unchanged); the subagent scan descends
 * into each `<uuid>/subagents/` dir. Non-subagent subdirs are ignored.
 */
function listFiles(): FileEntry[] {
  let projectDirs: string[];
  try {
    projectDirs = readdirSync(CLAUDE_PROJECTS);
  } catch {
    return [];
  }
  const files: FileEntry[] = [];
  for (const proj of projectDirs) {
    const dir = join(CLAUDE_PROJECTS, proj);
    let entries: string[];
    try {
      if (!statSync(dir).isDirectory()) continue;
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    for (const e of entries) {
      if (e.endsWith(".jsonl")) {
        files.push({ path: join(dir, e), parentUuid: null });
        continue;
      }
      // <uuid>/subagents/agent-<hash>.jsonl — nested subagent transcripts
      const subDir = join(dir, e, "subagents");
      let subEntries: string[];
      try {
        if (!statSync(subDir).isDirectory()) continue;
        subEntries = readdirSync(subDir);
      } catch {
        continue;
      }
      for (const se of subEntries)
        if (se.endsWith(".jsonl")) files.push({ path: join(subDir, se), parentUuid: e });
    }
  }
  return files;
}

interface Scan {
  sessionId: string | null;
  cwd: string | null;
  gitBranch: string | null;
  title: string | null;
  model: string | null;
  forkedFrom: string | null;
  version: string | null;
  sessionKind: string | null;
  startedAt: number | null;
  endedAt: number | null;
  records: number;
  turns: number;
}

/** Single pass over a transcript for listing metadata + fingerprint inputs. */
function scan(path: string): Scan {
  const s: Scan = {
    sessionId: null,
    cwd: null,
    gitBranch: null,
    title: null,
    model: null,
    forkedFrom: null,
    version: null,
    sessionKind: null,
    startedAt: null,
    endedAt: null,
    records: 0,
    turns: 0,
  };
  for (const r of readJsonl(path)) {
    s.records++;
    s.sessionId ??= asStr(r.sessionId);
    s.cwd ??= asStr(r.cwd);
    s.gitBranch ??= asStr(r.gitBranch);
    s.version ??= asStr(r.version);
    s.sessionKind ??= asStr(r.sessionKind);
    const ts = isoToEpochSec(r.timestamp);
    if (ts != null) {
      s.startedAt ??= ts;
      s.endedAt = ts;
    }
    const type = asStr(r.type);
    if (type === "ai-title") s.title ??= asStr(r.aiTitle);
    if (type === "assistant") s.model ??= asStr(asDict(r.message).model);
    if (r.forkedFrom && !s.forkedFrom) {
      const f = asDict(r.forkedFrom);
      const sid = asStr(f.sessionId);
      if (sid) s.forkedFrom = `claude:${sid}#${asStr(f.messageUuid) ?? ""}`;
    }
    if (type === "user" && !r.toolUseResult) s.turns++;
    else if (type === "assistant") s.turns++;
  }
  return s;
}

/** Emit events for one assistant content block. */
function pushAssistantBlock(
  b: Record<string, unknown>,
  ts: number | null,
  nativeId: string | null,
  parentId: string | null,
  tokens: number | null,
  out: EventBuilder
): void {
  const btype = asStr(b.type) ?? "text";
  const base = { nativeId, parentId };
  if (btype === "text") {
    out.push("assistant", asStr(b.text) ?? "", ts, { ...base, tokens });
  } else if (btype === "thinking") {
    out.push("thinking", asStr(b.thinking) ?? asStr(b.text) ?? "", ts, base);
  } else if (btype === "tool_use") {
    out.push("tool_call", compact(b.input ?? b), ts, { ...base, name: asStr(b.name), callId: asStr(b.id) });
  } else if (btype === "tool_result") {
    out.push("tool_result", compact(b.content ?? b), ts, {
      ...base,
      callId: asStr(b.tool_use_id),
      isError: b.is_error === true,
    });
  } else {
    out.push("system", compact(b), ts, { ...base, subtype: btype });
  }
}

/** A tool_result block on a user record (carries tool_use_id + is_error). */
function pushToolResultBlock(
  b: Record<string, unknown>,
  ts: number | null,
  nativeId: string | null,
  parentId: string | null,
  out: EventBuilder
): void {
  out.push("tool_result", compact(b.content ?? b), ts, {
    nativeId,
    parentId,
    callId: asStr(b.tool_use_id),
    isError: b.is_error === true,
  });
}

export const claudeParser: Parser = {
  agent: "claude",

  listSessions(sinceEpochSec) {
    const out: UnifiedSession[] = [];
    for (const { path, parentUuid } of listFiles()) {
      if (sinceEpochSec != null) {
        try {
          if (statSync(path).mtimeMs / 1000 < sinceEpochSec) continue;
        } catch {
          continue;
        }
      }
      const m = scan(path);

      if (parentUuid) {
        // Subagent: identity is the PATH (its sessionId equals the parent uuid,
        // so it can't be the key). hash = filename; agentType from the sidecar.
        const hash = path.replace(/\.jsonl$/, "").split("/").pop() ?? "";
        if (!hash) continue;
        const nativeId = `${parentUuid}/${hash}`;
        const meta = readJson(path.replace(/\.jsonl$/, ".meta.json"));
        const agentType = asStr(asDict(meta).agentType);
        out.push({
          id: `claude:${nativeId}`,
          agent: "claude",
          cwd: m.cwd,
          startedAt: m.startedAt,
          endedAt: m.endedAt,
          title: agentType ? `[${agentType} subagent]` : null, // no ai-title in bg files
          model: m.model,
          provider: null,
          gitBranch: m.gitBranch,
          parentSessionId: `claude:${parentUuid}`,
          forkedFrom: m.forkedFrom,
          turnCount: m.turns,
          eventCount: m.turns, // refined on read
          sources: [fileSource(path, "jsonl")],
          fingerprint: `${m.records}:${m.endedAt ?? 0}`,
          meta: { version: m.version, sessionKind: m.sessionKind, agentType, isSubagent: true },
        });
        continue;
      }

      const nativeId = m.sessionId ?? path.replace(/\.jsonl$/, "").split("/").pop() ?? "";
      if (!m.sessionId && !UUID_RE.test(nativeId)) continue;
      out.push({
        id: `claude:${nativeId}`,
        agent: "claude",
        cwd: m.cwd,
        startedAt: m.startedAt,
        endedAt: m.endedAt,
        title: m.title,
        model: m.model,
        provider: null,
        gitBranch: m.gitBranch,
        parentSessionId: null,
        forkedFrom: m.forkedFrom,
        turnCount: m.turns,
        eventCount: m.turns, // refined on read
        sources: [fileSource(path, "jsonl")],
        fingerprint: `${m.records}:${m.endedAt ?? 0}`,
        meta: { version: m.version, sessionKind: m.sessionKind },
      });
    }
    return out;
  },

  readEvents(nativeId): Event[] {
    // Subagent id is "<parent-uuid>/agent-<hash>" → nested at
    // <…>/<parent-uuid>/subagents/agent-<hash>.jsonl.
    const slash = nativeId.indexOf("/");
    const suffix =
      slash >= 0
        ? `/${nativeId.slice(0, slash)}/subagents/${nativeId.slice(slash + 1)}.jsonl`
        : `/${nativeId}.jsonl`;

    let target: string | null = null;
    for (const { path } of listFiles()) {
      if (path.endsWith(suffix)) {
        target = path;
        break;
      }
    }
    if (!target && slash < 0) {
      // top-level fallback: some files are named by uuid but carry a different sessionId
      for (const { path, parentUuid } of listFiles()) {
        if (parentUuid) continue;
        for (const r of readJsonl(path)) {
          if (asStr(r.sessionId) === nativeId) {
            target = path;
            break;
          }
        }
        if (target) break;
      }
    }
    if (!target) return [];

    const out = new EventBuilder();
    for (const r of readJsonl(target)) {
      const type = asStr(r.type);
      const ts = isoToEpochSec(r.timestamp);
      const uuid = asStr(r.uuid);
      const parent = asStr(r.parentUuid);

      if (type === "system") {
        out.push("system", flattenContent(r.content ?? r.data), ts, {
          subtype: asStr(r.subtype),
          nativeId: uuid,
          parentId: parent,
        });
        continue;
      }
      if (type === "mode" || type === "permission-mode") {
        out.push("system", compact(r.mode ?? r.permissionMode ?? r), ts, {
          subtype: type,
          nativeId: uuid,
          parentId: parent,
        });
        continue;
      }
      if (type === "attachment") {
        out.push("system", compact(r.attachment ?? r), ts, {
          subtype: "attachment",
          nativeId: uuid,
          parentId: parent,
        });
        continue;
      }
      if (type !== "user" && type !== "assistant") continue; // last-prompt/file-history-snapshot/ai-title: engine bookkeeping

      const msg = asDict(r.message);
      const content = msg.content;

      if (type === "user") {
        if (r.toolUseResult) {
          if (Array.isArray(content)) {
            for (const b of content)
              if (b && typeof b === "object")
                pushToolResultBlock(b as Record<string, unknown>, ts, uuid, parent, out);
          } else {
            out.push("tool_result", compact(r.toolUseResult), ts, { nativeId: uuid, parentId: parent });
          }
        } else {
          out.push("user", flattenContent(content), ts, { nativeId: uuid, parentId: parent });
        }
        continue;
      }

      // assistant: one event per content block, usage attached to the text block
      const usage = asDict(msg.usage);
      const tok = (asNum(usage.input_tokens) ?? 0) + (asNum(usage.output_tokens) ?? 0) || null;
      if (Array.isArray(content)) {
        for (const b of content)
          if (b && typeof b === "object")
            pushAssistantBlock(b as Record<string, unknown>, ts, uuid, parent, tok, out);
      } else {
        out.push("assistant", flattenContent(content), ts, { nativeId: uuid, parentId: parent, tokens: tok });
      }
    }
    return out.events;
  },
};
