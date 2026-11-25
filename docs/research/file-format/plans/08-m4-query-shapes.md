# cetra workload · M4: cetra's query shapes

Implementation plan for milestone M4 of [08-cetra-workload-design.md](08-cetra-workload-design.md). Revision 3, after two cold reviews.

M4 measures the queries cetra's UI and API run (spec 05), on M3's 30-day project, plus the trace detail of a 10,000-span trace on a new M1-style case. Every query is written once per format with the same algorithm, units and parallelism, and its answer is compared across formats. M4.1 to M4.3 are each done by one agent in one go, in order; each ends with the checks green and one commit. M4.4 is the runs and the report, done by the orchestrating session.

**Prerequisites:** M3 is committed, with its review fixes (`cw project`, `cw layout`, `bench/src/cw/many.rs`).

**Read first:**
- [08-cetra-workload-design.md](08-cetra-workload-design.md): § Decisions (CW-3, CW-7, CW-12, CW-17, CW-18, CW-23), § M4.
- [08-m1-likely-failures.md](08-m1-likely-failures.md): § Conventions, § Build; [08-m3-many-files.md](08-m3-many-files.md): the project directory and `many.rs`'s contracts.
- spec 05: § Full-text search, § Dashboard metrics (`bucket=auto`), § Trace and span APIs, § Logical views (`sessions`); spec 03 § 6 `TracesProvider` (partials merged per trace: `start_time` min, `end_time` max, `cost_total` sum).
- `bench/src/cw/many.rs`: `list`, `open`, `Opened`, `Listing`, `FileRef`, `ProjectCase`, and the rollup (`RollUnit`, `roll_unit`, `rollup_ranges`, `rollup`), whose unit pattern M4.2 generalizes.
- `bench/src/cw/read.rs`: `pattern`, `search_mask`, `Answer`.
- `bench/src/cw/cases.rs`: `gen_spans`, `write_case`, `CaseFiles`, `queries_for`, `pick_traces`, `NEEDLES`, `ABSENT`, the `keys*` and `giant` arms as examples.
- `crates/core/src/schema.rs` (~lines 200–230): `spans` columns and types: `start_time` is `Timestamp(Nanosecond, Some("UTC"))`, `duration_ns` `Int64`, `cost_total` `Float64`, `tags` `List<Utf8>`, `attributes` and `metadata` `Map<Utf8, Utf8>`; `traces` columns `trace_id`, `start_time`, `end_time`, `cost_total`, `session_id`, `user_id`.
- `testdata/src/datagen.rs`: `metadata` keys `customer_id` and `plan`.

**Conventions:** those of M1. Paths are under `docs/research/file-format/` unless they start with `crates/`. A rustc crash in thin-LTO codegen has happened several times on this machine: rerun the same build once.

## What it is, and what it is not

**When M4 is done:**
- A `mega` case (one trace of 10,000 spans among 10,000 traces), measured with M1's `trace_light` and `span_heavy`.
- `fmtbench cw needles <dir>` writes the project needles file into an existing directory.
- New `cw` queries on a project directory: `trace_list:start_time`, `trace_list:cost_total`, `trace_list:duration`, `sessions:7d`, `users:30d`, `facets:7d`, `dashboard:7d`, `dashboard:30d`, `search_1d:<name>`.
- M4.4's runs give M4's verdicts on the small and the verdict 30-day projects and on `mega`, locally and on R2.

**Not in M4:**
- the n-gram index path of search (M1 covers it; `search_1d` scans on both formats, inside the time window);
- concurrency (M5);
- the real datasets: M1 runs `search`, `scan_payload` and the lookups on `swe` and `wildchat`;
- deliberate cuts from the design's list: the trace-level error rate of the dashboard, and the `models` list facet (a `traces` column; M4's facets read `spans`).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| CW-3 | Each query is written once per format, with the same algorithm. |
| CW-7, CW-12 | Strata must be faster at median and p95, and use less memory. |
| CW-17 | Search is `ILIKE` with arrow's kernel. |
| CW-23 | The 30-day project's composition (2% of traces carry real payloads). |

Plan choices (not design decisions): the trace list breaks ties by `trace_id` descending, as cetra's fast path sorts (`start_time DESC, trace_id DESC`, spec 03 § 6); a null `cost_total` counts 0.

## Versions

No new dependency.

## Phase graph

