"""Format-agnostic DuckDB differential test runner for `cetra-query`.

Supports two modes:
1. `--slt`: cross-runs all non-skipped curated `.slt` queries from `crates/query/tests/slt/`
   against DuckDB (UTC) and `cetra-sql-diff`.
2. Generated Shape B SQL (`--cases N --seed S`): generates deterministic seeded SQL queries
   over an adversarial multi-table dataset and diffs every result against DuckDB.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypeVar

import duckdb

from dialect import (
    DUCKDB_INIT_SQL,
    FATAL_ERROR_KINDS,
    EngineResult,
    cetra_json_to_result,
    compare_query_results,
    duckdb_cursor_to_result,
    duckdb_value_to_cetra_json,
    has_array_agg,
    has_top_level_order_by,
    parse_drop_table_name,
    parse_slt_file,
    rewrite_slt_insert_for_duckdb,
    rewrite_sql_for_duckdb,
    translate_create_table_for_duckdb,
)

T = TypeVar("T")
BATCH_SIZE = 500
DEFAULT_LIMIT = 10_000
DEFAULT_MEMORY_CAP = 32 * 1024 * 1024


class SplitMix64:
    """Deterministic 64-bit PRNG (portable across Python versions, no `random` dependency)."""

    def __init__(self, seed: int) -> None:
        self._state = seed & 0xFFFFFFFFFFFFFFFF

    def next_u64(self) -> int:
        self._state = (self._state + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
        z = self._state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
        return z ^ (z >> 31)

    def randint(self, lo: int, hi: int) -> int:
        """Returns an integer in `[lo, hi]` inclusive."""
        span = hi - lo + 1
        return lo + (self.next_u64() % span)

    def choice(self, seq: Sequence[T]) -> T:
        return seq[self.next_u64() % len(seq)]

    def chance(self, num: int, den: int) -> bool:
        return (self.next_u64() % den) < num


@dataclass
class RunStats:
    """Aggregated counters and failure records for a differential run."""

    passed: int = 0
    failed: int = 0
    expected_errors: int = 0
    skipped: int = 0
    failures: list[str] | None = None

    def __post_init__(self) -> None:
        if self.failures is None:
            self.failures = []

    def add_failure(self, message: str, max_failures: int) -> None:
        self.failed += 1
        if self.failures is not None and len(self.failures) < max_failures:
            self.failures.append(message)


@dataclass(frozen=True)
class GeneratedQuery:
    """One generated differential test query with comparison metadata."""

    query_id: str
    family: str
    sql: str
    ordered: bool
    unordered_arrays: bool


def find_repo_root() -> Path:
    """Resolves the repository root from `tools/sql-diff/py/diff_runner.py`."""
    return Path(__file__).resolve().parents[3]


def resolve_cetra_bin(explicit: str | None) -> Path:
    """Locates the compiled `cetra-sql-diff` binary."""
    if explicit is not None:
        path = Path(explicit)
        if not path.is_file():
            raise FileNotFoundError(f"cetra-sql-diff binary not found at {path}")
        return path
    root = find_repo_root()
    target_dirs: list[Path] = []
    env_target = os.environ.get("CARGO_TARGET_DIR")
    if env_target:
        env_path = Path(env_target)
        if env_path.is_absolute():
            target_dirs.append(env_path)
        else:
            target_dirs.append(env_path)
            rooted = root / env_path
            if rooted.resolve() != env_path.resolve():
                target_dirs.append(rooted)
    target_dirs.append(root / "target")
    candidates: list[Path] = []
    for tdir in target_dirs:
        candidates.append(tdir / "release" / "cetra-sql-diff")
        candidates.append(tdir / "debug" / "cetra-sql-diff")
    for cand in candidates:
        if cand.is_file():
            return cand
    searched = ", ".join(str(c) for c in candidates)
    raise FileNotFoundError(
        f"cetra-sql-diff binary not found (searched: {searched}); "
        f"build with `cargo build -p cetra-sql-diff`"
    )


def run_cetra_batch(
    cetra_bin: Path,
    tables: list[dict[str, Any]],
    queries: list[dict[str, Any]],
) -> dict[str, EngineResult]:
    """Executes one JSON bundle through `cetra-sql-diff` and parses JSONL responses."""
    if not queries:
        return {}
    bundle_json = json.dumps({"tables": tables, "queries": queries}, separators=(",", ":"))
    proc = subprocess.run(  # noqa: S603
        [str(cetra_bin)],
        input=bundle_json,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"cetra-sql-diff exited with code {proc.returncode}: {proc.stderr.strip()}"
        )
    results: dict[str, EngineResult] = {}
    for line in proc.stdout.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        payload: dict[str, Any] = json.loads(stripped)
        qid = str(payload["id"])
        results[qid] = cetra_json_to_result(payload)
    return results


def run_duckdb_query(conn: duckdb.DuckDBPyConnection, sql: str) -> EngineResult:
    """Runs one SQL query against DuckDB and captures either result rows or error."""
    try:
        cur = conn.execute(sql)
        rows = cur.fetchall()
        return duckdb_cursor_to_result(cur.description, rows)
    except duckdb.Error as err:
        return EngineResult(status="error", columns=[], rows=[], error_message=str(err))


# ---------------------------------------------------------------------------
# Adversarial multi-table dataset (plan § 2.1)
# ---------------------------------------------------------------------------


def _sql_quote(text: str) -> str:
    return "'" + text.replace("'", "''") + "'"


def _cell_to_duckdb_sql(val: Any, col_type: str) -> str:
    if val is None:
        return "NULL"
    t = col_type.upper()
    if t == "BOOL":
        return "TRUE" if val else "FALSE"
    if t == "I64":
        return str(int(val))
    if t == "F64":
        if isinstance(val, str):
            return f"CAST({_sql_quote(val)} AS DOUBLE)"
        return repr(float(val))
    if t.startswith("DECIMAL("):
        return f"CAST({_sql_quote(str(val))} AS {t})"
    if t == "TIMESTAMPTZ":
        return f"TIMESTAMPTZ {_sql_quote(str(val))}"
    if t == "TIMESTAMP":
        return f"TIMESTAMP_NS {_sql_quote(str(val))}"
    if t == "DATE":
        return f"DATE {_sql_quote(str(val))}"
    if t == "UTF8":
        return _sql_quote(str(val))
    if t == "LIST(UTF8)":
        items = ["NULL" if item is None else _sql_quote(str(item)) for item in val]
        return f"CAST([{', '.join(items)}] AS VARCHAR[])"
    if t == "MAP_UTF8":
        if not val:
            return "CAST(MAP([], []) AS MAP(VARCHAR, VARCHAR))"
        keys = [_sql_quote(str(k)) for k in val]
        vals = ["CAST(NULL AS VARCHAR)" if v is None else _sql_quote(str(v)) for v in val.values()]
        return f"CAST(MAP([{', '.join(keys)}], [{', '.join(vals)}]) AS MAP(VARCHAR, VARCHAR))"
    raise ValueError(f"unsupported dataset column type: {col_type}")


def build_adversarial_dataset(seed: int) -> list[dict[str, Any]]:
    """Constructs deterministic `t_main` (64 rows), `t_right` (32 rows), `t_empty` (0 rows)."""
    rng = SplitMix64(seed ^ 0x517CC1B727220A95)

    grp_pool: list[str | None] = [
        "alpha",
        "beta",
        "gamma",
        "alpha",
        "beta",
        "",
        "a'b",
        "a%b_c",
        "café",
        "UPPER",
        "  padded  ",
        "x\\y",
        None,
        None,
    ]
    k_small_pool: list[int | None] = [
        -1000,
        -42,
        -7,
        -1,
        0,
        0,
        1,
        2,
        5,
        7,
        42,
        100,
        1000,
        None,
        None,
    ]
    k_edge_pool: list[int | None] = [
        -9223372036854775808,
        9223372036854775807,
        -1,
        0,
        1,
        42,
        -1000000000000,
        None,
    ]
    x_pool: list[float | str | None] = [
        -0.0,
        0.0,
        1.2,
        -2.4,
        3.7,
        100.1,
        "NaN",
        "Infinity",
        "-Infinity",
        None,
    ]
    x_fin_pool: list[float | None] = [
        -0.0,
        0.0,
        1.2,
        -2.4,
        3.7,
        -10.3,
        25.8,
        100.1,
        0.25,
        -0.75,
        None,
    ]
    dec_pool: list[str | None] = [
        "-999.99",
        "-12.34",
        "-0.01",
        "0.00",
        "0.50",
        "12.34",
        "100.25",
        "500.00",
        None,
    ]
    flag_pool: list[bool | None] = [True, False, None]
    ts_pool: list[str | None] = [
        "2025-01-01T00:00:00.000000Z",
        "2025-01-01T01:30:15.250000Z",
        "2025-01-02T12:00:00.000000Z",
        "2025-01-03T23:59:59.500000Z",
        "2025-01-05T06:15:30.000000Z",
        "2024-02-29T18:45:00.125000Z",
        None,
    ]
    date_pool: list[str | None] = [
        "1970-01-01",
        "2024-02-29",
        "2025-01-01",
        "2025-01-02",
        "2025-06-15",
        None,
    ]
    tags_pool: list[list[str | None] | None] = [
        [],
        ["a"],
        ["a", "b"],
        ["a", "a", "c"],
        ["x", None, "y"],
        ["", "z"],
        None,
    ]
    meta_pool: list[dict[str, str | None] | None] = [
        {},
        {"env": "prod", "tier": "1"},
        {"env": "dev", "opt": None},
        {"env": "staging"},
        {"k": ""},
        None,
    ]

    main_columns = [
        {"name": "id", "type": "I64", "nullable": False},
        {"name": "grp", "type": "UTF8", "nullable": True},
        {"name": "k_small", "type": "I64", "nullable": True},
        {"name": "k_edge", "type": "I64", "nullable": True},
        {"name": "x", "type": "F64", "nullable": True},
        {"name": "x_fin", "type": "F64", "nullable": True},
        {"name": "dec", "type": "DECIMAL(10,2)", "nullable": True},
        {"name": "flag", "type": "BOOL", "nullable": True},
        {"name": "ts", "type": "TIMESTAMPTZ", "nullable": True},
        {"name": "d", "type": "DATE", "nullable": True},
        {"name": "tags", "type": "LIST(UTF8)", "nullable": True},
        {"name": "meta", "type": "MAP_UTF8", "nullable": True},
    ]

    main_rows: list[list[Any]] = []
    for row_id in range(64):
        main_rows.append(
            [
                row_id,
                rng.choice(grp_pool),
                rng.choice(k_small_pool),
                rng.choice(k_edge_pool),
                rng.choice(x_pool),
                rng.choice(x_fin_pool),
                rng.choice(dec_pool),
                rng.choice(flag_pool),
                rng.choice(ts_pool),
                rng.choice(date_pool),
                rng.choice(tags_pool),
                rng.choice(meta_pool),
            ]
        )

    right_columns = [
        {"name": "id", "type": "I64", "nullable": False},
        {"name": "grp", "type": "UTF8", "nullable": True},
        {"name": "k_small", "type": "I64", "nullable": True},
        {"name": "val", "type": "F64", "nullable": True},
        {"name": "flag", "type": "BOOL", "nullable": True},
    ]
    right_grp_pool: list[str | None] = [
        "alpha",
        "beta",
        "delta",
        "",
        "café",
        None,
    ]
    right_k_pool: list[int | None] = [-42, -1, 0, 1, 2, 7, 42, 999, None]
    right_rows: list[list[Any]] = []
    for idx in range(32):
        right_rows.append(
            [
                100 + idx,
                rng.choice(right_grp_pool),
                rng.choice(right_k_pool),
                rng.choice(x_fin_pool),
                rng.choice(flag_pool),
            ]
        )

    return [
        {"name": "t_main", "columns": main_columns, "rows": main_rows},
        {"name": "t_right", "columns": right_columns, "rows": right_rows},
        {"name": "t_empty", "columns": main_columns, "rows": []},
    ]


def load_dataset_into_duckdb(
    conn: duckdb.DuckDBPyConnection,
    tables: list[dict[str, Any]],
) -> None:
    """Creates and populates every table from `tables` in DuckDB with UTC timezone."""
    conn.execute(DUCKDB_INIT_SQL)
    type_map = {
        "BOOL": "BOOLEAN",
        "I64": "BIGINT",
        "F64": "DOUBLE",
        "UTF8": "VARCHAR",
        "TIMESTAMPTZ": "TIMESTAMPTZ",
        "TIMESTAMP": "TIMESTAMP_NS",
        "DATE": "DATE",
        "LIST(UTF8)": "VARCHAR[]",
        "MAP_UTF8": "MAP(VARCHAR, VARCHAR)",
    }
    for table in tables:
        col_defs: list[str] = []
        col_types: list[str] = []
        for col in table["columns"]:
            raw_t = str(col["type"]).upper()
            duck_t = type_map.get(raw_t, raw_t)
            null_s = "" if col.get("nullable", True) else " NOT NULL"
            col_defs.append(f"{col['name']} {duck_t}{null_s}")
            col_types.append(raw_t)
        conn.execute(f"CREATE TABLE {table['name']} ({', '.join(col_defs)})")
        rows: list[list[Any]] = table["rows"]
        if not rows:
            continue
        tuples_sql = [
            "("
            + ", ".join(
                _cell_to_duckdb_sql(cell, ct) for cell, ct in zip(row, col_types, strict=True)
            )
            + ")"
            for row in rows
        ]
        conn.execute(f"INSERT INTO {table['name']} VALUES {', '.join(tuples_sql)}")


# ---------------------------------------------------------------------------
# Seeded Shape B SQL generator
# ---------------------------------------------------------------------------


class SqlGenerator:
    """Generates diverse Shape B SQL queries adhering to DuckDB↔cetra dialect rules."""

    def __init__(self, seed: int) -> None:
        self.rng = SplitMix64(seed ^ 0x9E3779B97F4A7C15)

    def _pred(self, prefix: str = "") -> str:
        p = f"{prefix}." if prefix else ""
        kind = self.rng.randint(0, 15)
        if kind == 0:
            op = self.rng.choice(["=", "<>", "<", "<=", ">", ">="])
            val = self.rng.choice([-42, -1, 0, 1, 7, 42, 100])
            return f"{p}k_small {op} {val}"
        if kind == 1:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            col = self.rng.choice(["grp", "k_small", "flag", "dec", "x_fin", "ts", "d"])
            return f"{p}{col} IS {neg}NULL"
        if kind == 2:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            truth = self.rng.choice(["TRUE", "FALSE"])
            return f"{p}flag IS {neg}{truth}"
        if kind == 3:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            rhs = self.rng.choice(["0", "42", "NULL"])
            return f"{p}k_small IS {neg}DISTINCT FROM {rhs}"
        if kind == 4:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            return f"{p}k_small {neg}BETWEEN -10 AND 50"
        if kind == 5:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            items = self.rng.choice(["(0, 1, 42)", "(-42, 7, NULL)", "('alpha', 'beta', NULL)"])
            col = "grp" if "'alpha'" in items else "k_small"
            return f"{p}{col} {neg}IN {items}"
        if kind == 6:
            op = self.rng.choice(["LIKE", "ILIKE", "NOT LIKE", "NOT ILIKE"])
            pat = self.rng.choice(
                [
                    "'a%'",
                    "'%a%'",
                    "'a\\%b\\_c'",
                    "'_lpha'",
                    "'x\\\\y'",
                    "'%é'",
                ]
            )
            esc = " ESCAPE '\\'" if "\\" in pat else ""
            return f"{p}grp {op} {pat}{esc}"
        if kind == 7:
            return f"COALESCE({p}k_small, 0) > {self.rng.choice([-5, 0, 10])}"
        if kind == 8:
            return f"{p}x_fin >= {self.rng.choice(['-5.0', '0.0', '1.0'])}"
        if kind == 9:
            lit = self.rng.choice(["-10.00", "0.00", "12.34"])
            return f"{p}dec > CAST('{lit}' AS DECIMAL(10, 2))"
        if kind == 10:
            return f"{p}ts >= TIMESTAMPTZ '2025-01-02T00:00:00Z'"
        if kind == 11:
            return f"{p}d >= DATE '2025-01-01'"
        if kind == 12:
            return f"length({p}grp) >= {self.rng.choice([0, 3, 5])}"
        if kind == 13:
            return f"({p}flag OR {p}k_small > 0)"
        if kind == 14:
            return f"NOT ({p}flag AND {p}k_small < 0)"
        return f"array_length({p}tags) >= {self.rng.choice([0, 1, 2])}"

    def _compound_pred(self, prefix: str = "") -> str:
        left = self._pred(prefix)
        if self.rng.chance(1, 2):
            return left
        conn = self.rng.choice(["AND", "OR"])
        right = self._pred(prefix)
        if self.rng.chance(1, 4):
            return f"NOT (({left}) {conn} ({right}))"
        return f"({left}) {conn} ({right})"

    def _order_by(self, cols: list[str], tie_col: str = "id") -> str:
        terms: list[str] = []
        for col in cols:
            direction = self.rng.choice(["ASC", "DESC"])
            nulls = self.rng.choice(["NULLS FIRST", "NULLS LAST"])
            terms.append(f"{col} {direction} {nulls}")
        terms.append(f"{tie_col} ASC NULLS LAST")
        return "ORDER BY " + ", ".join(terms)

    def _gen_filter_and_case(self, qid: str) -> GeneratedQuery:
        tbl = self.rng.choice(["t_main", "t_main", "t_main", "t_empty"])
        pred = self._compound_pred()
        exprs = [
            "id AS c0",
            "grp AS c1",
            "CASE WHEN flag THEN k_small WHEN k_small IS NULL THEN -999 ELSE k_small + 1 END AS c2",
            "COALESCE(NULLIF(k_small, 0), -1) AS c3",
            "GREATEST(k_small, id, 0) AS c4",
            "LEAST(k_small, id, 10) AS c5",
            "meta AS c6",
        ]
        chosen = exprs[: self.rng.randint(3, len(exprs))]
        sql = f"SELECT {', '.join(chosen)} FROM {tbl} WHERE {pred}"
        ordered = False
        if self.rng.chance(1, 2):
            sql += " " + self._order_by([self.rng.choice(["grp", "k_small", "flag"])])
            ordered = True
        return GeneratedQuery(qid, "filter_case", sql, ordered=ordered, unordered_arrays=False)

    def _gen_scalar_functions(self, qid: str) -> GeneratedQuery:
        variant = self.rng.randint(0, 4)
        pred = self._compound_pred()
        if variant == 0:
            # String scalar functions
            sql = (
                "SELECT id AS c0, lower(grp) AS c1, upper(grp) AS c2, length(grp) AS c3, "
                "trim(grp) AS c4, ltrim(grp) AS c5, rtrim(grp) AS c6, "
                "left(grp, 3) AS c7, right(grp, 2) AS c8, "
                "substr(grp, 1, 3) AS c9, strpos(grp, 'a') AS c10, "
                "starts_with(grp, 'a') AS c11, ends_with(grp, 'a') AS c12, "
                "contains(grp, 'b') AS c13, split_part(grp, 'a', 1) AS c14, "
                "concat(grp, ':', COALESCE(CAST(k_small AS VARCHAR), 'null')) AS c15, "
                "concat_ws('-', grp, CAST(id AS VARCHAR)) AS c16 "
                f"FROM t_main WHERE {pred}"
            )
        elif variant == 1:
            # Numeric & float math functions (with safe denominators & trunc casts)
            sql = (
                "SELECT id AS c0, abs(k_small) AS c1, floor(x_fin) AS c2, ceil(x_fin) AS c3, "
                "round(x_fin, 1) AS c4, sqrt(abs(x_fin)) AS c5, "
                "ln(abs(x_fin) + 1.0) AS c6, log10(abs(x_fin) + 1.0) AS c7, "
                "power(2.0, CAST(abs(k_small % 4) AS DOUBLE)) AS c8, "
                "isnan(x) AS c9, isfinite(x) AS c10, "
                "CAST(floor(x_fin) AS BIGINT) AS c11, "
                "CAST(k_small AS DOUBLE) / NULLIF(CAST(k_small + 3 AS DOUBLE), 0.0) AS c12 "
                f"FROM t_main WHERE {pred}"
            )
        elif variant == 2:
            # Decimal arithmetic
            sql = (
                "SELECT id AS c0, dec AS c1, abs(dec) AS c2, "
                "dec + CAST('1.25' AS DECIMAL(10, 2)) AS c3, "
                "dec - CAST('0.50' AS DECIMAL(10, 2)) AS c4, "
                "dec * CAST('2.00' AS DECIMAL(10, 2)) AS c5 "
                f"FROM t_main WHERE {pred}"
            )
        elif variant == 3:
            # Temporal functions
            unit = self.rng.choice(["'day'", "'hour'", "'month'"])
            sql = (
                f"SELECT id AS c0, date_trunc({unit}, ts) AS c1, "
                "EXTRACT(YEAR FROM d) AS c2, EXTRACT(MONTH FROM d) AS c3, "
                "EXTRACT(DAY FROM d) AS c4, EXTRACT(HOUR FROM ts) AS c5, "
                "d AS c6, ts AS c7 "
                f"FROM t_main WHERE {pred}"
            )
        else:
            # Array scalar functions & edge i64 comparisons
            sql = (
                "SELECT id AS c0, array_length(tags) AS c1, "
                "array_to_string(tags, '|') AS c2, "
                "k_edge = 9223372036854775807 AS c3, "
                "k_edge = -9223372036854775808 AS c4, "
                "k_edge < 0 AS c5 "
                f"FROM t_main WHERE {pred}"
            )
        return GeneratedQuery(qid, "scalar_fns", sql, ordered=False, unordered_arrays=False)

    def _gen_join(self, qid: str) -> GeneratedQuery:
        join_kind = self.rng.choice(
            ["INNER JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL OUTER JOIN", "CROSS JOIN"]
        )
        right_tbl = self.rng.choice(["t_right", "t_right", "t_right", "t_empty"])
        if join_kind == "CROSS JOIN":
            sql = (
                "SELECT l.id AS c0, r.id AS c1, l.k_small AS c2, r.k_small AS c3 "
                "FROM (SELECT id, k_small FROM t_main WHERE id < 10) l "
                "CROSS JOIN (SELECT id, k_small FROM t_right WHERE id < 108) r"
            )
            return GeneratedQuery(qid, "join", sql, ordered=False, unordered_arrays=False)

        key_mode = self.rng.randint(0, 2)
        if key_mode == 0:
            on_cond = "l.k_small = r.k_small"
        elif key_mode == 1:
            on_cond = "l.grp = r.grp"
        else:
            on_cond = "l.k_small = r.k_small AND l.grp = r.grp"

        if self.rng.chance(1, 2):
            residual = self.rng.choice(
                [
                    "l.flag IS DISTINCT FROM r.flag",
                    "COALESCE(l.k_small, 0) >= 0",
                    "l.id < r.id",
                ]
            )
            on_cond = f"{on_cond} AND {residual}"

        where_clause = ""
        if self.rng.chance(1, 2):
            where_clause = " WHERE " + self.rng.choice(
                [
                    "l.id IS NULL OR r.id IS NULL",
                    "COALESCE(l.k_small, r.k_small, 0) >= 0",
                    "l.flag IS TRUE OR r.flag IS FALSE",
                ]
            )

        if right_tbl == "t_empty":
            sql = (
                f"SELECT l.id AS c0, r.id AS c1, l.grp AS c2, r.grp AS c3 "
                f"FROM t_main l {join_kind} t_empty r ON {on_cond}{where_clause}"
            )
        else:
            sql = (
                f"SELECT l.id AS c0, r.id AS c1, l.grp AS c2, r.val AS c3 "
                f"FROM t_main l {join_kind} t_right r ON {on_cond}{where_clause}"
            )
        return GeneratedQuery(qid, "join", sql, ordered=False, unordered_arrays=False)

    def _gen_self_join(self, qid: str) -> GeneratedQuery:
        join_kind = self.rng.choice(["INNER JOIN", "LEFT JOIN"])
        sql = (
            "SELECT a.id AS c0, b.id AS c1, a.k_small AS c2, b.grp AS c3 "
            f"FROM t_main a {join_kind} t_main b "
            "ON a.k_small = b.k_small AND a.id < b.id "
            "WHERE a.id < 20"
        )
        return GeneratedQuery(qid, "self_join", sql, ordered=False, unordered_arrays=False)

    def _gen_grouped_agg(self, qid: str) -> GeneratedQuery:
        tbl = self.rng.choice(["t_main", "t_main", "t_main", "t_empty"])
        grp_exprs = self.rng.choice(
            [
                ("grp", "grp AS g0"),
                ("flag", "flag AS g0"),
                ("d", "d AS g0"),
                ("grp, flag", "grp AS g0, flag AS g1"),
                ("COALESCE(grp, 'NULL_GRP')", "COALESCE(grp, 'NULL_GRP') AS g0"),
                ("date_trunc('day', ts)", "date_trunc('day', ts) AS g0"),
            ]
        )
        group_by_sql, group_select = grp_exprs
        aggs = [
            "COUNT(*) AS a0",
            "COUNT(k_small) AS a1",
            "COUNT(DISTINCT k_small) AS a2",
            "SUM(k_small) AS a3",
            "AVG(x_fin) AS a4",
            "MIN(k_small) AS a5",
            "MAX(dec) AS a6",
            "BOOL_OR(flag) AS a7",
            "BOOL_AND(flag) AS a8",
            "COUNT(*) FILTER (WHERE flag IS TRUE) AS a9",
            "SUM(k_small) FILTER (WHERE k_small > 0) AS a10",
            "SUM(dec) AS a11",
            "AVG(dec) AS a12",
            "percentile_cont(0.5) WITHIN GROUP (ORDER BY k_small) AS a13",
            "ARRAY_AGG(k_small) AS a14",
            "ARRAY_AGG(DISTINCT grp) AS a15",
        ]
        k = self.rng.randint(2, 5)
        picked: list[str] = []
        used: set[int] = set()
        while len(picked) < k:
            idx = self.rng.randint(0, len(aggs) - 1)
            if idx not in used:
                used.add(idx)
                picked.append(aggs[idx])

        where_sql = f" WHERE {self._pred()}" if self.rng.chance(1, 2) else ""
        having_sql = ""
        if self.rng.chance(1, 2):
            having_sql = " HAVING " + self.rng.choice(
                [
                    "COUNT(*) >= 2",
                    "SUM(k_small) IS NOT NULL",
                    "MIN(k_small) < 50",
                    "COUNT(DISTINCT k_small) > 1",
                ]
            )
        sql = (
            f"SELECT {group_select}, {', '.join(picked)} FROM {tbl}{where_sql} "
            f"GROUP BY {group_by_sql}{having_sql}"
        )
        return GeneratedQuery(
            qid,
            "grouped_agg",
            sql,
            ordered=False,
            unordered_arrays=has_array_agg(sql),
        )

    def _gen_global_agg(self, qid: str) -> GeneratedQuery:
        tbl = self.rng.choice(["t_main", "t_main", "t_empty"])
        where_sql = self.rng.choice(["", f" WHERE {self._pred()}", " WHERE 1 = 0"])
        sql = (
            "SELECT COUNT(*) AS c0, COUNT(k_small) AS c1, COUNT(DISTINCT grp) AS c2, "
            "SUM(k_small) AS c3, AVG(x_fin) AS c4, MIN(grp) AS c5, MAX(ts) AS c6, "
            "BOOL_OR(flag) AS c7, BOOL_AND(flag) AS c8, "
            "SUM(k_small) FILTER (WHERE flag IS NOT FALSE) AS c9, "
            "percentile_cont(0.25) WITHIN GROUP (ORDER BY x_fin) AS c10 "
            f"FROM {tbl}{where_sql}"
        )
        return GeneratedQuery(qid, "global_agg", sql, ordered=False, unordered_arrays=False)

    def _gen_setops_and_distinct(self, qid: str) -> GeneratedQuery:
        if self.rng.chance(1, 3):
            cols = self.rng.choice(
                [
                    "grp AS c0, flag AS c1",
                    "k_small AS c0, flag AS c1",
                    "d AS c0",
                    "dec AS c0, flag AS c1",
                ]
            )
            where_sql = f" WHERE {self._pred()}" if self.rng.chance(1, 2) else ""
            sql = f"SELECT DISTINCT {cols} FROM t_main{where_sql}"
            ordered = False
            if self.rng.chance(1, 2):
                if "c1" in cols:
                    sql += " ORDER BY c0 ASC NULLS LAST, c1 DESC NULLS FIRST"
                else:
                    sql += " ORDER BY c0 DESC NULLS FIRST"
                ordered = True
                if self.rng.chance(1, 2):
                    lim = self.rng.choice([0, 1, 5, 10])
                    off = self.rng.choice([0, 1, 2])
                    sql += f" LIMIT {lim} OFFSET {off}"
            return GeneratedQuery(qid, "distinct", sql, ordered=ordered, unordered_arrays=False)

        op = self.rng.choice(["UNION", "UNION ALL", "INTERSECT", "EXCEPT"])
        left_pred = self._pred()
        right_tbl = self.rng.choice(["t_right", "t_main", "t_empty"])
        right_where = f" WHERE {self._pred()}" if right_tbl == "t_main" else ""
        sql = (
            f"SELECT grp AS c0, k_small AS c1 FROM t_main WHERE {left_pred} "
            f"{op} "
            f"SELECT grp AS c0, k_small AS c1 FROM {right_tbl}{right_where}"
        )
        return GeneratedQuery(qid, "setop", sql, ordered=False, unordered_arrays=False)

    def _gen_window(self, qid: str) -> GeneratedQuery:
        part = self.rng.choice(["PARTITION BY grp ", "PARTITION BY flag ", ""])
        variant = self.rng.randint(0, 3)
        if variant == 0:
            # Deterministic tie-broken order for row-position windows
            order = "ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST"
            sql = (
                f"SELECT id AS c0, grp AS c1, "
                f"ROW_NUMBER() OVER ({part}{order}) AS c2, "
                f"LAG(k_small, 1) OVER ({part}{order}) AS c3, "
                f"LAG(k_small, 2, -99) OVER ({part}{order}) AS c4, "
                f"LEAD(grp, 1) OVER ({part}{order}) AS c5, "
                f"FIRST_VALUE(k_small) OVER ({part}{order}) AS c6, "
                f"LAST_VALUE(k_small) OVER ({part}{order} "
                "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c7 "
                "FROM t_main"
            )
        elif variant == 1:
            # Peer-aware RANGE windows (no id tie-breaker, peer-invariant funcs only)
            order = "ORDER BY k_small DESC NULLS FIRST"
            sql = (
                f"SELECT id AS c0, k_small AS c1, "
                f"RANK() OVER ({part}{order}) AS c2, "
                f"DENSE_RANK() OVER ({part}{order}) AS c3, "
                f"SUM(k_small) OVER ({part}{order} "
                "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c4, "
                f"COUNT(*) OVER ({part}{order} "
                "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c5 "
                "FROM t_main"
            )
        elif variant == 2:
            # Bounded ROWS frame
            order = "ORDER BY k_small ASC NULLS FIRST, id ASC NULLS LAST"
            frame = self.rng.choice(
                [
                    "ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
                    "ROWS BETWEEN 2 PRECEDING AND CURRENT ROW",
                    "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
                    "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
                ]
            )
            sql = (
                f"SELECT id AS c0, "
                f"SUM(k_small) OVER ({part}{order} {frame}) AS c1, "
                f"COUNT(k_small) OVER ({part}{order} {frame}) AS c2, "
                f"MIN(k_small) OVER ({part}{order} {frame}) AS c3, "
                f"MAX(k_small) OVER ({part}{order} {frame}) AS c4 "
                "FROM t_main"
            )
        else:
            # Multi-window query with two distinct OVER specs
            sql = (
                "SELECT id AS c0, "
                "ROW_NUMBER() OVER (PARTITION BY grp "
                "ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST) AS c1, "
                "SUM(k_small) OVER (PARTITION BY flag ORDER BY id ASC NULLS LAST "
                "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c2 "
                "FROM t_main"
            )
        return GeneratedQuery(qid, "window", sql, ordered=False, unordered_arrays=False)

    def _gen_order_limit(self, qid: str) -> GeneratedQuery:
        order_cols = self.rng.choice(
            [
                ["k_small"],
                ["grp", "k_small"],
                ["flag", "dec"],
                ["ts", "grp"],
                ["d", "x_fin"],
            ]
        )
        where_sql = f" WHERE {self._pred()}" if self.rng.chance(1, 2) else ""
        limit_val = self.rng.choice([0, 1, 5, 12, 100])
        offset_val = self.rng.choice([0, 0, 2, 10, 70])
        order_sql = self._order_by(order_cols)
        sql = (
            f"SELECT id AS c0, grp AS c1, k_small AS c2, dec AS c3, flag AS c4 "
            f"FROM t_main{where_sql} {order_sql} LIMIT {limit_val} OFFSET {offset_val}"
        )
        return GeneratedQuery(qid, "order_limit", sql, ordered=True, unordered_arrays=False)

    def _gen_cte_and_derived(self, qid: str) -> GeneratedQuery:
        if self.rng.chance(1, 2):
            pred1 = self._pred()
            sql = (
                f"WITH base AS (SELECT id, grp, k_small, flag FROM t_main WHERE {pred1}), "
                "agg AS (SELECT grp, COUNT(*) AS cnt, SUM(k_small) AS total "
                "FROM base GROUP BY grp) "
                "SELECT a.grp AS c0, a.cnt AS c1, a.total AS c2 "
                "FROM agg a WHERE a.cnt >= 1"
            )
        else:
            pred1 = self._pred()
            sql = (
                "SELECT sub.grp AS c0, COUNT(*) AS c1, MAX(sub.k_small) AS c2 "
                f"FROM (SELECT id, grp, k_small FROM t_main WHERE {pred1}) sub "
                "GROUP BY sub.grp"
            )
        return GeneratedQuery(qid, "cte", sql, ordered=False, unordered_arrays=False)

    def _gen_subquery(self, qid: str) -> GeneratedQuery:
        variant = self.rng.randint(0, 4)
        if variant == 0:
            rhs_filter = self.rng.choice(["", " WHERE k_small IS NOT NULL", " WHERE flag IS TRUE"])
            sql = (
                "SELECT id AS c0, k_small AS c1 FROM t_main "
                f"WHERE k_small IN (SELECT k_small FROM t_right{rhs_filter})"
            )
        elif variant == 1:
            rhs_filter = self.rng.choice(
                [
                    "",
                    " WHERE k_small IS NOT NULL",
                    " WHERE k_small > 5000",
                    " WHERE flag IS TRUE AND k_small IS NOT NULL",
                ]
            )
            sql = (
                "SELECT id AS c0, k_small AS c1 FROM t_main "
                f"WHERE k_small NOT IN (SELECT k_small FROM t_right{rhs_filter})"
            )
        elif variant == 2:
            neg = "NOT " if self.rng.chance(1, 2) else ""
            sub_tbl = self.rng.choice(["t_right", "t_empty"])
            sub_where = self.rng.choice(["", " WHERE k_small > 10", " WHERE k_small > 99999"])
            sql = (
                "SELECT id AS c0, grp AS c1 FROM t_main "
                f"WHERE {neg}EXISTS (SELECT 1 FROM {sub_tbl}{sub_where})"
            )
        elif variant == 3:
            sql = (
                "SELECT grp AS c0, COUNT(*) AS c1 FROM t_main "
                "GROUP BY grp "
                "HAVING grp IN (SELECT grp FROM t_right WHERE grp IS NOT NULL)"
            )
        else:
            sql = (
                "SELECT grp AS c0, SUM(k_small) AS c1 FROM t_main "
                "WHERE k_small IS NOT NULL "
                "GROUP BY grp "
                "HAVING EXISTS (SELECT 1 FROM t_right WHERE flag IS TRUE)"
            )
        return GeneratedQuery(qid, "subquery", sql, ordered=False, unordered_arrays=False)

    def generate(self, count: int) -> list[GeneratedQuery]:
        """Generates `count` diverse Shape B SQL queries."""
        generators = [
            self._gen_filter_and_case,
            self._gen_scalar_functions,
            self._gen_join,
            self._gen_self_join,
            self._gen_grouped_agg,
            self._gen_global_agg,
            self._gen_setops_and_distinct,
            self._gen_window,
            self._gen_order_limit,
            self._gen_cte_and_derived,
            self._gen_subquery,
        ]
        out: list[GeneratedQuery] = []
        for idx in range(count):
            gen_fn = self.rng.choice(generators)
            out.append(gen_fn(f"gen_{idx}"))
        return out


# ---------------------------------------------------------------------------
# Mode 1: Curated .slt cross-runner
# ---------------------------------------------------------------------------


def _snapshot_duckdb_tables(
    conn: duckdb.DuckDBPyConnection,
    tables_meta: dict[str, list[tuple[str, str, bool]]],
) -> list[dict[str, Any]]:
    """Extracts every active table from DuckDB into `cetra-sql-diff` JSON wire format."""
    out: list[dict[str, Any]] = []
    for table_name, cols in tables_meta.items():
        raw_rows = conn.execute(f"SELECT * FROM {table_name}").fetchall()
        wire_cols = [
            {"name": col_name, "type": wire_type, "nullable": nullable}
            for col_name, wire_type, nullable in cols
        ]
        wire_rows = [
            [
                duckdb_value_to_cetra_json(cell, wire_type)
                for cell, (_, wire_type, _) in zip(row, cols, strict=True)
            ]
            for row in raw_rows
        ]
        out.append({"name": table_name, "columns": wire_cols, "rows": wire_rows})
    return out


@dataclass(frozen=True)
class _PendingSltQuery:
    """One queued `.slt` query or expected-error statement awaiting batched execution."""

    query_id: str
    sql: str
    line_no: int
    ordered: bool
    unordered_arrays: bool
    kind: str
    expected_error: str = ""


def _run_single_slt_file(
    cetra_bin: Path,
    slt_path: Path,
    partitions: int,
    max_failures: int,
    stats: RunStats,
) -> None:
    """Executes all non-skipped entries in one `.slt` file against DuckDB and `cetra-sql-diff`."""
    entries = parse_slt_file(slt_path.read_text(encoding="utf-8"))
    conn = duckdb.connect(":memory:")
    try:
        conn.execute(DUCKDB_INIT_SQL)
        tables_meta: dict[str, list[tuple[str, str, bool]]] = {}
        pending_queries: list[_PendingSltQuery] = []

        def flush_pending() -> None:
            if not pending_queries:
                return
            wire_tables = _snapshot_duckdb_tables(conn, tables_meta)
            bundle_queries = [
                {
                    "id": pq.query_id,
                    "sql": pq.sql,
                    "limit": DEFAULT_LIMIT,
                    "memory_cap_bytes": DEFAULT_MEMORY_CAP,
                    "target_partitions": partitions,
                }
                for pq in pending_queries
            ]
            cetra_results = run_cetra_batch(cetra_bin, wire_tables, bundle_queries)
            for pq in pending_queries:
                cetra_res = cetra_results.get(pq.query_id)
                if cetra_res is None:
                    stats.add_failure(
                        f"[SLT {slt_path.name}:{pq.line_no}] missing cetra output for "
                        f"{pq.query_id}",
                        max_failures,
                    )
                    continue
                if pq.kind == "statement_error":
                    if cetra_res.status == "leak":
                        stats.add_failure(
                            f"[SLT {slt_path.name}:{pq.line_no}] LEAK on error SQL: {pq.sql}",
                            max_failures,
                        )
                    elif cetra_res.status == "error" and cetra_res.error_kind in FATAL_ERROR_KINDS:
                        stats.add_failure(
                            f"[SLT {slt_path.name}:{pq.line_no}] fatal error "
                            f"({cetra_res.error_kind}: {cetra_res.error_message}) "
                            f"on SQL: {pq.sql}",
                            max_failures,
                        )
                    elif cetra_res.status != "error":
                        stats.add_failure(
                            f"[SLT {slt_path.name}:{pq.line_no}] expected error, got ok: {pq.sql}",
                            max_failures,
                        )
                    elif (
                        pq.expected_error
                        and pq.expected_error.lower() not in (cetra_res.error_message or "").lower()
                    ):
                        stats.add_failure(
                            f"[SLT {slt_path.name}:{pq.line_no}] error message mismatch: "
                            f"expected substring {pq.expected_error!r}, "
                            f"got {cetra_res.error_message!r}\n  SQL: {pq.sql}",
                            max_failures,
                        )
                    else:
                        stats.expected_errors += 1
                    continue

                duck_sql = rewrite_sql_for_duckdb(pq.sql)
                duck_res = run_duckdb_query(conn, duck_sql)
                diff = compare_query_results(
                    cetra_res,
                    duck_res,
                    ordered=pq.ordered,
                    unordered_arrays=pq.unordered_arrays,
                )
                if diff is not None:
                    stats.add_failure(
                        f"[SLT {slt_path.name}:{pq.line_no}] {diff}\n  SQL: {pq.sql}",
                        max_failures,
                    )
                elif cetra_res.status == "error":
                    stats.expected_errors += 1
                else:
                    stats.passed += 1
            pending_queries.clear()

        for idx, entry in enumerate(entries):
            if entry.duckdb_skip is not None:
                stats.skipped += 1
                continue
            if entry.kind == "statement_ok":
                flush_pending()
                drop_name = parse_drop_table_name(entry.sql)
                if drop_name is not None:
                    conn.execute(entry.sql)
                    tables_meta.pop(drop_name, None)
                elif entry.sql.lstrip().upper().startswith("CREATE TABLE"):
                    tname, duck_ddl, cetra_cols = translate_create_table_for_duckdb(entry.sql)
                    conn.execute(duck_ddl)
                    tables_meta[tname] = cetra_cols
                else:
                    conn.execute(rewrite_slt_insert_for_duckdb(entry.sql))
            elif entry.kind == "statement_error":
                qid = f"slt_{slt_path.stem}_{idx}"
                pending_queries.append(
                    _PendingSltQuery(
                        query_id=qid,
                        sql=entry.sql,
                        line_no=entry.line_no,
                        ordered=False,
                        unordered_arrays=False,
                        kind=entry.kind,
                        expected_error=entry.expected_error,
                    )
                )
            elif entry.kind == "query":
                qid = f"slt_{slt_path.stem}_{idx}"
                ordered = entry.sort_mode != "rowsort" and has_top_level_order_by(entry.sql)
                unordered_arrays = entry.unordered_arrays or has_array_agg(entry.sql)
                pending_queries.append(
                    _PendingSltQuery(
                        query_id=qid,
                        sql=entry.sql,
                        line_no=entry.line_no,
                        ordered=ordered,
                        unordered_arrays=unordered_arrays,
                        kind=entry.kind,
                    )
                )
        flush_pending()
    finally:
        conn.close()


def run_slt_suite(
    cetra_bin: Path,
    slt_dir: Path,
    partitions: int,
    max_failures: int,
    stats: RunStats,
) -> None:
    """Cross-runs all non-skipped queries from `crates/query/tests/slt/*.slt`."""
    if not slt_dir.is_dir():
        stats.add_failure(f"[SLT] directory does not exist: {slt_dir}", max_failures)
        return
    slt_files = sorted(slt_dir.glob("*.slt"))
    if not slt_files:
        stats.add_failure(f"[SLT] no .slt files found in {slt_dir}", max_failures)
        return
    for slt_path in slt_files:
        _run_single_slt_file(cetra_bin, slt_path, partitions, max_failures, stats)


# ---------------------------------------------------------------------------
# Mode 2: Generated differential campaign
# ---------------------------------------------------------------------------


def run_generated_suite(
    cetra_bin: Path,
    cases: int,
    seed: int,
    partitions: int,
    max_failures: int,
    stats: RunStats,
) -> None:
    """Generates `cases` seeded Shape B queries and diffs them against DuckDB."""
    tables = build_adversarial_dataset(seed)
    gen = SqlGenerator(seed)
    queries = gen.generate(cases)

    conn = duckdb.connect(":memory:")
    try:
        load_dataset_into_duckdb(conn, tables)

        for start in range(0, len(queries), BATCH_SIZE):
            chunk = queries[start : start + BATCH_SIZE]
            bundle_queries = [
                {
                    "id": q.query_id,
                    "sql": q.sql,
                    "limit": DEFAULT_LIMIT,
                    "memory_cap_bytes": DEFAULT_MEMORY_CAP,
                    "target_partitions": partitions,
                }
                for q in chunk
            ]
            cetra_results = run_cetra_batch(cetra_bin, tables, bundle_queries)
            for q in chunk:
                cetra_res = cetra_results.get(q.query_id)
                if cetra_res is None:
                    stats.add_failure(
                        f"[{q.query_id} ({q.family})] missing cetra-sql-diff output\n"
                        f"  SQL: {q.sql}",
                        max_failures,
                    )
                    continue
                duck_res = run_duckdb_query(conn, q.sql)
                diff = compare_query_results(
                    cetra_res,
                    duck_res,
                    ordered=q.ordered,
                    unordered_arrays=q.unordered_arrays,
                )
                if diff is not None:
                    stats.add_failure(
                        f"[{q.query_id} ({q.family}, seed={seed})] {diff}\n  SQL: {q.sql}",
                        max_failures,
                    )
                elif cetra_res.status == "error":
                    stats.expected_errors += 1
                else:
                    stats.passed += 1
    finally:
        conn.close()


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Differential SQL verifier comparing cetra-query against DuckDB."
    )
    parser.add_argument(
        "--cases",
        type=int,
        default=2000,
        help="Number of generated Shape B SQL queries to execute (default: 2000).",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=0,
        help="Deterministic PRNG seed for dataset and query generation (default: 0).",
    )
    parser.add_argument(
        "--slt",
        action="store_true",
        help="Also cross-run non-skipped queries from crates/query/tests/slt/*.slt.",
    )
    parser.add_argument(
        "--slt-only",
        action="store_true",
        help="Only run crates/query/tests/slt/*.slt cross-checks (skip generated queries).",
    )
    parser.add_argument(
        "--cetra-bin",
        type=str,
        default=None,
        help="Path to cetra-sql-diff binary (auto-detected under target/ if omitted).",
    )
    parser.add_argument(
        "--partitions",
        type=int,
        default=1,
        help="target_partitions passed to cetra-sql-diff (default: 1).",
    )
    parser.add_argument(
        "--max-failures",
        type=int,
        default=20,
        help="Maximum failure details to print (default: 20).",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    cetra_bin = resolve_cetra_bin(args.cetra_bin)
    stats = RunStats()

    if args.slt or args.slt_only:
        slt_dir = find_repo_root() / "crates" / "query" / "tests" / "slt"
        run_slt_suite(cetra_bin, slt_dir, args.partitions, args.max_failures, stats)

    if not args.slt_only and args.cases > 0:
        run_generated_suite(
            cetra_bin,
            args.cases,
            args.seed,
            args.partitions,
            args.max_failures,
            stats,
        )

    sys.stdout.write(
        f"sql-diff summary: passed={stats.passed} failed={stats.failed} "
        f"expected_errors={stats.expected_errors} skipped={stats.skipped}\n"
    )
    if stats.failures:
        for failure in stats.failures:
            sys.stderr.write(f"\nFAIL: {failure}\n")
        if stats.failed > len(stats.failures):
            omitted = stats.failed - len(stats.failures)
            sys.stderr.write(f"\n... and {omitted} more failures (increase --max-failures)\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
