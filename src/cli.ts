#!/usr/bin/env bun
/**
 * TraceDB's unified query CLI.
 * ms-level FTS5 search, cross-agent ranking, structure-preserving reconstruction,
 * and pointer-based export back to the native trace.
 *
 * Commands (all support --json for cron consumption):
 *   search <query>   FTS5 discovery, BM25 ranked, deduped to one row per session
 *   show <id>        reconstruct one session's events (sanitized display budget)
 *   recent           browse newest sessions
 *   stats            per-agent counts, span, DB size
 *   sources <id>     the raw-trace pointer(s) for a session
 *   export <id>      reconstruct native trace(s) from the pointer(s) → --out DIR
 *   reindex          rebuild events_fts from events (no native-store reads)
 *   ingest           mechanical scan → upsert (delegates to ingest.ts)
 *
 * Exit codes via errors.ts: 0 ok · 1 generic · 2 not-found · 4 usage.
 */

import { parseArgs } from "util";
import { copyFileSync, mkdirSync, statSync, writeFileSync } from "fs";
import { basename, join } from "path";
import { Database } from "bun:sqlite";
import type { SQLQueryBindings } from "bun:sqlite";
import { db, rebuildFts } from "./db.js";
import { ingest } from "./ingest.js";
import { NotFoundError, SessError, UsageError } from "./errors.js";
import { TRACE_DB } from "./paths.js";
import { redact, truncate } from "./util.js";
import { AGENTS, EVENT_KINDS, NON_CONTENT_KINDS, type Agent, type EventKind } from "./types.js";

// ---------- helpers ----------

function out(value: unknown, json: boolean, human: () => string): void {
  if (json) process.stdout.write(JSON.stringify(value, null, 2) + "\n");
  else process.stdout.write(human() + "\n");
}

function parseAgent(v: string | undefined): Agent[] | null {
  if (!v || v === "all") return null;
  const list = v.split(",").map((s) => s.trim()) as Agent[];
  for (const a of list) if (!AGENTS.includes(a)) throw new UsageError(`unknown agent: ${a}`);
  return list;
}

function parseKind(v: string | undefined): EventKind[] | null {
  if (!v || v === "all") return null;
  const list = v.split(",").map((s) => s.trim()) as EventKind[];
  for (const k of list) if (!EVENT_KINDS.includes(k)) throw new UsageError(`unknown kind: ${k}`);
  return list;
}

/** --since accepts YYYY-MM-DD or an integer number of days ("7"). → epoch sec. */
function parseSince(v: string | undefined): number | null {
  if (!v) return null;
  if (/^\d+$/.test(v)) return Math.floor(Date.now() / 1000) - Number(v) * 86400;
  const ms = Date.parse(v);
  if (Number.isNaN(ms)) throw new UsageError(`bad --since: ${v}`);
  return Math.floor(ms / 1000);
}

function fmtTime(epoch: number | null): string {
  if (epoch == null) return "—";
  return new Date(epoch * 1000).toISOString().replace("T", " ").slice(0, 16);
}

