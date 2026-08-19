/**
 * parsers/opencode.ts — OpenCode sessions.
 *
 * The odd one out: not per-file but one shared SQLite DB (multi-GB, tens of
 * thousands of sessions). We open it READ-ONLY and query directly rather than
 * shell out to `opencode export` per session. Layout (ground-truthed 2026-07):
 *   session(id, parent_id, directory, title, model, agent, provider via message,
 *           time_created/updated ms, tokens_*, cost, …)
 *   message(id, session_id, time_created, data JSON{role,modelID,providerID,
 *           parentID,tokens,cost,finish,mode,path,time})
 *   part(id, message_id, session_id, time_created, data JSON{type,text,tool,
 *        callID,state,…})
 * source_path is "<db_path>#<session_id>" (no per-session file).
 *
 * Part → event mapping (one event per part):
 *   text      → assistant/user text (role from message.data)
 *   reasoning → thinking
 *   tool      → tool_call (state.input, callID) + tool_result (state.output,
 *               state.status→isError), two events sharing callID
 *   patch     → system subtype=patch (code edit)
 *   step-start/step-finish/snapshot/file → system subtype=<type> (structural markers)
 *
 * Tree A: message.data.parentID. Tree B: session.parent_id (subagent child rows,
 * titled "… (@explore subagent)"). Both preserved.
 */

import { existsSync } from "fs";
import { statSync } from "fs";
import { Database } from "bun:sqlite";
import { OPENCODE_DB_CANDIDATES } from "../paths.js";
import type { Event, EventKind, Parser, UnifiedSession } from "../types.js";
import { asDict, asNum, asStr, compact, EventBuilder, millisToEpochSec } from "../util.js";

function dbPath(): string | null {
  for (const p of OPENCODE_DB_CANDIDATES) if (existsSync(p)) return p;
  return null;
}

let _ro: Database | null = null;
function ro(): Database {
  if (_ro) return _ro;
  const p = dbPath();
  if (!p) throw new Error("opencode db not found");
  _ro = new Database(p, { readonly: true });
  return _ro;
}

interface SessRow {
  id: string;
  parent_id: string | null;
  directory: string | null;
  title: string | null;
  agent: string | null;
  model: string | null;
  time_created: number | null;
  time_updated: number | null;
  cost: number | null;
  tokens_input: number | null;
  tokens_output: number | null;
  tokens_reasoning: number | null;
  tokens_cache_read: number | null;
  tokens_cache_write: number | null;
}

