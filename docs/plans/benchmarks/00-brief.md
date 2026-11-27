# Benchmarks against the competitors: brief

This brief is for an engineer (or a model) with no context beyond this file and the repository. It explains why the benchmarks are needed, what to measure, how to keep the comparison fair, what already exists in the repo, and what to deliver. Read it whole before starting. Where it says "ask", ask the user and wait: don't guess.

## 1. Why

cetra's homepage will get a performance section in the style of bun.sh and dragonflydb.io: three outcome cards (for example "N× less memory", "a $X box instead of six services") above a bar chart with tabs (ingest, query latency, memory, start-up), where cetra's bar stands out and each competitor's bar is labelled. Under the chart, one line names the machine, the dataset and the setup, and a **Reproduce →** link points to the scripts. A mockup exists (described here; ask the user for the image if you need it).

None of these numbers exist yet. This work produces them. Engineers trust such charts only if:
- the competitors are named, with their versions;
- each tool runs its own documented self-hosted setup;
- the workload is the same for all and public;
- anyone can rerun it with one command and get the same picture.

The numbers decide the page, not the other way around. If cetra loses a measurement, report it as it is; the page will lead with what we win and may concede the rest. Never tune the workload or the setups to make cetra win.

## 2. cetra in one page

cetra (cetra.sh) is an open-source LLM observability and evaluation platform written in Rust. It runs as **one binary on one machine**: OTLP ingestion, its own storage engine (write-ahead log → memtable → Parquet files, queried with Apache DataFusion), a SQLite metadata database, and a React UI embedded in the binary. Optional S3 for the Parquet files.

- Project rules: `AGENTS.md` (also `CLAUDE.md`) at the repo root. Read it: dependency pinning, no `unwrap`, privacy of logs, the `just` commands, where tools go (`tools/`, never a dependency of a shipped crate).
- Build: `just build-release` (web build, then `cargo build --release -p cetra-cli`); the binary is `target/release/cetra`. `just dist <target>` builds a release archive; the Docker image is built by `just image` from it.
- Run: `cetra serve` (see `website/src/content/docs/docs/self-hosting/install.md` and `configuration.md`). Every setting is in `website/src/content/docs/docs/self-hosting/configuration-reference.md`.
- Send traces: OTLP/HTTP at `http://<host>:8080/v1/traces` (or port 4318), OTLP/gRPC as documented in `website/src/content/docs/docs/get-started/opentelemetry.mdx`. Ingest needs a project API key (`sk-cetra-…`); how to create the first user, project and key is in the quickstart (`website/src/content/docs/docs/get-started/quickstart.mdx`) and the authentication page.
- Read traces: the HTTP API (`crates/server/openapi.json`; the trace list is `GET /api/v1/projects/{project}/traces`, a trace is `GET …/traces/{trace_id}`).
- **Memory settings matter.** `website/src/content/docs/docs/self-hosting/resources.md` says the defaults want 8 GiB or more, and gives a "small machine" configuration for 2-4 GiB. On a small machine, run cetra with that documented small-machine configuration (and say so in the results). Don't invent other tuning.

## 3. What exists already

