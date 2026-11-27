# Developer tools

Tools that are not part of the product. Benchmark scripts live here too.

## Fixture capture (`tools/capture/`)

`cetra-capture` records the spans a real instrumentation library emits. A short Python or TypeScript program calls a local mock of the OpenAI, Anthropic and Gemini HTTP APIs and exports OTLP to a receiver in the same tool. The tool then writes `fixtures/otlp/<group>/<case>/case.json` and `input.json`.

Scenarios live in `tools/capture/py/scenarios/<group>/<case>/` (uv) and `tools/capture/ts/scenarios/<group>/<case>/` (pnpm). A Python scenario whose packages cannot share `tools/capture/py`'s lock lives under `tools/capture/envs/<env>/scenarios/<group>/<case>/` and runs in that environment. Each directory holds `scenario.json` and the program.

The Python harness that scenarios import is a uv package of its own, `tools/capture/harness` (`cetra-capture-harness`, imported as `capture_py`). `tools/capture/py` and any scenario environment under `tools/capture/envs/<env>/` depend on it by path, so the import path does not change.

The recipes are in the tool; their design is in K9–K10 of [`docs/plans/b2-capture/`](../docs/plans/b2-capture/README.md):

- `just capture <group>/<case>` records one scenario.
- `just capture-all` records every scenario.
- `just check-capture` runs the scenario projects' lint and type checks, then recaptures every scenario and compares it with the committed `input.json` (timing excluded).

A captured `input.json` is never edited by hand. Change the scenario and recapture (D122).

## SQL differential harness (`tools/sql-diff/`)

`cetra-sql-diff` and its Python DuckDB runner (`tools/sql-diff/py/`, uv) cross-check `cetra-query` against DuckDB over format-agnostic `BatchSource` datasets:

- `just sql-diff [cases] [seed]` builds `cetra-sql-diff` in release mode and runs the curated `.slt` corpus plus generated Shape B SQL queries against DuckDB.
- `just check-sql-diff` lints, type-checks, unit-tests, and runs a 50-query + `.slt` smoke diff.

See [`tools/sql-diff/README.md`](sql-diff/README.md) for the wire protocol and documented dialect differences.
