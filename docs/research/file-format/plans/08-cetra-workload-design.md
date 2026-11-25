# Design: judging Strata on cetra's workload

Date: 2026-11-25. Status: draft, for review before any plan.

## Why

Until now Strata was judged on bench.vortex.dev suites (ClickBench, TPC-H, JOB, …) through DataFusion, and on fmtbench's nine queries over four large `spans` files per dataset. That shows how Strata does as a general columnar format inside DataFusion. It does not answer the question that matters now: should cetra's own query engine, which will replace DataFusion, read Strata instead of Parquet?

cetra does not run TPC-H. Its storage (spec 03) writes three tables (`spans`, `traces`, `scores`) per flush, in small L0 files that compaction merges into L1 files, partitioned by project and day, often on S3. Its queries (spec 05) are a short, known list: trace list, trace detail, facets, dashboard series, sessions and users, and full-text search.

This design changes what Strata is measured on, and how.

## Goals

1. Measure Strata against cetra's own Parquet files on cetra's own file layout and queries, locally and on S3. Every result reports latency (median, p95, p99), CPU, bytes read, GET count and peak RSS.
2. Find early the cases most likely to rule Strata out: map-key explosion, payloads with no repetition, and short or common search needles.
3. Improve Strata where it loses, driven by profiles, with each change A/B'd on these benchmarks so nothing gets slower.
4. End with a go/no-go for using Strata in cetra's engine, per milestone, written up in [../strata-readiness.md](../strata-readiness.md).

## Non-goals

- **GDPR deletion** (the deletion rewriter, spec 03 § 7). Left out by decision; Strata has no rewrite path and none is built here.
- **Vortex.** It drops out of the comparison. It reads 30–250 MB per trace lookup and can't store 16-byte IDs, so it is not a candidate for cetra.
- **DataFusion.** No benchmark here goes through it. The bench.vortex.dev suites stay only as a regression check on Strata's general scan speed.
- **Building cetra's engine.** The query shapes are hand-written for the benchmark, once per format, and are not the engine.
- **Changing cetra's storage.** Spec 03 stays on Parquet. Nothing in the product changes.

## Architecture

The benchmark grows out of fmtbench (`bench/`), which already writes both formats, reads from local disk or R2, simulates S3, probes the store and records request timelines.

