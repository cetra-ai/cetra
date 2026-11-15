# Storage hardening: findings

Track R (`docs/plans/r-hardening/`) checks the storage engine against its spec and records what it finds here. Each milestone has a section. Each finding has an id, `M<milestone>-<n>`, which the commits and the plan refer to.

## M1: Model-based correctness

The model test in `crates/storage/src/model_tests/` (plan `docs/plans/r-hardening/01-m1-model.md`, decisions D-R-3 and D-R-4). A proptest runs random operations against the real `Storage` and against a small in-memory model. After every operation it reads storage back through the same read functions the server calls and compares the results with the model.

What it covers:
- **Writes:** span appends and re-sends, score versions and score deletions, across two projects that share trace numbers.
- **Reads:** the span list (paged), the trace list (windowed, with and without evaluation traces), the score list, scores by id and latest scores, sessions, users, metrics KPIs, the filter-deletion trace ids and alert measures.
- **Deletions:** trace and user deletions, their scope reads, evaluation traces, the applied status, retention and project drop.
- **Lifecycle:** restarts and in-process crashes (the memtables are lost, the WAL is kept; lossy crashes come in M2), compaction, garbage collection and settling.
- **Settled files:** after each settle, the files on disk hold no row a deletion covers, nothing before the retention watermark, no file of a dropped project, no duplicate span, no unmerged score, one partial per trace (two for class-2 traces that cross midnight), and no staged object.

### Runs

| Date | Commit | Cases | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | R1.7 | 500 | release | not run | deferred (long run) |

### Findings

| Id | Shrunk case | Cause | Classification | Fix |
|---|---|---|---|---|
| M1-1 | `regression_m1_1_span_paging_with_duplicates` | `span_list` returned duplicate copies of a re-sent span, and its keyset cursor could not tell them apart, so pages depended on the page size. | storage | e002170 and 8e907d8: sort `ingest_time DESC` last, drop adjacent copies per page, bound the refetch |
