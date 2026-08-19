/**
 * paths.ts — every on-disk location the archive touches. Native store roots are
 * ground-truthed against the live machine; see README "Native formats".
 * Native roots use each tool's standard location. The database path is explicit
 * and deployable: TRACEDB_PATH wins, then XDG_DATA_HOME, then ~/.local/share.
 */

import { homedir } from "os";
import { join } from "path";

const HOME = homedir();

/** The unified episodic store — mechanical, rebuildable, disposable. */
export const TRACE_DB = process.env.TRACEDB_PATH ??
  join(process.env.XDG_DATA_HOME ?? join(HOME, ".local", "share"), "trace-db", "trace.db");

// --- native session stores ---
export const CLAUDE_PROJECTS = join(HOME, ".claude", "projects");
export const CODEX_SESSIONS = join(HOME, ".codex", "sessions");
export const GEMINI_TMP = join(HOME, ".gemini", "tmp");
export const GEMINI_PROJECTS_JSON = join(HOME, ".gemini", "projects.json");
export const PI_SESSIONS = join(HOME, ".pi", "agent", "sessions");
export const OPENCODE_DB_CANDIDATES = [
  join(HOME, ".local", "share", "opencode", "opencode.db"),
  join(HOME, ".opencode", "opencode.db"),
];