/** FTS5 operators/syntax that signal the user is hand-writing a query. */
const FTS_OPERATOR = /(^|\s)(OR|AND|NOT|NEAR)(\s|$)|[":*()]/;

/** Quote one token as an FTS5 phrase (handles embedded quotes). */
function quotePhrase(s: string): string {
  return `"${s.replace(/"/g, '""')}"`;
}

/**
 * Plan an FTS5 MATCH expression from a raw query.
 *
 * The old behavior quoted the WHOLE query as one phrase, so `netlify deploy
 * error` only matched those three tokens ADJACENT — brittle for episodic recall.
 * The planner instead ORs the whole-phrase match (precision: bm25 rewards the
 * adjacent hit) with an AND of per-token phrases (recall: the tokens may be
 * scattered across a long turn). Single-token queries collapse to one phrase.
 *
 * If the user already wrote FTS5 syntax (OR / NEAR / quotes / prefix `*` /
 * parens), we pass it through untouched — power users keep full control.
 */
function planFts(q: string): string {
  const trimmed = q.trim();
  if (FTS_OPERATOR.test(trimmed)) return trimmed;
  const terms = trimmed.split(/\s+/).filter(Boolean);
  if (terms.length <= 1) return quotePhrase(trimmed);
  const whole = quotePhrase(trimmed);
  const andAll = terms.map(quotePhrase).join(" AND ");
  return `${whole} OR (${andAll})`;
}

interface SessionRowFull {
  id: string;
  agent: string;
  cwd: string | null;
  started_at: number | null;
  ended_at: number | null;
  title: string | null;
  model: string | null;
  provider: string | null;
  git_branch: string | null;
  parent_session_id: string | null;
  forked_from: string | null;
  turn_count: number;
  event_count: number;
  fingerprint: string | null;
  meta: string | null;
}

function getSession(id: string): SessionRowFull {
  const s = db().query<SessionRowFull, [string]>("SELECT * FROM sessions WHERE id = ?").get(id);
  if (!s) throw new NotFoundError(`no session ${id}`);
  return s;
}

interface RawSourceRow {
  path: string;
  kind: string;
  bytes: number | null;
  mtime: number | null;
}

function getSources(id: string): RawSourceRow[] {
  return db()
    .query<RawSourceRow, [string]>(
      "SELECT path, kind, bytes, mtime FROM raw_sources WHERE session_id = ? ORDER BY path"
    )
    .all(id);
}

// ---------- commands ----------

// ---------- search scoring knobs ----------

/**
 * Post-aggregation weights (higher combined score = better; opposite of raw
 * bm25). Overridable via env so the ranking is tunable without a rebuild. Set
 * every non-BEST weight to 0 and the ranking collapses back to pure bm25 order
 * (today's behavior). `bm25` itself can't weight by kind (single indexed
 * column), so kind weighting is applied here as a per-session multiplier.
 */
const W = {
  best: envNum("TRACEDB_W_BEST", 1.0),
  cover: envNum("TRACEDB_W_COVER", 0.3),
  kind: envNum("TRACEDB_W_KIND", 0.25),
  recency: envNum("TRACEDB_W_RECENCY", 0.4),
  title: envNum("TRACEDB_W_TITLE", 0.2),
  lineage: envNum("TRACEDB_W_LINEAGE", 0.15),
};
const DEFAULT_HALFLIFE_DAYS = envNum("TRACEDB_HALFLIFE_DAYS", 30);

/** How much a hit's kind says about the session's topic (user > assistant > …). */
const KIND_BONUS: Record<string, number> = {
  user: 1.0,
  assistant: 0.8,
  system: 0.5,
  thinking: 0.4,
  tool_call: 0.3,
};

const PER_SESSION_HIT_CAP = 50; // bound work for stopword-ish queries
const CANDIDATE_HIT_CAP = 5000; // total candidate hits pulled before aggregation
const MAIN_WINDOW = 5; // ±N events around the strongest hit
const SECONDARY_WINDOW = 3; // ±N around each distant secondary hit
const MAX_SECONDARY = 2; // extra hit clusters beyond the main one
const CTX_CAP = 200; // char cap for context/bookend previews (kept scannable)

function envNum(name: string, dflt: number): number {
  const v = process.env[name];
  if (v == null || v.trim() === "") return dflt;
  const n = Number(v);
  return Number.isFinite(n) ? n : dflt;
}

/** Short preview: redact secrets, collapse whitespace, hard cap. */
function preview(text: string, cap = CTX_CAP): string {
  const s = redact(text).replace(/\s+/g, " ").trim();
  return s.length <= cap ? s : s.slice(0, cap) + "…";
}

// ---------- lineage (tree B) resolution ----------

interface LineageEdge {
  parent: string | null; // parent_session_id ("agent:nativeid")
  forked: string | null; // forked_from origin, "#…" suffix stripped
}

/**
 * Load every session's tree-B edges once (a few thousand tiny rows — far cheaper
 * than walking the DB per candidate). `forked_from` is "agent:sid#messageUuid";
 * we keep only the "agent:sid" origin.
 */
function loadLineageEdges(): Map<string, LineageEdge> {
  const rows = db()
    .query<{ id: string; parent_session_id: string | null; forked_from: string | null }, []>(
      "SELECT id, parent_session_id, forked_from FROM sessions"
    )
    .all();
  const m = new Map<string, LineageEdge>();
  for (const r of rows) {
    const forked = r.forked_from ? r.forked_from.split("#")[0] : null;
    m.set(r.id, { parent: r.parent_session_id, forked });
  }
  return m;
}

/**
 * Walk up parent → forked origin to the lineage root, cycle-guarded. An edge to
 * an unknown session stops the walk there (that id becomes the root). Collapsing
 * to the root groups "parent + N subagents/forks" into one strong unit instead
 * of N competing rows.
 */
function lineageRoot(id: string, edges: Map<string, LineageEdge>): string {
  let cur = id;
  const seen = new Set<string>([cur]);
  for (;;) {
    const e = edges.get(cur);
    const next = e?.parent ?? e?.forked ?? null;
    if (!next || !edges.has(next) || seen.has(next)) return cur;
    seen.add(next);
    cur = next;
  }
}

// ---------- search ----------

interface Hit {
  eid: number;
  idx: number;
  kind: string;
  bm25: number; // lower = better
  snippet: string;
}

interface ContextEvent {
  idx: number;
  kind: string;
  label: string;
  text: string;
}

function cmdSearch(argv: string[]): void {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      agent: { type: "string" },
      cwd: { type: "string" },
      since: { type: "string" },
      kind: { type: "string" },
      limit: { type: "string" },
      "half-life": { type: "string" },
      "no-recency": { type: "boolean", default: false },
      "no-collapse": { type: "boolean", default: false },
      json: { type: "boolean", default: false },
    },
  });
  const query = positionals.join(" ").trim();
  if (!query) throw new UsageError("search needs a query");

  const agents = parseAgent(values.agent);
  const since = parseSince(values.since);
  const limit = values.limit ? Number(values.limit) : 20;
  const kinds = parseKind(values.kind);
  const halfLife = values["half-life"] ? Number(values["half-life"]) : DEFAULT_HALFLIFE_DAYS;
  const useRecency = !values["no-recency"] && W.recency > 0 && halfLife > 0;
  const collapse = !values["no-collapse"];
  const now = Math.floor(Date.now() / 1000);
  const qTerms = query.toLowerCase().split(/\s+/).filter(Boolean);

  const where: string[] = ["events_fts MATCH ?"];
  const params: SQLQueryBindings[] = [planFts(query)];
  if (agents) {
    where.push(`s.agent IN (${agents.map(() => "?").join(",")})`);
    params.push(...agents);
  }
  if (values.cwd) {
    where.push("s.cwd LIKE ?");
    params.push(`%${values.cwd}%`);
  }
  if (since != null) {
    where.push("s.ended_at >= ?");
    params.push(since);
  }
  if (kinds) {
    where.push(`ev.kind IN (${kinds.map(() => "?").join(",")})`);
    params.push(...kinds);
  }

  // Stage 1 — candidate hits, capped per session and overall so a stopword-ish
  // query can't fan out unbounded. bm25()/snippet() can't survive GROUP BY, so
  // we keep raw event rows and aggregate in TS (also where kind weighting lives).
  const hitRows = db()
    .query<
      { sid: string; eid: number; idx: number; kind: string; score: number; snippet: string },
      SQLQueryBindings[]
    >(
      `WITH hits AS (
         SELECT ev.session_id AS sid, ev.id AS eid, ev.idx AS idx, ev.kind AS kind,
                bm25(events_fts) AS score,
                snippet(events_fts, 0, '«', '»', '…', 12) AS snippet
         FROM events_fts
         JOIN events ev ON ev.id = events_fts.rowid
         JOIN sessions s ON s.id = ev.session_id
         WHERE ${where.join(" AND ")}
       ),
       ranked AS (
         SELECT *, ROW_NUMBER() OVER (PARTITION BY sid ORDER BY score ASC) AS rn FROM hits
       )
       SELECT sid, eid, idx, kind, score, snippet FROM ranked
       WHERE rn <= ${PER_SESSION_HIT_CAP}
       ORDER BY score ASC
       LIMIT ${CANDIDATE_HIT_CAP}`
    )
    .all(...params);

  if (!hitRows.length) {
    out({ query, count: 0, results: [] }, values.json, () => `no matches for "${query}"`);
    return;
  }

  // group hits by session (already score-ordered, so [0] is each session's best)
  const bySession = new Map<string, Hit[]>();
  for (const r of hitRows) {
    let arr = bySession.get(r.sid);
    if (!arr) bySession.set(r.sid, (arr = []));
    arr.push({ eid: r.eid, idx: r.idx, kind: r.kind, bm25: r.score, snippet: r.snippet });
  }

  // session metadata for all candidates in one query
  const sids = [...bySession.keys()];
  const metaRows = db()
    .query<
      {
        id: string;
        agent: string;
        cwd: string | null;
        ended_at: number | null;
        title: string | null;
        turn_count: number;
      },
      SQLQueryBindings[]
    >(
      `SELECT s.id, s.agent, s.cwd, s.ended_at, s.title, s.turn_count
       FROM sessions s
       WHERE s.id IN (${sids.map(() => "?").join(",")})`
    )
    .all(...sids);
  const meta = new Map(metaRows.map((m) => [m.id, m]));

  // Stage 2 — per-session scoring. bm25 is negative (lower=better) → relevance
  // = -bm25; we min-max normalize across candidates so weights are comparable.
  const rels = [...bySession.values()].map((h) => -h[0].bm25);
  const relMin = Math.min(...rels);
  const relMax = Math.max(...rels);
  const relSpan = relMax - relMin || 1;
  const covMax = Math.log1p(Math.max(...[...bySession.values()].map((h) => h.length))) || 1;

  interface Scored {
    id: string;
    score: number;
    breakdown: Record<string, number>;
    hits: Hit[];
    m: (typeof metaRows)[number];
  }
  const scored: Scored[] = [];
  for (const [sid, hits] of bySession) {
    const m = meta.get(sid);
    if (!m) continue; // candidate session vanished (shouldn't happen); skip
    const normBest = (-hits[0].bm25 - relMin) / relSpan;
    const cover = Math.log1p(hits.length) / covMax;
    const kindBonus = KIND_BONUS[hits[0].kind] ?? 0.2;
    const recency =
      useRecency && m.ended_at != null
        ? Math.exp((-Math.LN2 * (now - m.ended_at)) / 86400 / halfLife)
        : 0;
    const hay = (m.title ?? "").toLowerCase();
    const titleHit = hay.trim() && qTerms.some((t) => hay.includes(t)) ? 1 : 0;
    const breakdown = {
      best: W.best * normBest,
      cover: W.cover * cover,
      kind: W.kind * kindBonus,
      recency: W.recency * recency,
      title: W.title * titleHit,
    };
    const score = Object.values(breakdown).reduce((a, b) => a + b, 0);
    scored.push({ id: sid, score, breakdown, hits, m });
  }

  // Stage 3 — collapse tree-B lineage (fork/subagent) to a single root unit.
  const edges = collapse ? loadLineageEdges() : null;
  const scoredById = new Map(scored.map((s) => [s.id, s]));
  const groups = new Map<string, Scored[]>();
  for (const s of scored) {
    const root = edges ? lineageRoot(s.id, edges) : s.id;
    let g = groups.get(root);
    if (!g) groups.set(root, (g = []));
    g.push(s);
  }

  interface Result {
    rep: Scored;
    root: string;
    related: { id: string; agent: string; role: string; score: number }[];
    aggScore: number;
  }
  const results: Result[] = [];
  for (const [root, members] of groups) {
    members.sort((a, b) => b.score - a.score);
    const rep = members[0];
    const others = members.slice(1);
    const related = others.map((o) => {
      const e = edges?.get(o.id);
      const role = e?.forked ? "fork" : e?.parent ? "subagent" : "member";
      return { id: o.id, agent: o.m.agent, role, score: round(o.score) };
    });
    const aggScore = rep.score + W.lineage * others.reduce((a, o) => a + o.score, 0);
    results.push({ rep, root, related, aggScore });
  }
  results.sort((a, b) => b.aggScore - a.aggScore);
  const top = results.slice(0, limit);

  // Stage 4 — context assembly for the top reps in TWO batch queries (no N+1):
  // (a) the ±window ranges around main + secondary hits, (b) the bookends.
  const repIds = top.map((t) => t.rep.id);
  const rangeClauses: string[] = [];
  const rangeParams: SQLQueryBindings[] = [];
  // remember which idxs are "hits" per session for labeling
  const secondaryByRep = new Map<string, Hit[]>();
  for (const t of top) {
    const hits = t.rep.hits;
    const main = hits[0];
    rangeClauses.push("(session_id = ? AND idx BETWEEN ? AND ?)");
    rangeParams.push(t.rep.id, main.idx - MAIN_WINDOW, main.idx + MAIN_WINDOW);
    // secondary hits: far enough from main and each other to be a distinct cluster
    const picked: Hit[] = [];
    for (const h of hits.slice(1)) {
      if (Math.abs(h.idx - main.idx) <= MAIN_WINDOW) continue;
      if (picked.some((p) => Math.abs(p.idx - h.idx) <= SECONDARY_WINDOW * 2)) continue;
      picked.push(h);
      rangeClauses.push("(session_id = ? AND idx BETWEEN ? AND ?)");
      rangeParams.push(t.rep.id, h.idx - SECONDARY_WINDOW, h.idx + SECONDARY_WINDOW);
      if (picked.length >= MAX_SECONDARY) break;
    }
    secondaryByRep.set(t.rep.id, picked);
  }

  const ctxRows = repIds.length
    ? db()
        .query<
          {
            session_id: string;
            idx: number;
            kind: string;
            name: string | null;
            subtype: string | null;
            is_error: number | null;
            text: string;
          },
          SQLQueryBindings[]
        >(
          `SELECT session_id, idx, kind, name, subtype, is_error, text
           FROM events WHERE ${rangeClauses.join(" OR ")} ORDER BY session_id, idx`
        )
        .all(...rangeParams)
    : [];
  const ctxByRep = new Map<string, ContextEvent[]>();
  for (const r of ctxRows) {
    let arr = ctxByRep.get(r.session_id);
    if (!arr) ctxByRep.set(r.session_id, (arr = []));
    let label = r.name ? `${r.kind}:${r.name}` : r.subtype ? `${r.kind}/${r.subtype}` : r.kind;
    if (r.is_error) label += " ✗";
    arr.push({ idx: r.idx, kind: r.kind, label, text: preview(r.text) });
  }

  // bookends: first user turn (the ask) + last assistant turn (the outcome)
  const bookRows = repIds.length
    ? db()
        .query<{ session_id: string; role: string; idx: number; kind: string; text: string }, SQLQueryBindings[]>(
          `SELECT e.session_id, 'ask' AS role, e.idx, e.kind, e.text FROM events e
             WHERE e.kind='user' AND e.session_id IN (${repIds.map(() => "?").join(",")})
               AND e.idx = (SELECT MIN(idx) FROM events WHERE session_id=e.session_id AND kind='user')
           UNION ALL
           SELECT e.session_id, 'outcome' AS role, e.idx, e.kind, e.text FROM events e
             WHERE e.kind='assistant' AND e.session_id IN (${repIds.map(() => "?").join(",")})
               AND e.idx = (SELECT MAX(idx) FROM events WHERE session_id=e.session_id AND kind='assistant')`
        )
        .all(...repIds, ...repIds)
    : [];
  const askByRep = new Map<string, { idx: number; kind: string; text: string }>();
  const outcomeByRep = new Map<string, { idx: number; kind: string; text: string }>();
  for (const b of bookRows) {
    const rec = { idx: b.idx, kind: b.kind, text: preview(b.text) };
    (b.role === "ask" ? askByRep : outcomeByRep).set(b.session_id, rec);
  }

  // Stage 5 — assemble the output shape.
  const payload = top.map((t) => {
    const m = t.rep.m;
    const ctx = ctxByRep.get(t.rep.id) ?? [];
    const secondaryIdx = new Set(secondaryByRep.get(t.rep.id)?.map((h) => h.idx) ?? []);
    const mainIdx = t.rep.hits[0].idx;
    return {
      id: t.rep.id,
      root: t.root === t.rep.id ? null : t.root,
      agent: m.agent,
      cwd: m.cwd,
      endedAt: m.ended_at,
      title: m.title,
      turnCount: m.turn_count,
      score: round(t.aggScore),
      scoreBreakdown: Object.fromEntries(
        Object.entries(t.rep.breakdown).map(([k, v]) => [k, round(v)])
      ),
      hitCount: t.rep.hits.length,
      related: t.related,
      ask: askByRep.get(t.rep.id) ?? null,
      outcome: outcomeByRep.get(t.rep.id) ?? null,
      mainIdx,
      mainSnippet: truncate(t.rep.hits[0].snippet),
      matchKind: t.rep.hits[0].kind,
      secondaryHits: [...secondaryIdx].map((idx) => {
        const h = t.rep.hits.find((x) => x.idx === idx)!;
        return { idx, kind: h.kind, snippet: truncate(h.snippet) };
      }),
      context: ctx,
    };
  });

  out({ query, count: payload.length, results: payload }, values.json, () => {
    if (!payload.length) return `no matches for "${query}"`;
    return payload
      .map((r) => {
        const lines: string[] = [];
        lines.push(r.id + (r.related.length ? `  (+${r.related.length} lineage)` : ""));
        lines.push(
          `  ${r.agent} · ${fmtTime(r.endedAt)} · ${r.turnCount} turns · score ${r.score} · ${r.cwd ?? "—"}`
        );
        if (r.title) lines.push(`  ${r.title}`);
        if (r.ask) lines.push(`  ↦ ask [${r.ask.idx}]: ${r.ask.text}`);
        lines.push(`  ✱ (${r.matchKind}) [${r.mainIdx}] ${r.mainSnippet.replace(/\n/g, " ")}`);
        for (const s of r.secondaryHits)
          lines.push(`  · (${s.kind}) [${s.idx}] ${s.snippet.replace(/\n/g, " ")}`);
        if (r.outcome) lines.push(`  ↤ outcome [${r.outcome.idx}]: ${r.outcome.text}`);
        return lines.join("\n");
      })
      .join("\n\n");
  });
}