| Phase | Edit | Depends on |
|---|---|---|
| M4.1 mega case and needles | `bench/src/main.rs` (`USAGE`), `bench/src/cw/mod.rs` (`"needles"` arm), `bench/src/cw/cases.rs`, `bench/src/cw/project.rs` | M3 |
| M4.2 trace queries | `bench/src/cw/mod.rs`, `bench/src/cw/many.rs` (visibility only), `bench/src/cw/shapes.rs` (new), `bench/src/cw/harness.rs` | M4.1 |
| M4.3 span queries | `bench/src/cw/shapes.rs` | M4.2 |
| M4.4 runs and report | `strata-readiness.md`, `plans/08-cetra-workload-design.md` (orchestrator) | M4.3 |

---

## M4.1: mega case and needles

**Goal:** a case with a 10,000-span trace, and needles for projects.

**Edit:** `bench/src/main.rs` (`USAGE`: ` | cw needles <dir>`), `bench/src/cw/mod.rs` (the `"needles"` arm), `bench/src/cw/cases.rs`, `bench/src/cw/project.rs`.

### Contracts

```rust
// cases.rs
/// The needles of a project (`cw_needles.txt`): planted ones of `NEEDLES`, which a project built
/// with `--payloads` from a planted case carries, and natural ones.
pub const PROJECT_NEEDLES: [(&str, &str); 5] = [
    ("absent", ABSENT), ("bug", "bug"), ("404", "404"), ("common", "commonneedle"), ("role", "\"role\""),
];

// project.rs
/// `cw needles <dir>`: writes `cw_needles.txt` with `PROJECT_NEEDLES` (M1's format) into an
/// existing project directory, replacing it if present.
pub fn needles_cmd(args: &[String]) -> crate::R<()>;
```

### Steps

1. **`mega` case** (`cases.rs`): a new case name, default 10,000 traces, generated as the others (`World::new(7)`, `Rng::new(42)`): add `"mega" => (10_000, None)` to the `(default_traces, keys)` match, and a `mega` flag checked where `gen_spans` calls `plant` and builds the needles, so that the mega case gets no payload stripping, no `plant`, and an empty needle list (no `cw_needles.txt` is written when the list is empty). The mega trace is the first generated trace that has an LLM span with a non-null `input`; it is extended in place to exactly 10,000 spans (same `trace_id`; the trace count stays the requested one) by appending clones of that LLM span. Clone `j` (from 0): `span_id = SpanId::from_bytes(rng.next().to_be_bytes())`, drawn again while it fails or repeats an id of the trace; `parent_span_id = Some(root span id)`; `start_time = root.start_time + 1_000 * (j + 1)`; `end_time = start_time + 500`; everything else as the cloned span. The clone loop runs right after generation, before any picking, so the rng order is fixed. Add `mega` to the error text listing the cases and an explicit `"mega"` arm to `queries_for` (its final `_` arm is `intl`): `write`, `trace_light`, `span_heavy`.
   - `cw_traces.txt`: the mega trace's id first, then 19 ids from a new `pick_traces_excluding(spans, rng, exclude)`, which picks as `pick_traces` does but skips the excluded id.
   - `cw_spans.txt`: written by hand, not by `pick_pairs`: 20 clones of the mega trace (`j = 0, 500, 1000, …, 9500`), whose input is the cloned span's, so non-null.
   - Known limit: the clones share one input and output, and their `duration_ns` is the cloned span's, not `end − start`; both formats store the same rows.
2. **Project needles** (`project.rs`): both project forms write `cw_needles.txt` with `PROJECT_NEEDLES`; `needles_cmd` writes it into an existing directory (for the projects built before M4).

### Tests

- `mega_case_has_one_big_trace` (`cases.rs`): `gen_spans("mega", Some(50))`: exactly one trace has 10,000 spans; its span ids are distinct; `cw_traces.txt`'s first id is it and the other 19 differ from it; every `cw_spans.txt` span belongs to it.
- `projects_write_needles` (`project.rs`): the 2-day test project has `cw_needles.txt` with the five names; `needles_cmd` on a directory without it writes it.

### Done when

In `bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib`:
- `cargo build --release`, `cargo test --release cw::` pass.
- `target/release/fmtbench cw gen mega /tmp/cw-mega --traces 500`, then `target/release/fmtbench cw run /tmp/cw-mega --rounds 2 --runs 2`: no `ANSWER MISMATCH`, a verdict per query.

**Commit:** `Add the fmtbench cw mega case and project needles`

