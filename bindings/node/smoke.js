"use strict";

const assert = require("node:assert/strict");
const { mkdtempSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { TraceDb } = require("./index.js");

const root = mkdtempSync(join(tmpdir(), "tracedb-node-"));
writeFileSync(
  join(root, "session-node.json"),
  JSON.stringify({
    sessionId: "node",
    startTime: "2026-08-19T00:00:00Z",
    lastUpdated: "2026-08-19T00:00:01Z",
    messages: [{ id: "u", type: "user", content: "deploy node" }],
  }),
);
const db = TraceDb.open(join(root, "trace.db"));
const report = db.ingest({ agents: ["gemini"], root });
assert.equal(report.agents[0].ingested, 1);
assert.equal(db.list({ agent: "gemini" }).sessions[0].id, "gemini:node");
assert.equal(db.search("deploy")[0].id, "gemini:node");
assert.equal(db.show("gemini:node").events[0].text, "deploy node");
assert.deepEqual(db.reconstruct("gemini:node", join(root, "restored")), []);
assert.equal(JSON.parse(db.statsJson()).totalSessions, 1);
db.reindex();
console.log("node binding smoke: ok");
