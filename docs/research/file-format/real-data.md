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

## Payload scans

`scan_all_payload` reads every value of `input` and `output` (`sum(length(…))`). It was the query where Strata trailed most: on wildchat 587 ms against Vortex compact's 424 and Parquet's 657, with 6.2 s of CPU against 4.6 and 2.6.

**Where the time went** (wildchat, 2026-11-20; pprof, then perf with kernel stacks):
- zstd decompression of the frames: about 40%. Strata decompresses 3.2 GB of frames for 5.9 GB of values.
- Rebuilding the values whose chunks are not consecutive in one frame: 19% of the rows, but 3.4 GB of the values, mostly conversation histories made of chunks of earlier values. Each is copied into the array (libc `memcpy` 14%); the other 2.5 GB are views into the frames.
- DataFusion's `length()`: 12–16%, the same work for every format.
- Local reads (`pread` and the page-cache copy): about 10%. Zeroing fresh pages in the kernel: under 2%.
- **Frames decoded twice.** DataFusion's 12 partitions split each of the 4 files into 3 row ranges. A row in one range points back to chunks of frames in the range before, so both partitions fetch and decode them: 48,214 frames instead of 38,582 (+25%).
- **Idle build threads.** A partition built a step of batches on its threads, then waited while DataFusion computed `length()` on them in its own task.

**The changes:**
- A scan that reads a payload column takes one partition per file, each building on its share of the cores (`StrataExec::new`). Narrow scans are still split.
- Such a scan builds its batches on a blocking thread, up to a step ahead of DataFusion (`built_ahead`). Dropping the stream stops the building. On every scan, the channel cost light's small scans 0.4–1.5 ms, so only payload scans with more than one step of batches use it.

**Results** (Strata, ms, mean of three alternating rounds of 20 runs; CPU in ms):

| Dataset | Before | After | CPU before | CPU after | Vortex compact | Parquet default |
|---|---|---|---|---|---|---|
| wildchat | 586 | **517** | 6,233 | 5,016 | 424 | 657 |
| toucan | 281 | 285 | 2,570 | 2,582 | 211 | 299 |
| payload | 350 | 353 | 2,975 | 3,020 | 266 | 340 |
| swe | 241 | 243 | 2,446 | 2,447 | | |
| openswe | 242 | 245 | 1,050 | 1,041 | | |

