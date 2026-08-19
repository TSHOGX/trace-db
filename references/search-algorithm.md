# TraceDB search design

TraceDB searches episodic coding-agent history. The useful result is a session,
not an isolated event, so retrieval is intentionally session-oriented.

## Pipeline

1. The planner combines an exact phrase arm with individual-term recall arms;
   explicit FTS5 syntax is passed through unchanged.
2. FTS5 produces at most 50 hits per session and 5,000 total event candidates
   in BM25 order.
3. Agent, working-directory, and time filters are applied in SQL.
4. Candidates are aggregated by session while preserving the first and strongest
   hit as the representative ordering signal.
5. Explainable relevance, coverage, kind, recency, and title components are
   calculated for each session.
6. Sessions are walked to their parent or fork root with cycle protection.
7. Related sessions collapse into one result and their hit counts are merged.
8. First-user and last-assistant bookends are loaded for all top lineages in one
   batch query.

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

Search loads the small session-edge table once per request. For each candidate,
it follows `parent_session_id`, then the session portion of `forked_from`, until
it reaches a known root. Cycles terminate the walk safely. The strongest member
remains the representative and hit counts from related members are added.

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
`log1p(hit_count)`. `term_coverage` is the fraction of plain query terms found
across matched snippets and the title. Recency uses a 30-day exponential
half-life. The kind bonus is `user > assistant > system > thinking > tool_call`.

The public result exposes the full score breakdown, strongest matched event and
snippet, title and timestamps, lineage root and related members, the first user
request, and the last assistant outcome. When the strongest lineage member is a
subagent without an outcome, context assembly falls back to a related member.

## Tokenizers

The default binary uses SQLite `unicode61` for portable installation. The
optional `fts5-jieba` extension adds Chinese word segmentation, Unicode folding,
and colocated English Porter stems. The selected tokenizer is recorded in
`schema_meta` when the database is created.

Changing the tokenizer requires recreating `events_fts` and rebuilding it from
the gated event table. The `reindex` command deliberately uses `delete-all`
followed by a filtered insert; the FTS5 `rebuild` command would bypass the event
kind gating and index noisy tool-result and usage rows.

## Performance invariants

- Candidate event count is bounded globally and per session before aggregation.
- Lineage loading is one small query, not an N+1 walk.
- Search never reads native trace files.
- Reindex never reads native trace files.
- Result context assembly uses one batch query across representatives and
  related lineage members.
- Exact filters run in SQL before aggregation.
