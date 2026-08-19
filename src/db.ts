/**
 * db.ts — the rebuildable trace.db store.
 *
 * TWO SQLite files, split by rebuildability:
 *
 *   trace.db (mechanical, rebuildable, disposable — gitignored, safe to rm)
 *     sessions     one row per "agent:nativeid". Cross-agent fields (model,
 *                  provider, git_branch, parent_session_id, forked_from) are
 *                  columns; the agent-specific remainder stays in `meta` JSON.
 *                  `title` here is the NATIVE title (Claude ai-title / OpenCode)
 *                  — it comes from ingest, so it's rebuildable and lives here.
 *                  `fingerprint` is the incremental change-key.
 *     raw_sources  POINTER(s) to the verbatim native trace(s) — abs path + kind +
 *                  bytes/mtime for drift detection. We never delete native traces,
 *                  so this is the "lossless" anchor: `events` is the working copy,
 *                  raw_sources is where to re-read the ground truth. Gemini spans
 *                  several chunk files → multiple rows per session.
 *     events       one row per semantically-distinct piece (the 7 primitives:
 *                  user/assistant/thinking/tool_call/tool_result/system/usage),
 *                  full text + cross-agent attrs (subtype/name/call_id/is_error/
 *                  native_id/parent_id/tokens).
 *     events_fts   fts5 external-content over events.text, tokenize='jieba'
 *                  (中英文 word segmentation + English stemming; needs the
 *                  fts5-jieba loadable extension — loaded in db() below).
 *
 * FTS gating: tool_result + usage events are STORED but NOT indexed. Flip
 * UNINDEXED_KINDS to change. This module is mechanical only — no LLM.
 */

import { Database } from "bun:sqlite";
import { mkdirSync } from "fs";
import { dirname } from "path";
import { TRACE_DB } from "./paths.js";
import { turnCountOf, UNINDEXED_KINDS, type Event, type UnifiedSession } from "./types.js";
import { requireJieba, useJiebaSqlite } from "./tokenizer.js";

// setCustomSQLite() is process-global and must run before the first Database is
// opened, so invoke it at import time (before db() ever constructs a handle).
useJiebaSqlite();

/** SQL string list of the FTS-excluded kinds, e.g. "'tool_result','usage'". */
const UNINDEXED_SQL = UNINDEXED_KINDS.map((k) => `'${k}'`).join(", ");

const SCHEMA = `
CREATE TABLE IF NOT EXISTS sessions (
  id                TEXT PRIMARY KEY,          -- "agent:nativeid" stable key
  agent             TEXT NOT NULL,             -- claude|codex|opencode|gemini|pi
  cwd               TEXT,
  started_at        INTEGER,                   -- epoch seconds
  ended_at          INTEGER,                   -- epoch seconds (approx: last event)
  title             TEXT,                      -- NATIVE title (Claude/OpenCode) else NULL (rebuildable)
  model             TEXT,
  provider          TEXT,
  git_branch        TEXT,
  parent_session_id TEXT,                      -- tree B: subagent/child parent ("agent:nativeid")
  forked_from       TEXT,                      -- tree B: fork origin ("agent:nativeid#messageUuid")
  turn_count        INTEGER NOT NULL DEFAULT 0, -- user+assistant events
  event_count       INTEGER NOT NULL DEFAULT 0, -- total events incl. thinking/tool/system/usage
  fingerprint       TEXT,                      -- incremental change-key
  meta              TEXT,                      -- JSON: cli_version/sandbox/cost/tokens/…
  last_ingest_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_agent   ON sessions(agent);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(ended_at);
CREATE INDEX IF NOT EXISTS idx_sessions_parent  ON sessions(parent_session_id);

CREATE TABLE IF NOT EXISTS raw_sources (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  path       TEXT NOT NULL,                    -- abs file path, or "<db>#<session_id>"
  kind       TEXT NOT NULL,                    -- jsonl|json|sqlite
  bytes      INTEGER,
  mtime      INTEGER,                          -- epoch seconds
  PRIMARY KEY (session_id, path)
);

CREATE TABLE IF NOT EXISTS events (
  id         INTEGER PRIMARY KEY,              -- rowid alias, bound by external-content FTS
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,                 -- 0-based order within session
  kind       TEXT NOT NULL,                    -- the 7 primitives
  subtype    TEXT,                             -- system/thinking discriminator
  name       TEXT,                             -- tool name for tool_call/tool_result
  call_id    TEXT,                             -- tool_call ↔ tool_result join key
  is_error   INTEGER,                          -- tool_result error flag (0/1)
  native_id  TEXT,                             -- this event's native id (tree A)
  parent_id  TEXT,                             -- parent event's native id (tree A)
  tokens     INTEGER,                          -- per-event token total where present
  text       TEXT NOT NULL,                    -- FULL, untruncated
  ts         INTEGER                           -- epoch seconds
);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, idx);
CREATE INDEX IF NOT EXISTS idx_events_kind    ON events(kind);
CREATE INDEX IF NOT EXISTS idx_events_call    ON events(session_id, call_id);

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
  text,
  content='events',
  content_rowid='id',
  tokenize='jieba'
);

CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events
WHEN new.kind NOT IN (${UNINDEXED_SQL}) BEGIN
  INSERT INTO events_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events
WHEN old.kind NOT IN (${UNINDEXED_SQL}) BEGIN
  INSERT INTO events_fts(events_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS events_au AFTER UPDATE ON events BEGIN
  INSERT INTO events_fts(events_fts, rowid, text)
    SELECT 'delete', old.id, old.text WHERE old.kind NOT IN (${UNINDEXED_SQL});
  INSERT INTO events_fts(rowid, text)
    SELECT new.id, new.text WHERE new.kind NOT IN (${UNINDEXED_SQL});
END;
`;