**Layout writer.** It turns a dataset into a cetra-shaped project:
- `spans` files sorted by `(trace_id, start_time)`;
- a `traces` file derived from each `spans` file, with one partial per trace, as spec 01's merge rules compute it;
- `scores` files sorted by `created_at`;
- partitioned by day, as L0 files the size of a flush (from a few hundred rows to 256 MiB), and as L1 files after compaction;
- with about 40% of traces split across files, as late spans and flush boundaries split them in cetra (fmtbench's `trace_bench` uses the same share), so a trace's partials are merged across files.

Both formats get the same rows, cut at the same boundaries, with the same sort orders. The Parquet files use cetra's settings (spec 03 § 3: zstd 3, 64 MiB / 100,000-row row groups, 1 MiB pages, page index, bloom filters on the same columns); this is fmtbench's `pq_cetra` variant. Strata files use the settings cetra would use: the object-storage read profile (exact tail length from the catalog, 256 KiB lookup gap capped at 1 MiB).

**Readers.** Each query shape is written twice, once on each format's own reader, with the same algorithm on both:
- Strata: `StrataFile`'s scan and lookup calls.
- Parquet: arrow-rs with the page index, bloom filters and row selections, as fmtbench's `pq_fast` does for lookups. Parquet gets the pruning cetra's spec relies on; no shape is allowed a trick on one format that the other could have.
- Both read through the same object store wrapper, which counts GETs and bytes and can add cetra's 1 MiB-block read cache (spec 03 § 6).

**Answers.** Both readers' answers are compared row for row on every run; a difference fails the run.

**Measurements.** Every run reports, per query: median, p95 and p99 latency, process CPU time, bytes and GETs from the store, waves (rounds of requests that wait for each other), and peak memory.
- **Memory.** A process's peak RSS (`VmHWM`) only grows, so one process can't measure several queries or both formats. Every measured run is a fresh process per query and format (CW-18), which reads its own `VmHWM`; that also counts mapped pages and allocator fragmentation. CW-12 is judged on it.
- **Rounds.** The formats run in alternating rounds. On R2, latency jitters (the `wildchat` scan took 3.5 s in one round and 7.0 s in another), and CW-7 has no tolerance, so verdict runs take at least 25 alternating rounds per query, and the p95 is computed over all their runs.
- **Where.** S3 runs go to the US R2 bucket from ovh-us, on an otherwise idle machine. The simulator, calibrated within ±15% of R2, is used to iterate between real runs.

**Data.**
- The six real datasets already on R2 (`light`, `payload`, `swe`, `openswe`, `toucan`, `wildchat`), laid out as above.
- A 30-day project with many files, built from the datasets and fmtbench's generator (CW-8), in two sizes with the same 100–500 files: 30M traces for verdict runs, and 1–3M traces for daily A/B runs (CW-13).
- Adversarial generators for milestone 1: many distinct map keys, high-entropy payloads, mostly non-LLM spans, spans near the 8 MiB payload cap (D40), and non-ASCII text.

## Decisions

| ID | Decision | Rejected alternative and why |
|---|---|---|
| CW-1 | The baseline is cetra's Parquet configuration (`pq_cetra`), read without DataFusion. | Best-of-three Parquet variants: cetra ships one configuration, so the others don't answer the question. |
| CW-2 | Vortex is dropped. | Keeping it: it fails trace lookups by 2–6× and can't store cetra's IDs, so it can't be chosen. |
| CW-3 | Each query shape is hand-written per format, same algorithm on both. | DataFusion on both: the new engine replaces it, and its plans differ per format (as ClickBench q23 showed). |
| CW-4 | Each milestone ends in a verdict. A failure that can't be fixed stops the later milestones. | Measuring everything first: milestone 1 targets the likeliest failures so a no-go costs least. |
| CW-5 | Strata changes found along the way are made in the milestone that finds them and A/B'd on these benchmarks before they are kept. | A separate fix phase: findings go stale and A/Bs lose their context. |
| CW-6 | GDPR deletion is out of scope. | Asked by the user. |
| CW-7 | Strata wins a shape only when it is faster than Parquet at both the median and p95, locally and on R2. A shape it doesn't win is a failure for that milestone's verdict. | A 10% tolerance: rejected by the user. |
| CW-8 | The 30-day project holds 1M traces a day (about 30M traces, 100–500 files), built by repeating the real datasets' rows with new IDs and times. | 100k traces a day: too small to show fan-out on S3. |
| CW-9 | `scores` files come from fmtbench's generator, 1–3 scores per trace. | Leaving scores out: they are a third of the files a flush writes. |
| CW-10 | M5's memory cap is cetra's default: 2 GiB per query, 8 GiB in total (spec 03 § Resource limits). | — |
| CW-11 | The uncommitted top-k work on ClickBench is committed if its A/B shows no regression and every answer matches Parquet. Work on the bench.vortex.dev suites then stops. | — |
| CW-12 | File size and peak RSS must also beat Parquet: smaller files for each table and layout, and lower peak RSS for each query and write. | A bound (10% larger files, 1.5× the RSS): rejected by the user. |
| CW-13 | The 30-day project exists in two sizes with the same file count: 30M traces for verdicts, 1–3M for A/B iterations. | One size: rewriting and uploading 30M traces for every Strata change would take hours per A/B. |
| CW-14 | Strata pays for a payload feature only where it pays off. A frame whose zstd saves too little is stored raw, a column whose chunks find almost no duplicates skips content-defined chunking, and a frame's n-gram index is written only when it is small next to the frame and rules frames out. On incompressible text Strata then stores about the raw bytes, against Parquet's zstd of them (slightly larger), and searches it with no decompression, so CW-7 and CW-12 can both hold. The thresholds are set from M1's measurements. | Always writing the index and the chunk table: a few percent larger than Parquet on incompressible text, which CW-12 rejects. Dropping search speed on such data: CW-7 rejects it. |
| CW-15 | Writes and compaction come second (M2), before the many-file and query milestones. | Last: building the 30M-trace project with today's write speed would slow every later milestone. |
| CW-16 | A write wins only when it beats Parquet on both wall time (each format with its own parallelism: Strata compresses payload frames on every core, Parquet's writer uses one) and process CPU time. | Wall time alone: cetra flushes up to 8 files at once (spec 03 § Flush), so a writer that wins by using every core loses under load. |
| CW-17 | Search means spec 05's search: `ILIKE '%q%'` with `\`, `%` and `_` escaped, over `input`, `output`, `name` and `system_instructions`, with arrow's `ilike` kernel as the semantics on both formats. Today Strata's n-gram index is case-sensitive, so its reader uses the index only for needles without cased characters; M1 measures that state. | Measuring `LIKE`: cetra's search box is case-insensitive. |
| CW-18 | Every measured run is a fresh process per query, format and round, which runs one warm-up and then a few measured runs and reports its own peak RSS; the formats alternate in each round. | One process for everything: `VmHWM` only grows, so later readings would carry earlier peaks. |
| CW-19 | M1 writes `spans` files only, four per case, sorted by `(trace_id, start_time)`. The derived `traces` files, `scores` files and day partitions come with M3's layout. | The full layout in M1: none of M1's cases reads `traces` or `scores`. |
| CW-20 | M2's compaction measures the format-specific part: the k-way merge of about ten `spans` L0 files into one L1 file, per format (Parquet: a streaming merge into cetra's Parquet settings; Strata: `strata::merge`). Rebuilding the derived `traces` partials costs the same whatever the format (spec 01's merge operator, `cetra-storage`'s crate-private `partials_from_spans`); writing a `traces` file is measured with M3's small files. Decided overnight, for the user's review. | Rebuilding `traces` in M2: needs the merge operator, which `cetra-storage` doesn't export, and would add a format-independent cost to both sides. |
| CW-21 | M3's Parquet project is written by `cetra-storage` itself (open, `append_spans`, `append_scores`, `flush_now`, `compact`), so its files, `traces` partials, levels and day partitions are cetra's own. Each of its data files is then rewritten as a Strata file with the same rows and order. Decided overnight, for the user's review. | A layout writer in the bench: it would have to reimplement the anchor rules, the partials and compaction, and could drift from cetra. |
| CW-22 | M2's breakdown of write time (content-defined chunking, n-gram index, narrow encodings, zstd) comes from profiles of Strata writes on ovh-us, grouped by function, not from switches in Strata. | Switches in the writer: they change the code being measured. |

## Milestones

### M1 — Harness and the likely failures

**Delivers:**
- the harness (CW-18), the case generators, the two readers for the queries below, and the answer checks;
- verdicts on the three cases likeliest to rule Strata out:
  - **map-key explosion:** `attributes` with 100, 1,000 and 5,000 distinct keys (numbered tool arguments, UUID keys). Write speed, file and footer size, read RAM, and the speed of a filter and of a key enumeration on one key;
  - **payloads with no repetition:** unique RAG chunks, base64-like and high-entropy JSON. The size and write-time cost of content-defined chunking and the n-gram index against plain zstd;
  - **search needles:** absent, rare (0.01% of rows), common (10%), and short (under 5 bytes: `404`, `bug`, `SQL`), plus non-ASCII prompts (accents, CJK, emoji) under `ILIKE`, checked for both speed and zero false negatives;
  - **extreme span shapes:** 95% non-LLM spans (small telemetry rows with null `input` and `output`), and spans near the 8 MiB payload cap (D40), which are the likeliest to push peak memory up.

**Depends on:** nothing.

**Leaves out:** writes and compaction (M2), many-file layouts (M3) and the product query shapes (M4).

### M2 — Writes and compaction

**Delivers:**
- where a write's time goes: content-defined chunking, the n-gram index, narrow encodings and zstd, on `swe` and `wildchat`;
- L0 → L1 compaction of about 10 files with the derived `traces` files rebuilt: throughput, CPU and peak RAM. Whether payload chunks can be carried over without being decompressed and chunked again. Carrying over only the chunks still referenced would also make a later deletion rewrite cheap, though deletion is out of scope here (CW-6);
- peak writer RAM with 4–8 projects flushing 256 MiB batches at once.

**Depends on:** M1's harness.

**Leaves out:** deletion rewrites (non-goal).

### M3 — Many files and small files

**Delivers:**
- per-file overhead of small L0 `spans`, `traces` and `scores` files (500–5,000 rows): size, write time, open time;
- a 30-day project of 50–500 files on R2: total GETs, bytes, p95 and p99 latency for a cold trace lookup (bloom probing across files) and a 30-day model rollup;
- RAM held per open file;
- the startup rebuild of the anchor map from `traces` files (spec 03 § Trace anchor; 51 ms for 5M traces on Parquet per D86).

**Depends on:** M1's harness and M2's write speed, to build the 30-day project.

**Leaves out:** the query shapes on this layout (M4 runs them on it).

### M4 — cetra's query shapes

**Delivers** a verdict on each shape, locally and on R2, on the real datasets and on M3's project:
1. **Trace list** sorted by `start_time`, `cost_total` and `duration_ns`, two-phase: rank on `(trace_id, sort column, filter columns)` over 1–5M traces, top 50, then fetch those 50 traces.
2. **Trace detail** of a 10,000-span trace: the light tree (no `input`, `output`, `attributes`), then one span's heavy columns.
3. **Facets:** 10–20 fields in one pass: low, medium and high cardinality, list columns (`tags`, `models`), numeric histograms, and key enumeration of `attributes` and `metadata`.
4. **Dashboard series** over 7 and 30 days: time buckets, p95 duration, cost sum, error rate, broken down by `model`, `tags` and `metadata['k']`.
5. **Sessions and users:** high-cardinality `GROUP BY session_id` and `user_id` over 7–30 days.
6. **Search** on the `spans` columns spec 05 names, with M1's needle cases, inside a time window.
7. **Full payload scan** on `wildchat`, with range coalescing for scans, where Strata lost on S3.

**Depends on:** M1 and M3.

**Leaves out:** memory under load (M5).

### M5 — Memory and concurrency

**Delivers:**
- peak RSS with 8–16 queries at once (trace list, facets, a 30-day dashboard and a full-text scan) under a hard memory cap;
- how Strata's byte ranges fit cetra's read cache (1 MiB blocks, LRU): whether footers, filters and narrow chunks stay cached while one-off payload reads pass through.

**Depends on:** M4.

**Leaves out:** nothing further; its verdict closes the comparison.

## Open questions

None left. Settled with the user: CW-7 to CW-12. CW-13 to CW-15 follow the review of the draft.