function round(n: number): number {
  return Math.round(n * 1000) / 1000;
}

function cmdShow(argv: string[]): void {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      around: { type: "string" },
      window: { type: "string" },
      kind: { type: "string" },
      "include-tools": { type: "boolean", default: false },
      raw: { type: "boolean", default: false },
      json: { type: "boolean", default: false },
    },
  });
  const id = positionals[0];
  if (!id) throw new UsageError("show needs a session id");
  const session = getSession(id);

  // --kind selects exact kinds; else default is content-only (user+assistant),
  // and --include-tools widens to every kind (thinking/tool/system/usage).
  const explicitKinds = parseKind(values.kind);
  const includeTools = values["include-tools"];
  const around = values.around ? Number(values.around) : null;
  const window = values.window ? Number(values.window) : 10;
  const raw = values.raw;

  let evs = db()
    .query<
      {
        idx: number;
        kind: string;
        subtype: string | null;
        name: string | null;
        call_id: string | null;
        is_error: number | null;
        tokens: number | null;
        text: string;
        ts: number | null;
      },
      [string]
    >(
      `SELECT idx, kind, subtype, name, call_id, is_error, tokens, text, ts
       FROM events WHERE session_id = ? ORDER BY idx ASC`
    )
    .all(id);

  if (explicitKinds) {
    const set = new Set(explicitKinds);
    evs = evs.filter((m) => set.has(m.kind as EventKind));
  } else if (!includeTools) {
    const hidden = new Set<string>(NON_CONTENT_KINDS);
    evs = evs.filter((m) => !hidden.has(m.kind));
  }
  if (around != null) {
    const lo = Math.max(0, around - window);
    const hi = around + window;
    evs = evs.filter((m) => m.idx >= lo && m.idx <= hi);
  }

  const events = evs.map((m) => ({
    idx: m.idx,
    kind: m.kind,
    subtype: m.subtype,
    name: m.name,
    callId: m.call_id,
    isError: m.is_error == null ? null : m.is_error === 1,
    tokens: m.tokens,
    ts: m.ts,
    text: raw ? m.text : truncate(m.text),
  }));

  out(
    {
      id: session.id,
      agent: session.agent,
      cwd: session.cwd,
      startedAt: session.started_at,
      endedAt: session.ended_at,
      title: session.title,
      model: session.model,
      provider: session.provider,
      gitBranch: session.git_branch,
      parentSessionId: session.parent_session_id,
      forkedFrom: session.forked_from,
      turnCount: session.turn_count,
      eventCount: session.event_count,
      sources: getSources(id),
      meta: session.meta ? JSON.parse(session.meta) : {},
      events,
    },
    values.json,
    () => {
      const head =
        `${session.id} · ${session.agent} · ${fmtTime(session.started_at)} → ${fmtTime(session.ended_at)}\n` +
        `cwd: ${session.cwd ?? "—"}\n` +
        `model: ${session.model ?? "—"}${session.provider ? ` (${session.provider})` : ""}\n` +
        `title: ${session.title ?? "—"}\n` +
        (session.parent_session_id ? `parent: ${session.parent_session_id}\n` : "") +
        (session.forked_from ? `forked from: ${session.forked_from}\n` : "") +
        `turns: ${session.turn_count} · events: ${session.event_count}\n`;
      const body = events
        .map((m) => {
          let label = m.name ? `${m.kind}:${m.name}` : m.subtype ? `${m.kind}/${m.subtype}` : m.kind;
          if (m.isError) label += " ✗";
          return `[${m.idx}] ${label}: ${m.text.replace(/\n/g, " ")}`;
        })
        .join("\n");
      return head + "\n" + body;
    }
  );
}

