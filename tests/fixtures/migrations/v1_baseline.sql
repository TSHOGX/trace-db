-- Historical TraceDB v1 archive schema from the initial Rust archive release.
-- Keep this fixture stable: it represents an archive produced before tokenizer
-- and archive-contract metadata were persisted by later releases.
CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO schema_meta(key, value) VALUES ('schema_version', '1');

CREATE TABLE sessions (
  id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT, started_at_ms INTEGER,
  ended_at_ms INTEGER, title TEXT, model TEXT, provider TEXT, git_branch TEXT,
  parent_session_id TEXT, forked_from TEXT, mode TEXT NOT NULL DEFAULT 'partial',
  fingerprint TEXT NOT NULL, meta_json TEXT NOT NULL, ingested_at_ms INTEGER NOT NULL
);
CREATE INDEX sessions_agent_idx ON sessions(agent);
CREATE INDEX sessions_ended_idx ON sessions(ended_at_ms);
CREATE INDEX sessions_parent_idx ON sessions(parent_session_id);

CREATE TABLE raw_sources (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  locator TEXT NOT NULL, kind TEXT NOT NULL, restore_path TEXT NOT NULL,
  role TEXT, bytes INTEGER, mtime_ns INTEGER, mode INTEGER, object_hash TEXT,
  PRIMARY KEY(session_id, locator)
);

CREATE TABLE objects (
  hash TEXT PRIMARY KEY, compression TEXT NOT NULL, bytes INTEGER NOT NULL,
  payload BLOB NOT NULL, created_at_ms INTEGER NOT NULL
);

CREATE TABLE events (
  id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL, kind TEXT NOT NULL, subtype TEXT, role TEXT, name TEXT,
  call_id TEXT, is_error INTEGER, native_id TEXT, parent_id TEXT, model TEXT,
  provider TEXT, usage_json TEXT, text TEXT NOT NULL, data_json TEXT, created_at_ms INTEGER
);
CREATE INDEX events_session_idx ON events(session_id, idx);
CREATE INDEX events_kind_idx ON events(kind);
CREATE INDEX events_call_idx ON events(session_id, call_id);

CREATE VIRTUAL TABLE events_fts USING fts5(
  text, content='events', content_rowid='id', tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER events_ai AFTER INSERT ON events WHEN new.kind NOT IN ('tool_result','usage') BEGIN
  INSERT INTO events_fts(rowid, text) VALUES(new.id, new.text);
END;
CREATE TRIGGER events_ad AFTER DELETE ON events WHEN old.kind NOT IN ('tool_result','usage') BEGIN
  INSERT INTO events_fts(events_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER events_au AFTER UPDATE ON events BEGIN
  INSERT INTO events_fts(events_fts, rowid, text)
    SELECT 'delete', old.id, old.text WHERE old.kind NOT IN ('tool_result','usage');
  INSERT INTO events_fts(rowid, text)
    SELECT new.id, new.text WHERE new.kind NOT IN ('tool_result','usage');
END;

INSERT INTO sessions(
  id, agent, cwd, started_at_ms, ended_at_ms, title, mode, fingerprint, meta_json, ingested_at_ms
) VALUES (
  'codex:historical-v1', 'codex', '/workspace/legacy', 1000, 2000,
  'Historical deploy', 'partial', 'historical-fingerprint', '{}', 3000
);
INSERT INTO events(session_id, idx, kind, text, created_at_ms)
VALUES
  ('codex:historical-v1', 0, 'user', 'Investigate the legacy deploy failure', 1000),
  ('codex:historical-v1', 1, 'assistant', 'The legacy deploy was fixed', 2000);
