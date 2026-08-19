/** Programmatic TraceDB surface. The CLI is available as `trace-db`. */
export { db, rebuildFts, upsertSession, existingFingerprints } from "./db.js";
export { ingest } from "./ingest.js";
export { getParser, PARSERS } from "./parsers/index.js";
export * from "./types.js";
export { TRACE_DB } from "./paths.js";
export { jiebaExtPath, loadJieba, requireJieba, useJiebaSqlite } from "./tokenizer.js";
export { runCli } from "./cli.js";
