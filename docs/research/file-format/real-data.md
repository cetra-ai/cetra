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

| Dataset | **Strata** | pq_default | pq_tuned | pq_cetra | vx_default | vx_compact |
|---|---|---|---|---|---|---|
| openswe | **29** | 47 | 149 | 48 | 702 | 158 |
| toucan | **196** | 246 | 295 | 247 | 770 | 233 |
| wildchat | 592 | 592 | 655 | 596 | 1,429 | **584** |
| swe (existing) | **25** | 36 | 65 | 36 | 560 | 31 |

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

- The Strata columns are measured after the UTF-8 changes below; `substring_input` and `scan_all_payload` after the one-pass check, `list_llm_1h` after the scan threads change (§ Narrow queries on one thread). Parquet and Vortex come from the first run.
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
  - Vortex compact still wins `list_llm_1h` and `model_rollup_day` on toucan and wildchat, though by less on `list_llm_1h` since the scan threads change below. This matches `light`, where its narrow-column decode is also cheaper.

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
- What is left against Vortex: Strata reads 5.7 MB for toucan's `list_llm_1h` against Vortex's 3.6 MB, and zstd is still the largest cost.

## Reproduce

```bash
# on ubuntu-vm, pyarrow 24 in ~/cfb/venv
python -I bench/convert_convos.py openswe raw/openswe.parquet 220 claudecode.parquet openhands.parquet
python -I bench/convert_convos.py toucan raw/toucan.parquet 1000000 k2-0.parquet k2-1.parquet k2-2.parquet
python -I bench/convert_convos.py wildchat raw/wildchat.parquet 1000000 w-0.parquet w-1.parquet w-2.parquet
fmtbench gen ~/bench/openswe 0 4 --convo 100000000 raw/openswe.parquet
fmtbench query ~/bench/openswe 30
```
