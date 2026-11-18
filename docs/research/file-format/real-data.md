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
| wildchat | 594 | 592 | 655 | 596 | 1,429 | **584** |
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
| count_errors_6h | **0.76** | 2.31 | 1.84 | **0.93** | 2.27 | 2.09 | **0.88** | 2.28 | 2.12 |
| model_rollup_day | **3.46** | 5.19 | 4.84 | 11.04 | 10.95 | **8.09** | 16.10 | 15.78 | **12.45** |
| list_llm_1h | **4.32** | 6.63 | 6.36 | 29.82 | 24.27 | **10.55** | 34.73 | 23.95 | **14.60** |
| user_filter | **0.86** | 2.70 | 2.94 | **0.88** | 2.83 | 3.99 | **2.54** | 5.05 | 6.40 |
| metadata_filter | **0.78** | 3.72 | n/a | **2.26** | 10.23 | n/a | **3.41** | 16.71 | n/a |
| trace_lookup_full | **5.90** | 11.19 | 35.61 | **5.60** | 12.19 | 22.70 | **4.89** | 12.24 | 28.01 |
| trace_lookup_light | **0.99** | 3.73 | 3.74 | **0.99** | 4.81 | 5.30 | **0.99** | 5.53 | 6.16 |
| substring_input | **11.66** | 266.86 | 172.29 | **41.71** | 168.63 | 98.73 | **62.73** | 312.04 | 204.27 |
| scan_all_payload | 290.82 | **286.10** | 313.07 | 316.17 | 281.04 | **205.34** | 768.31 | 538.04 | **404.95** |

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

- **The agent results hold on other frameworks and tasks.** On openswe and toucan, Strata has the smallest files, the fastest lookups (about 2× faster than Parquet, 3–6× faster than Vortex compact) and the fastest search (10–32× Parquet, 2.4–12× Vortex compact).
- **Chat traffic is Strata's weak case.** With many short conversations:
  - files are only as small as the others;
  - `scan_all_payload` takes 1.4× Parquet's time and 1.9× Vortex compact's;
  - `list_llm_1h` takes 1.45× Parquet's time and 2.4× Vortex compact's.

  The cost is per value: wildchat has 18× openswe's spans for about the same payload volume. This is the open per-chunk decode cost from [timeseries.md](timeseries.md) gap 4.
- **Vortex compact wins `list_llm_1h` and `model_rollup_day` on every dataset with many spans.** That matches `light`, and the cause is the same: its narrow-column decode is cheaper.

## Reproduce

```bash
# on ubuntu-vm, pyarrow 24 in ~/cfb/venv
python -I bench/convert_convos.py openswe raw/openswe.parquet 220 claudecode.parquet openhands.parquet
python -I bench/convert_convos.py toucan raw/toucan.parquet 1000000 k2-0.parquet k2-1.parquet k2-2.parquet
python -I bench/convert_convos.py wildchat raw/wildchat.parquet 1000000 w-0.parquet w-1.parquet w-2.parquet
fmtbench gen ~/bench/openswe 0 4 --convo 100000000 raw/openswe.parquet
fmtbench query ~/bench/openswe 30
```