function cmdRecent(argv: string[]): void {
  const { values } = parseArgs({
    args: argv,
    options: {
      agent: { type: "string" },
      cwd: { type: "string" },
      since: { type: "string" },
      limit: { type: "string" },
      json: { type: "boolean", default: false },
    },
  });
  const agents = parseAgent(values.agent);
  const since = parseSince(values.since);
  const limit = values.limit ? Number(values.limit) : 20;

  const where: string[] = ["1=1"];
  const params: SQLQueryBindings[] = [];
  if (agents) {
    where.push(`agent IN (${agents.map(() => "?").join(",")})`);
    params.push(...agents);
  }
  if (values.cwd) {
    where.push("cwd LIKE ?");
    params.push(`%${values.cwd}%`);
  }
  if (since != null) {
    where.push("ended_at >= ?");
    params.push(since);
  }

  const rows = db()
    .query<
      {
        id: string;
        agent: string;
        cwd: string | null;
        ended_at: number | null;
        title: string | null;
        turn_count: number;
      },
      SQLQueryBindings[]
    >(
      `SELECT s.id, s.agent, s.cwd, s.ended_at, s.title, s.turn_count
       FROM sessions s
       WHERE ${where.join(" AND ")}
       ORDER BY s.ended_at DESC LIMIT ?`
    )
    .all(...params, limit);

  out({ count: rows.length, sessions: rows }, values.json, () =>
    rows.length
      ? rows
          .map(
            (r) =>
              `${r.id}\n  ${r.agent} · ${fmtTime(r.ended_at)} · ${r.turn_count} turns · ${
                r.cwd ?? "—"
              }\n  ${r.title ?? "—"}`
          )
          .join("\n\n")
      : "no sessions"
  );
}

