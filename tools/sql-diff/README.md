# `tools/sql-diff` — DuckDB Differential SQL Verifier

Format-agnostic differential testing tool that compares `cetra-query` against DuckDB (UTC) over:
- Curated `.slt` suites (`crates/query/tests/slt/*.slt`), honoring `# duckdb: skip` and `# unordered_arrays`.
- Seeded Shape B SQL queries generated over an adversarial multi-table dataset (`t_main`, `t_right`, `t_empty`).

Because `cetra-sql-diff` loads tables purely via `cetra_query::exec::BatchSource` + `SqlCatalog::register_batch_source`, it has zero Parquet, Arrow, or `cetra-storage` dependency.

## Usage

From the repository root:

```sh
# Build cetra-sql-diff, lint/type-check Python, run unit tests, and run a 50-query + .slt smoke diff
just check-sql-diff

# Run a deeper differential campaign (default: 2000 generated queries, seed 0)
just sql-diff 5000 42
```

Direct invocation from `tools/sql-diff/py/`:

```sh
uv run --locked python diff_runner.py --cases 2000 --seed 0 --slt --partitions 4
uv run --locked python diff_runner.py --slt-only
```

## JSON Wire Protocol (`cetra-sql-diff`)

`cetra-sql-diff` reads one JSON bundle from a file path argument or `stdin`:

```json
{
  "tables": [
    {
      "name": "t_main",
      "columns": [
        {"name": "id", "type": "I64", "nullable": false},
        {"name": "dec", "type": "DECIMAL(10,2)", "nullable": true},
        {"name": "tags", "type": "LIST(UTF8)", "nullable": true},
        {"name": "meta", "type": "MAP_UTF8", "nullable": true}
      ],
      "rows": [[0, "12.34", ["a", null], {"env": "prod"}]]
    }
  ],
  "queries": [
    {
      "id": "q0",
      "sql": "SELECT id AS c0, dec AS c1 FROM t_main ORDER BY id ASC NULLS LAST",
      "limit": 10000,
      "memory_cap_bytes": 33554432,
      "target_partitions": 1
    }
  ]
}
```

Supported wire types: `BOOL`, `I64`, `F64`, `DECIMAL(p,s)`, `TIMESTAMP`, `TIMESTAMPTZ`, `DATE`, `INTERVAL`, `UTF8`, `BINARY`, `FIXEDBYTES8`, `FIXEDBYTES16`, `LIST(<inner>)`, `MAP_UTF8`.

For every query, `cetra-sql-diff` asserts `pool.reserved() == 0` and `spill.spilled_bytes() == 0` after execution and writes one JSONL line to `stdout`:
- `{"status":"ok","id":"q0","columns":[{"name":"c0","type":"integer"},...],"rows":[...],"truncated":false}`
- `{"status":"error","id":"q0","kind":"invalid|invalid_request|too_large|cancelled|busy|query_failed|internal","message":"..."}`
- `{"status":"leak","id":"q0","reserved_bytes":...,"spilled_bytes":...,"outcome":"ok|error"}`

## Intentional DuckDB vs `cetra-query` Dialect Differences

Handled explicitly in [`py/dialect.py`](py/dialect.py) and [`py/diff_runner.py`](py/diff_runner.py):

| Area | `cetra-query` (Shape B / Postgres) | DuckDB | Handling in `sql-diff` |
|---|---|---|---|
| Timezone & timestamps | UTC, nanosecond `TimestampNs` | Session TZ; microsecond `TIMESTAMPTZ`, `TIMESTAMP_NS` | `SET TimeZone = 'UTC'`; `TIMESTAMP` → `TIMESTAMP_NS`; microsecond `TIMESTAMPTZ` inputs |
| `ORDER BY` NULLs | `ASC → NULLS LAST`, `DESC → NULLS FIRST` | `NULLS LAST` for both | Generator always emits explicit `NULLS FIRST` / `NULLS LAST` |
| `LIKE` / `ILIKE` escape | Default `ESCAPE '\'` | No default escape | Generator always emits explicit `ESCAPE '\'` when pattern contains `\` |
| Division & div-by-zero | Truncating `I64 / I64`; `query_failed` on `/ 0` (int & float) | `DOUBLE` for `/`, `//` for int div; `inf`/`nan`/`NULL` on `/ 0` | Generator uses float division and `NULLIF(..., 0.0)` guards |
| `CAST(F64 AS BIGINT)` | Truncates toward zero | Rounds to nearest | Generator uses integer-valued `floor(x)` before casting |
| `SUM(BIGINT)` overflow | Checked `I64` | Widens to `HUGEINT` | Bounded `k_small` column for `SUM` queries |
| Non-finite JSON floats | `NaN` / `±Inf` → JSON `null` (D98) | Python `nan` / `±inf` | Normalized to `None`; finite floats compared with `rel_tol=1e-9, abs_tol=1e-12` |
| `AVG(DECIMAL)` | Returns `Decimal128` | Returns `DOUBLE` | Compared numerically via `Decimal` / `math.isclose` |
| `MAP['k']` subscript | Scalar `Utf8` (`NULL` if missing) | `VARCHAR[]` list (`[]` if missing) | Full map columns compared directly; map subscripts verified in `.slt` + metamorphic tests |
| Unordered results & `ARRAY_AGG` | Engine-defined order without `ORDER BY` | Engine-defined order | Unordered queries compared as sorted multisets; `ARRAY_AGG` lists sorted |
| Cetra-only functions / sandbox | `encode`, `json_get`, `array_has`, `approx_percentile_cont`, sandbox refusals | Absent or different | Verified in `.slt` + metamorphic tests; tagged `# duckdb: skip <reason>` |
