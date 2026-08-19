# TraceDB search design

TraceDB searches episodic coding-agent history. The useful result is a session,
not an isolated event, so retrieval is intentionally session-oriented.

## Pipeline

1. FTS5 produces bounded event candidates in BM25 order.
2. Agent, working-directory, and time filters are applied in SQL.
3. Candidates are aggregated by session while preserving the first and strongest
   hit as the representative ordering signal.
4. Sessions are walked to their parent or fork root with cycle protection.
5. Related sessions collapse into one result and their hit counts are merged.

The current Rust implementation caps the SQL event stream and returns at most
the requested number of collapsed sessions. Tool results and usage events remain
available through `show` but are not included in FTS by default.

## BM25 constraints

SQLite FTS5 auxiliary functions such as `bm25()` cannot be used directly in an
aggregate query. TraceDB therefore streams ranked event rows and performs
session aggregation in Rust. This also keeps the implementation ready for kind,
recency, title, and coverage scoring without changing the FTS schema.

BM25 scores are smaller for stronger matches. SQL must use ascending order.
Regression tests protect this invariant.

## Lineage collapse

Search loads the small session-edge table once per request. For each candidate,
it follows `parent_session_id`, then the session portion of `forked_from`, until
it reaches a known root. Cycles terminate the walk safely. The strongest member
remains the representative and hit counts from related members are added.

This prevents a parent task and its subagents from occupying multiple result
slots while still rewarding work spread across the lineage.

## Planned scoring model

The next scoring layer will combine normalized components:

```text
score = best_bm25
      + coverage_weight * log1p(hit_count)
      + kind_weight * kind_bonus
      + recency_weight * exp(-ln(2) * age_days / half_life_days)
      + title_weight * title_match
      + lineage_weight * related_score
```

The public result type will expose the score breakdown, strongest hit, distant
secondary hit clusters, the first user request, and the last assistant outcome.
All candidate and context fan-out will remain explicitly bounded.

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

- Candidate event count is bounded before Rust aggregation.
- Lineage loading is one small query, not an N+1 walk.
- Search never reads native trace files.
- Reindex never reads native trace files.
- Result context assembly must use batch queries.
- Exact filters run in SQL before aggregation.
