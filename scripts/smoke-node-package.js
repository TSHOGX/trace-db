"use strict";

// Install a packed Node artifact into an isolated prefix and require it.
const assert = require("node:assert/strict");
const { execFileSync } = require("node:child_process");
const { mkdtempSync, writeFileSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join, resolve } = require("node:path");

const packagePath = process.argv[2] && resolve(process.argv[2]);
if (!packagePath) {
  throw new Error("usage: node scripts/smoke-node-package.js PACKAGE_TGZ");
}

const installRoot = mkdtempSync(join(tmpdir(), "tracedb-node-package-"));
const fixtureRoot = mkdtempSync(join(tmpdir(), "tracedb-node-fixture-"));
const npm = process.platform === "win32" ? "npm.cmd" : "npm";

try {
  execFileSync(
    npm,
    ["install", "--prefix", installRoot, "--ignore-scripts", packagePath],
    { stdio: "inherit", shell: process.platform === "win32" },
  );
  const { TraceDb } = require(join(installRoot, "node_modules/@tracedb/core"));
  const session = join(fixtureRoot, "session-node.json");
  writeFileSync(
    session,
    JSON.stringify({
      sessionId: "node-package",
      startTime: "2026-08-19T00:00:00Z",
      lastUpdated: "2026-08-19T00:00:01Z",
      messages: [{ id: "u", type: "user", content: "deploy packaged node" }],
    }),
  );
  const db = TraceDb.open(join(fixtureRoot, "trace.db"));
  const report = db.ingest({ agents: ["gemini"], root: fixtureRoot });
  assert.equal(report.agents[0].ingested, 1);
  assert.equal(db.search("deploy")[0].id, "gemini:node-package");
  assert.equal(db.show("gemini:node-package").events[0].text, "deploy packaged node");
  console.log("node package install smoke: ok");
} finally {
  rmSync(installRoot, { recursive: true, force: true });
  rmSync(fixtureRoot, { recursive: true, force: true });
}
