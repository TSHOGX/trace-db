/**
 * parsers/gemini.ts — Gemini CLI sessions.
 *
 * Store: ~/.gemini/tmp/<slug>/chats/session-*.{json,jsonl}. Two formats coexist:
 *   OLD .json   one object { sessionId, startTime, lastUpdated, messages:[…], summary? }
 *   NEW .jsonl  a change log: line 0 header { sessionId, startTime, projectHash, kind };
 *               then { "$set": { messages:[…] } } snapshot; then per-message
 *               append records { id, timestamp, type, content, model?, thoughts?,
 *               toolCalls?, tokens? }. Folded on read.
 * A sessionId can span multiple chunk files → merge by sessionId, order by
 * startTime, dedup messages by id. The synthetic first turn (user message whose
 * text opens with "<session_context>") is filtered out.
 *
 * Event mapping:
 *   type=user            → user
 *   type=gemini          → assistant text (usage from .tokens.total); .thoughts[]
 *                          → thinking; .toolCalls[] → tool_call (+ tool_result)
 *   type=info | error    → system (subtype = the native type)
 *
 * cwd not in transcript → recovered from <slug>/.project_root or projects.json.
 * No native title; old .json may carry a summary. Tree A via message id (no
 * parent pointer in the format). No cross-session fork edge.
 */

import { readdirSync, readFileSync, statSync } from "fs";
import { join } from "path";
import { GEMINI_PROJECTS_JSON, GEMINI_TMP } from "../paths.js";
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
  parseJsonLine,
  readJson,
} from "../util.js";

interface GMsg {
  id?: string;
  timestamp?: string;
  type?: string; // user | gemini | info | error
  content?: unknown;
  message?: unknown;
  model?: unknown;
  thoughts?: unknown;
  toolCalls?: unknown;
  tokens?: unknown;
}

interface Chunk {
  path: string;
  sessionId: string | null;
  startTime: string | null;
  lastUpdated: string | null;
  summary: string | null;
  projectHash: string | null;
  slug: string;
  messages: GMsg[];
}

/** Fold one file (old .json or new .jsonl) into a chunk. */
function readChunk(path: string, slug: string): Chunk | null {
  const c: Chunk = {
    path,
    sessionId: null,
    startTime: null,
    lastUpdated: null,
    summary: null,
    projectHash: null,
    slug,
    messages: [],
  };
  if (path.endsWith(".jsonl")) {
    let text: string;
    try {
      text = readFileSync(path, "utf8");
    } catch {
      return null;
    }
    for (const line of text.split("\n")) {
      const r = parseJsonLine(line);
      if (!r) continue;
      if (r.sessionId) {
        c.sessionId ??= asStr(r.sessionId);
        c.startTime ??= asStr(r.startTime);
        c.projectHash ??= asStr(r.projectHash);
        c.lastUpdated = asStr(r.lastUpdated) ?? c.lastUpdated;
        continue;
      }
      if (r.$set && typeof r.$set === "object") {
        const set = r.$set as Record<string, unknown>;
        if (Array.isArray(set.messages)) c.messages.push(...(set.messages as GMsg[]));
        continue;
      }
      if (r.type && (r.type === "user" || r.type === "gemini" || r.type === "info" || r.type === "error")) {
        c.messages.push(r as GMsg);
      }
    }
  } else {
    const obj = asDict(readJson(path));
    if (!obj) return null;
    c.sessionId = asStr(obj.sessionId);
    c.startTime = asStr(obj.startTime);
    c.lastUpdated = asStr(obj.lastUpdated);
    c.summary = asStr(obj.summary);
    c.projectHash = asStr(obj.projectHash);
    if (Array.isArray(obj.messages)) c.messages.push(...(obj.messages as GMsg[]));
  }
  return c.sessionId ? c : null;
}

/** All (slug, chatfile) pairs under ~/.gemini/tmp. */
function listChunkFiles(): { path: string; slug: string }[] {
  let slugs: string[];
  try {
    slugs = readdirSync(GEMINI_TMP);
  } catch {
    return [];
  }
  const out: { path: string; slug: string }[] = [];
  for (const slug of slugs) {
    const chatsDir = join(GEMINI_TMP, slug, "chats");
    let entries: string[];
    try {
      if (!statSync(chatsDir).isDirectory()) continue;
      entries = readdirSync(chatsDir);
    } catch {
      continue;
    }
    for (const e of entries) {
      if (e.startsWith("session-") && (e.endsWith(".json") || e.endsWith(".jsonl")))
        out.push({ path: join(chatsDir, e), slug });
    }
  }
  return out;
}

/** Group chunks by sessionId. */
function bySession(): Map<string, Chunk[]> {
  const map = new Map<string, Chunk[]>();
  for (const { path, slug } of listChunkFiles()) {
    const c = readChunk(path, slug);
    if (!c || !c.sessionId) continue;
    const arr = map.get(c.sessionId);
    if (arr) arr.push(c);
    else map.set(c.sessionId, [c]);
  }
  return map;
}

