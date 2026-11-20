# Strata on more real LLM data

The bench had one real dataset: `swe`, from 2 shards of `nebius/SWE-agent-trajectories`, one agent framework doing coding tasks. This page adds three public datasets that look more like what cetra will store, and runs the same `fmtbench` queries on them (ubuntu-vm, local disk, 4 files per variant, min of 30 runs).

## Data

| Dataset | Source (licence) | What it is | Taken | Traces | Spans | Payload strings |
|---|---|---|---|---|---|---|
| `openswe` | [nvidia/Open-SWE-Traces](https://huggingface.co/datasets/nvidia/Open-SWE-Traces) (CC BY 4.0) | Coding agents, with reasoning and tool calls | 220 runs from `claudecode/qwen38_flash_next/scale-swe/thinking-medium` and 220 from `openhands/minimax_m25/swe-rebench-v2` | 440 | 37,463 | 2,370 MiB |
| `toucan` | [Agent-Ark/Toucan-1.5M](https://huggingface.co/datasets/Agent-Ark/Toucan-1.5M) (Apache-2.0) | Kimi-K2 calling tools from 495 real MCP servers (search, finance, maps, …) | Kimi-K2 shards 0–2 | 38,889 | 373,975 | 2,648 MiB |
| `wildchat` | [allenai/WildChat-1M](https://huggingface.co/datasets/allenai/WildChat-1M) (ODC-BY) | Real users chatting with ChatGPT, 68 languages, no tools | shards 0–2 | 179,571 | 659,225 | 2,829 MiB |

**How they become spans.** [bench/convert_convos.py](bench/convert_convos.py) writes each dataset as conversations of OTel GenAI messages, with text, `reasoning`, `tool_call` and `tool_call_response` parts. [testdata/src/convo.rs](testdata/src/convo.rs) reads them (`fmtbench gen <dir> 0 4 --convo <max> <file>`).
- Each conversation is one trace: a root agent span, then one LLM span per assistant message and one tool span per tool result.
- An LLM span's `input` is every message before it (the API request resends the history), its `output` is the message, and it carries the dataset's system prompt and tool definitions.
- A tool span's `input` is the call's arguments, and its `output` is the result.
- Timestamps, ids, models and costs come from the generator's templates, as for `swe`.

**Converter details.**
- Toucan's consecutive assistant messages (text, then a `function_call`) become one reply.
- Toucan's system message is Kimi's tool declaration, so it is dropped: `tools` carries the definitions.
- Tool-call arguments that are not valid JSON are kept as strings: `Infinity` (2 calls) and integers of 300+ digits.

## Size (MiB, 4 files)

| Dataset | **Strata** | Strata cold | pq_default | pq_tuned | pq_cetra | vx_default | vx_compact |
|---|---|---|---|---|---|---|---|
| openswe | **30** | 26 | 47 | 149 | 48 | 702 | 158 |
| toucan | **190** | 167 | 246 | 295 | 247 | 770 | 233 |
| wildchat | **564** | 493 | 592 | 655 | 596 | 1,429 | 584 |
| swe (existing) | **25** | 21 | 36 | 65 | 36 | 560 | 31 |

Strata is the default configuration after the size work of 2026-11-19 (§ Smaller files); Strata cold is the same rows rewritten with `Config::cold()`, as compaction would. Before that work Strata took 29, 195, 590 and 25 MiB. The text below describes the sizes before it.

- **Agent data:**
  - Strata is 38% smaller than the best Parquet on openswe and 20% smaller on toucan. Against Vortex compact it is 82% and 16% smaller.
  - Dedup is the reason: each step resends the history, so 2.4 GB of openswe payloads take 25 MiB.
- **Chat data:**
  - All formats come out within 2% of each other on wildchat.
  - Conversations are 1–3 turns, so little is repeated.
  - Strata's payload columns are still 14% smaller than Parquet's (372 against 431 MiB), but its narrow columns lose that back. That is the size gap already seen on `light`, at 659k spans.

**Write time** (one run each, during `gen`):
- Strata writes at 0.70× of Parquet's speed on openswe, and 0.78× on toucan and wildchat.
- Vortex compact writes at 0.07× (openswe), 0.43× (toucan) and 0.50× (wildchat).

## Queries (ms, min of 30 runs)

| Query | openswe st | pq best | vx_compact | toucan st | pq best | vx_compact | wildchat st | pq best | vx_compact |
|---|---|---|---|---|---|---|---|---|---|
| count_errors_6h | **0.80** | 2.31 | 1.84 | **0.86** | 2.27 | 2.09 | **0.88** | 2.28 | 2.12 |
| model_rollup_day | **3.05** | 5.19 | 4.84 | 11.18 | 10.95 | **8.09** | 15.22 | 15.78 | **12.45** |
| list_llm_1h | **4.27** | 6.63 | 6.36 | 16.75 | 24.27 | **10.55** | 18.75 | 23.95 | **14.60** |
| user_filter | **0.75** | 2.70 | 2.94 | **0.82** | 2.83 | 3.99 | **2.45** | 5.05 | 6.40 |
| metadata_filter | **0.70** | 3.72 | n/a | **2.16** | 10.23 | n/a | **3.10** | 16.71 | n/a |
| trace_lookup_full | **5.66** | 11.19 | 35.61 | **5.39** | 12.19 | 22.70 | **4.66** | 12.24 | 28.01 |
| trace_lookup_light | **0.89** | 3.73 | 3.74 | **0.96** | 4.81 | 5.30 | **0.99** | 5.53 | 6.16 |
| substring_input | **12.13** | 266.86 | 172.29 | **48.17** | 168.63 | 98.73 | **66.42** | 312.04 | 204.27 |
| scan_all_payload | **234.07** | 286.10 | 313.07 | 274.41 | 281.04 | **205.34** | 544.11 | 538.04 | **404.95** |

- The Strata columns are measured after the UTF-8 changes below; `substring_input` and `scan_all_payload` after the one-pass check, `list_llm_1h` after the scan threads change (§ Narrow queries on one thread). § Top rows in the scan later halved `list_llm_1h` (median of 20 runs: openswe 3.2 ms, toucan 7.2 ms, wildchat 9.6 ms). Parquet and Vortex come from the first run.
- "pq best" is the fastest of pq_default, pq_tuned and pq_cetra for each query.
- Vortex cannot run `metadata_filter` (it can't push down `map['key']`).

Substring search over 20 needles taken from the data (min per needle; KiB read cold):

| Dataset | st | pq_default | vx_compact |
|---|---|---|---|
| openswe | **16.1 ms**, 3,157 KiB | 521.8 ms, 32,904 KiB | 200.6 ms, 157,565 KiB |
| toucan | **47.5 ms**, 16,764 KiB | 496.8 ms, 82,550 KiB | 114.1 ms, 94,290 KiB |
| wildchat | **79.1 ms**, 35,955 KiB | 1,141.7 ms, 217,381 KiB | 221.6 ms, 228,930 KiB |

Cold trace lookup (mean per trace over 20 ids):

| Dataset | st | Best Parquet | vx_compact |
|---|---|---|---|
| openswe | **7.2 ms, 620 KiB** | pq_tuned: 13.6 ms, 2,694 KiB | 41.9 ms, 90,982 KiB |
| toucan | **8.9 ms, 1,019 KiB** | pq_tuned: 16.6 ms, 5,946 KiB | 29.6 ms, 51,157 KiB |
| wildchat | **9.8 ms, 1,147 KiB** | pq_tuned: 18.0 ms, 7,909 KiB | 36.9 ms, 83,170 KiB |

## What the new data shows

- **The agent results hold on other frameworks and tasks.** On openswe and toucan, Strata has:
  - the smallest files;
  - the fastest lookups: about 2× faster than Parquet and 3–6× faster than Vortex compact;
  - the fastest search: 10–32× faster than Parquet and 2.4–12× faster than Vortex compact.
- **Chat traffic was Strata's weak case.** The first run had `scan_all_payload` at 1.4× Parquet's time on wildchat, and `list_llm_1h` at 1.45×. The cause was UTF-8 checking, fixed below. What remains:
  - files are only as small as the others';
  - the full scan takes about Parquet's time, and 1.34× Vortex compact's;
  - Vortex compact still wins `model_rollup_day` on toucan and wildchat. This matches `light`, where its narrow-column decode is also cheaper. It won `list_llm_1h` too until Strata learned to keep only the top rows (§ Top rows in the scan).

## UTF-8 checking

The first wildchat profile of `scan_all_payload` spent 38% of the time checking the UTF-8 of each value. For `list_llm_1h` the figure was 34%.

**Why it is slow.**
- Every safe arrow constructor of a string array checks with `std::str::from_utf8`. That is fast on ASCII but several times slower than SIMD on other text, and WildChat is multilingual.
- Parquet avoids the cost: its reader checks each page in one pass with `simdutf8`.

**The change.** `unsafe` stays out, so a value can skip arrow's check only by being appended as a `&str`, and that copies it.
- **Payload columns** ([strata/src/payload/read.rs](strata/src/payload/read.rs) `build_string_views`):
  - an ASCII value that lies in one frame is still a zero-copy view into the frame;
  - every other value is checked with `simdutf8` and copied into blocks of at most 256 KiB.
- **Narrow `Varbin` chunks** ([strata/src/narrow/decode.rs](strata/src/narrow/decode.rs) `decode_varbin`):
  - an all-ASCII chunk still goes through `StringArray::try_new`;
  - any other chunk is checked once with `simdutf8`, and each value is sliced with `str::get`, which checks character boundaries.

**A memory trap.** Arrow's `StringViewBuilder::append_block` closes the block being filled. Adding shared frame bodies as they came therefore left the copy blocks two-thirds empty: 16.5 GB of capacity held 5.4 GB of data. Peak memory doubled, to 3.9 GB on payload and 7.5 GB on wildchat. The reader now adds every shared body before it appends any value.

**Results** (Strata `scan_all_payload`, min of 5 runs, before → after, with peak RSS):

| Dataset | Before | After | Peak RSS before | Peak RSS after |
|---|---|---|---|---|
| wildchat | 773 ms | 621 ms | 2.85 GB | 3.27 GB |
| openswe | 281 ms | 243 ms | 3.34 GB | 3.39 GB |
| swe | 288 ms | 226 ms | 3.00 GB | 3.02 GB |
| toucan | 312 ms | 294 ms | 1.58 GB | 1.65 GB |
| payload | 354 ms | 356 ms | 2.03 GB | 1.98 GB |

Other results:
- `list_llm_1h` on wildchat went from 32.2 ms to 22.0 ms (60 runs each, old and new binaries alternating). On `light` (18.0 → 17.7 ms) and `payload` (18.7 → 19.1 ms) it did not change.
- Every other Strata query on the six datasets stayed within run-to-run noise.
- Wildchat's peak memory rose 15%, because its non-ASCII values are now copied instead of shared.

### One pass

The change above still read most bytes twice: an ASCII value was checked by `is_ascii`, then by arrow when its view was added, and a non-ASCII value was checked and then copied. Checking each byte once, and sharing every value, needs a constructor that skips arrow's check, and those are `unsafe`. Strata now allows `unsafe` on exactly two calls (`#![deny(unsafe_code)]` instead of `forbid`), each with a SAFETY note:
- **Payload columns** ([build_string_views](strata/src/payload/read.rs)):
  - the array is built as `BinaryView`, whose builder checks each view's block and bounds but not UTF-8;
  - the chunk data of each frame is checked once per read with `simdutf8`, as a whole (`FrameBody::span_is_utf8`);
  - a value in valid chunk data is valid if it starts and ends on a character boundary, a two-byte test;
  - a value in a frame that fails, and a value rebuilt from chunks in several places, is checked on its own;
  - then `to_string_view_unchecked` relabels the array as `Utf8View`. Every value that lies in one frame is now shared, ASCII or not.
- **Narrow `Varbin` chunks** ([decode_varbin](strata/src/narrow/decode.rs)): one `simdutf8` pass over the chunk, a boundary test at each offset, `BinaryArray::try_new` for the offsets and nulls, then `StringArray::new_unchecked`.

**The chunker had to change.** A first count on wildchat found a third of the frame bytes failing the whole-frame check: the gear chunker cut inside characters, so a chunk could start with a continuation byte after a chunk of another value. A gear cut now moves back to the first byte of its character (at most 3 bytes). It stays content-defined, and the chunks of UTF-8 text are UTF-8 (a property test checks it). The files keep their size (wildchat 595 → 592 MiB, the others unchanged). A first version also checked a frame once per batch instead of once per read: toucan's scan checked 5.8 GB of frame data for 0.86 GB of values, and got 13% slower.

**Results** (Strata, min of 30 runs, a2148c6 on the old files → this change on files rewritten with the new chunker):

| Dataset | scan_all_payload | Peak RSS (5-run A/B) |
|---|---|---|
| wildchat | 624 → 544 ms (−13%; Parquet 538) | 3.1–3.3 → 2.8 GB |
| toucan | 299 → 274 ms (−8%) | unchanged |
| payload | 350 → 333 ms (−5%) | unchanged |
| openswe | 240 → 234 ms (−3%) | unchanged |
| swe, light | unchanged | unchanged |

- A wildchat profile no longer shows `simdutf8` among its top functions (under 2% of samples): zstd decoding and the allocator lead.
- The other queries stayed within noise on the six datasets, `list_llm_1h` included (60-run A/B).
- One exception is layout luck: the absent needle of toucan's `substring_input` passes more frame filters in the new layout (849 requests against 668), +18%. Over the 20 needles taken from the data, the new files cost +2.4% time and +3.7% bytes.

## Narrow queries on one thread

`list_llm_1h` on toucan took 27 ms against Vortex compact's 10.4 ms, with 29 ms of CPU against Vortex's 39 ms: Strata ran it on one thread.
- A file of toucan has 93k rows, under the 2 × 65,536 a file needs before the DataFusion scan splits it, so each file is one partition. The hour window leaves few files with rows to read.
- In that partition, the chunk segments were decompressed one after another (43% of the profile), then decoded one after another (21%). Only a wave of 4 MiB or more was decoded on several threads, and each wave here was smaller.
- A lower threshold for every read helped toucan but slowed `light` by up to 2.5×: `light` runs about 12 partitions at once, and each one also took every core.

**The change.** `scan_batches` takes the scan's threads: DataFusion gives each partition its share of the cores (one when there are as many partitions as cores), and `scan` gives all of them. A scan wave of at least 1 MiB (`SCAN_PARALLEL_BYTES`) is decompressed and decoded on those threads; the batches were already built on them. Lookups and the other reads keep the 4 MiB rule.

**Results** (min of 3 × 30 runs, 3533a02 → this change, on the rewritten files):

| Dataset | list_llm_1h | Vortex compact |
|---|---|---|
| toucan | 26.5 → 16.8 ms (−37%) | 10.4 |
| wildchat | 21.3 → 18.8 ms (−12%) | 14.6 |
| swe, light | unchanged | |
| payload | 18.6 → 19.9 ms (+7%) | |

- A 512 KiB threshold gave the same gains but made swe's `list_llm_1h` 27% slower: starting threads for about a millisecond of work cost more than it saved.
- The other queries stayed within noise (one 20-run round of the whole suite at 512 KiB, then three rounds of the filter queries at 1 MiB). `payload`'s `list_llm_1h` uses the threads without gaining from them; not investigated.
- What is left against Vortex: Strata reads 5.7 MB for toucan's `list_llm_1h` against Vortex's 3.6 MB, and zstd is still the largest cost. Both are answered below.

## Plain integer chunks

On `light`, Strata's narrow columns were 17% larger than Parquet's. Comparing them column by column showed where:
- `cost_*` floats: 16.7 MB against Parquet's 6.1 MB (Parquet uses a dictionary);
- previews: +4.3 MB (also a dictionary in Parquet);
- `ingest_time`: 6.2 MB against 3.9 MB; token counts: +3.1 MB.

An experiment on one file (re-encoding each column's chunks in Python) found that a whole-file dictionary is the best fit for the costs. For `ingest_time` and the token counts, plain values under zstd beat bit-packing: bit-packing hides repeated values from zstd, and `ingest_time` repeats in runs (one per ingested batch).

**The change.** The writer now encodes an integer chunk twice, bit-packed (`ForBitpack`) and `Plain`, compresses both by the narrow rule, and stores the plain one only if it saves at least 30% over the bit-packed one. That is the same bar zstd already has to clear on a narrow chunk, since every scan decompresses it.
- A first version kept whichever was smaller. It saved a little more (light 172 MiB) but made narrow queries up to 19% slower: most switched chunks were barely smaller and now paid for zstd decompression instead of a cheap bit-unpack. Examples: `count_errors_6h` on `light` +19%, `model_rollup_day` on `payload` +15%, `list_llm_1h` on openswe +13.5%.
- With the 30% bar, only chunks that really gain switch. On `light`, every `ingest_time` chunk is plain (6,193 → 2,097 KiB), and every other integer column stays bit-packed.

**Results.** `light` 177 → 173 MiB (narrow columns 181.7 → 177.6 MB), toucan 197 → 195, wildchat 592 → 590, swe 26 → 25; payload and openswe unchanged. The queries stayed within noise on the six datasets (two 20-run rounds, old and new files alternating). Still open on `light`: a dictionary for the float costs (about −10 MB, a format change) and for the previews.

## Top rows in the scan

Why did `list_llm_1h` read more than Vortex? Running the query with one column at a time on toucan (`FMT_LIST_COLS`) gave each column's cost. With the filter and sort columns that every variant reads (`span_kind`, `start_time`, `trace_id`, `span_id`), Strata read 820 KiB against Vortex's 542. `input_preview` added 3,256 KiB against 2,194. The other columns added little.
- The query keeps 50 rows of an hour window. Strata evaluated the filter, then decoded every chunk of every projected column that held a selected row. The files are sorted by trace id, so the window's rows were in every chunk.
- Most of the extra bytes are not decoded data. A wave makes a few large requests (4 on toucan), and ranges up to 1 MiB apart are merged, so the narrow region is fetched almost whole either way.

**The change.** DataFusion 55 can push a sort into a scan (`ExecutionPlan::try_pushdown_sort`), which sets the sort's limit with `with_fetch`. `StrataExec` accepts a sort whose first key is a narrow column of every file and whose other keys are projected columns. Each partition then:
- runs the filter wave as before;
- decodes the first sort key for the selected rows (already decoded when it is a filter column);
- keeps the rows that rank in the first `fetch` by it, plus the rows tied with the last one (`ScanRequest::top`, `TopRows`);
- decodes the other columns only in the chunks that hold those rows, then sorts the rows by the full order and keeps `fetch` of them.

DataFusion drops its `SortExec` and merges the partitions with its `SortPreservingMergeExec`. The rows are exactly those of DataFusion's own sort. A test compares both on ties, nulls, both directions, two partitions and no limit. A sort by a payload column or by an expression stays in DataFusion.

**Results** (`list_llm_1h`, median of 20 runs, two rounds alternating with the previous binary):

| Dataset | Before | After | CPU before → after | Vortex compact |
|---|---|---|---|---|
| light | 21.8 ms | **11.1** | 134 → 58 ms | 19.2 |
| payload | 20.9 ms | **7.4** | 28 → 7 ms | 9.8 |
| swe | 5.0 ms | **3.1** | 4.8 → 2.8 ms | 7.2 |
| openswe | 4.8 ms | **3.2** | 7.5 → 5.0 ms | 6.6 |
| toucan | 16.9 ms | **7.2** | 65 → 11 ms | 11.8 |
| wildchat | 20.0 ms | **9.6** | 111 → 23 ms | 14.7 |

- Strata now wins `list_llm_1h` on all six datasets.
- The bytes fetched barely moved (toucan 5,729 → 5,566 KiB) because of the merged ranges. On S3 that is the intended trade; locally, the gain is the decoding saved.
- Each partition keeps its own top rows, so a table of many partitions decodes `fetch` rows per partition, not `fetch` in all. A bound shared between partitions would cut that further; not done.

## Smaller files

Customers pay S3 for every byte, so three changes target size (2026-11-19). A breakdown of the files first (`restrata`'s tables): on the four agent and chat datasets, payload frames and their gram index are 85–90% of each file (wildchat: 317 MB of shared frames, 54 MB of their gram index, 136 MB of preview frames, 67 MB of narrow columns). On `light`, which has no payloads, the gap to Vortex was in the narrow columns: the float costs took 16.7 MB against Parquet's 6.1, and the previews 54 MB.

**What changed:**
1. **Previews.** Their payload stores (`own_store_columns`) used zstd level 1 and no dictionary; they now use the shared store's level and a trained dictionary. Narrow text columns (`VARBIN`, `MAP`, `IPC` chunks, at least 1 MiB) train a 64 KiB zstd *chunk dictionary*, kept when it saves at least 5% (prototype-plan § S3, kind 10). On wildchat text, measured first in Python: 64 KiB frames at level 1 gave ratio 2.70, level 3 with a dictionary 3.24.
2. **Cold files.** `Config::cold()` sets every zstd level to 19, and `strata::rewrite` writes a file again under a config, payload stores included (a merge copies them as they are). A test checks that rewriting a file cold gives the bytes of writing its rows cold.
3. **Float dictionaries.** A Float64 column whose values repeat at least 8 times on average gets a file dictionary, and its chunks store bit-packed codes (`FLOAT_DICT`). Costs computed from token counts repeat: on one `light` file, `cost_total` has 15,719 distinct values in 397,726 rows. Two other ideas were measured and dropped: decimal encoding (ALP) failed on every chunk, since costs are not short decimals, and byte-stream split made zstd do worse (1.99 against 1.58 MB).

**Sizes** (MiB, 4 files; `bench4` before, `bench5` after with the default config, `bench6` cold):

| Dataset | Before | After | Cold | vx_compact | pq_default |
|---|---|---|---|---|---|
| light | 174 | 159 (−9%) | 146 (−16%) | **137** | 152 |
| payload | 260 | 253 (−3%) | **225** (−13%) | 317 | 333 |
| swe | 26 | 25 (−4%) | **21** (−19%) | 33 | 37 |
| openswe | 30 | 30 (−0%) | **26** (−13%) | 159 | 48 |
| toucan | 196 | 190 (−3%) | **167** (−15%) | 234 | 247 |
| wildchat | 591 | 564 (−5%) | **493** (−17%) | 585 | 593 |

- On `light` the three cost columns went from 16.7 to 8.5 MB and the previews from 54.3 to 47.2 MB. It is still 16% larger than Vortex compact (7% cold).
- Strata is now the smallest on wildchat too, 4% under Vortex compact; cold files are 13–19% smaller than before on every dataset.

**Write time** (`restrata`, one run): the default config writes 10–30% slower, from the dictionary training and the second compression of text chunks: light 8.9 → 11.6 s, wildchat 15.0 → 17.1 s, toucan 7.1 → 8.1 s. Cold writes run at 9–21 MiB/s of Arrow on the large datasets (wildchat 335 s): only background compaction can afford them.

**Queries** (min of the medians of two rounds of 20 runs, all three read by the new binary):
- The default config is within the run-to-run noise (±10%) on every query, except the cold trace lookup, 6–11% slower: a lookup now also reads the chunk dictionaries of text columns in its metadata wave.
- Cold files read 2–6% slower on full payload scans, 6–9% slower on substring search on toucan and wildchat, and 14–21% slower on cold trace lookups (wildchat 9.8 → 11.6 ms). Narrow queries do not change beyond the noise.

## Braintrust's search benchmark

Braintrust published a search benchmark of Brainstore against two unnamed vendors (blog post "Nitro", 2026-09-24): 4 million spans of 10 KB of FineWeb text, 40 GB, a rare phrase in 4,000 spans and a longer one in one span, on a 16-vCPU machine with the data on S3 and a local NVMe cache. `fmtbench fineweb` rebuilds it (2026-11-20) and `FMT_QUERYSET=braintrust fmtbench query` runs its five published queries.

**Differences from theirs:**
- Six files of `HuggingFaceFW/fineweb` `sample/10BT` (12.9 GB) hold 1,936,962 spans of 10,000 bytes (19.4 GB), half of theirs. The rare phrase is in 1,937 spans (one in 1,000, as theirs), the long one in one.
- Each span is one trace whose `input` is the text as an OTel GenAI message; start times are spread over 3 days; 20 files of 100,000 spans each, sorted by (trace_id, start_time).
- Local disk, OS page cache warm, 12 cores, median of 5 runs. Their numbers are cold (process restarted, NVMe cache cleared, S3 behind it), so ours compare with each other, not with theirs.
- `LIKE '%…%'` matches bytes; their phrase search matches words, so "help me" counts differ (49,835 here; 490,217 in their 40 GB).

**Sizes:** Parquet default 7,892 MiB, Vortex compact 7,775 MiB, Strata 9,444 MiB (+21%). Unique web text gives dedup nothing, and the gram index and previews add their bytes. Strata wrote at 96 MiB/s of Arrow, Parquet at 155, Vortex compact at 170.

**Queries** (ms, median of 5; Braintrust's published cold numbers for comparison of shapes only):

| Query | Strata | Parquet | Vortex compact | Brainstore (theirs) | Vendor A | Vendor B |
|---|---|---|---|---|---|---|
| Count matches of a phrase found nowhere | **90** | 6,401 | 5,777 | 698 | 36,223 | 2,253 |
| Count matches of "help me" | 3,390 | 6,531 | 5,831 | 1,301 | 23,795 | wrong count |
| Newest 100 matches of the rare phrase | **272** | 7,724 | 9,732 | 3,528 | 38,637 | 4,403 |
| Newest matches of the long phrase (1 match) | **84** | 6,403 | 5,848 | 2,248 | 39,296 | 3,180 |
| Spans per hour (aggregate) | 80 | 88 | **78** | 718 | 432 | 1,343 |
| Cold trace lookup (ours only) | **11.7** | 29.5 | 124.9 | | | |

- Every format returns the same counts and rows (0; 49,835; 1,937; 1).
- Rare and absent phrases: the gram index leaves Strata 16–36 MiB of frames to read, against 7.4 GB for Parquet and Vortex: 70–100× faster.
- A common phrase is Strata's weak case: nearly every frame is a candidate, so it decompresses 2.4 GB of frames (3.4 s), only 2× faster than a Parquet scan. Brainstore counts it from posting lists without reading text. § Common phrases makes its newest matches 38× faster.
- **Bug found and fixed:** the "help me" scan issued 64,561 range reads over 20 files with up to 2,560 in flight (64 per wave, two waves per file at once). It failed with "Too many open files" under the default limit of 1,024 descriptors, so the run above raised the limit to 65,536. Strata now holds at most 256 reads in flight in the whole process (`MAX_GETS_IN_FLIGHT`). Rerun under the default limit, every query passes with the same counts, and the times stay within 2% ("help me" 3,434 ms, phrase none 87 ms, cold lookup 12.0 ms).

## Common phrases

"help me" was the one Braintrust query where Strata trailed: 3.4 s to count it, and 3.6 s for its newest 100 matches (`bt_newest_help_me`, added for this: `SELECT input … LIKE '%help me%' ORDER BY start_time DESC LIMIT 100`), against Parquet's 6.5 s and 3.7 s.

**Profile of the count** (36 s of CPU over 12 cores):
- zstd decompression of the candidate frames: about 50%;
- about 17% in the threads of tokio's blocking pool, waking and locking: the local store reads each of the 64,561 ranges on a blocking thread;
- about 8% in fresh allocator pages: every candidate frame of a file was decoded and held at once.

**The changes:**
- **Search while decoding.** Each candidate frame is searched on the decoding threads as soon as it is decoded (`StrataFile::fetch_map`), and its body is kept only when one of its chunks ends with a prefix of the needle.
- **Dense frames read like a scan.** When the candidate frames hold at least a quarter of the bytes between the first and the last, they are fetched with scan coalescing (gaps up to 1 MiB, ranges up to 16 MiB) instead of one request per frame (`frames_kind`).
- **Staged top.** A sort with a limit over a `LIKE` search, by a timestamp or integer column, ranks the files by their zones (newest first for a descending sort) and searches them in stages of 1, 4, 16, … files. Once 100 rows are kept, the 100th one's `start_time` is added to the filter of later files, and a file whose zones rule it out reads no payload. The rows are the full sort's: ties pass the bound, and a null key is kept when nulls sort first (`prototype-plan.md` § S8).

**Results** (Strata, ms, median of 5; before: two rounds alternating with the previous binary; after: two rounds of the final binary; default open-file limit):

| Query | Before | After | Parquet | Vortex compact |
|---|---|---|---|---|
| Count matches of "help me" | 3,486 | 3,222 | 6,498 | 5,888 |
| Newest 100 matches of "help me" | 3,607 | **94** | 3,668 | 7,123 |
| Newest 100 matches of the rare phrase | 282 | **108** | 7,883 | 9,695 |
| Newest matches of the long phrase (1 match) | 83 | 98 | 6,353 | 5,901 |
| Count matches of a phrase found nowhere | 90 | 83 | 6,573 | 5,896 |

- **Newest matches of a common phrase: 38× faster.** The newest file holds about 2,500 matches, so one file of 20 is searched. Brainstore's posting lists would do the same with no text read; this does it with the text format as it is.
- **Rare phrase: 2.6× faster.** About 97 matches per file, so the first stage keeps 97 rows and the second stage (4 files) completes the 100.
- **Single match: 15 ms slower.** Nothing bounds the search, so every file is read in three stages (1, 4, 15 files) instead of all at once. A first version ran a stage's files one after another in one task: 331 ms. Each file is now its own task.
- **Count of "help me": 8% faster.** The dense read cut the requests from 64,561 to 9,744 but fetches 7.2 GB instead of 2.5 GB, and CPU rose from 35 to 37 s (§ Simulated S3 later cut it to 4.8 GB with a 128 KiB gap). Locally the extra bytes are copies from the page cache; on S3 the change trades bytes for 6.6× fewer requests. Counting a common phrase still decompresses every frame that holds it: only a word index (posting lists, as Brainstore's) avoids that.
- The other datasets' `substring_input` stayed within noise (swe 22 → 21 ms with 242 → 167 requests; openswe, toucan, wildchat unchanged).

## Time ranges

A UI search usually comes with a time range. Four queries ending at the newest span: `bt_help_me_1d` and `bt_help_me_1h` count "help me" over the last day and hour; `bt_newest_help_me_5m` and `bt_newest_rare_1h` return the newest 100 matches of "help me" over the last 5 minutes and of the rare phrase over the last hour.

Before, the file pruning already dropped the files outside the range, but inside a file the search ran before the time filter, over every row: a 5-minute range cost a whole file's search. Rows inside a file are sorted by trace id, so their times are spread over all its chunks, and the zones cannot narrow them.

**The change.** When every `Contains` term is a conjunct of the filter, the other conjuncts run first, and the search reads the refs pages of the rows they keep, then only the candidate frames those rows use (`prototype-plan.md` § S4, Searching after the other filters). The refs pages come with the index rows, so the waves do not grow. A filter with no other conjunct keeps the old path. The early stop of § Common phrases benefits too: its bound on `start_time` is such a conjunct.

**Results** (ms; local: mean of four alternating rounds of 5 runs; simulated S3: median of 3):

| Query | Strata before | Strata after | Parquet | Vortex compact | S3 sim. before | S3 sim. after | Parquet S3 sim. | Vortex S3 sim. |
|---|---|---|---|---|---|---|---|---|
| Count "help me", last day | 1,212 | **1,044** | 2,536 | 2,263 | 4,100 | **2,991** | 7,256 | 3,008 |
| Count "help me", last hour | 77 | **73** | 530 | 189 | 781 | **419** | 662 | 613 |
| Newest 100 "help me", last 5 min | 90 | **54** | 785 | 322 | 951 | **344** | 1,008 | 782 |
| Newest 100 rare phrase, last hour | 23 | 23–27 | 947 | 366 | 273 | **250** | 985 | 726 |

- The 5-minute search reads half the frames (47 → 24 MB). The hour reads as many as before: an hour is 28% of its file's rows, and with about six spans per frame that still touches most frames. The gain grows as the range shrinks against the file.
- The S3 gains also come from § Simulated S3's changes (4 MiB ranges, 256 requests per wave).
- A check of the queries that do not use the new path (absent phrase, newest matches without a range, `substring_input` and `list_llm_1h` of openswe and wildchat; three alternating rounds against the committed code) found them within the run-to-run spread: 88 against 93 ms, 98 against 106, 9.7 against 9.7, and the substring searches within 1 ms (7%).

## Writer threads and file size

**Parallel writer.** A store's frames are compressed, and their index bitmaps built, on `Config::write_threads` threads (default: the cores), in batches of 1,024 frames, each thread with one compressor. The frames are written in order, so the file is byte for byte the same for any thread count (a test writes with 1 and 7 threads; the golden hash did not move). Rewriting FineWeb's 20 files (`fmtbench restrata`, two rounds): **94–95 → 282–308 MiB/s**, against Parquet's 143 MiB/s on one thread. Chunking, deduplication and the narrow columns still run on one thread.

**Where FineWeb's extra 21% goes** (MiB, 20 files; Strata 9,444, Parquet 7,892):

| Part | Strata | Parquet | Difference |
|---|---|---|---|
| `input` text: frames / column chunks | 7,667 | 7,380 | +287 |
| `input` gram index | 1,166 | — | +1,166 |
| `input` refs pages and store segment | 17 | — | +17 |
| `input_preview` (own store, with its own index) | 501 | 407 | +94 |
| narrow and other columns | 93 | 105 | −12 |

- **The gram index is three quarters of the gap**: 15% of the frames' size. On unique text it costs about 0.5 bit per byte of raw text; it is what makes rare and absent phrases 70–100× faster (§ Braintrust's search benchmark). It could be made smaller only by setting fewer rows per frame, at more false-positive frames for short needles (about 55% of a frame's rows are set, so a frame without a 9-byte needle, five grams, passes with probability 0.55⁵ ≈ 5%; with half the rows, 80% are set and it passes with 0.8⁵ ≈ 33%).
- **Frames: +4%** against Parquet's text column. Frames are 64 KiB so that a point read decompresses little; Parquet's pages are about 1 MB.
- **Previews: +23%**, of which about 64 MiB is the preview store's own index and refs. Previews are rarely searched; dropping their index would save 0.7% of the file, and make a `LIKE` on a preview column read every frame.

## Simulated S3

Every number above is local disk with the OS page cache warm, where a request costs microseconds. On S3 a request costs a first-byte latency, and one request runs at one connection's throughput, so the same reader can rank differently. `FMT_SIM_S3=1` estimates S3 times without S3 (2026-11-20, `bench/src/sim.rs`):
- Each GET reads the local file, then waits until the time it would end on S3: 30 ms of first-byte latency, plus its bytes at 90 MB/s, and no earlier than a shared 10 Gbit/s link allows for all the bytes in flight (`FMT_SIM_LATENCY_MS`, `FMT_SIM_CONN_MBPS`, `FMT_SIM_LINK_GBPS`).
- Dependent waves, concurrency and the CPU work between requests come from each reader, so every format pays for its own access pattern.
- Not modelled: throttling (S3 allows about 5,500 GETs per second per prefix), tail latency, retries, and connection set-up. The constants are AWS's typical figures; one run against real S3 would calibrate them.
- Request cost on S3 Standard is $0.0004 per 1,000 GETs: the full "help me" count's 21,742 requests cost $0.009, Parquet's 20 requests nothing measurable.

**Before the changes below** (ms, median of 3; Strata / best Parquet / Vortex compact; bold where Strata is fastest):

| Dataset | count_errors_6h | model_rollup_day | list_llm_1h | trace_lookup_full | trace_lookup_cold | substring_input | scan_all_payload |
|---|---|---|---|---|---|---|---|
| light | **52** / 120 / 160 | **95** / 129 / 303 | **120** / 271 / 335 | **47** / 125 / 410 | 168 / 165 / 432 | 7 / 3 / 67 | 7 / 6 / 82 |
| payload | **57** / 74 / 146 | 125 / 107 / 118 | 234 / 196 / 195 | **86** / 118 / 414 | 173 / 164 / 451 | **207** / 410 / 215 | 725 / 622 / 521 |
| swe | 5 / 2 / 4 | 90 / 82 / 81 | 147 / 155 / 122 | **83** / 105 / 214 | 157 / 155 / 297 | **127** / 431 / 251 | 487 / 416 / 320 |
| openswe | **1** / 2 / 7 | **79** / 85 / 82 | 143 / 155 / 129 | **91** / 105 / 444 | 157 / 153 / 528 | **105** / 459 / 380 | **505** / 521 / 555 |
| toucan | 6 / 2 / 9 | 123 / 115 / 112 | **132** / 205 / 187 | **84** / 114 / 332 | **164** / 168 / 434 | 293 / 536 / 169 | 723 / 731 / 333 |
| wildchat | 6 / 3 / 4 | 146 / 110 / 141 | **153** / 197 / 188 | **84** / 117 / 466 | 199 / 170 / 550 | **273** / 656 / 277 | 949 / 889 / 523 |

FineWeb (Braintrust's queries): Strata 307 ms for the absent phrase against Parquet's 15,038 and Vortex's 6,504; 8.0 s for the "help me" count against 15.1 and 6.4 s; Parquet 175 ms and Strata 206 ms for a cold trace lookup.

**What the simulation showed:**
- **Large merged ranges.** A scan merged ranges up to 16 MiB, and one request of 16 MiB takes 190 ms at one connection's 90 MB/s: payload's `list_llm_1h` made 4 requests for 7.3 MB, `scan_all_payload` 16 requests of 10 MB. Merged ranges now stop at 4 MiB, which takes about as long to transfer as a request's latency.
- **64 requests per wave.** A wave allowed 64 requests in flight, so the 1,063 requests of a one-hour search took 17 latencies. The process-wide limit of 256 (§ Braintrust's search benchmark) now applies alone.
- **The dense gap.** With 256 requests of 30 ms in flight, a request costs about as much as 150 KB on a 10 Gbit/s link. Candidate frames that fill a quarter of their span (§ Common phrases) were merged with the scan's 1 MiB gap, which read 7.2 GB for the "help me" count; they now merge gaps under 128 KiB (`ScanKind::Dense`): 4.8 GB in 21,742 requests.
- **Still behind:** a cold trace lookup takes 5 dependent waves (open, metadata, sort-key chunks, row chunks and refs, frames) and ties with Parquet; Vortex compact's full scans stay faster (333–523 ms against Strata's 530–842 on toucan, payload and wildchat).

**After** (Strata, ms, median of 3, committed code → this change):

| Query | Before | After |
|---|---|---|
| FineWeb: count "help me" | 8,101 | 6,478 |
| FineWeb: absent phrase | 312 | 271 |
| FineWeb: newest 100 "help me" | 962 | 477 |
| FineWeb: single match | 571 | 420 |
| payload: `scan_all_payload` | 740 | 661 |
| toucan: `scan_all_payload` | 682 | 530 |
| wildchat: `scan_all_payload` | 946 | 842 |
| payload: `substring_input` | 209 | 177 |
| toucan: `substring_input` | 287 | 220 |
| wildchat: `substring_input` | 285 | 236 |

The other queries of the six datasets moved within the run-to-run noise (a few ms; the 5 ms queries by up to a few ms either way).

## Reproduce

```bash
# on ubuntu-vm, pyarrow 24 in ~/cfb/venv
python -I bench/convert_convos.py openswe raw/openswe.parquet 220 claudecode.parquet openhands.parquet
python -I bench/convert_convos.py toucan raw/toucan.parquet 1000000 k2-0.parquet k2-1.parquet k2-2.parquet
python -I bench/convert_convos.py wildchat raw/wildchat.parquet 1000000 w-0.parquet w-1.parquet w-2.parquet
fmtbench gen ~/bench/openswe 0 4 --convo 100000000 raw/openswe.parquet
fmtbench query ~/bench/openswe 30
```
