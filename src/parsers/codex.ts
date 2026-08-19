/**
 * parsers/codex.ts — Codex CLI rollout sessions.
 *
 * Store: one JSONL per session at ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl.
 * Top-level `type`: session_meta, turn_context, event_msg, response_item,
 * world_state, compacted. Ground-truthed 2026-07.
 *
 * Chat lives across two overlapping streams; we take user turns from event_msg
 * (cleaner) and skip response_item role=user/developer to avoid double-count:
 *   event_msg / user_message            → user
 *   response_item / message role=assistant → assistant
 *   response_item / reasoning           → thinking (.summary)
 *   response_item / function_call | custom_tool_call | tool_search_call | web_search_call
 *                                       → tool_call (name + args + call_id)
 *   response_item / function_call_output | custom_tool_call_output | tool_search_output
 *                                       → tool_result (linked by call_id)
 * Engine events → system (subtype = the native event name):
 *   event_msg: task_started, task_complete, patch_apply_end, web_search_end,
 *              turn_aborted, thread_rolled_back, context_compacted
 *   type: compacted, world_state
 * Usage → event_msg/token_count.info.last_token_usage.total_tokens.
 * session_meta carries model_provider/cli_version/git; turn_context the model +
 * effort + sandbox. No native title. `--ephemeral` runs write no rollout file.
 *
 * SUBAGENTS (tree B, parent-derived — the OPPOSITE of Claude's dir-derivation).
 * A Codex subagent is spawned by a `spawn_agent` tool call in the PARENT rollout;
 * its `function_call_output` (paired by `call_id`) returns `{agent_id, nickname}`,
 * and that `agent_id` IS the child's own session id — the child gets a normal
 * top-level rollout file (rollout-…-<agent_id>.jsonl) with session_meta.id ==
 * agent_id and NO back-reference to the parent. So the ONLY place the edge lives
 * is the parent's spawn_agent output; a child read in isolation can't know its
 * parent. `buildLineage()` does a one-time cross-file pre-pass pairing every
 * spawn_agent call↔output to yield `childId → {parentId, agentType}`, and
 * `listSessions` patches parentSessionId + role from it. agentType comes from the
 * spawn_agent call's `arguments.agent_type` (explorer/worker/awaiter/default).
 */

import { readdirSync, statSync } from "fs";
import { join } from "path";
import { CODEX_SESSIONS } from "../paths.js";
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
  readJsonl,
} from "../util.js";

/** Recursively collect rollout-*.jsonl under the YYYY/MM/DD tree. */
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
      else if (e.startsWith("rollout-") && e.endsWith(".jsonl")) out.push(p);
    }
  };
  walk(CODEX_SESSIONS);
  return out;
}

/** One tree-B edge: a spawned child's parent + its role (agent_type). */
interface Lineage {
  parentId: string; // "codex:<parent-session-id>"
  agentType: string | null; // spawn_agent arguments.agent_type
}

/**
 * Cross-file pre-pass: pair every parent's spawn_agent call↔output (by call_id)
 * to map each spawned child session id → {parent, role}. This is the only source
 * of the edge — children carry no back-reference. Walks all rollout files once;
 * the result is cached module-level (invalidated per `listSessions` call so an
 * incremental ingest still sees newly-spawned children). Cheap: one linear scan,
 * only spawn_agent records matter.
 */
function buildLineage(): Map<string, Lineage> {
  const edges = new Map<string, Lineage>();
  for (const path of listFiles()) {
    // parent session id = the file's session_meta.id (first record)
    const records = readJsonl(path);
    if (!records.length) continue;
    let parentSid: string | null = null;
    // call_id → agent_type captured from spawn_agent calls, resolved when the
    // matching output arrives.
    const pendingType = new Map<string, string | null>();
    for (const r of records) {
      const payload = asDict(r.payload);
      const rtype = asStr(r.type);
      if (rtype === "session_meta") {
        parentSid ??= asStr(payload.id) ?? asStr(payload.session_id);
        continue;
      }
      if (rtype !== "response_item") continue;
      const ptype = asStr(payload.type);
      const callId = asStr(payload.call_id);
      if (ptype === "function_call" && asStr(payload.name) === "spawn_agent") {
        if (!callId) continue;
        let agentType: string | null = null;
        try {
          agentType = asStr(asDict(JSON.parse(asStr(payload.arguments) ?? "")).agent_type);
        } catch {
          /* args not JSON — leave role null */
        }
        pendingType.set(callId, agentType);
      } else if (ptype === "function_call_output" && callId && pendingType.has(callId)) {
        const out = asStr(payload.output);
        let childId: string | null = null;
        try {
          childId = asStr(asDict(JSON.parse(out ?? "")).agent_id);
        } catch {
          /* output not JSON — no child id to link */
        }
        if (childId && parentSid) {
          edges.set(`codex:${childId}`, {
            parentId: `codex:${parentSid}`,
            agentType: pendingType.get(callId) ?? null,
          });
        }
        pendingType.delete(callId);
      }
    }
  }
  return edges;
}