export const opencodeParser: Parser = {
  agent: "opencode",

  listSessions(sinceEpochSec) {
    const p = dbPath();
    if (!p) return [];
    const d = ro();
    const rows = d
      .query<SessRow, []>(
        `SELECT id, parent_id, directory, title, agent, model, time_created, time_updated,
                cost, tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write
         FROM session ORDER BY time_updated DESC`
      )
      .all();
    // message count + a cheap provider probe per session in one grouped query
    const counts = new Map<string, number>();
    for (const c of d
      .query<{ session_id: string; n: number }, []>(
        "SELECT session_id, COUNT(*) n FROM message GROUP BY session_id"
      )
      .all())
      counts.set(c.session_id, c.n);

    let bytes: number | null = null;
    let mtime: number | null = null;
    try {
      const st = statSync(p);
      bytes = st.size;
      mtime = Math.floor(st.mtimeMs / 1000);
    } catch {
      /* ignore */
    }

    const out: UnifiedSession[] = [];
    for (const r of rows) {
      const started = millisToEpochSec(r.time_created);
      const ended = millisToEpochSec(r.time_updated) ?? started;
      if (sinceEpochSec != null && (ended ?? 0) < sinceEpochSec) continue;
      const n = counts.get(r.id) ?? 0;
      out.push({
        id: `opencode:${r.id}`,
        agent: "opencode",
        cwd: r.directory,
        startedAt: started,
        endedAt: ended,
        title: r.title || null,
        model: r.model,
        provider: null,
        gitBranch: null,
        parentSessionId: r.parent_id ? `opencode:${r.parent_id}` : null,
        forkedFrom: null,
        turnCount: n,
        eventCount: n,
        sources: [{ path: `${p}#${r.id}`, kind: "sqlite", bytes, mtime }],
        // fingerprint: message count + updated time (grows on any new message)
        fingerprint: `${n}:${r.time_updated ?? 0}`,
        meta: {
          agent: r.agent,
          cost: r.cost,
          tokensInput: r.tokens_input,
          tokensOutput: r.tokens_output,
          tokensReasoning: r.tokens_reasoning,
          tokensCacheRead: r.tokens_cache_read,
          tokensCacheWrite: r.tokens_cache_write,
        },
      });
    }
    return out;
  },

  readEvents(nativeId): Event[] {
    if (!dbPath()) return [];
    const d = ro();
    const rows = d
      .query<
        {
          message_id: string;
          message_data: string;
          message_time: number;
          part_data: string | null;
          part_time: number | null;
        },
        [string]
      >(
        `SELECT m.id AS message_id, m.data AS message_data, m.time_created AS message_time,
                p.data AS part_data, p.time_created AS part_time
         FROM message m
         LEFT JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?
         ORDER BY m.time_created ASC, m.id ASC, p.time_created ASC, p.id ASC`
      )
      .all(nativeId);

    interface Grp {
      data: Record<string, unknown>;
      time: number;
      parts: Record<string, unknown>[];
    }
    const grouped = new Map<string, Grp>();
    const order: string[] = [];
    for (const row of rows) {
      let g = grouped.get(row.message_id);
      if (!g) {
        g = { data: safeJson(row.message_data), time: row.message_time, parts: [] };
        grouped.set(row.message_id, g);
        order.push(row.message_id);
      }
      if (row.part_data) g.parts.push(safeJson(row.part_data));
    }

    const out = new EventBuilder();
    for (const mid of order) {
      const g = grouped.get(mid)!;
      const role: EventKind = normalizeRole(asStr(g.data.role));
      const ts = millisToEpochSec(asDict(g.data.time).created) ?? millisToEpochSec(g.time);
      const parentId = asStr(g.data.parentID);
      const tok = asNum(asDict(g.data.tokens).output) ?? asNum(asDict(g.data.tokens).total);
      for (const part of g.parts) {
        const ptype = asStr(part.type) ?? "";
        if (ptype === "text") {
          out.push(role, asStr(part.text) ?? "", ts, { nativeId: mid, parentId, tokens: tok });
        } else if (ptype === "reasoning") {
          out.push("thinking", asStr(part.text) ?? "", ts, { nativeId: mid, parentId });
        } else if (ptype === "tool") {
          const name = asStr(part.tool) ?? null;
          const callId = asStr(part.callID);
          const state = asDict(part.state);
          out.push("tool_call", compact(state.input), ts, { name, callId, nativeId: mid, parentId });
          const output = state.output ?? asDict(state.metadata).output;
          if (output != null || asStr(state.status) === "error") {
            out.push("tool_result", compact(output ?? state.error ?? ""), ts, {
              name,
              callId,
              isError: asStr(state.status) === "error",
              nativeId: mid,
              parentId,
            });
          }
        } else if (ptype === "patch") {
          out.push("system", compact(part), ts, { subtype: "patch", nativeId: mid, parentId });
        } else if (ptype) {
          // step-start/step-finish/snapshot/file → structural markers
          out.push("system", compact(part), ts, { subtype: ptype, nativeId: mid, parentId });
        }
      }
    }
    return out.events;
  },
};

function safeJson(s: string | null): Record<string, unknown> {
  if (!s) return {};
  try {
    const v = JSON.parse(s);
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
}

/** message.data.role → the text-event kind for that message. */
function normalizeRole(role: string | null): EventKind {
  if (role === "user") return "user";
  if (role === "system") return "system";
  return "assistant";
}
