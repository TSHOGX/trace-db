import { afterAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "fs";
import { join } from "path";
import { tmpdir } from "os";
import type { Event } from "../src/types.js";

const root = mkdtempSync(join(tmpdir(), "trace-db-test-"));
process.env.TRACEDB_PATH = join(root, "trace.db");

const api = await import("../src/index.js");

afterAll(() => {
  api.db().close();
  rmSync(root, { recursive: true, force: true });
});

test("stores all events while excluding noisy kinds from FTS", () => {
  api.upsertSession(
    {
      id: "codex:test",
      agent: "codex",
      cwd: "/tmp/project",
      startedAt: 1,
      endedAt: 2,
      title: "Tokenizer work",
      model: "test-model",
      provider: null,
      gitBranch: "main",
      parentSessionId: null,
      forkedFrom: null,
      turnCount: 0,
      eventCount: 0,
      sources: [],
      fingerprint: "v1",
      meta: {},
    },
    [
      {
        idx: 0,
        kind: "user",
        subtype: null,
        name: null,
        callId: null,
        isError: null,
        nativeId: "a",
        parentId: null,
        tokens: null,
        text: "部署 tokenizer",
        ts: 1,
      },
      {
        idx: 1,
        kind: "tool_result",
        subtype: null,
        name: "exec",
        callId: "call-1",
        isError: false,
        nativeId: "b",
        parentId: "a",
        tokens: null,
        text: "secret noisy output",
        ts: 2,
      },
    ] satisfies Event[]
  );

  expect(api.db().query("SELECT count(*) AS n FROM events").get()).toEqual({ n: 2 });
  expect(
    api.db().query("SELECT count(*) AS n FROM events_fts WHERE events_fts MATCH 'tokenizer'").get()
  ).toEqual({ n: 1 });
  expect(
    api.db().query("SELECT count(*) AS n FROM events_fts WHERE events_fts MATCH 'secret'").get()
  ).toEqual({ n: 0 });
});

test("schema contains no semantic overlay or fan-out state", () => {
  const names = api.db()
    .query<{ name: string }, []>("SELECT name FROM sqlite_master WHERE type='table'")
    .all()
    .map((row) => row.name);
  expect(names).not.toContain("overlay");
  expect(names).not.toContain("fan_out_state");
});