function userText(payload: Record<string, unknown>): string {
  const msg = payload.message;
  if (typeof msg === "string" && msg) return msg;
  const els = payload.text_elements;
  if (Array.isArray(els)) return flattenContent(els);
  return "";
}

const TOOL_CALL_TYPES = new Set(["function_call", "custom_tool_call", "tool_search_call", "web_search_call"]);
const TOOL_OUT_TYPES = new Set(["function_call_output", "custom_tool_call_output", "tool_search_output"]);

export const codexParser: Parser = {
  agent: "codex",

  listSessions(sinceEpochSec) {
    const out: UnifiedSession[] = [];
    // Tree-B edges are parent-derived, so resolve them across ALL files up front
    // (a `since`-bounded child still needs its parent's spawn record, which may
    // predate the window). One linear pre-pass; see buildLineage().
    const lineage = buildLineage();
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
      let id: string | null = null;
      let cwd: string | null = null;
      let branch: string | null = null;
      let model: string | null = null;
      let provider: string | null = null;
      let cliVersion: string | null = null;
      let startedAt: number | null = null;
      let endedAt: number | null = null;
      let turns = 0;
      for (const r of records) {
        const payload = asDict(r.payload);
        const rtype = asStr(r.type);
        if (rtype === "session_meta") {
          id ??= asStr(payload.id) ?? asStr(payload.session_id);
          cwd ??= asStr(payload.cwd);
          branch ??= asStr(asDict(payload.git).branch);
          provider ??= asStr(payload.model_provider);
          cliVersion ??= asStr(payload.cli_version);
        } else if (rtype === "turn_context") {
          model ??= asStr(payload.model);
        }
        const ts = isoToEpochSec(r.timestamp);
        if (ts != null) {
          startedAt ??= ts;
          endedAt = ts;
        }
        if (rtype === "event_msg" && asStr(payload.type) === "user_message") turns++;
        else if (
          rtype === "response_item" &&
          asStr(payload.type) === "message" &&
          asStr(payload.role) === "assistant"
        )
          turns++;
      }
      if (!id) continue;
      const edge = lineage.get(`codex:${id}`);
      const agentType = edge?.agentType ?? null;
      out.push({
        id: `codex:${id}`,
        agent: "codex",
        cwd,
        startedAt,
        endedAt,
        title: edge ? `[${agentType ?? "codex"} subagent]` : null, // spawned children have no native title
        model,
        provider,
        gitBranch: branch,
        parentSessionId: edge?.parentId ?? null,
        forkedFrom: null,
        turnCount: turns,
        eventCount: turns,
        sources: [fileSource(path, "jsonl")],
        fingerprint: `${records.length}:${endedAt ?? 0}`,
        meta: edge ? { cliVersion, agentType, isSubagent: true } : { cliVersion },
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
        const first = readJsonl(path)[0];
        if (first && asStr(asDict(first.payload).id) === nativeId) {
          target = path;
          break;
        }
      }
    }
    if (!target) return [];

    const out = new EventBuilder();
    for (const r of readJsonl(target)) {
      const rtype = asStr(r.type);
      const payload = asDict(r.payload);
      const ptype = asStr(payload.type);
      const ts = isoToEpochSec(r.timestamp);

      if (rtype === "event_msg") {
        if (ptype === "user_message") {
          out.push("user", userText(payload), ts);
        } else if (ptype === "token_count") {
          const last = asDict(asDict(payload.info).last_token_usage);
          out.push("usage", compact(payload.info), ts, {
            subtype: "token_count",
            tokens: asNum(last.total_tokens),
          });
        } else if (ptype && ptype !== "agent_message") {
          // task_started/complete, patch_apply_end, web_search_end, turn_aborted,
          // thread_rolled_back, context_compacted — engine lifecycle
          out.push("system", compact(payload), ts, { subtype: ptype });
        }
      } else if (rtype === "response_item") {
        if (ptype === "message") {
          const role = asStr(payload.role) ?? "assistant";
          if (role === "user" || role === "developer") continue; // dedup with event stream
          out.push("assistant", flattenContent(payload.content), ts);
        } else if (ptype === "reasoning") {
          const summary = payload.summary;
          if (summary) out.push("thinking", compact(summary), ts, { subtype: "summary" });
        } else if (ptype && TOOL_CALL_TYPES.has(ptype)) {
          const name = asStr(payload.name) ?? (ptype === "web_search_call" ? "web_search" : ptype);
          out.push("tool_call", compact(payload.arguments ?? payload.input ?? payload.query ?? payload), ts, {
            name,
            callId: asStr(payload.call_id) ?? asStr(payload.id),
            subtype: ptype,
          });
        } else if (ptype && TOOL_OUT_TYPES.has(ptype)) {
          out.push("tool_result", compact(payload.output ?? payload), ts, {
            callId: asStr(payload.call_id),
            subtype: ptype,
          });
        }
      } else if (rtype === "compacted" || rtype === "world_state") {
        out.push("system", compact(payload), ts, { subtype: rtype });
      }
    }
    return out.events;
  },
};