---

## M4.2: trace queries

**Goal:** the queries on the `traces` files: the trace list in three sorts, and the sessions and users rollups.

**Edit:** `bench/src/cw/mod.rs` (`pub mod shapes;`), `bench/src/cw/many.rs` (make `pub(crate)`: `rollup_ranges`, `column`, `add`, `day_text`, `answer_of`, `text_lines`, `int64_col`, `llm_mask`, `BATCH_ROWS`, `LLM`, `DAY_NS`, `HOUR_NS`; no other change), `bench/src/cw/shapes.rs` (new), `bench/src/cw/harness.rs`.

### Contracts

```rust
/// The M4 queries on a project.
pub(crate) fn is_shape(query: &str) -> bool;
/// The table a query reads: `traces` for trace_list, sessions, users; `spans` for the rest.
pub(crate) fn table_of(query: &str) -> &'static str;
/// One run. `opened` holds the opened files of `table_of(query)`, in the order of `files`.
pub(crate) async fn run(query: &str, files: &[FileRef], opened: &Opened, case: &ShapeCase)
    -> crate::R<Answer>;

/// The project's clock (`cw_clock.txt`) and needles (`cw_needles.txt`, M1's format).
pub(crate) struct ShapeCase { pub clock: i64, pub needles: Vec<(String, String)> }

/// The rows a scan keeps, the same on both formats: `start_time` in `[from, to)`, and
/// `span_kind = 'llm'` when `llm_only`, and `trace_id` in `ids` when given.
pub(crate) struct Window { pub from: i64, pub to: i64, pub llm_only: bool, pub ids: Option<Arc<Vec<[u8; 16]>>> }

/// A per-unit result that merges with another; merges are commutative and associative.
pub(crate) trait Fold: Default + Send + 'static { fn merge(&mut self, other: Self); }

/// What a unit does with each batch it reads.
pub(crate) type FoldFn<T> = Arc<dyn Fn(&RecordBatch, &mut T) -> crate::R<()> + Send + Sync>;

/// Scans `columns` of the files whose day is in the window's days ±1 (every day from the day of
/// `from` minus one to the day of `to` plus one, compared as `many::day_text` strings), with
/// `window`, unit by unit, `cores` units at a time
/// (`buffer_unordered`), folding each batch with `fold`, and merges each unit's result into
/// one accumulator as the unit finishes (merges are commutative, so completion order does
/// not matter). `files` and `opened` are index-aligned.
pub(crate) async fn scan_fold<T: Fold>(files: &[FileRef], opened: &Opened, columns: &[&str],
    window: &Window, fold: FoldFn<T>) -> crate::R<T>;
```

**Units of `scan_fold`**, as `many::roll_unit`:
- Parquet: one per (file, row group). A row group whose `start_time` min/max statistics miss the window is skipped (on files sorted by `(trace_id, start_time)` this rarely prunes; the day filter does the work). The output projection is `ProjectionMask::roots` of `columns`. A `RowFilter` with one `ArrowPredicateFn` whose `ProjectionMask` holds `start_time` (plus `span_kind` when `llm_only`, `trace_id` when `ids` is given) keeps the window's rows, finding each column with `column_by_name`: `start_time` values (`TimestampNanosecondArray`, compared as `i64`) in `[from, to)`, `span_kind == "llm"`, `trace_id` in `ids`.
- Strata: one per `many::rollup_ranges` range of each file (its `u64` bounds converted with `u32::try_from`, as `roll_unit` does), `scan_batches` with threads 1, `views: false`, projection `columns` by schema index, and `predicate = Predicate::And` of `Cmp(start_time, GtEq, Lit::Int(from))`, `Cmp(start_time, Lt, Lit::Int(to))`, when `llm_only` `Cmp(span_kind, Eq, Lit::Utf8("llm"))`, and when `ids` is given `In(trace_id, ids as Lit::Bytes)`.

Parquet returns the projected columns in file-schema order and Strata in the requested order, so **every fold looks its columns up by name** (`column_by_name`, as `many::fold_rollup` does via `many::column`), never by position. Strings are `StringArray`.

**Windows.** `clock` is `cw_clock.txt`. `7d` is `[clock − 7 days, clock)`, `30d` `[clock − 30 days, clock)`, `1d` `[clock − 1 day, clock)`. The trace queries scan the padded window `[from − 1 h, to + 1 h)` and keep a trace when its merged `start_time` is in `[from, to)`.