function cmdSources(argv: string[]): void {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: { json: { type: "boolean", default: false } },
  });
  const id = positionals[0];
  if (!id) throw new UsageError("sources needs a session id");
  getSession(id); // 404 if unknown
  const sources = getSources(id);
  out({ id, sources }, values.json, () =>
    sources.length
      ? sources.map((s) => `${s.kind}\t${s.path}\t${s.bytes ?? "?"}B\t${fmtTime(s.mtime)}`).join("\n")
      : "no sources"
  );
}

function cmdExport(argv: string[]): void {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      out: { type: "string" },
      json: { type: "boolean", default: false },
    },
  });
  const id = positionals[0];
  if (!id) throw new UsageError("export needs a session id");
  const session = getSession(id);
  const sources = getSources(id);
  if (!sources.length) throw new NotFoundError(`no raw sources recorded for ${id}`);
  const outDir = values.out ?? ".";
  mkdirSync(outDir, { recursive: true });

  const written: string[] = [];
  for (const src of sources) {
    if (src.kind === "jsonl" || src.kind === "json") {
      // the pointed-to file IS the verbatim native trace — copy it out
      const dest = join(outDir, basename(src.path));
      copyFileSync(src.path, dest);
      written.push(dest);
    } else if (src.kind === "sqlite") {
      // "<db_path>#<session_id>" → re-read the three tables, dump the rows verbatim
      const hash = src.path.lastIndexOf("#");
      const dbFile = src.path.slice(0, hash);
      const sid = src.path.slice(hash + 1);
      const ro = new Database(dbFile, { readonly: true });
      const sess = ro.query("SELECT * FROM session WHERE id = ?").all(sid);
      const msgs = ro.query("SELECT * FROM message WHERE session_id = ? ORDER BY time_created").all(sid);
      const parts = ro
        .query("SELECT * FROM part WHERE session_id = ? ORDER BY time_created")
        .all(sid);
      ro.close();
      const dest = join(outDir, `${session.agent}-${sid}.json`);
      writeFileSync(dest, JSON.stringify({ session: sess, message: msgs, part: parts }, null, 2));
      written.push(dest);
    }
  }
  out({ id, outDir, written }, values.json, () => `exported ${id} → ${written.join(", ")}`);
}

