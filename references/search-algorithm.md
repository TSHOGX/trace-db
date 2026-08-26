# TraceDB search design

TraceDB searches episodic coding-agent history. The useful result is a session,
not an isolated event, so retrieval is intentionally session-oriented.

## Pipeline

Retrieval runs in two phases. Ranking needs BM25, event metadata, and term
coverage; only the handful of results that survive ranking need snippet text.

Phase 1 — rank:

1. The planner combines an exact phrase arm with individual-term recall arms;
   explicit FTS5 syntax is passed through unchanged.
2. FTS5 produces at most 50 hits per session and 5,000 total event candidates
   in BM25 order. No snippet is generated in this phase.
3. Agent, working-directory, and time filters are applied in SQL; time filters
   use the end time and fall back to the start time for active sessions.
4. Per-candidate term coverage is computed in SQL against the event's full text
   by the registered `tracedb_term_coverage` scalar, which returns a bitmask.
5. Candidates are aggregated by session while preserving the first and strongest
   hit as the representative ordering signal, along with its FTS rowid.
6. Explainable relevance, coverage, kind, recency, and title components are
   calculated for each session.
7. A recursive lineage query loads only the parent/fork closure reachable from
   matched sessions, with cycle protection in the Rust walk.
8. Related sessions collapse into one result and their hit counts are merged,
   then results are truncated to the requested limit.

Phase 2 — present:

9. Snippets are generated for the surviving results only, in one batch query
   that re-issues the same planned `MATCH` constrained to the retained FTS
   rowids. Delimiters and the token budget are shared constants, so deferring
   generation cannot change snippet text; a regression test asserts byte
   equality against single-phase generation.
10. First-user and last-assistant bookends are read from materialized session
    columns for all top lineages in one batch query, including bounded ancestor
    paths that did not themselves match the query.

Tool results and usage events remain available through `show` but are not
included in FTS by default.

For a plain query such as `deploy netlify`, the planner emits:

```text
"deploy netlify" OR "deploy" OR "netlify"
```

The phrase arm rewards precision, while the individual arms retain sessions
where the terms occur in different events. Session term coverage then rewards
results that explain more of the query.

## BM25 constraints

SQLite FTS5 auxiliary functions such as `bm25()` cannot be used directly in an
aggregate query. TraceDB therefore streams ranked event rows and performs
session aggregation and scoring in Rust.

BM25 scores are smaller for stronger matches. SQL must use ascending order.
Regression tests protect this invariant.

## Lineage collapse

Search issues one recursive SQL query rooted at matched session IDs to load the
reachable session-edge closure. For each candidate, Rust follows the single
`parent_session_id` edge until it reaches a known root. Cycles terminate the walk safely. The strongest member remains
the representative and hit counts from related members are added.

This prevents a parent task and its subagents from occupying multiple result
slots while still rewarding work spread across the lineage.

## Scoring model

TraceDB combines normalized components:

```text
score = best_match
      + 0.25 * hit_coverage
      + 0.35 * term_coverage
      + 0.20 * kind_bonus
      + 0.25 * recency
      + 0.15 * title_match
      + 0.10 * sum(related_session_scores)
```

`best_match` is min-max normalized within the candidate set after reversing
FTS5's smaller-is-better BM25 direction. `hit_coverage` is normalized
`log1p(hit_count)`. `term_coverage` is the fraction of plain query terms found in
matched events' full text unioned with the session title. Recency uses a 30-day
exponential half-life. The kind bonus is
`user > assistant > system > thinking > tool_call`.

### Term coverage

Coverage is evaluated against complete event text, not a snippet excerpt. It was
previously derived from the 24-token snippet window, so a term that matched the
event but fell outside that window scored as uncovered — a ranking error at the
second-heaviest weight in the model. Deferring snippets to phase 2 also makes the
old derivation impossible, since no snippet exists when scoring runs.

The comparison runs in a registered Rust scalar rather than SQL's `instr(lower(
text),term)` for two reasons. Full event text never crosses the SQLite boundary,
and SQLite's built-in `lower()` folds ASCII only — computing coverage in SQL
would silently score accented and other non-ASCII terms as uncovered while the
Rust path folds them correctly. One definition of "this term is present" serves
event text and titles alike. Queries carrying more than 63 terms degrade
gracefully: terms past the bitmask width simply contribute no coverage.

The public result exposes the full score breakdown, strongest matched event and
snippet, title and timestamps, lineage root and related members, the first user
request, and the last assistant outcome. When the strongest lineage member is a
subagent without an outcome, context assembly first falls back to a matched
related member and then to bounded parent/fork ancestors, so an unmatched
parent can still supply the task request or final outcome.

## Tokenizers

The default binary uses SQLite `unicode61` for portable installation. The
optional `fts5-jieba` extension adds Chinese word segmentation, Unicode folding,
and colocated English Porter stems. The selected tokenizer is recorded in
`schema_meta` when the database is created.

Changing the tokenizer requires recreating `events_fts` and rebuilding it from
the gated event table. The `reindex` command recreates the external-content FTS5
table transactionally and inserts only searchable event kinds; the generic FTS5
`rebuild` command would bypass the event-kind gate and index noisy tool-result
and usage rows.

## Performance invariants

- Candidate event count is bounded globally and per session before aggregation.
- Snippet generation is proportional to returned results, not to candidates.
  Building snippets inside the candidate query dominated its cost.
- Term coverage adds no extra queries and transfers no event text.
- Lineage loading is one recursive query rooted at matched sessions, not a
  full-archive scan or an N+1 walk.
- Search never reads native trace files.
- Reindex never reads native trace files.
- Result context assembly uses one batch query across representatives, related
  lineage members, and bounded ancestor paths.
- Exact filters run in SQL before aggregation.