- Wildchat: −12% in time, −20% in CPU. With one partition per file and no build-ahead, CPU fell as much but the time did not move (577 against 576 ms): the cores were idle a quarter of the time (5.1 s of CPU in 576 ms on 12 cores).
- Toucan, payload, swe and openswe moved within ±2%: their partitions shared few frames.
- The other queries (two rounds over the six datasets and FineWeb's Braintrust queries, then the doubtful ones again in four rounds) stayed within the spread between two binaries, a few percent; light's queries of 1–10 ms moved by at most 0.45 ms, including `substring_input`, whose code path did not change.
- Simulated S3: payload's scan 664 → 558 ms, toucan's 516 → 500, wildchat's 804 → 828 (one round each).

**Tried and dropped:**
- Copying a rebuilt value's chunks straight into the array's blocks instead of through a scratch buffer: 1–4% slower. The cost is writing 3.4 GB to fresh memory, not the extra copy.
- Checking a rebuilt value's UTF-8 by its chunks' boundaries in already-checked frames instead of in full: no difference. Almost all of `simdutf8`'s time is the one check of each frame.
- `MIMALLOC_PURGE_DELAY=-1`, keeping freed memory instead of returning it to the system: 2–5% faster for every format, Parquet and Vortex too. It is a setting of the server, not of the format.

**What is left.** Vortex compact still takes 18–26% less time on these scans. It also decompresses with zstd; most likely the repeated histories inside its larger blocks become long matches, which zstd copies as it decompresses (its profile is dominated by the copy loop of `ZSTD_decompressFrame`). Strata decompresses less, then copies the histories again to make each value contiguous, as `Utf8View` requires. That copy is the price of deduplication, which makes Strata's files 4–20% smaller than Vortex compact's on these datasets and makes trace reads and searches faster.

## Narrow aggregates

Vortex compact won `model_rollup_day` on toucan and wildchat and FineWeb's spans per hour. After the changes above (ms, local, median of 30 runs; FineWeb 10):

| Query | Strata | Vortex compact | Parquet default |
|---|---|---|---|
| wildchat `model_rollup_day` | 17.3 (CPU 104) | 14.1 (CPU 82) | 23.0 |
| toucan `model_rollup_day` | 11.4 (CPU 49) | 10.8 (CPU 30) | 15.4 |
| light `model_rollup_day` | 21.0 | 20.6 | 47.4 |
| FineWeb spans per hour | 81 | 84 | 83 |

Before the change below, only wildchat's rollup still trailed, by 3 ms. Its profile: DataFusion's grouping on the `model` strings 15% of the CPU, decoding chunks 12%, gathering the selected rows 7%, casting the dictionary codes to `Utf8View` 6%, starting the threads of each wave about 5% (`pthread_create`, `mmap`, `munmap`), local reads 10%.

**Tried and dropped: rayon's pool instead of threads per wave.** `map_in_order` ran its groups on rayon's global pool (one thread per core) instead of starting scoped threads. Against the committed code, two alternating rounds: wildchat's rollup −6%, payload's −15%, but full scans +6 to +24% (payload 352 → 436 ms) and many queries of a few ms +10 to +40%. Rayon's threads compete with DataFusion's tokio threads for the same cores, and its idle threads wake slower than a thread starts. The scoped threads stay.

**Grouping on codes.** A `GROUP BY` on a narrow Utf8 column with plain aggregates now runs in Strata (`strata/src/aggregate.rs`). `StrataAggregateRule`, a physical optimizer rule registered with the session, replaces DataFusion's aggregate over a plain `StrataExec` by a `StrataAggregateExec` when:
- there is one group column, a narrow Utf8 column of every file;
- every aggregate is `count`, `sum`, `avg`, `min` or `max` of a narrow signed-integer column (`count`, `sum` and `avg` also take floats, `count` a literal), with no `DISTINCT`, `FILTER` or `ORDER BY`, possibly through DataFusion's casts to Float64;
- every filter is applied exactly by the scan (no lookup, limit or pushed sort).

The scan reads the group column as dictionary arrays (`ScanBatches::keep_dictionaries`), and each part of a file (the row ranges of a scan's partitions, one task each) accumulates per code, then merges by value once per code and batch. A file without a dictionary for the column groups by value. The results are DataFusion's: a test compares every supported aggregate, with NULL groups and values, filters, a file without dictionary and several partitions, and checks that other shapes keep DataFusion's plan.

**Results** (`model_rollup_day`, ms, mean of three alternating rounds of 30 runs, CPU in brackets; simulated S3 one round of 5):

| Dataset | Without the rule | With it | Vortex compact | S3 sim. without | S3 sim. with |
|---|---|---|---|---|---|
| light | 21.4 (188) | **17.1** (155) | 20.5 (155) | 144 | 128 |
| payload | 10.0 (42) | **9.4** (35) | 10.3 (28) | 128 | 124 |
| swe | 5.6 (15) | **4.5** (12) | 6.3 (17) | 88 | 83 |
| openswe | 3.7 (8) | **2.8** (6) | 5.1 (13) | 80 | 74 |
| toucan | 11.8 (49) | 11.0 (42) | **9.8** (28) | 124 | 92 |
| wildchat | 16.0 (98) | 13.8 (80) | **12.7** (77) | 132 | 105 |

- Strata now wins the rollup on four datasets, and trails Vortex compact by about 1 ms on toucan and wildchat, where decoding and gathering the rows remain.
- A first version ran one task per file and accumulated each file on one thread: light's four large files took 28.6 ms instead of 20.6. Splitting the files as the scan's partitions fixed it.
- The rule changes no other query of the bench: the others have no `GROUP BY`, or group on an expression (`date_trunc`), which stays in DataFusion.

## ClickHouse with Langfuse's schema

Langfuse, now owned by ClickHouse, stores spans in ClickHouse. This compares Strata with that
storage on our data (2026-11-20, ClickHouse 26.12.1.59, `bench/clickhouse/`).

**Setup.**
- Langfuse's current tables, from its migrations: `events_full`, ordered by (project, start minute, trace id hash), with zstd(3) `input` and `output`, full-text `text` indexes on their lowercase forms, and bloom filters on the trace, span, user and session ids; and `events_core`, filled by a materialized view with `input` and `output` cut to 200 characters, which Langfuse's lists and rollups read.
- Each dataset's `pq_default` spans are loaded into them (`load.sql`: `span_kind = 'llm'` becomes `type = 'GENERATION'`, an error status `level = 'ERROR'`, tokens and costs go to `usage_details` and `cost_details`, metadata to `metadata_names` and `metadata_values`), then merged with `OPTIMIZE ... FINAL`, as a long-running table would be.
- The bench's queries are written for these tables as Langfuse would: lists and rollups on `events_core`, payload and full trace reads on `events_full`, a trace by `trace_id =`. A text search is Langfuse's own predicate (`events-predicate.ts`): `position(lower(input), lower(needle)) > 0` with `hasAllTokens` on the text index; it matches words, case-insensitively. "Exact" is `position(input, needle) > 0`, the case-sensitive substring the other formats run; no index serves it.
- One server on the same VM, its caches as configured by default, the query cache and the query condition cache off (they return or skip by earlier runs of the same query). Times are the server's (`query_log`), median of 30 runs (FineWeb 5); ours are in-process, median of 30 (5), all run in one session after ClickHouse's; the idle ClickHouse server stayed up during ours (about a tenth of one of the 12 cores).

**Size** (MiB; ClickHouse is `events_full` + `events_core`):

| Dataset | Strata | Parquet default | Vortex compact | ClickHouse |
|---|---|---|---|---|
| light | 159 | 152 | 137 | 315 (158 + 157) |
| payload | 253 | 333 | 317 | 507 (390 + 117) |
| swe | 25 | 37 | 33 | 85 (76 + 9) |
| openswe | 30 | 48 | 159 | 149 (140 + 8) |
| toucan | 190 | 247 | 234 | 409 (327 + 82) |
| wildchat | 564 | 593 | 585 | 912 (749 + 163) |
| FineWeb | 9,444 | 7,892 | 7,775 | 9,006 (8,766 + 239) |

ClickHouse takes 1.6–5× Strata's space on the agent and chat data: it compresses each value on its own, so resent histories are stored again, and `events_core` keeps a second copy of the narrow columns and of 200-character previews. On unique text (FineWeb) the two are within 5%.

**Queries** (ms):

| Dataset | Query | Strata | Parquet | Vortex compact | ClickHouse |
|---|---|---|---|---|---|
| light | `count_errors_6h` | 7.1 | 15.4 | 6.9 | **6.1** |
| light | `model_rollup_day` | 22.4 | 45.3 | **21.4** | 67.1 |
| light | `list_llm_1h` | 12.0 | 79.2 | 21.7 | **11.0** |
| light | `metadata_filter` | **5.2** | 51.6 | — | 28.1 |
| light | `user_filter` | **3.3** | 10.5 | 8.7 | 5.0 |
| light | trace lookup, all columns | **5.6** | 27.1 | 29.4 | 17.7 |
| light | trace lookup, light columns | **1.2** | 8.4 | 7.2 | 6.4 |
| toucan | `model_rollup_day` | 11.7 | 15.2 | **10.4** | 18.0 |
| toucan | `list_llm_1h` | 8.6 | 28.9 | 12.4 | **7.1** |
| toucan | `metadata_filter` | **3.1** | 16.9 | — | 9.4 |
| toucan | trace lookup, all columns | **6.2** | 23.8 | 25.5 | 23.7 |
| toucan | `substring_input` (a rare word) | 57.2 | 192.5 | 101.8 | **3.7** Langfuse / 107 exact |
| toucan | `scan_all_payload` | 296 | 307 | **214** | 228 (1.1 from `input_length`) |
| wildchat | `model_rollup_day` | 14.9 | 23.0 | **13.4** | 31.2 |
| wildchat | `list_llm_1h` | 10.1 | 47.8 | 20.5 | **9.8** |
| wildchat | `substring_input` (a rare word) | 74.2 | 378 | 250 | **4.7** Langfuse / 197 exact |
| wildchat | `scan_all_payload` | 637 | 728 | 453 | **426** |
| payload | `substring_input` (a rare word) | 33.8 | 201 | 138 | 34.8 Langfuse / 123 exact |
| FineWeb | absent phrase | **100** | 6,665 | 5,889 | 221 Langfuse / 4,725 exact |
| FineWeb | count "help me" | **2,866** | 6,578 | 5,875 | 5,551 Langfuse / 4,678 exact |
| FineWeb | newest 100 "help me" | **95** | 3,672 | 6,943 | 994 / 876 |
| FineWeb | newest 100 rare phrase | **113** | 8,382 | 10,358 | 1,400 / 1,510 |
| FineWeb | newest match of the long phrase | **108** | 6,535 | 5,878 | 388 / 4,725 |
| FineWeb | count "help me", last day | **1,060** | 2,583 | 2,283 | 1,908 / 1,635 |
| FineWeb | count "help me", last hour | **71** | 505 | 189 | 333 / 322 |
| FineWeb | newest "help me", last 5 min | 49 | 820 | 335 | 44 / **41** |
| FineWeb | newest rare phrase, last hour | **24** | 835 | 356 | 318 / 319 |
| FineWeb | spans per hour | 79 | 83 | 79 | **12** |

The other datasets follow the same pattern. On payload, swe and openswe Strata is 1.5–6× faster on trace lookups, metadata and user filters, and rollups; `list_llm_1h` goes to Strata on swe and openswe (3.1 and 3.5 ms against 6.2 and 5.7) and to ClickHouse on payload (7.3 against 9.5); Langfuse's word search takes 3.2–3.5 ms against Strata's 17–24 on swe and openswe, and ties on payload (35 against 34).

**What it shows:**
- **Trace views are Strata's.** A full trace takes 5–7 ms against ClickHouse's 18–34 ms: ClickHouse finds the trace's granules by bloom filter, then decompresses whole granules of `input` and `output` (64 MiB `index_granularity_bytes`); Strata reads the trace's rows and their chunks.
- **Search: ClickHouse wins rare words, Strata wins phrases and newest-first.** The text index answers a rare word from its postings in 3–5 ms, where Strata decompresses the candidate frames (17–74 ms). For phrases, ClickHouse's postings only narrow to the rows holding every word, and it decompresses all of them: on FineWeb Strata is 2× faster on an absent phrase, 1.9× on the "help me" count, and 9–12× on the newest matches, which ClickHouse finds only after evaluating every candidate. Langfuse's search is also not a substring search: it counts 50,870 "help me" rows (case-insensitive), where the byte search counts 49,835.
- **Rollups and filters on narrow columns: Strata or Vortex, not ClickHouse**, whose rollups are 1.5–3× slower; it wins `count_errors_6h` on light (6.1 against 7.1) and an hourly histogram over FineWeb, where its 239 MiB `events_core`, sorted by start minute, answers in 12 ms against 79.
- **Full payload scans: ClickHouse and Vortex** are 23–35% ahead of Strata on payload, toucan and wildchat, level on swe, behind on openswe; with its precomputed `input_length` column ClickHouse needs no scan at all for lengths.
- **Writes:** loading the Parquet files took ClickHouse 14–47 s per dataset plus 7–61 s of merging; FineWeb took 473 s plus 158 s, against Strata's 69–75 s (§ Writer threads and file size). These include reading the Parquet files and building the text indexes, so they compare only roughly.

## Simulated S3

Every number above is local disk with the OS page cache warm, where a request costs microseconds. On S3 a request costs a first-byte latency, and one request runs at one connection's throughput, so the same reader can rank differently. `FMT_SIM_S3=1` estimates S3 times without S3 (2026-11-20, `bench/src/sim.rs`):
- Each GET reads the local file, then waits until the time it would end on S3: 30 ms of first-byte latency, plus its bytes at 90 MB/s, and no earlier than a shared 10 Gbit/s link allows for all the bytes in flight (`FMT_SIM_LATENCY_MS`, `FMT_SIM_CONN_MBPS`, `FMT_SIM_LINK_GBPS`).
- Dependent waves, concurrency and the CPU work between requests come from each reader, so every format pays for its own access pattern.
- Not modelled: throttling (S3 allows about 5,500 GETs per second per prefix), retries, and connection set-up. The constants are AWS's typical figures. Tail latency and the LIST round trip were added after a run on real S3 (§ Real S3); the tables here are without them.
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
- **Still behind:** a cold trace lookup takes 5 dependent waves (open, metadata, sort-key chunks, row chunks and refs, frames) and ties with Parquet (since fixed: § Cold lookups on S3); Vortex compact's full scans stay faster (333–523 ms against Strata's 530–842 on toucan, payload and wildchat).

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

## Cold lookups on S3

A cold trace lookup on simulated S3 took 4 waves (5 on wildchat and FineWeb) and tied with Parquet (§ Simulated S3). One whole wave fetched metadata only: each column's block-table pages, dictionary and chunk dictionary, and the payload stores and headers, 640 KB to 950 KB per file in about 55 requests. These segments sat next to each column's data, spread over the whole file, so they could not come with the open.

**The change** (`prototype-plan.md` § S1):
- The writer keeps the metadata segments (block-table pages, dictionaries, chunk dictionaries, key filters, payload stores and headers) in a buffer of their own and writes them together after the data, before the hot tail. The postscript records where they start (`meta_offset`, from its reserved bytes).
- The open reads that section with the hot tail when it is at most `Config::open_metadata_max` (4 MiB), and keeps it. Later reads of its segments are served from memory, with no request and no wave. A lookup on a freshly opened file then takes the path of a warm one: the sort-key, narrow-chunk and refs reads in one wave, then the frames.
- The default first read at open grows from 64 KiB to 1 MiB (`Config::open_default_suffix`). `FileSummary::tail_len` gives the exact length, for a catalog to keep as the hint, so one read covers the tail.
- The files keep their size: the same segments, in another order (within 1 KB on every dataset).

**Results** (simulated S3, ms, mean of two rounds of 3, the committed code on the old files against this change on the same rows rewritten; Parquet from § Simulated S3):

| Dataset | Before | After | Requests | Waves | Best Parquet |
|---|---|---|---|---|---|
| light | 169 | **151** | 131 → 71 | 4 → 3 | 165 |
| payload | 173 | **143** | 129 → 68 | 4 → 3 | 164 |
| swe | 156 | **135** | 53 → 28 | 4 → 3 | 155 |
| openswe | 157 | **139** | 48 → 29 | 4 → 3 | 153 |
| toucan | 165 | **147** | 117 → 61 | 4 → 3 | 168 |
| wildchat | 201 | 179 | 115 → 60 | 5 → 4 | 170 |
| FineWeb | 208 | **145** | 93 → 46 | 5 → 3 | 175 |

- A cold lookup now beats the best Parquet on every dataset but wildchat, by 9–20%, and makes half the requests.
- Wildchat keeps one more wave: its tail (970 KiB of metadata and a 70 KiB hot tail) is just over the 1 MiB first read, so the open reads twice. `light` (1.4 MiB of metadata) does too. A catalog's hint, or a 2 MiB first read, would remove it.
- **Locally** a cold lookup costs 0 to 0.8 ms more (+0–12%; payload −9%): the open reads 0.1–1.5 MiB per file instead of 64 KiB, and the page cache makes the saved wave nearly free.
- **The other queries** run on files already open, where the metadata was cached before too. On simulated S3 they moved within the run-to-run noise. Locally too, except FineWeb's absent phrase: 82 → 88 ms (median of 15, four alternating rounds), with the same requests and bytes. Profiles of both binaries show no new cost; the cause is not found.
- **Memory:** an open file keeps its metadata section, 66 KiB (openswe) to 1.4 MiB (`light`) here, about what its decoded caches held after a first query.

## Real S3

The cold lookups ran on a real object store, to check the simulator (2026-11-21): Cloudflare R2, EU jurisdiction, read from ubuntu-vm in Helsinki (a guest on big-server, about 1 Gbit/s of link). The six datasets were uploaded with `fmtbench upload` (154 files, 11.5 GiB, in under 2 minutes). The machine was idle before each step, and its load stayed under 1.5 during the R2 runs, with fmtbench the only busy process.

**The store** (`fmtbench probe`, the AnyBlob method: ranged GETs of 1 KiB to 16 MiB on one 64 MiB object, then 1 to 256 GETs in flight; three runs between 13:00 and 16:30 UTC):
- First byte: median 59–69 ms at every size; p90 73–115 ms, p99 80–230 ms on one connection.
- One request: 99–118 MB/s. The fitted model is 61–64 ms + size / (99–118 MB/s).
- HEAD and LIST: median 57–63 ms, one round trip like a GET. A GET on a new client costs 80–85 ms: 20–25 ms of connection set-up.
- In flight together: at 16 and 64 GETs of 64 KiB, the median stays at 65–71 ms but p90 is 101–135 ms and p99 138–273 ms. All requests together reach about 105 MB/s, the VM's link: with 64 GETs of 1 MiB in flight a request waits 0.5 s, and a few took 8–30 s.

**Cold lookups** (ms per trace id, mean of 3 passes over 20 ids; real R2 at 14:00–15:05 UTC, one round per dataset; the simulator at the probe's numbers, 60 ms, 115 MB/s and 0.9 Gbit/s, first without and then with the changes below; best Parquet is the best of `pq_default`, `pq_tuned` and `pq_cetra` through DataFusion; Strata is `st_fast`):

| Dataset | R2: best Parquet | R2: Strata | R2: Vortex compact | Simulator before: Parquet / Strata | Simulator after: Parquet / Strata |
|---|---|---|---|---|---|
| light | 536 | **458** | 1,039 | 303 / 242 | 498 / 431 |
| payload | 553 | **468** | 1,121 | 299 / 264 | 521 / 454 |
| swe | **375** | 383 | 702 | 270 / 246 | 459 / 432 |
| openswe | 406 | **393** | 1,281 | 270 / 233 | 459 / 430 |
| toucan | 481 | **425** | 1,015 | 291 / 247 | 504 / 438 |
| wildchat | 598 | **481** | 1,449 | 332 / 318 | 517 / 512 |

- **On real R2, Strata is 12–20% faster than the best Parquet** on light, payload, toucan and wildchat, and level on swe (+2%) and openswe (−3%), which one round cannot separate. Strata through SQL (`st`) is 20–90 ms slower than `st_fast` and still ahead on four datasets. Vortex is 2–3× slower everywhere: it reads 30–250 MB per lookup.
- **The simulator gave half the real times.** Strata's 3 waves at a 60 ms median predicted about 240 ms against 380–480 measured. Two costs were missing:
  - each cold lookup lists the variant's files first, one round trip that the simulator answered at once;
  - a wave sends 20–40 requests at once and waits for the slowest, which on R2 is near the p90–p99, not the median.
- **The change** (`bench/src/sim.rs`): a LIST waits one first-byte latency, and each first-byte latency is drawn from a lognormal whose median is `FMT_SIM_LATENCY_MS` and whose shape is `FMT_SIM_LATENCY_SIGMA` (fixed seed). σ = 0.35 matches the probe's spread under concurrency (p90/p50 ≈ 1.6). The default stays 0, so the earlier tables reproduce.
- **After it, the simulator is within ±15% of R2** for Strata and Parquet through DataFusion (Strata −6% to +13%; Parquet −14% to +13%, and +22% on swe), against −40% to −50% before. It still ranks Strata first on swe and openswe, where R2 shows a tie.
- **Still not modelled: queueing.** `pq_fast` makes 250–4,400 requests per lookup; R2 took 1.6–2.4 s for it, 1.0–2.1× the simulator's estimate, as the link and the store queue them. Connection set-up (20–25 ms on a new client) is also left out: the benchmark's store reuses its connections.
- For estimates of R2 from this machine: `FMT_SIM_S3=1 FMT_SIM_LATENCY_MS=60 FMT_SIM_CONN_MBPS=115 FMT_SIM_LINK_GBPS=0.9 FMT_SIM_LATENCY_SIGMA=0.35`. The defaults (30 ms, 90 MB/s, 10 Gbit/s) remain AWS's in-region figures, unmeasured here.

## Real S3 from Virginia

The same tests from a second machine, closer to its bucket (2026-11-21 and 22): ovh-us, a bare-metal server in Virginia (8 cores, about 1 Gbit/s of link, Cloudflare's IAD colo), reading an R2 bucket in Eastern North America (`cetra-fmtbench-us`). `fmtbench download` copied the datasets from the EU bucket and `fmtbench upload` put them in the US one. The machine was otherwise idle: `sar` shows a 1-minute load of at most 1.4 and the CPU 93% idle over the night run.

**The store** (`fmtbench probe`): 44–50 ms to the first byte (median), 101–113 MB/s per request, about 115 MB/s in total; HEAD and LIST 41–43 ms. The fit is 53 ms + size / 113 MB/s: 10–15 ms closer than from Helsinki, with the same link.

**Every query, two rounds in alternating order** (ms, mean of the two rounds' medians, night of 21–22 November; Strata through SQL except the cold lookup, which is `st_fast`; best Parquet of `pq_default`, `pq_tuned` and `pq_cetra`; the two rounds differ by 7% at the median):

| Query | light | payload | swe | openswe | toucan | wildchat |
|---|---|---|---|---|---|---|
| `list_llm_1h`: Strata / Parquet / Vortex | **227** / 577 / 393 | **289** / 309 / 397 | 249 / 218 / **193** | 258 / 239 / **185** | **183** / 309 / 290 | **235** / 318 / 316 |
| `model_rollup_day` | **215** / 327 / 451 | 166 / 176 / **160** | **130** / 140 / 156 | 141 / **138** / 150 | 188 / **156** / 157 | 273 / **207** / 222 |
| `trace_lookup_full` | **140** / 263 / 807 | **212** / 227 / 823 | 168 / **151** / 360 | 195 / **151** / 910 | 214 / **181** / 524 | **200** / 226 / 1,002 |
| `substring_input` | – | **369** / 1,273 / 1,119 | **285** / 497 / 390 | **237** / 564 / 1,658 | **444** / 913 / 1,100 | **436** / 2,161 / 2,233 |
| `scan_all_payload` | – | **1,810** / 2,558 / 2,431 | 513 / 551 / **474** | **507** / 677 / 1,708 | **1,411** / 1,805 / 1,766 | 5,247 / 4,372 / **4,070** |
| cold lookup | **383** / 412 / 897 | 402 / **384** / 926 | 382 / **325** / 525 | 368 / **299** / 1,151 | 388 / **341** / 777 | 440 / **354** / 1,214 |

- On S3, Strata leads every search (2–5× ahead of Parquet on agent and chat data) and most scans; Strata's wildchat scan took 3.5 s in one round and 7.0 s in the other, while every other format stayed within 6%.
- Cold lookups were Strata's weak point: from Virginia the best Parquet was ahead on five datasets, where from Helsinki Strata led on four. Parquet's time fell by 50–240 ms with the shorter latency, Strata's by 10–40 ms.
- Vortex compact is 2–6× slower on lookups; `metadata_filter` fails on it (a Utf8 against Utf8View comparison in its DataFusion integration).

**Where a cold lookup spends its time** (`FMT_TIMELINE=1`: each request's start, first byte and end). On swe both formats take a LIST and three waves:

| Step | Parquet (`pq_default`) | Strata |
|---|---|---|
| LIST | done at 42 ms | done at 32 ms |
| open | 4 footers of 512 KiB, done at 115 ms | 4 tails of 1 MiB, done at 108 ms |
| second wave | 4 batched reads (page index), done at 176 ms | 18 GETs of 1–38 KiB in one file, the last done at 180 ms |
| third wave | 1 batch of 1.4 MB, done at 234 ms | 4 GETs of 7–31 KiB, the last done at 275 ms |

The waves are the same; the requests are not. A wave ends with its slowest GET, and Strata's waves send many small ones (about 45 on wildchat). Parquet sends few: it reads through object_store's `get_ranges`, which merges ranges under 1 MiB apart and runs 10 merged GETs at a time. The `reqs` column counts those ranges, so Parquet's 75–110 "requests" per lookup are about ten HTTP requests.

**Three changes for object storage**, all read-side (the files do not change) and off by default:
- **The open's exact tail length**, as cetra's catalog would keep it (`FileSummary::tail_len`; `FMT_OPEN_HINT`): 15–55 ms faster per cold lookup. A 64 KiB first read is slower than 1 MiB: most tails are longer, so it costs a wave.
- **A wider gap for lookup reads** (`Config::point_gap`, `FMT_POINT_GAP`), capped by `Config::point_max_range`. Cold lookups with the hint, ms, mean of two rounds:

| Gap | light | payload | swe | openswe | toucan | wildchat |
|---|---|---|---|---|---|---|
| 4 KiB (default) | 285 | 324 | 280 | 291 | 318 | 331 |
| 64 KiB | 273 | 311 | 278 | 254 | 290 | 306 |
| 256 KiB | 270 | 317 | 260 | 283 | 295 | 316 |
| 1 MiB | 332 | 336 | 267 | 264 | 299 | 329 |
| 4 MiB | 519 | 400 | 336 | 276 | 383 | 363 |
| `pq_cetra` | 356 | 328 | 335 | 300 | 349 | 327 |

  Without a cap a 1 MiB gap made one 5.6 MB read on wildchat, which alone took 118 ms.
- **Searches keep 4 KiB** (`ScanKind::Sparse`). Searches had read their scattered candidate frames as point reads, and a 256 KiB gap made them up to 2.8× slower (wildchat 378 → 1,054 ms), even with a 256 KiB cap: the merged gaps cost more bytes on a 1 Gbit link than the requests they save. Index pages and sparse frames now always merge at 4 KiB.

**Tried and dropped: hedged GETs** (a second copy of a GET still running after 50, 70 or 100 ms; in the store for every format, `FMT_HEDGE_MS`, and inside Strata). Neither beat no hedging: every result moved within ±10% for Strata and both Parquets, either way. The slow GETs of a wave are not slow enough, or not independent enough, for a copy to finish first; the Strata version was removed.

**The remote profile** (tail hint, 256 KiB lookup gap, 1 MiB cap, searches at 4 KiB) against Strata's defaults, every query, two alternating rounds (22 November, 15:28–16:51 UTC, a noisier afternoon: the rounds differ by 8% at the median and 53% at the 90th percentile):
- **Lookups** are 8–32% faster: `trace_lookup_full` 0.79–0.91×, `trace_lookup_light` 0.68–0.98×, cold lookups 0.78–0.92×.
- **Searches, scans and rollups** move within the noise (0.87–1.05×). `list_llm_1h` is noisy (light took 258 and 724 ms by default); only openswe is slower in both rounds (+20%).
- **Cold lookups against Parquet** (ms, mean of two rounds; Strata default → remote / `pq_cetra` / `pq_default`): light 365 → **285** / 397 / 418, payload 380 → **350** / 372 / 359, swe 336 → 294 / 310 / **280**, openswe 341 → **294** / 335 / 299, toucan 358 → **330** / 383 / 371, wildchat 435 → 376 / **365** / 388. Strata now leads on four datasets and is within 5% on swe and wildchat.

**The simulator** at this machine's numbers (53 ms, 113 MB/s, 0.9 Gbit/s, σ 0.35) matched Strata and Vortex within 3% on cold lookups, and full scans and searches mostly within 10%, but put Parquet's lookups 60–120 ms too high: it sent one GET per range of a `get_ranges` batch. It now merges them as object_store does; Parquet's estimate falls to within 1–14% of R2 for `pq_default` (still 15–23% high for `pq_cetra`), and Strata's stays within 3%.

**What it means for cetra:** on object storage Strata should open with the catalog's tail length and merge lookup reads across 256 KiB (at most 1 MiB per request); on local disk the defaults stay. The next gain is fewer waves: a lookup's frames still wait for its references, one wave later.

## Narrow chunks

**Where `light` loses size** (MiB, the four files; Strata's narrow columns against `pq_default`'s column chunks):

| Columns | Strata | Parquet |
|---|---|---|
| `span_id`, `trace_id`, `parent_span_id` (with the sort-key filter) | 20.8 | 17.5 |
| `cost_total`, `cost_input`, `cost_output` | 8.3 | 5.0 |
| token counts (total, input, output, cache read) | 8.7 | 4.9 |
| `user_id`, `session_id` (with their key filters) | 4.9 | 2.5 |
| low-cardinality text (`model`, `name`, `provider`, …), each | 0.2–1.0 | 0.1–0.8 |
| `start_time`, `end_time`, `duration_ns` | **24.0** | 28.6 |
| `attributes` | **24.7** | 31.0 |
| `ingest_time` | **2.0** | 3.5 |

A narrow chunk holds 2,048 rows and is stored raw unless zstd saves 30%. On these columns zstd saves 10–25% of a bit-packed chunk, so they stay raw, while Parquet compresses whole 1 MiB pages.

**Tried** (scratch copy, `light` and `toucan`, sizes then local and simulated-S3 queries):

| Variant | `light` size | Queries |
|---|---|---|
| zstd from 10% saved | 158 → 151 MiB | `model_rollup_day` +14% (17.8 → 20.3 ms, four alternating rounds), `metadata_filter` +6%, the rest within noise |
| 8,192-row chunks | 158 → 150 MiB | trace lookups about 2× slower locally and 11–25% slower on S3; `list_llm_1h` +40% |
| both | 158 → 146 MiB | as 8,192-row chunks |

- A 5% bar saves no more than 10% (154.2 against 154.5 MiB of narrow columns).
- At 10%, `span_id` matches Parquet (10.5 MiB) and the token, cost, `user_id` and dictionary-code columns shrink by 10–20% each. The rollup reads `model`, costs and tokens, whose chunks are then decompressed on every scan. The other datasets change by under 1%: most of their bytes are payloads.
- Larger chunks make a lookup fetch and decode four times the rows it needs, which gives up Strata's main lead for 1–3% of size.
- **Kept: 30% and 2,048 rows.** `light` stays 4% bigger than Parquet and 15% bigger than Vortex compact. What would close it without slowing scans is encodings Strata lacks: delta for sorted timestamps, ALP for floats, FSST for short strings, and zstd on ID columns only, which aggregates never scan.

## Reproduce

```bash
# on ubuntu-vm, pyarrow 24 in ~/cfb/venv
python -I bench/convert_convos.py openswe raw/openswe.parquet 220 claudecode.parquet openhands.parquet
python -I bench/convert_convos.py toucan raw/toucan.parquet 1000000 k2-0.parquet k2-1.parquet k2-2.parquet
python -I bench/convert_convos.py wildchat raw/wildchat.parquet 1000000 w-0.parquet w-1.parquet w-2.parquet
fmtbench gen ~/bench/openswe 0 4 --convo 100000000 raw/openswe.parquet
fmtbench query ~/bench/openswe 30
# real S3 (AWS_ENDPOINT, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_DEFAULT_REGION=auto for R2)
fmtbench probe s3://<bucket>/fmtbench 50
fmtbench upload ~/bench8/light s3://<bucket>/bench8/light
fmtbench query s3://<bucket>/bench8/light 3 trace_lookup_cold
FMT_SIM_S3=1 FMT_SIM_LATENCY_MS=60 FMT_SIM_CONN_MBPS=115 FMT_SIM_LINK_GBPS=0.9 FMT_SIM_LATENCY_SIGMA=0.35 fmtbench query ~/bench8/light 3 trace_lookup_cold
fmtbench download s3://<bucket>/bench8 ~/bench8
FMT_TIMELINE=1 FMT_VARIANTS=st_fast,pq_default fmtbench query s3://<bucket>/bench8/swe 1 trace_lookup_cold
FMT_OPEN_HINT=1 FMT_POINT_GAP=262144 FMT_POINT_MAX_RANGE=1048576 fmtbench query s3://<bucket>/bench8/swe 3
FMT_HEDGE_MS=70 fmtbench query s3://<bucket>/bench8/swe 3 trace_lookup_cold
```
