/**
 * ingest.ts — the mechanical scan (PLAN §六). No LLM. For each agent: enumerate
 * sessions, and for any that is new or whose `fingerprint` changed, read its
 * full event list and upsert (session row + raw_source pointers + events).
 * Deterministic and cheap; semantic processing is outside this project.
 *
 * Incremental key: each session's freshly-listed `fingerprint` (record/message
 * count + last-event time) vs the stored one. This moves whenever ANY record
 * grows — including non-turn records like tool calls or compaction events —
 * unlike the old turn_count key. `--full` forces a re-read of every session.
 */

import { existingFingerprints, upsertSession } from "./db.js";
import { getParser } from "./parsers/index.js";
import { AGENTS, turnCountOf, type Agent } from "./types.js";

export interface IngestResult {
  agent: Agent;
  scanned: number; // sessions enumerated
  upserted: number; // sessions (re)written
  skipped: number; // unchanged
  errors: number;
}

export interface IngestOptions {
  agents?: Agent[]; // default: all
  sinceEpochSec?: number | null; // bound listing by updatedAt when cheap
  full?: boolean; // re-read even unchanged sessions
}

export function ingest(opts: IngestOptions = {}): IngestResult[] {
  const agents = opts.agents ?? AGENTS;
  const since = opts.sinceEpochSec ?? null;
  const results: IngestResult[] = [];

  for (const agent of agents) {
    const parser = getParser(agent);
    const res: IngestResult = { agent, scanned: 0, upserted: 0, skipped: 0, errors: 0 };
    let sessions;
    try {
      sessions = parser.listSessions(since);
    } catch {
      // a whole store being unreadable shouldn't kill the run — degrade gracefully
      res.errors++;
      results.push(res);
      continue;
    }
    const prior = existingFingerprints(agent);
    for (const s of sessions) {
      res.scanned++;
      const known = prior.get(s.id);
      if (!opts.full && known !== undefined && known === s.fingerprint) {
        res.skipped++;
        continue;
      }
      try {
        const nativeId = s.id.slice(agent.length + 1); // strip "agent:"
        const events = parser.readEvents(nativeId);
        // reconcile the metadata counts with the real event stream
        upsertSession({ ...s, turnCount: turnCountOf(events), eventCount: events.length }, events);
        res.upserted++;
      } catch {
        res.errors++;
      }
    }
    results.push(res);
  }
  return results;
}