function resolveCwd(slug: string): string | null {
  try {
    const t = readFileSync(join(GEMINI_TMP, slug, ".project_root"), "utf8").trim();
    if (t) return t;
  } catch {
    /* ignore */
  }
  const projects = asDict(asDict(readJson(GEMINI_PROJECTS_JSON)).projects);
  for (const [dir, s] of Object.entries(projects)) if (s === slug) return dir;
  return null;
}

function isSynthetic(m: GMsg): boolean {
  if (m.type !== "user") return false;
  return flattenContent(m.content ?? m.message).startsWith("<session_context>");
}

/** Merge + order + dedup a session's chunks into one message list. */
function mergeMessages(chunks: Chunk[]): GMsg[] {
  const sorted = [...chunks].sort((a, b) => (a.startTime ?? "").localeCompare(b.startTime ?? ""));
  const seen = new Set<string>();
  const merged: GMsg[] = [];
  for (const c of sorted) {
    for (const m of c.messages) {
      if (m.id && seen.has(m.id)) continue;
      if (m.id) seen.add(m.id);
      merged.push(m);
    }
  }
  merged.sort((a, b) => (a.timestamp ?? "").localeCompare(b.timestamp ?? ""));
  return merged;
}

export const geminiParser: Parser = {
  agent: "gemini",

  listSessions(sinceEpochSec) {
    const out: UnifiedSession[] = [];
    for (const [sessionId, chunks] of bySession()) {
      const merged = mergeMessages(chunks);
      const first = chunks.reduce((a, b) => ((a.startTime ?? "") <= (b.startTime ?? "") ? a : b));
      const last = chunks.reduce((a, b) => ((a.lastUpdated ?? "") >= (b.lastUpdated ?? "") ? a : b));
      const startedAt = isoToEpochSec(first.startTime);
      const endedAt = isoToEpochSec(last.lastUpdated) ?? startedAt;
      if (sinceEpochSec != null && (endedAt ?? 0) < sinceEpochSec) continue;
      let turns = 0;
      let model: string | null = null;
      for (const m of merged) {
        if ((m.type === "user" && !isSynthetic(m)) || m.type === "gemini") turns++;
        if (m.type === "gemini") model ??= asStr(m.model);
      }
      const summary = chunks.map((c) => c.summary).find((s) => s) ?? null;
      out.push({
        id: `gemini:${sessionId}`,
        agent: "gemini",
        cwd: resolveCwd(first.slug),
        startedAt,
        endedAt,
        title: null,
        model,
        provider: "google",
        gitBranch: null,
        parentSessionId: null,
        forkedFrom: null,
        turnCount: turns,
        eventCount: turns,
        sources: chunks.map((c) => fileSource(c.path, c.path.endsWith(".jsonl") ? "jsonl" : "json")),
        fingerprint: `${merged.length}:${endedAt ?? 0}`,
        meta: { summary, projectHash: first.projectHash, sourceFileCount: chunks.length },
      });
    }
    return out;
  },

  readEvents(nativeId): Event[] {
    const chunks = bySession().get(nativeId);
    if (!chunks) return [];
    const merged = mergeMessages(chunks);
    const out = new EventBuilder();
    for (const m of merged) {
      if (isSynthetic(m)) continue;
      const ts = isoToEpochSec(m.timestamp);
      const type = m.type;
      const id = asStr(m.id);
      if (type === "user") {
        out.push("user", flattenContent(m.content ?? m.message), ts, { nativeId: id });
      } else if (type === "gemini") {
        const tok = asNum(asDict(m.tokens).total);
        out.push("assistant", flattenContent(m.content ?? m.message), ts, { nativeId: id, tokens: tok });
        if (Array.isArray(m.thoughts)) {
          for (const t of m.thoughts) {
            const td = asDict(t);
            const txt = [asStr(td.subject), asStr(td.description)].filter(Boolean).join(": ");
            out.push("thinking", txt || compact(t), ts, { nativeId: id });
          }
        }
        if (Array.isArray(m.toolCalls)) {
          for (const tc of m.toolCalls) {
            const t = asDict(tc);
            const name = asStr(t.name);
            const callId = asStr(t.callId) ?? asStr(t.id);
            out.push("tool_call", compact(t.args), ts, { name, callId, nativeId: id });
            const result = t.resultDisplay ?? t.result;
            if (result != null)
              out.push("tool_result", compact(result), ts, {
                name,
                callId,
                nativeId: id,
                isError: asStr(t.status) === "error" || t.success === false,
              });
          }
        }
      } else if (type === "info" || type === "error") {
        out.push("system", flattenContent(m.content ?? m.message), ts, { subtype: type, nativeId: id });
      }
    }
    return out.events;
  },
};
