export type Agent = "claude" | "codex" | "opencode" | "gemini" | "pi";
export type IngestMode = "partial" | "full";

export interface SearchOptions {
  limit?: number;
  agent?: Agent;
  cwd?: string;
  sinceMs?: number;
}

export interface ListOptions {
  limit?: number;
  cursor?: string;
  agent?: Agent;
  cwd?: string;
  sinceMs?: number;
  mode?: IngestMode;
  model?: string;
  provider?: string;
}

export interface IngestOptions {
  agents?: Agent[];
  mode?: IngestMode;
  root?: string;
  sinceMs?: number;
}

export interface AgentIngestReport {
  agent: Agent;
  root: string;
  discovered: number;
  parsed: number;
  ingested: number;
  unchanged: number;
  skipped: number;
  skippedBySince: number;
  failed: number;
  warnings: IngestIssue[];
  failures: IngestIssue[];
}

export interface IngestIssue {
  stage: "discovery" | "parsing" | "database";
  locator: string;
  category:
    | "unsupported_format"
    | "corrupt_data"
    | "permission"
    | "transient_read"
    | "read"
    | "database";
  message: string;
}

export interface IngestReport {
  agents: AgentIngestReport[];
}

export interface SearchResult {
  id: string;
  lineageRootId: string;
  agent: Agent;
  cwd: string | null;
  title: string | null;
  startedAtMs: number | null;
  endedAtMs: number | null;
  score: number;
  hits: number;
  ask: string | null;
  outcome: string | null;
  relatedSessionIds: string[];
  [key: string]: unknown;
}

export interface ArchiveStats {
  path: string;
  totalSessions: number;
  totalEvents: number;
  totalFullSessions: number;
  agents: Array<{
    agent: Agent;
    sessions: number;
    events: number;
    fullSessions: number;
  }>;
}

export interface SessionTrace {
  session: Record<string, unknown>;
  mode: IngestMode;
  events: Array<Record<string, unknown>>;
}

export class TraceDb {
  static open(path?: string): TraceDb;

  statsJson(): string;
  stats(): ArchiveStats;

  searchJson(
    query: string,
    limit?: number,
    agent?: Agent,
    cwd?: string,
    sinceMs?: number,
  ): string;
  search(query: string, options?: SearchOptions): SearchResult[];

  listJson(
    limit?: number,
    cursor?: string,
    agent?: Agent,
    cwd?: string,
    sinceMs?: number,
    mode?: IngestMode,
    model?: string,
    provider?: string,
  ): string;
  list(options?: ListOptions): {
    sessions: Array<Record<string, unknown>>;
    nextCursor: string | null;
  };

  ingestJson(
    agents?: Agent[],
    mode?: IngestMode,
    root?: string,
    sinceMs?: number,
  ): string;
  ingest(options?: IngestOptions): IngestReport;

  showJson(sessionId: string): string;
  show(sessionId: string): SessionTrace | null;

  reconstructJson(sessionId: string, outDir: string): string;
  reconstructJsonWithOptions(
    sessionId: string,
    outDir: string,
    overwrite?: boolean,
  ): string;
  reconstruct(sessionId: string, outDir: string, overwrite?: boolean): string[];

  reindex(): void;
}
