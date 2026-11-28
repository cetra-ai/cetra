# Strata v0 · M5F: DataFusion pushdown and fast-path fixes from the M5–M6 code review

This is one sub-phase, done by one Haiku agent in one go. It ends with the checks green and one commit.

The review confirmed three cases where an `Exact` pushdown returns wrong rows or fails, plus two `trace_list` defects. The merge and fast paths otherwise matched brute force over randomised runs. Its probe tests are in `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad/ff/strata/tests/{probe,mergefuzz,fastfuzz}.rs`. Read them: they show how each defect was reproduced.

**Prerequisites:** M6.2 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Conventions.
- [`04-m4-scan.md`](04-m4-scan.md): § Rules for every sub-phase.
- [`05-m5-datafusion-fast-paths.md`](05-m5-datafusion-fast-paths.md): § Filter translation.
- [`05-datafusion-api.md`](05-datafusion-api.md).
- `strata/src/{datafusion.rs,predicate.rs,fast.rs,scan.rs}`.

**Edit:**
- `strata/src/{predicate.rs,datafusion.rs,fast.rs,scan.rs}`;
- `strata/tests/{datafusion.rs,fast_paths.rs}`;
- `docs/research/file-format/prototype-plan.md` (§ S8, § S9).

GOLDEN must not change.

## Steps

1. **Boolean constants in predicates** (`predicate.rs`, `scan.rs`, `datafusion.rs`).
   - **The bug.** DataFusion's simplifier can rewrite a filter after `supports_filters_pushdown` has accepted it. For example, `i IN (1, NULL)` becomes `i = 1 OR NULL`. `scan` must translate the rewritten form too.
   - **New variant.** Add `/// A constant: TRUE, FALSE or NULL. Const(Option<bool>)` to `Predicate`, and the same to `Bound`.
     - `bind` accepts it as is.
     - `evaluate` returns a `BooleanArray` of `rows` copies of the value, or `rows` nulls for `None`.
     - In pruning, `Const(Some(false))` and `Const(None)` as a top-level conjunct prune every chunk. Any other use prunes nothing.
     - `check_size` counts it as one node.
   - **Translation.** `translate` maps `Expr::Literal(ScalarValue::Boolean(v), _)` to `Exact(Const(v))`.
   - **Tests.** Add `Const` to the M4.2 predicate proptest generator, and add a unit test of each value.

2. **No silently dropped filter** (`datafusion.rs`, `scan`). If any filter given to `scan` translates to `No`, return `DataFusionError::Plan(format!("StrataTable cannot evaluate a pushed filter: {expr}"))`. DataFusion only passes filters it was told are `Exact` or `Inexact`, so a `No` here means the two translations disagree. A loud error is safe; a dropped filter returns wrong rows. Update the module doc, which claims the two "always agree", to describe this guard.

3. **No float pushdown** (`datafusion.rs`, `lit_of`).
   - DataFusion compares floats with `-0.0 == 0.0`, while Arrow's kernels, which Strata uses, use the IEEE total order.
   - So `lit_of` returns `None` for `Float32` and `Float64` columns, and every float comparison becomes `No`.
   - Update the translation table in the doc comment and § S8: "float comparisons are not pushed; DataFusion's float equality differs from Arrow's total order".

4. **Combined size** (`datafusion.rs`).
   - **Pushdown.** `supports_filters_pushdown` translates the filters in order and keeps a running `And` of the predicates of those it pushes. A filter whose addition would make `check_size(&And(kept + [it]))` fail is answered `Unsupported`.
   - **Scan.** `scan` builds `kept` the same way. If the combined predicate still fails `check_size`, it returns `DataFusionError::Plan`, by the same reasoning as step 2.

5. **`trace_list` with an empty projection** (`fast.rs`). Build the output batch with `RecordBatch::try_new_with_options(schema, columns, &RecordBatchOptions::new().with_row_count(Some(n)))`, so that zero columns work. Test: `trace_list(.., &[])` returns a batch with 0 columns and the right row count.

6. **`trace_list` memory** (`fast.rs`).
   - **Compaction.** After each chunk, when the kept batches hold more than `2 × k + 8_192` rows in total, compact them:
     - build one batch of the heap's current rows with `interleave`;
     - replace `kept` with that single batch;
     - rewrite each heap entry's `(batch, row)` to `(0, i)` in heap order.
   - **Purpose.** The memory then stays proportional to `k`, not to the window.
   - **Tests.** The existing tests must still pass. Add a test with `k = 3` over a window of more than 20,000 rows, in a file written with `chunk_max_rows: 256`, that matches the DataFusion or brute-force result.

7. **Regression tests** (`tests/datafusion.rs`). Add a **non-ignored** test `pushdown_edge_cases`:
   - **Data.** A small table of 1,000 rows with:
     - a sort key `trace_id`;
     - `i Int64` with nulls;
     - `f Float64` holding `-0.0`, `0.0`, `1.5`, NaN and null;
     - `ts Timestamp(ns, "UTC")`.

     Write it as one Strata file, and register it as `t` (Strata) and as `m` (a DataFusion `MemTable` from the same batch).
   - **Queries.** Each runs on `t` and `m`, and the row counts must be equal:
     - `i IN (1, NULL)`, `i NOT IN (1, NULL)`, `NOT (i IN (1, NULL))`, `i IN (1, 2, NULL) OR i IS NULL`, `i IN (NULL)` and `i NOT IN (NULL)`;
     - `f = 0`, `f <> 0`, `f IN (0.0)`, `f < 0`, `f >= 0`, `f BETWEEN 0 AND 0`, `f > -0.0` and `f = 'NaN'::double`;
     - `i IN (<2,100 literals>) AND i IN (<2,100 other literals>)`: it must not error;
     - `ts >= TIMESTAMP '…Z'` with a timestamp inside the data, and `i > 5 AND NOT (i < 900 OR i IS NULL)`.

   The queries use `IN` over `i` and comparisons over `f` exactly as the review's probe does. Copy its SQL from `probe.rs`.

8. **Docs.** § S8 records:
   - floats are not pushed;
   - the `Const` predicate;
   - the `scan` guard;
   - the running size check.

   § S9 records the compaction in `trace_list`.

## Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light scan_datagen datafusion_equivalence fast_paths merge_memory fuzz_scan fuzz_lookup --nocapture
```

**Commit:** `strata: fix pushdown of rewritten, float and oversized filters; bound trace_list memory (M5F)`