function cmdReindex(argv: string[]): void {
  const { values } = parseArgs({ args: argv, options: { json: { type: "boolean", default: false } } });
  rebuildFts();
  out({ reindexed: true }, values.json, () => "events_fts rebuilt from events");
}

function cmdStats(argv: string[]): void {
  const { values } = parseArgs({ args: argv, options: { json: { type: "boolean", default: false } } });
  const d = db();
  const perAgent = d
    .query<
      { agent: string; n: number; min_start: number | null; max_end: number | null; turns: number; events: number },
      []
    >(
      `SELECT agent, COUNT(*) n, MIN(started_at) min_start, MAX(ended_at) max_end,
              SUM(turn_count) turns, SUM(event_count) events
       FROM sessions GROUP BY agent ORDER BY agent`
    )
    .all();
  let dbBytes = 0;
  try {
    dbBytes = statSync(TRACE_DB).size;
  } catch {
    /* ignore */
  }

  const stats = {
    dbPath: TRACE_DB,
    dbBytes,
    agents: perAgent.map((a) => ({
      agent: a.agent,
      sessions: a.n,
      turns: a.turns,
      events: a.events,
      span: [fmtTime(a.min_start), fmtTime(a.max_end)],
    })),
  };

  out(stats, values.json, () => {
    const lines = [`trace.db  ${(dbBytes / 1e6).toFixed(1)} MB  (${TRACE_DB})`, ""];
    for (const a of stats.agents)
      lines.push(
        `${a.agent.padEnd(9)} ${String(a.sessions).padStart(6)} sessions  ${String(a.turns).padStart(
          7
        )} turns  ${String(a.events).padStart(8)} events  ${a.span[0]} → ${a.span[1]}`
      );
    return lines.join("\n");
  });
}

