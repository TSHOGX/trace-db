/**
 * types.ts — the unified trace model.
 *
 * The goal is NOT byte-lossless storage. The verbatim native traces are never
 * deleted, so we keep only a POINTER to them (raw_sources) and normalize the
 * semantically-important content into one cross-agent shape (events) that the
 * retrieval and downstream consumers can reason over.
 *
 * A session decomposes into an ordered list of typed EVENTS. Ground-truthing
 * all five stores, the union of semantically-distinct pieces reduces
 * to exactly SEVEN primitives (EventKind). Everything else an agent emits —
 * world_state, turn_context, step-start/finish, mode changes, compaction
 * boundaries, task_started/complete, hook fire/output — is non-conversational
 * engine activity and collapses into `system` with a `subtype` discriminator.
 * (Hooks are NOT a first-class stream in any agent: in Claude they surface as a
 * system record with subtype=stop_hook_summary. So hook ⊂ system.)
 *
 * TWO orthogonal trees exist and both are preserved:
 *   - intra-session lineage: event.parentId → event.nativeId (Claude parentUuid,
 *     Pi parentId, OpenCode message.parentID). The message chain inside ONE session.
 *   - cross-session fork/subagent: session.parentSessionId + session.forkedFrom.
 *     OpenCode subagents are child session rows (session.parent_id); Claude
 *     subagents run as their own `bg` session files linked via the parent's
 *     Agent/Task tool_call; Claude resume/fork carries forkedFrom{sessionId,messageUuid}.
 *     Event-level parentId can NOT express this — it never crosses a session.
 */

export type Agent = "claude" | "codex" | "opencode" | "gemini" | "pi";

export const AGENTS: Agent[] = ["claude", "codex", "opencode", "gemini", "pi"];

/**
 * The seven primitives. This is the axis everything filters on.
 *   user         a human turn
 *   assistant    the assistant's visible answer text
 *   thinking     the assistant's reasoning / thoughts (verbatim or summary)
 *   tool_call    one tool invocation (name + arguments + callId)
 *   tool_result  that call's output (linked by callId; isError flag)
 *   system       any non-conversational engine event, disambiguated by `subtype`
 *                (compaction boundary, mode/permission change, task lifecycle,
 *                 slash-command expansion, hook summary, step markers, errors…)
 *   usage        token / cost accounting (numeric; not searched)
 */
export type EventKind =
  | "user"
  | "assistant"
  | "thinking"
  | "tool_call"
  | "tool_result"
  | "system"
  | "usage";

export const EVENT_KINDS: EventKind[] = [
  "user",
  "assistant",
  "thinking",
  "tool_call",
  "tool_result",
  "system",
  "usage",
];

/** The "conversational" kinds — what a human reading the transcript sees. */
export const CONTENT_KINDS: EventKind[] = ["user", "assistant"];

/** Tool / reasoning / system / usage activity — hidden in `show` unless asked for. */
export const NON_CONTENT_KINDS: EventKind[] = [
  "thinking",
  "tool_call",
  "tool_result",
  "system",
  "usage",
];

/** Kinds stored but excluded from the FTS index (bulk / numeric, not useful to search). */
export const UNINDEXED_KINDS: EventKind[] = ["tool_result", "usage"];

/**
 * One event: a semantically-distinct piece of a session, in order. `text` is the
 * normalized, full (untruncated) content — the display budget applies only when
 * `sess` prints. Fields beyond kind/text are the cross-agent primitives that make
 * the normalized layer useful without re-reading the raw trace:
 *   subtype  discriminator for `system` (native subtype) and `thinking` ("summary")
 *   name     tool name for tool_call / tool_result
 *   callId   links tool_call ↔ tool_result (Claude id/tool_use_id, Codex call_id,
 *            OpenCode callID, Pi toolCallId) — every agent has it; enables pairing
 *   isError  tool_result error flag
 *   nativeId / parentId  intra-session lineage (tree A)
 *   tokens   per-event token count where the agent records one (total)
 */
export interface Event {
  idx: number; // 0-based order within the session
  kind: EventKind;
  subtype: string | null; // system/thinking discriminator; else null
  name: string | null; // tool name for tool_call/tool_result
  callId: string | null; // tool_call ↔ tool_result join key
  isError: boolean | null; // tool_result error flag
  nativeId: string | null; // this event's native id (tree A)
  parentId: string | null; // native id of the parent event (tree A)
  tokens: number | null; // per-event token total where present
  text: string; // full normalized content, never empty (builder drops empties)
  ts: number | null; // epoch seconds, normalized
}

/** A pointer to one verbatim native trace file (or opencode.db#session). */
export interface RawSource {
  path: string; // absolute file path, or "<db_path>#<session_id>" for OpenCode
  kind: "jsonl" | "json" | "sqlite"; // native container format
  bytes: number | null; // for drift / staleness detection
  mtime: number | null; // epoch seconds
}

/**
 * Session-level metadata. Fields promoted out of the old JSON `meta` bag because
 * they are cross-agent and useful for filtering/grouping: model, provider,
 * gitBranch, and the two cross-session tree edges. `fingerprint` is the cheap
 * change-key ingest compares to decide whether to re-read (replaces turn_count —
 * it also moves when non-turn records like tool calls or compaction events grow).
 */
export interface UnifiedSession {
  id: string; // stable "agent:nativeid"
  agent: Agent;
  cwd: string | null;
  startedAt: number | null; // epoch seconds
  endedAt: number | null; // epoch seconds (approx — last event)
  title: string | null; // native title (Claude ai-title / OpenCode) else null
  model: string | null; // primary model where known
  provider: string | null; // provider where known
  gitBranch: string | null;
  parentSessionId: string | null; // tree B: subagent/child parent ("agent:nativeid")
  forkedFrom: string | null; // tree B: fork origin ("agent:nativeid#messageUuid")
  turnCount: number; // user+assistant events (conversation length)
  eventCount: number; // total events incl. thinking/tool/system/usage
  sources: RawSource[]; // pointer(s) to the verbatim trace(s)
  fingerprint: string; // cheap change-key for incremental ingest
  meta: Record<string, unknown>; // agent-specific remainder (cli_version, sandbox, cost, …)
}

/**
 * The parser contract. `listSessions` enumerates candidates cheaply (with a
 * fingerprint, no full read); `readEvents` reconstructs one session's ordered,
 * structure-preserving event stream from the raw trace.
 */
export interface Parser {
  agent: Agent;
  /** Enumerate sessions with metadata + fingerprint. `sinceEpochSec` bounds by updatedAt when cheap. */
  listSessions(sinceEpochSec: number | null): UnifiedSession[];
  /** Read one session's events in order. `nativeId` is the part after "agent:". */
  readEvents(nativeId: string): Event[];
}

/** Count user+assistant events (the conversation-length metric). */
export function turnCountOf(events: Event[]): number {
  let n = 0;
  for (const e of events) if (e.kind === "user" || e.kind === "assistant") n++;
  return n;
}
