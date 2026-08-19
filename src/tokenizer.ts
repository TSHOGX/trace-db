/**
 * Load TraceDB's bundled fts5-jieba tokenizer into bun:sqlite.
 *
 * Two macOS gotchas, both handled here (see the fts5-jieba memory fact):
 *   1. bun:sqlite's bundled SQLite disallows extensions → point it at
 *      Homebrew's libsqlite3 via setCustomSQLite() BEFORE any Database opens.
 *   2. The extension is loaded by absolute path to target/release/libfts5jieba.
 *
 * setCustomSQLite() is process-global and must run before the first Database is
 * constructed, so callers invoke useJiebaSqlite() at module top-level (import
 * side effect) prior to opening their db. It's idempotent + best-effort: if the
 * Homebrew lib is missing we swallow the error so a host without it still opens
 * a (non-extension) db rather than crashing at import.
 */

import { Database } from "bun:sqlite";
import { dirname, join } from "path";
import { fileURLToPath } from "url";

const SQLITE_CANDIDATES: Partial<Record<NodeJS.Platform, string[]>> = {
  darwin: [
    "/opt/homebrew/opt/sqlite/lib/libsqlite3.dylib",
    "/usr/local/opt/sqlite/lib/libsqlite3.dylib",
  ],
};

/** Absolute path to the compiled extension (no file extension — SQLite appends
 *  .dylib/.so/.dll). Resolution order:
 *    1. $TRACEDB_JIEBA_EXT override (explicit, wins).
 *    2. package-relative native/fts5-jieba/target/release/libfts5jieba.
 */
export function jiebaExtPath(): string {
  const override = process.env.TRACEDB_JIEBA_EXT ?? process.env.FTS5_JIEBA_EXT;
  if (override) return override;
  const url = import.meta.url;
  if (url.includes("$bunfs")) {
    return join(dirname(process.execPath), "native", "libfts5jieba");
  }
  const here = dirname(fileURLToPath(url));
  return join(here, "..", "native", "fts5-jieba", "target", "release", "libfts5jieba");
}

let _customSet = false;

/** Point bun:sqlite at Homebrew's libsqlite3 so .loadExtension() is allowed.
 *  Process-global; call before opening any Database. Idempotent + best-effort. */
export function useJiebaSqlite(): void {
  if (_customSet) return;
  _customSet = true;
  const override = process.env.TRACEDB_SQLITE_LIB;
  for (const candidate of override ? [override] : (SQLITE_CANDIDATES[process.platform] ?? [])) {
    try {
      Database.setCustomSQLite(candidate);
      return;
    } catch {
      // Try the next platform-specific candidate.
    }
  }
}

/** Load the jieba tokenizer into an open Database. Returns true on success.
 *  On failure (extension missing / host disallows) returns false so the caller
 *  can fall back to a plain tokenizer or LIKE search instead of crashing. */
export function loadJieba(db: Database): boolean {
  try {
    db.loadExtension(jiebaExtPath());
    return true;
  } catch {
    return false;
  }
}

export function requireJieba(db: Database): void {
  if (loadJieba(db)) return;
  throw new Error(
    `Unable to load the fts5-jieba extension at ${jiebaExtPath()}. ` +
      "Run `bun run build:native` or set TRACEDB_JIEBA_EXT and TRACEDB_SQLITE_LIB."
  );
}