- **Benchmark corpus:** `just fetch-corpus` downloads OpenAssistant oasst2 (Apache-2.0), pinned by SHA-256, into `target/bench-corpus/`. Use it as the text of every span, for every tool.
- **Storage-level benches** (cetra's storage engine alone, not the HTTP server): `crates/storage/tests/ingest_bench.rs` (ingest rate, flush, compaction) and `crates/storage/tests/trace_bench.rs` (trace list and trace queries). Results and method: `docs/research/storage/benchmarks.md`, `docs/research/storage/traces-benchmark.md`. They don't measure the server, so they can't be compared to competitors as they are, but their payload builder (`crates/storage/tests/common/payload.rs`, `common/corpus.rs`) shows realistic span shapes.
- **Soak:** `just soak` drives the storage for hours with ingest, scores, deletions, retention and queries; method and results (RSS, ingest rate on a VM) in `docs/research/storage/soak.md` and `docs/research/storage/soak/`. Useful for the method (samples from `/proc`, pacing, oracle checks).
- **Demo traffic generator:** `tools/demo/` (`cetra-demo`) sends realistic traces built from the captured fixtures over OTLP. Its README explains it. It may be a base for the load generator.
- **Captured OTLP fixtures:** `fixtures/otlp/` (real instrumentation output from OpenInference, OpenLLMetry, the OTel GenAI conventions and more). Good span shapes for a fair workload, since every competitor accepts OTLP.
- **Competitors' source code** is checked out read-only at `/home/sebastien/github/{lmnr,phoenix,langfuse,langwatch,helicone,opik,mlflow}`. Read it to find their OTLP endpoints, their self-hosting compose files and their documented resource needs. Never copy code from it.
- **Competitors' architectures** (from `docs/research/competitors.md` § storage):

| Product | App | Metadata | Traces | Queue | Other |
|---|---|---|---|---|---|
| Langfuse | Next.js web + worker | Postgres | ClickHouse | Redis | S3 |
| Phoenix | Python | SQLite or Postgres | same SQL DB | — | — |
| Opik | Java + Python | MySQL | ClickHouse | Redis | S3, Zookeeper |
| LangWatch | Node + Go + Python | Postgres | ClickHouse | Redis | S3 |
| Laminar | Rust + Next.js | Postgres | ClickHouse | RabbitMQ (opt) | Redis, Quickwit, S3 (opt) |
| Helicone | Next.js + Express | Postgres | ClickHouse | Kafka | S3, Redis |
| MLflow | Python | SQL DB | same SQL DB | Redis (jobs) | artifacts |

## 4. What to compare

**Competitors.** Required: **Langfuse** (the market leader, ClickHouse stack) and **Phoenix** (the simple single-process stack). Optional, if time allows: Opik, then Laminar. Each in its latest stable release at the time of the run, installed from its **official self-hosting instructions** (docker compose or the documented single container), with the configuration those instructions give. Record the exact versions and image digests.

**Workload.** The same OTLP traffic to every tool, over OTLP/HTTP protobuf (every tool above accepts OTLP; find each one's endpoint and auth in its docs or source and record it):
- traces of the three common shapes: chat (1 LLM span), RAG (retriever + LLM), agent (5-30 spans: LLM, tool, chain); the mix and span counts fixed and published;
- LLM span input and output text from the oasst2 corpus; total payload per span in a fixed, published range (the soak uses 10-50 KiB on LLM spans, under 1 KiB elsewhere: reuse that);
- GenAI attributes (model, tokens) on LLM spans, so every tool does its normal cost and token work;
- a fixed seed, so the traffic is identical between tools and runs.

**Machines.** Two sizes, the same for every tool: a small one (2 vCPU, 4 GiB RAM, SSD) and a medium one (8 vCPU, 16-32 GiB). If a competitor's documented minimum is larger than the small machine, run it on the smallest machine its docs allow and say so: "needs 16 GiB" is itself a result. Record CPU model, RAM, disk type, kernel, Docker version. **Ask the user which machines to use**: some of the team's machines are reserved for other runs. The load generator runs on a separate machine on the same network, so it doesn't steal CPU.

## 5. The measurements

Each one with its exact method, 3 runs, median and spread reported, after a warm-up.

| # | Measurement | Method |
|---|---|---|
| M1 | **Sustained ingest** (spans/s) | Ramp the send rate until the tool falls behind: accepted spans per second where every span is accepted (no 4xx/5xx, no drop) **and** queryable within 60 s, held for 10 minutes. Report the highest rate that holds. |
| M2 | **Ingest-to-visible latency** | At half of M1's rate, the p50 and p95 time from sending a trace to seeing it in the tool's trace-list API. |
| M3 | **Trace list latency** | With 1M and 10M spans stored (state the trace count), p50/p95 of the tool's trace-list API call for the last 24 hours, first page, default sort, cold and warm; then the same with one filter (by name or by tag). Use each tool's own API, the one its UI calls. |
| M4 | **Trace detail latency** | p50/p95 of fetching one full trace of 30 spans, from the 10M-span set. |
| M5 | **Memory** | Resident memory of the whole stack (sum of every container or process: `docker stats` or cgroup `memory.current`, and `/proc/<pid>/status` VmRSS for cetra), sampled every 5 s: idle after start, during M1's held rate, and peak. |
| M6 | **Disk** | Bytes on disk per million spans after the tool has settled (cetra: after compaction; ClickHouse: after merges). |
| M7 | **Start-up** | Time from start command to the first successful ingest and the first successful trace-list call, on a warm machine (images already pulled). |
| M8 | **Footprint** | Number of services and containers, image sizes (or binary size), and the documented minimum hardware. |
| M9 | **Cost** | From M1/M5: the smallest cloud machine (one named provider's public price list, dated) that sustains a stated load (for example 1M traces/day) for each tool; its monthly price. |

Every measurement records its errors (rejected or dropped spans, timeouts). A rate with drops doesn't count.

## 6. Fairness rules

- Each competitor runs its documented self-hosted setup, defaults included. No tuning of a competitor beyond its docs; no tuning of cetra beyond its docs (the small-machine settings of `resources.md` on the small machine, defaults on the medium one).
- Same machine size, same network, same workload, same seed, same warm-up for all.
- Report what you couldn't do (a tool that didn't install, an API you couldn't call) instead of leaving it out silently.
- Publish everything needed to rerun: scripts, compose files, versions and digests, raw samples, the exact commands.
- Before anything is published, the user decides whether competitors are named; write the results with real names.

## 7. Deliverables

1. **`tools/bench/`**: a load generator (OTLP/HTTP protobuf, the workload of § 4, seeded) and the measurement driver (M1-M7), plus one directory per tool with its pinned compose file or install steps. Follow `AGENTS.md`: language and dependencies pinned (Rust crates in the root `[workspace.dependencies]`, or Python with `uv` and a lock file, like `tools/soak/`), never a dependency of a shipped crate. Look up real versions; never guess one.
2. **`just bench-compare <tool> <machine-label>`** (and the recipe that runs all of them), added to the `justfile` and to the `AGENTS.md` command table, not part of `just check`.
3. **Raw results** under `docs/research/benchmarks/<date>-<machine>/`: `run.json` (machine, versions, digests, commands), the samples, and a `summary.json` with one value per tool per measurement.
4. **A write-up**, `docs/research/benchmarks/README.md`: method, machines, every result table, the charts' data, what was hard, what we lose and why.
5. **The homepage data**: a small JSON with the values the section will show (per tab: tool, value, unit, "higher/lower is better"), and the three largest ratios in cetra's favour with their raw values. The website change itself is not part of this work.
6. A decision line for `docs/mvp-decisions.md` in the benchmarks section (ask the coordinating session for the ID scheme before adding it).

## 8. Order of work

1. Read § 2 and § 3's files; run cetra locally; send it one trace with the OTel SDK; fetch the trace list.
2. Write the load generator; check it against cetra on one machine (traces arrive, counts match).
3. Install Phoenix, then Langfuse, from their docs; send the same traffic; check counts.
4. The driver for M1-M7 on one tool; then on all; then the small and medium machines.
5. Results, write-up, homepage JSON.
Stop and ask after step 1 if anything about running cetra or creating its key is unclear, and after step 3 with the versions and setups you chose before the long runs.

## 9. Open questions for the user

- Which machines (provider, sizes) the runs use, and who pays for them.
- Whether Opik and Laminar are in scope.
- Whether the published page names the competitors (the write-up names them either way).
- The load used for the cost card (M9): 1M traces/day, or another figure.
