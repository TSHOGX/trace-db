export type Agent = "claude" | "codex" | "opencode" | "gemini" | "pi";

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
  model?: string;
  provider?: string;
  /** Match `cwd` as a normalized exact path instead of a substring. */
  cwdExact?: boolean;
  /** Hide a child whose direct parent satisfies the same filters. */
  collapseLineage?: boolean;
}

export interface IngestOptions {
  agents?: Agent[];
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
  ack: IngestAck | null;
}

export interface IngestAck {
  sequence: number;
  committedAtMs: number;
}

export interface ScoreBreakdown {
  bestMatch: number;
  hitCoverage: number;
  termCoverage: number;
  kind: number;
  recency: number;
  title: number;
  lineage: number;
}

export interface SearchMatch {
  eventIdx: number;
  kind: string;
  bm25: number;
  snippet: string;
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
  scoreBreakdown: ScoreBreakdown;
  hits: number;
  bestMatch: SearchMatch;
  ask: string | null;
  outcome: string | null;
  relatedSessionIds: string[];
  [key: string]: unknown;
}

export interface ArchiveStats {
  path: string;
  totalSessions: number;
  totalEvents: number;
  agents: Array<{
    agent: Agent;
    sessions: number;
    events: number;
  }>;
}

export interface SessionTrace {
  session: Record<string, unknown>;
  events: Array<Record<string, unknown>>;
  /** Turn-internal tool and delegation trajectories. */
  spans: Array<Record<string, unknown>>;
}

export class TraceDb {
  static open(path?: string): TraceDb;

  statsJson(): string;
  stats(): ArchiveStats;

  coverageJson(sessionId: string): string;
  coverage(sessionId: string): Record<string, unknown> | null;

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
    model?: string,
    provider?: string,
    cwdExact?: boolean,
    collapseLineage?: boolean,
  ): string;
  list(options?: ListOptions): {
    sessions: Array<Record<string, unknown>>;
    nextCursor: string | null;
  };

  ingestJson(agents?: Agent[], root?: string, sinceMs?: number): string;
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
