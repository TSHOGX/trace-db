/**
 * parsers/pi.ts — Pi coding-agent sessions (@earendil-works/pi-coding-agent).
 *
 * Store: ~/.pi/agent/sessions/, two layouts coexist:
 *   OLD  <ts>_<uuid>.jsonl at top level
 *   NEW  <enc-cwd>/<ts>_<uuid>.jsonl  (cwd-encoded subdir)
 * Records (event stream, parent_id chain tree):
 *   session               { id, version, cwd, timestamp }        — line 0
 *   model_change          { id, parentId, provider, modelId }    — meta → system
 *   thinking_level_change { id, parentId, thinkingLevel }        — meta → system
 *   message               { id, parentId, message:{ role, content[], timestamp(ms) } }
 * message.role ∈ user | assistant | toolResult; content blocks ∈
 *   text | thinking | toolCall(camelCase: name, arguments).
 *
 * Event mapping:
 *   role=user      text blocks → user
 *   role=assistant text → assistant · thinking → thinking · toolCall → tool_call
 *   role=toolResult content → tool_result (toolName→name, toolCallId→callId, isError)
 *   model_change / thinking_level_change → system (subtype = the record type)
 * Tree A: each record's parentId→id (Pi's native chain). No usage; no title.
 * Filtering: drop test runs — provider "faux" and/or cwd under pi-*-test / tmp.
 */

import { readdirSync, statSync } from "fs";
import { join } from "path";
import { PI_SESSIONS } from "../paths.js";
import type { Event, Parser, UnifiedSession } from "../types.js";
import {
  asDict,
  asStr,
  compact,
  EventBuilder,
  fileSource,
  flattenContent,
  isoToEpochSec,
  millisToEpochSec,
  readJsonl,
} from "../util.js";

/** Recursively collect *.jsonl (top level + one enc-cwd subdir deep). */
function listFiles(): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      return;
    }
    for (const e of entries) {
      const p = join(dir, e);
      let st;
      try {
        st = statSync(p);
      } catch {
        continue;
      }
      if (st.isDirectory()) walk(p);
      else if (e.endsWith(".jsonl")) out.push(p);
    }
  };
  walk(PI_SESSIONS);
  return out;
}

/** Is this a throwaway test session? */
function isTestSession(cwd: string | null, provider: string | null): boolean {
  if (provider === "faux") return true;
  if (!cwd) return false;
  return (
    (cwd.includes("/pi-") && cwd.includes("-test-")) ||
    cwd.startsWith("/private/var/folders/") ||
    cwd === "/private/tmp" ||
    cwd === "/tmp"
  );
}

interface Head {
  id: string | null;
  cwd: string | null;
  version: string | null;
  provider: string | null;
  modelId: string | null;
}

function readHead(records: Record<string, unknown>[]): Head {
  const h: Head = { id: null, cwd: null, version: null, provider: null, modelId: null };
  for (const r of records) {
    const t = asStr(r.type);
    if (t === "session") {
      h.id ??= asStr(r.id);
      h.cwd ??= asStr(r.cwd);
      h.version ??= asStr(r.version) ?? (typeof r.version === "number" ? String(r.version) : null);
    } else if (t === "model_change") {
      h.provider ??= asStr(r.provider);
      h.modelId ??= asStr(r.modelId);
    }
    if (h.id && h.provider) break;
  }
  return h;
}

export const piParser: Parser = {
  agent: "pi",

  listSessions(sinceEpochSec) {
    const out: UnifiedSession[] = [];
    for (const path of listFiles()) {
      if (sinceEpochSec != null) {
        try {
          if (statSync(path).mtimeMs / 1000 < sinceEpochSec) continue;
        } catch {
          continue;
        }
      }
      const records = readJsonl(path);
      if (!records.length) continue;
      const head = readHead(records);
      if (!head.id) continue;
      if (isTestSession(head.cwd, head.provider)) continue;

      let startedAt: number | null = null;
      let endedAt: number | null = null;
      let turns = 0;
      for (const r of records) {
        const ts = isoToEpochSec(r.timestamp);
        if (ts != null) {
          startedAt ??= ts;
          endedAt = ts;
        }
        if (asStr(r.type) === "message") {
          const role = asStr(asDict(r.message).role);
          if (role === "user" || role === "assistant") turns++;
        }
      }
      out.push({
        id: `pi:${head.id}`,
        agent: "pi",
        cwd: head.cwd,
        startedAt,
        endedAt,
        title: null,
        model: head.modelId,
        provider: head.provider,
        gitBranch: null,
        parentSessionId: null,
        forkedFrom: null,
        turnCount: turns,
        eventCount: turns,
        sources: [fileSource(path, "jsonl")],
        fingerprint: `${records.length}:${endedAt ?? 0}`,
        meta: { version: head.version },
      });
    }
    return out;
  },

  readEvents(nativeId): Event[] {
    let target: string | null = null;
    for (const path of listFiles()) {
      if (path.includes(nativeId)) {
        target = path;
        break;
      }
    }
    if (!target) {
      for (const path of listFiles()) {
        const first = readJsonl(path).find((r) => asStr(r.type) === "session");
        if (first && asStr(first.id) === nativeId) {
          target = path;
          break;
        }
      }
    }
    if (!target) return [];

    const out = new EventBuilder();
    for (const r of readJsonl(target)) {
      const rtype = asStr(r.type);
      const recId = asStr(r.id);
      const recParent = asStr(r.parentId);

      if (rtype === "model_change" || rtype === "thinking_level_change") {
        out.push("system", compact(r), isoToEpochSec(r.timestamp), {
          subtype: rtype,
          nativeId: recId,
          parentId: recParent,
        });
        continue;
      }
      if (rtype !== "message") continue;

      const m = asDict(r.message);
      const role = asStr(m.role);
      const ts = millisToEpochSec(m.timestamp) ?? isoToEpochSec(r.timestamp);
      const base = { nativeId: recId, parentId: recParent };

      if (role === "toolResult") {
        out.push("tool_result", flattenContent(m.content), ts, {
          ...base,
          name: asStr(m.toolName),
          callId: asStr(m.toolCallId),
          isError: m.isError === true,
        });
        continue;
      }

      const textKind = role === "user" ? "user" : "assistant";
      const content = m.content;
      if (Array.isArray(content)) {
        for (const b of content) {
          if (!b || typeof b !== "object") continue;
          const block = b as Record<string, unknown>;
          const bt = asStr(block.type);
          if (bt === "text") {
            out.push(textKind, asStr(block.text) ?? "", ts, base);
          } else if (bt === "thinking") {
            out.push("thinking", asStr(block.thinking) ?? "", ts, base);
          } else if (bt === "toolCall") {
            out.push("tool_call", compact(block.arguments), ts, {
              ...base,
              name: asStr(block.name),
              callId: asStr(block.toolCallId) ?? asStr(block.id),
            });
          } else {
            out.push("system", compact(block), ts, { ...base, subtype: bt });
          }
        }
      } else {
        out.push(textKind, flattenContent(content), ts, base);
      }
    }
    return out.events;
  },
};