| Query | Fold, the same on both formats | Rows | Answer bytes |
|---|---|---|---|
| `trace_list:<sort>`, window 7d | Phase 1: columns `trace_id, start_time, end_time, cost_total`; a map per `trace_id`: min start, max end, sum of `round(cost × 1e6)` as `i64` (null = 0). Keep traces whose merged start is in the window. Sort descending by the sort value (`start_time`; `cost_total`; `duration` = end − start), then `trace_id` descending; take 50. Phase 2, the detail fetch: a second `scan_fold`, padded window, with `ids` = the 50 ids, over every `traces` column (the names of the first opened file's schema; no file means an empty answer), whose fold counts the rows. | 50 | the 50 ids in order (16 bytes each), then the phase-2 row count `u64` |
| `sessions:7d` | columns `trace_id, start_time, end_time, cost_total, session_id`: merge partials per trace as above (keeping the first non-null `session_id` seen, any order: a trace has one), keep the window's traces, then group by non-null session: trace count, min start, max end, cost sum | sessions | sessions sorted bytewise, each: id bytes, `0x00`, count `u64`, min start `i64`, max end `i64`, cost `i64`, little-endian |
| `users:30d` | the same with `user_id`, window 30d | users | as `sessions` |

**`harness.rs`:** in `one`, route `shapes::is_shape(query)` before the `is_write_like` fall-through to a new `shape_format(dir, format, query, runs)`: list the files (`many::list`), read `ShapeCase` (`clock` from `cw_clock.txt`; `needles` from `read_case(&loc)`'s needles, which are already hex-decoded), open only `table_of(query)`'s files (`many::open`), all outside the timed region; one warm-up run, then `loc.store.reset()`, then `runs` measured runs, one timing each; `gets` and `bytes` from `loc.store.stats()` after the first measured run, as `many_format` does; every run's answer must equal the warm-up's. `waves` is `null`.

### Tests (`shapes.rs`)

On a project made once in a temp dir with `cw project --days 9 --traces-per-day 400 --payloads <a text_unique case>` (the case built by `cases::gen_spans("text_unique", Some(200))` and `cases::write_case`, so the payloads carry the planted needles):
- `trace_lists_agree`: the three sorts give equal answers on both formats, 50 ids each, in the stated order (check the order on values recomputed from the Parquet `traces` rows).
- `sessions_and_users_agree`: equal answers on both formats; at least one session and one user.
- `scan_fold_respects_the_window`: on the `traces` files, `scan_fold` with columns `["trace_id", "start_time"]`, the unpadded 7d window and a fold that counts rows gives the same count on both formats, equal to a count over every `traces` row read in full with `ParquetRecordBatchReaderBuilder` and the same window applied.

### Done when

- `cargo build --release`, `cargo test --release cw::` pass.
- `target/release/fmtbench cw gen text_unique /tmp/cw-tu --traces 2000`, `target/release/fmtbench cw project /tmp/cw-p9 --days 9 --traces-per-day 2000 --payloads /tmp/cw-tu`, then `target/release/fmtbench cw run /tmp/cw-p9 --queries trace_list:start_time,trace_list:cost_total,trace_list:duration,sessions:7d,users:30d --rounds 2 --runs 2`: no `ANSWER MISMATCH`, a verdict per query.

**Commit:** `Add fmtbench cw trace list, sessions and users queries`

---

## M4.3: span queries

**Goal:** the queries on the `spans` files: facets, dashboard series and the windowed search.

**Edit:** `bench/src/cw/shapes.rs`.

**Bucket width** (spec 05 § Dashboard metrics, `bucket=auto`): the smallest of 1 m, 5 m, 10 m, 15 m, 30 m, 1 h, 6 h, 12 h, 1 d whose buckets aligned to the epoch number at most 120 over `[from, to)` (count = `floor((to − 1) / w) − floor(from / w) + 1`), else 1 d. For `7d` that is 6 h; for `30d` from an unaligned clock, 12 h.

| Query | Columns, window | Fold | Answer lines |
|---|---|---|---|
| `facets:7d` | `status, span_kind, environment, request_model, service_name, user_id, session_id, tags, duration_ns, cost_total, attributes, metadata`, window 7d | counts per value of the first five (null = `""`); for `user_id` and `session_id`, counts per non-null value (the answer keeps the distinct count and the top 20); counts per tag occurrence (`tags` is `List<Utf8>`: each element of each row's list); `duration_ns` in log2 buckets (`63 − leading_zeros(v)` for `v > 0`, bucket −1 for null or `v ≤ 0`); `cost_total` by decade (`floor(log10(c))` for `c > 0`, bucket −99 for null or `c ≤ 0`); counts per key of `attributes` and of `metadata` (`Map<Utf8, Utf8>`: `MapArray::value(i)` is the row's entries, keys in its column 0; a key counts once per occurrence) | `<facet>\t<value>\t<count>` for every counted value (histogram buckets as decimal integers); for `user_id` and `session_id` instead `<facet>\t#distinct\t<n>` and `<facet>\ttop\t<rank:02>\t<value>\t<count>` for the 20 largest counts (count descending, then value ascending) |
| `dashboard:7d`, `dashboard:30d` | `start_time, request_model, status, cost_total, duration_ns, tags, metadata`, window 7d or 30d, `llm_only` | per (bucket, `request_model`, null = `""`): count, errors (`status = "error"`), cost micro-units, every non-null `duration_ns` (a `Vec<i64>`; the merge concatenates); per (bucket, tag occurrence): count; per (bucket, `metadata['plan']` value, absent = `""`): count | `m\t<bucket start>\t<model>\t<count>\t<errors>\t<cost>\t<p95>` with the p95 by nearest rank over the sorted durations (empty: `-`), `t\t<bucket start>\t<tag>\t<count>`, `k\t<bucket start>\t<plan>\t<count>` |
| `search_1d:<name>` | `trace_id, span_id, input, output, name, system_instructions`, window 1d | keep rows where `read::search_mask(batch, &read::pattern(needle))` is true (the closure captures the pattern); collect their `(trace_id, span_id)` | none: the answer is `(trace_id, span_id)` pairs sorted, 24 bytes each |

For the facets and the dashboard, the answer is all its lines sorted bytewise, joined by `\n`, hashed; the row count is the number of lines. Facet names in the lines: `status`, `span_kind`, `environment`, `request_model`, `service_name`, `user_id`, `session_id`, `tag`, `duration_log2`, `cost_decade`, `attr_key`, `meta_key`. The p95 is the value at index `(95 * n).div_ceil(100) - 1` of the sorted durations (integer arithmetic). `search_1d` is a window scan on both formats by design: it reads the six columns of every row in the window, unlike M1's search, which Strata answers with its index or a row filter.

### Tests (`shapes.rs`)

On M4.2's 9-day project:
- `facets_agree`: equal answers on both formats; the `span_kind` lines' counts add up to the window's span count.
- `dashboards_agree`: equal answers for 7d and 30d on both formats; at least one `m` line; the bucket width used equals the rule's result for that window (a pure function `bucket_width(from, to)`, tested alone: `[0, 7 days)` gives 6 h, `[1 h, 30 days + 1 h)` gives 12 h, `[0, 30 days)` gives 6 h).
- `window_searches_agree`: `search_1d:bug`, `search_1d:common` and `search_1d:absent` equal on both formats and equal to a reference computed from every `spans` row of the window read in full (per row, per column, `ilike` on a one-value array); `absent` has 0 rows.

### Done when

- `cargo build --release`, `cargo test --release cw::` pass.
- `target/release/fmtbench cw run /tmp/cw-p9 --queries facets:7d,dashboard:7d,dashboard:30d,search_1d:bug,search_1d:common --rounds 2 --runs 2`: no `ANSWER MISMATCH`, a verdict per query.

**Commit:** `Add fmtbench cw facets, dashboard and windowed search queries`

---

## M4.4: runs and report (orchestrator)

On ovh-us, idle, after M3's runs: `cw needles` on the projects built before M4.1; `cw run <dir> --queries <the nine M4 queries, search_1d for each needle> --rounds 26 --runs 2` on `~/cw/p30-small` and the verdict project, locally and on R2 (S3 profile); `cw gen mega ~/cw/mega`, `cw run ~/cw/mega --rounds 26 --runs 2`, locally and on R2. Write M4's verdicts in `strata-readiness.md` § M4 and mark M4 in the design doc.

## Unverified

- Memory of the exact p95 at 30M traces: the 30-day dashboard holds every LLM span's duration (about 60M `i64`, 480 MB), the same for both formats.
- `trace_list` merges the partials inside the padded window only; a trace whose partials lie more than 1 h outside it is cut, the same way on both formats (spec 03 § 6's known limit).