function cmdIngest(argv: string[]): void {
  const { values } = parseArgs({
    args: argv,
    options: {
      agent: { type: "string" },
      since: { type: "string" },
      full: { type: "boolean", default: false },
      json: { type: "boolean", default: false },
    },
  });
  const agents = parseAgent(values.agent) ?? undefined;
  const since = parseSince(values.since);
  const results = ingest({ agents, sinceEpochSec: since, full: values.full });

  out({ results }, values.json, () =>
    results
      .map(
        (r) =>
          `${r.agent.padEnd(9)} scanned ${r.scanned}  upserted ${r.upserted}  skipped ${r.skipped}  errors ${r.errors}`
      )
      .join("\n")
  );
}

// ---------- dispatch ----------

const COMMANDS: Record<string, (argv: string[]) => void> = {
  search: cmdSearch,
  show: cmdShow,
  recent: cmdRecent,
  sources: cmdSources,
  export: cmdExport,
  reindex: cmdReindex,
  stats: cmdStats,
  ingest: cmdIngest,
};

function printHelp(): void {
  process.stdout.write(
    `Usage: trace-db <command> [options]

Commands:
  search <query>   FTS5 search across all agents (--agent --cwd --since --kind --limit --json)
                   multi-word → OR-of-phrases (no adjacency needed); FTS5 syntax passes through
                   ranked by relevance + coverage + kind + recency; lineage (fork/subagent) collapsed
                   --half-life DAYS (recency decay, default 30) · --no-recency · --no-collapse
                   --kind filters match to user,assistant,thinking,tool_call,system
                   each result carries the ask/outcome bookends + ±5 context around the top hit
  show <id>        Reconstruct one session (--kind K --around IDX --window N --include-tools --raw --json)
                   default shows user+assistant; --include-tools adds thinking/tool_call/tool_result/system/usage
  recent           Newest sessions (--agent --cwd --since --limit --json)
  sources <id>     Raw-trace pointer(s) for a session (--json)
  export <id>      Reconstruct native trace(s) from the pointer(s) (--out DIR --json)
  reindex          Rebuild events_fts from events (no native-store reads) (--json)
  stats            Per-agent counts, span, DB size (--json)
  ingest           Mechanical scan → upsert (--agent --since --full --json)

Session ids are "agent:nativeid" (e.g. claude:<uuid>, opencode:ses_…).
`
  );
}

export function runCli(argv = process.argv.slice(2)): number {
  const cmd = argv[0];
  if (!cmd || cmd === "-h" || cmd === "--help") {
    printHelp();
    return 0;
  }
  const handler = COMMANDS[cmd];
  if (!handler) {
    process.stderr.write(`unknown command: ${cmd}\n`);
    printHelp();
    return 4;
  }
  try {
    handler(argv.slice(1));
    return 0;
  } catch (e) {
    if (e instanceof SessError) {
      process.stderr.write(`error: ${e.message}\n`);
      return e.code;
    }
    process.stderr.write(`error: ${e instanceof Error ? e.message : String(e)}\n`);
    return 1;
  }
}

if (import.meta.main) process.exit(runCli());
