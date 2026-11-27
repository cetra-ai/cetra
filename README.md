<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/cetra-logo-on-dark.svg">
    <img alt="cetra" src="assets/brand/cetra-logo.svg" height="56">
  </picture>
</h1>

Open-source LLM observability and evaluation that runs as one binary on one machine.

cetra records the traces of LLM applications, shows their cost, latency and errors, and evaluates
their output. One process, `cetra serve`, holds everything: OpenTelemetry ingestion, its own
storage engine, the API and the web UI. It keeps its data in one directory, or in an S3-compatible
bucket.

> **Status:** in development. There is no release yet, and the SDKs are not published.

## Features

- **Tracing over OpenTelemetry.** OTLP over HTTP and gRPC. It reads spans from the OpenTelemetry
  GenAI conventions, OpenInference, OpenLLMetry and the Vercel AI SDK, so you can send traces
  without our SDK.
- **Traces, sessions and users.** Cost and token counts for each span, full-text search, a filter
  language, and a read-only SQL console.
- **Evaluation.** Scores from code, from users and from LLM judges. Online rules score new traces as
  they arrive.
- **Datasets and experiments.** Versioned datasets, CSV and JSONL import, and experiment runs you can
  compare side by side.
- **Alerts** on error rate, latency, cost and scores, sent to Slack, email or a signed webhook.
- **Storage.** A write-ahead log and Parquet files queried with cetra's native columnar query
  engine, plus SQLite for metadata. Files and media can live in S3.
- **Privacy.** Per-trace deletion that erases the data, and retention limits.
- **SDKs** for Python and TypeScript, built on OpenTelemetry.
- **MCP server**, so agents can query your traces.

## Run it from source

You need `rustup`, [`just`](https://github.com/casey/just), Node and `pnpm`.

```bash
git clone https://github.com/cetra-ai/cetra
cd cetra
just build-release
./target/release/cetra serve
```

On the first start, cetra logs a setup link: open it to create the first admin, organization and
project. It listens on port 8080 for the web UI, the API and OTLP/HTTP, and on 4317 for OTLP/gRPC.
Its data goes to `./cetra-data`.

Then send traces from an application:

```python
import cetra

cetra.init(api_key="sk-cetra-…", base_url="http://localhost:8080")


@cetra.observe()
def answer(question: str) -> str:
    return "…"
```

## Documentation

- [Self-hosting](docs/self-hosting/README.md): install, configuration, S3, backups, monitoring,
  upgrades.
- SDKs: [Python](sdks/python/README.md) and [TypeScript](sdks/typescript/README.md).
- [Technical specification](docs/spec/README.md): data model, ingestion, storage, query, evaluation,
  API.
- [Roadmap](docs/roadmap.md) and [decisions](docs/mvp-decisions.md).
- [Contributing](AGENTS.md): layout, rules and commands for anyone working in this repository.

## Licence

The core is under the [MIT licence](LICENSE). A few directories (`crates/server/src/ee/`,
`crates/token/`, `web/src/ee/` and `tools/license/`) are under the Elastic License 2.0, and their
features need a licence key.

## Contact

[cetra.sh](https://cetra.sh) · hello@cetra.sh