let _db: Database | null = null;

export function db(): Database {
  if (_db) return _db;
  mkdirSync(dirname(TRACE_DB), { recursive: true });
  const d = new Database(TRACE_DB, { create: true });
  d.exec("PRAGMA journal_mode = WAL;");
  d.exec("PRAGMA busy_timeout = 5000;");
  d.exec("PRAGMA foreign_keys = ON;");
  // Load jieba BEFORE the schema — the events_fts `tokenize='jieba'` clause is
  // rejected if the tokenizer isn't registered yet. If it fails to load
  // (extension missing) we still exec the schema; the CREATE VIRTUAL TABLE will
  // error loudly, which is the right signal that the build step was skipped.
  requireJieba(d);
  d.exec(SCHEMA);
  _db = d;
  return d;
}

export interface SessionRow {
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
  last_ingest_at: number;
}

/**
 * Upsert one session with its raw-source pointer(s) and full event list in a
 * single transaction. Idempotent: raw_sources + events are replaced wholesale
 * (delete+reinsert). The FTS triggers ride along with the event writes.
 *
 * This project owns only trace.db; semantic overlays are intentionally outside
 * its schema and API.
 */
export function upsertSession(session: UnifiedSession, events: Event[]): void {
  const d = db();
  const now = Math.floor(Date.now() / 1000);
  const turns = turnCountOf(events);
  const tx = d.transaction(() => {
    d.query(
      `INSERT INTO sessions (id, agent, cwd, started_at, ended_at, title, model, provider,
                             git_branch, parent_session_id, forked_from, turn_count, event_count,
                             fingerprint, meta, last_ingest_at)
       VALUES ($id, $agent, $cwd, $started, $ended, $title, $model, $provider,
               $branch, $parent, $forked, $turns, $events, $fp, $meta, $now)
       ON CONFLICT(id) DO UPDATE SET
         cwd=$cwd, started_at=$started, ended_at=$ended,
         title=COALESCE($title, sessions.title),
         model=$model, provider=$provider, git_branch=$branch,
         parent_session_id=$parent, forked_from=$forked,
         turn_count=$turns, event_count=$events, fingerprint=$fp, meta=$meta, last_ingest_at=$now`
    ).run({
      $id: session.id,
      $agent: session.agent,
      $cwd: session.cwd,
      $started: session.startedAt,
      $ended: session.endedAt,
      $title: session.title,
      $model: session.model,
      $provider: session.provider,
      $branch: session.gitBranch,
      $parent: session.parentSessionId,
      $forked: session.forkedFrom,
      $turns: turns,
      $events: events.length,
      $fp: session.fingerprint,
      $meta: JSON.stringify(session.meta ?? {}),
      $now: now,
    });

    d.query("DELETE FROM raw_sources WHERE session_id = ?").run(session.id);
    const insSrc = d.query(
      `INSERT OR REPLACE INTO raw_sources (session_id, path, kind, bytes, mtime)
       VALUES ($sid, $path, $kind, $bytes, $mtime)`
    );
    for (const src of session.sources) {
      insSrc.run({
        $sid: session.id,
        $path: src.path,
        $kind: src.kind,
        $bytes: src.bytes,
        $mtime: src.mtime,
      });
    }

    d.query("DELETE FROM events WHERE session_id = ?").run(session.id);
    const insEv = d.query(
      `INSERT INTO events (session_id, idx, kind, subtype, name, call_id, is_error,
                           native_id, parent_id, tokens, text, ts)
       VALUES ($sid, $idx, $kind, $subtype, $name, $call, $err, $nid, $pid, $tok, $text, $ts)`
    );
    for (const e of events) {
      insEv.run({
        $sid: session.id,
        $idx: e.idx,
        $kind: e.kind,
        $subtype: e.subtype,
        $name: e.name,
        $call: e.callId,
        $err: e.isError == null ? null : e.isError ? 1 : 0,
        $nid: e.nativeId,
        $pid: e.parentId,
        $tok: e.tokens,
        $text: e.text,
        $ts: e.ts,
      });
    }
  });
  tx();
}

/** Existing (id → fingerprint) map for one agent, so ingest can skip unchanged sessions. */
export function existingFingerprints(agent: string): Map<string, string> {
  const rows = db()
    .query<{ id: string; fingerprint: string | null }, [string]>(
      "SELECT id, fingerprint FROM sessions WHERE agent = ?"
    )
    .all(agent);
  const m = new Map<string, string>();
  for (const r of rows) if (r.fingerprint != null) m.set(r.id, r.fingerprint);
  return m;
}

/**
 * Rebuild the FTS index from events (no native-store reads).
 *
 * NOT the FTS5 `'rebuild'` command: that re-indexes every content row directly,
 * bypassing the trigger `WHEN` gating, so it silently pulls the UNINDEXED_KINDS
 * (tool_result/usage) into the index. We instead clear the index and re-insert
 * only the eligible kinds, matching exactly what the INSERT trigger would index.
 */
export function rebuildFts(): void {
  const d = db();
  d.transaction(() => {
    d.exec("INSERT INTO events_fts(events_fts) VALUES('delete-all');");
    d.exec(
      `INSERT INTO events_fts(rowid, text)
       SELECT id, text FROM events WHERE kind NOT IN (${UNINDEXED_SQL});`
    );
  })();
}
