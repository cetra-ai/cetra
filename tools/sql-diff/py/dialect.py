"""Dialect rules, DDL translation, `.slt` parsing, and result comparison between
`cetra-query` and DuckDB.

Every intentional semantic difference between DuckDB and `cetra-query` (plan § 3.3) is
handled explicitly here so differential testing produces zero false-positive noise:
1. UTC timezone on every DuckDB connection; `TIMESTAMP` -> `TIMESTAMP_NS`; microsecond
   `TIMESTAMPTZ` inputs.
2. Explicit `NULLS FIRST` / `NULLS LAST` on every generated `ORDER BY` term.
3. Explicit `ESCAPE '\\'` on `LIKE` / `ILIKE` with backslashes.
4. Safe denominators (`NULLIF(..., 0)`), float division for `/` (or `//` for integer div).
5. Avoid fractional `CAST(F64 AS BIGINT)` (or use `CAST(trunc(x) AS BIGINT)`).
6. Bounded `k_small` for `SUM(BIGINT)` to avoid checked `I64` vs `HUGEINT` overflow.
7. Unordered multiset comparison when no top-level `ORDER BY`; sorted `ARRAY_AGG` elements
   when `unordered_arrays=True`; unique top-level aliases.
8. Float/Decimal/Temporal/Interval/Binary/Map normalization (DuckDB `NaN`/`±Inf` -> `None`,
   finite floats within `rel_tol=1e-9, abs_tol=1e-12`, numeric `Decimal` equality, UTC ISO
   timestamps, lowercase hex bytes, normalized maps).
9. `.slt` DDL type translation (`I64` -> `BIGINT`, `F64` -> `DOUBLE`, `FIXEDBYTES8`/`16` ->
   `BLOB`, `TIMESTAMP` -> `TIMESTAMP_NS`, `MAP(VARCHAR, VARCHAR)` -> `MAP(VARCHAR, VARCHAR)`).
"""

from __future__ import annotations

import datetime
import math
import re
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
from typing import Any

DUCKDB_INIT_SQL = "SET TimeZone = 'UTC';"
FLOAT_REL_TOL = 1e-9
FLOAT_ABS_TOL = 1e-12
FATAL_ERROR_KINDS = frozenset({"internal", "too_large", "cancelled", "busy"})
EXPECTED_ERROR_KINDS = frozenset({"invalid", "invalid_request", "query_failed"})

_CREATE_TABLE_RE = re.compile(
    r"^\s*CREATE\s+TABLE\s+([A-Za-z_][A-Za-z0-9_]*)\s*\((.*)\)\s*;?\s*$",
    re.IGNORECASE | re.DOTALL,
)
_DROP_TABLE_RE = re.compile(
    r"^\s*DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*;?\s*$",
    re.IGNORECASE,
)
_DECIMAL_TYPE_RE = re.compile(
    r"^(?:DECIMAL|NUMERIC)\s*\(\s*(\d+)\s*,\s*(-?\d+)\s*\)$",
    re.IGNORECASE,
)
_LIST_TYPE_RE = re.compile(r"^LIST\s*\((.*)\)$", re.IGNORECASE)
_MAP_TYPE_RE = re.compile(r"^MAP\s*\(.*\)$", re.IGNORECASE)
_HEX_LITERAL_RE = re.compile(r"\bX'([0-9A-Fa-f]*)'", re.IGNORECASE)
_ISO_TS_RE = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?(?:Z|[+-]00:00)?$"
)
_INTERVAL_PART_RE = re.compile(
    r"([+-]?\d+(?:\.\d+)?)\s*(years?|mons?|months?|days?|hours?|mins?|minutes?|secs?|seconds?)",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class EngineResult:
    """Normalized query execution result from either `cetra-sql-diff` or DuckDB."""

    status: str
    columns: list[tuple[str, str]]
    rows: list[list[Any]]
    truncated: bool = False
    error_kind: str | None = None
    error_message: str | None = None
    reserved_bytes: int = 0
    spilled_bytes: int = 0


@dataclass(frozen=True)
class SltEntry:
    """One parsed record from a curated `.slt` file."""

    kind: str  # "statement_ok" | "statement_error" | "query"
    sql: str
    line_no: int
    type_sig: str = ""
    sort_mode: str = "nosort"
    expected_error: str = ""
    duckdb_skip: str | None = None
    unordered_arrays: bool = False


def cetra_json_to_result(payload: dict[str, Any]) -> EngineResult:
    """Converts one JSONL output object from `cetra-sql-diff` into an `EngineResult`."""
    status = str(payload.get("status", "error"))
    if status == "leak":
        return EngineResult(
            status="leak",
            columns=[],
            rows=[],
            reserved_bytes=int(payload.get("reserved_bytes", 0)),
            spilled_bytes=int(payload.get("spilled_bytes", 0)),
            error_message=f"leak after {payload.get('outcome', 'unknown')}",
        )
    if status == "error":
        return EngineResult(
            status="error",
            columns=[],
            rows=[],
            error_kind=str(payload.get("kind", "unknown")),
            error_message=str(payload.get("message", "")),
        )
    raw_cols: list[dict[str, Any]] = payload.get("columns", [])
    columns = [(str(col["name"]), str(col["type"])) for col in raw_cols]
    raw_rows: list[dict[str, Any]] = payload.get("rows", [])
    rows: list[list[Any]] = []
    for row_idx, row in enumerate(raw_rows):
        row_vals: list[Any] = []
        for col_name, _ in columns:
            if col_name not in row:
                return EngineResult(
                    status="error",
                    columns=[],
                    rows=[],
                    error_kind="internal",
                    error_message=(
                        f"malformed cetra-sql-diff JSON: row {row_idx} missing column key "
                        f"{col_name!r}"
                    ),
                )
            row_vals.append(row[col_name])
        rows.append(row_vals)
    return EngineResult(
        status="ok",
        columns=columns,
        rows=rows,
        truncated=bool(payload.get("truncated", False)),
    )


def duckdb_cursor_to_result(description: Any, raw_rows: list[tuple[Any, ...]]) -> EngineResult:
    """Converts DuckDB cursor description and fetched tuples into an `EngineResult`."""
    columns: list[tuple[str, str]] = []
    if description:
        for desc in description:
            col_name = str(desc[0])
            col_type = str(desc[1]) if len(desc) > 1 and desc[1] is not None else "unknown"
            columns.append((col_name, col_type))
    rows = [list(row) for row in raw_rows]
    return EngineResult(status="ok", columns=columns, rows=rows)


def _split_top_level(text: str, sep: str = ",") -> list[str]:
    """Splits `text` on `sep` outside quotes and parentheses."""
    parts: list[str] = []
    buf: list[str] = []
    depth = 0
    in_single = False
    in_double = False
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if in_single:
            buf.append(ch)
            if ch == "'":
                if i + 1 < n and text[i + 1] == "'":
                    buf.append(text[i + 1])
                    i += 1
                else:
                    in_single = False
        elif in_double:
            buf.append(ch)
            if ch == '"':
                if i + 1 < n and text[i + 1] == '"':
                    buf.append(text[i + 1])
                    i += 1
                else:
                    in_double = False
        elif ch == "'":
            in_single = True
            buf.append(ch)
        elif ch == '"':
            in_double = True
            buf.append(ch)
        elif ch == "(":
            depth += 1
            buf.append(ch)
        elif ch == ")":
            depth = max(0, depth - 1)
            buf.append(ch)
        elif ch == sep and depth == 0:
            parts.append("".join(buf).strip())
            buf = []
        else:
            buf.append(ch)
        i += 1
    tail = "".join(buf).strip()
    if tail:
        parts.append(tail)
    return parts


def translate_slt_type_for_duckdb(raw_type: str) -> str:
    """Translates a `.slt` / `cetra-column` DDL type name into a DuckDB SQL type."""
    t = raw_type.strip()
    if t.endswith("[]"):
        return f"{translate_slt_type_for_duckdb(t[:-2])}[]"
    list_m = _LIST_TYPE_RE.match(t)
    if list_m:
        return f"{translate_slt_type_for_duckdb(list_m.group(1))}[]"
    dec_m = _DECIMAL_TYPE_RE.match(t)
    if dec_m:
        return f"DECIMAL({dec_m.group(1)}, {dec_m.group(2)})"
    if _MAP_TYPE_RE.match(t):
        return "MAP(VARCHAR, VARCHAR)"
    upper = t.upper()
    mapping = {
        "BOOL": "BOOLEAN",
        "BOOLEAN": "BOOLEAN",
        "I64": "BIGINT",
        "INT64": "BIGINT",
        "BIGINT": "BIGINT",
        "INT": "BIGINT",
        "INTEGER": "BIGINT",
        "F64": "DOUBLE",
        "FLOAT64": "DOUBLE",
        "DOUBLE": "DOUBLE",
        "FLOAT": "DOUBLE",
        "UTF8": "VARCHAR",
        "VARCHAR": "VARCHAR",
        "TEXT": "VARCHAR",
        "STRING": "VARCHAR",
        "BYTEA": "BLOB",
        "BINARY": "BLOB",
        "BLOB": "BLOB",
        "FIXEDBYTES8": "BLOB",
        "FIXEDBYTES16": "BLOB",
        "TIMESTAMP": "TIMESTAMP_NS",
        "TIMESTAMPTZ": "TIMESTAMPTZ",
        "DATE": "DATE",
        "INTERVAL": "INTERVAL",
        "MAP_UTF8": "MAP(VARCHAR, VARCHAR)",
    }
    return mapping.get(upper, t)


def translate_slt_type_for_cetra_bundle(raw_type: str) -> str:
    """Translates a `.slt` column DDL type into a `cetra-sql-diff` JSON wire type."""
    t = raw_type.strip()
    if t.endswith("[]"):
        inner = translate_slt_type_for_cetra_bundle(t[:-2])
        return f"LIST({inner})"
    list_m = _LIST_TYPE_RE.match(t)
    if list_m:
        inner = translate_slt_type_for_cetra_bundle(list_m.group(1))
        return f"LIST({inner})"
    dec_m = _DECIMAL_TYPE_RE.match(t)
    if dec_m:
        return f"DECIMAL({dec_m.group(1)},{dec_m.group(2)})"
    if _MAP_TYPE_RE.match(t):
        return "MAP_UTF8"
    upper = t.upper()
    mapping = {
        "BOOL": "BOOL",
        "BOOLEAN": "BOOL",
        "I64": "I64",
        "INT64": "I64",
        "BIGINT": "I64",
        "INT": "I64",
        "INTEGER": "I64",
        "F64": "F64",
        "FLOAT64": "F64",
        "DOUBLE": "F64",
        "FLOAT": "F64",
        "UTF8": "UTF8",
        "VARCHAR": "UTF8",
        "TEXT": "UTF8",
        "STRING": "UTF8",
        "BYTEA": "BINARY",
        "BINARY": "BINARY",
        "BLOB": "BINARY",
        "FIXEDBYTES8": "FIXEDBYTES8",
        "FIXEDBYTES16": "FIXEDBYTES16",
        "TIMESTAMP": "TIMESTAMP",
        "TIMESTAMPTZ": "TIMESTAMPTZ",
        "DATE": "DATE",
        "INTERVAL": "INTERVAL",
        "MAP_UTF8": "MAP_UTF8",
    }
    if upper not in mapping:
        raise ValueError(f"unsupported .slt column type: {raw_type}")
    return mapping[upper]


def translate_create_table_for_duckdb(
    sql: str,
) -> tuple[str, str, list[tuple[str, str, bool]]]:
    """Parses `.slt` `CREATE TABLE` and returns `(table_name, duckdb_ddl, cetra_columns)`.

    `cetra_columns` is a list of `(col_name, cetra_wire_type, nullable)`.
    """
    match = _CREATE_TABLE_RE.match(sql)
    if not match:
        raise ValueError(f"not a CREATE TABLE statement: {sql}")
    table_name = match.group(1).lower()
    body = match.group(2)
    col_defs = _split_top_level(body, ",")
    duck_cols: list[str] = []
    cetra_cols: list[tuple[str, str, bool]] = []
    for col_def in col_defs:
        parts = col_def.strip().split(None, 1)
        if len(parts) < 2:
            raise ValueError(f"malformed column definition: {col_def}")
        col_name = parts[0].strip('"')
        rest = parts[1].strip()
        nullable = True
        not_null_m = re.search(r"\bNOT\s+NULL\s*$", rest, re.IGNORECASE)
        if not_null_m:
            nullable = False
            rest = rest[: not_null_m.start()].strip()
        else:
            null_m = re.search(r"\bNULL\s*$", rest, re.IGNORECASE)
            if null_m:
                rest = rest[: null_m.start()].strip()
        duck_type = translate_slt_type_for_duckdb(rest)
        cetra_type = translate_slt_type_for_cetra_bundle(rest)
        null_suffix = " NOT NULL" if not nullable else ""
        duck_cols.append(f"{col_name} {duck_type}{null_suffix}")
        cetra_cols.append((col_name, cetra_type, nullable))
    duck_ddl = f"CREATE TABLE {table_name} ({', '.join(duck_cols)})"
    return table_name, duck_ddl, cetra_cols


def parse_drop_table_name(sql: str) -> str | None:
    """Returns canonical lowercase table name if `sql` is a `DROP TABLE` statement."""
    match = _DROP_TABLE_RE.match(sql)
    return match.group(1).lower() if match else None


def _rewrite_map_calls_for_duckdb(sql: str) -> str:
    """Rewrites `.slt` `MAP('k1', 'v1', ...)` calls to DuckDB `MAP(['k1', ...], ['v1', ...])`."""
    out: list[str] = []
    i = 0
    n = len(sql)
    in_single = False
    while i < n:
        ch = sql[i]
        if in_single:
            out.append(ch)
            if ch == "'":
                if i + 1 < n and sql[i + 1] == "'":
                    out.append(sql[i + 1])
                    i += 1
                else:
                    in_single = False
            i += 1
            continue
        if ch == "'":
            in_single = True
            out.append(ch)
            i += 1
            continue
        if (
            sql[i : i + 3].upper() == "MAP"
            and (i == 0 or not (sql[i - 1].isalnum() or sql[i - 1] == "_"))
            and i + 3 < n
        ):
            j = i + 3
            while j < n and sql[j].isspace():
                j += 1
            if j < n and sql[j] == "(":
                depth = 1
                k = j + 1
                inner_single = False
                while k < n and depth > 0:
                    c = sql[k]
                    if inner_single:
                        if c == "'":
                            if k + 1 < n and sql[k + 1] == "'":
                                k += 1
                            else:
                                inner_single = False
                    elif c == "'":
                        inner_single = True
                    elif c == "(":
                        depth += 1
                    elif c == ")":
                        depth -= 1
                    k += 1
                inner = sql[j + 1 : k - 1].strip()
                if not inner:
                    out.append("CAST(MAP([], []) AS MAP(VARCHAR, VARCHAR))")
                    i = k
                    continue
                args = _split_top_level(inner, ",")
                if not args[0].startswith("[") and len(args) % 2 == 0:
                    keys = args[0::2]
                    vals = [
                        "CAST(NULL AS VARCHAR)" if v.strip().upper() == "NULL" else v
                        for v in args[1::2]
                    ]
                    out.append(
                        f"CAST(MAP([{', '.join(keys)}], [{', '.join(vals)}]) "
                        f"AS MAP(VARCHAR, VARCHAR))"
                    )
                    i = k
                    continue
        out.append(ch)
        i += 1
    return "".join(out)


def rewrite_slt_insert_for_duckdb(sql: str) -> str:
    """Rewrites `.slt` `INSERT` literals (`MAP(...)`, `X'...'`) for DuckDB."""
    rewritten = _rewrite_map_calls_for_duckdb(sql)
    return _HEX_LITERAL_RE.sub(lambda m: f"from_hex('{m.group(1)}')", rewritten)


def rewrite_sql_for_duckdb(sql: str) -> str:
    """Applies minimal syntax translations needed for DuckDB execution of `.slt` queries."""
    return _HEX_LITERAL_RE.sub(lambda m: f"from_hex('{m.group(1)}')", sql)


def parse_slt_file(text: str) -> list[SltEntry]:
    """Parses a curated `.slt` file into `SltEntry` records, preserving DuckDB directives."""
    lines = text.splitlines()
    entries: list[SltEntry] = []
    i = 0
    n = len(lines)
    pending_skip: str | None = None
    pending_unordered_arrays = False

    while i < n:
        stripped = lines[i].strip()
        if not stripped:
            pending_skip = None
            pending_unordered_arrays = False
            i += 1
            continue
        if stripped.startswith("#"):
            comment = stripped[1:].strip()
            low = comment.lower()
            if low.startswith("duckdb: skip"):
                reason = comment[len("duckdb: skip") :].strip() or "skipped"
                pending_skip = reason
            elif low.startswith("unordered_arrays"):
                pending_unordered_arrays = True
            i += 1
            continue

        if stripped.startswith("statement "):
            header = stripped[len("statement ") :].strip()
            start_line = i + 1
            i += 1
            sql_lines: list[str] = []
            while i < n and lines[i].strip() != "":
                sql_lines.append(lines[i])
                i += 1
            sql = "\n".join(sql_lines).strip()
            if header == "ok":
                entries.append(
                    SltEntry(
                        kind="statement_ok",
                        sql=sql,
                        line_no=start_line,
                        duckdb_skip=pending_skip,
                        unordered_arrays=pending_unordered_arrays,
                    )
                )
            elif header.startswith("error"):
                expected = header[len("error") :].strip()
                entries.append(
                    SltEntry(
                        kind="statement_error",
                        sql=sql,
                        line_no=start_line,
                        expected_error=expected,
                        duckdb_skip=pending_skip,
                        unordered_arrays=pending_unordered_arrays,
                    )
                )
            pending_skip = None
            pending_unordered_arrays = False
            continue

        if stripped.startswith("query "):
            parts = stripped.split()
            type_sig = parts[1] if len(parts) > 1 else ""
            sort_mode = parts[2] if len(parts) > 2 else "nosort"
            start_line = i + 1
            i += 1
            sql_lines = []
            while i < n and lines[i].strip() not in ("----", ""):
                sql_lines.append(lines[i])
                i += 1
            if i < n and lines[i].strip() == "----":
                i += 1
                while i < n and lines[i].strip() != "":
                    i += 1
            sql = "\n".join(sql_lines).strip()
            entries.append(
                SltEntry(
                    kind="query",
                    sql=sql,
                    line_no=start_line,
                    type_sig=type_sig,
                    sort_mode=sort_mode,
                    duckdb_skip=pending_skip,
                    unordered_arrays=pending_unordered_arrays,
                )
            )
            pending_skip = None
            pending_unordered_arrays = False
            continue

        i += 1

    return entries


def has_top_level_order_by(sql: str) -> bool:
    """Returns True if `sql` has a top-level `ORDER BY` outside quotes and parentheses."""
    depth = 0
    in_single = False
    in_double = False
    tokens: list[str] = []
    buf: list[str] = []
    i = 0
    n = len(sql)
    while i < n:
        ch = sql[i]
        if in_single:
            if ch == "'":
                if i + 1 < n and sql[i + 1] == "'":
                    i += 1
                else:
                    in_single = False
        elif in_double:
            if ch == '"':
                if i + 1 < n and sql[i + 1] == '"':
                    i += 1
                else:
                    in_double = False
        elif ch == "'":
            if buf:
                tokens.append("".join(buf).upper())
                buf = []
            in_single = True
        elif ch == '"':
            if buf:
                tokens.append("".join(buf).upper())
                buf = []
            in_double = True
        elif ch == "(":
            if buf:
                tokens.append("".join(buf).upper())
                buf = []
            depth += 1
        elif ch == ")":
            if buf:
                tokens.append("".join(buf).upper())
                buf = []
            depth = max(0, depth - 1)
        elif depth == 0:
            if ch.isalnum() or ch == "_":
                buf.append(ch)
            elif buf:
                tokens.append("".join(buf).upper())
                buf = []
        i += 1
    if buf and depth == 0:
        tokens.append("".join(buf).upper())
    limit = len(tokens) - 1
    return any(tokens[idx] == "ORDER" and tokens[idx + 1] == "BY" for idx in range(limit))


def has_array_agg(sql: str) -> bool:
    """Returns True when `sql` invokes `array_agg` (case-insensitive)."""
    return bool(re.search(r"\barray_agg\s*\(", sql, re.IGNORECASE))


def _parse_iso_timestamp_us(text: str) -> int | None:
    """Parses an ISO-8601 UTC/naive timestamp string to integer microseconds since epoch."""
    match = _ISO_TS_RE.match(text.strip())
    if not match:
        return None
    year = int(match.group(1))
    month = int(match.group(2))
    day = int(match.group(3))
    hour = int(match.group(4))
    minute = int(match.group(5))
    second = int(match.group(6))
    frac = match.group(7) or ""
    micros = int(frac[:6].ljust(6, "0"))
    dt = datetime.datetime(year, month, day, hour, minute, second, micros, tzinfo=datetime.UTC)
    epoch = datetime.datetime(1970, 1, 1, tzinfo=datetime.UTC)
    delta = dt - epoch
    return (delta.days * 86_400 + delta.seconds) * 1_000_000 + delta.microseconds


def _trunc_nanos_to_us(nanos: int) -> int:
    """Truncates integer nanoseconds to microseconds toward zero without float precision loss."""
    return -(abs(nanos) // 1_000) if nanos < 0 else nanos // 1_000


def _interval_canonical(val: Any) -> tuple[int, int] | None:
    """Converts a cetra interval string or Python timedelta/relativedelta into
    `(months, total_us)` without folding months into days."""
    if isinstance(val, datetime.timedelta):
        total_us = (val.days * 86_400 + val.seconds) * 1_000_000 + val.microseconds
        return (0, total_us)
    if isinstance(val, dict) and {"months", "days"}.issubset(val.keys()):
        months = int(val.get("years", 0)) * 12 + int(val["months"])
        days = int(val["days"])
        micros = int(val.get("micros", 0)) + _trunc_nanos_to_us(int(val.get("nanos", 0)))
        return (months, days * 86_400_000_000 + micros)
    if hasattr(val, "months") and hasattr(val, "days"):
        months = int(getattr(val, "years", 0)) * 12 + int(val.months)
        days = int(val.days)
        micros = int(getattr(val, "microseconds", 0))
        seconds = int(getattr(val, "seconds", 0))
        minutes = int(getattr(val, "minutes", 0))
        hours = int(getattr(val, "hours", 0))
        time_us = (((hours * 60 + minutes) * 60 + seconds) * 1_000_000) + micros
        return (months, days * 86_400_000_000 + time_us)
    if isinstance(val, str):
        months = 0
        days = 0
        total_us = 0
        matched = False
        for m in _INTERVAL_PART_RE.finditer(val):
            matched = True
            num_str = m.group(1)
            unit = m.group(2).lower()
            if unit.startswith("year"):
                months += int(Decimal(num_str)) * 12
            elif unit.startswith("mon"):
                months += int(Decimal(num_str))
            elif unit.startswith("day"):
                days += int(Decimal(num_str))
            elif unit.startswith("hour"):
                total_us += int(Decimal(num_str) * 3_600_000_000)
            elif unit.startswith("min"):
                total_us += int(Decimal(num_str) * 60_000_000)
            elif unit.startswith("sec"):
                total_us += int(Decimal(num_str) * 1_000_000)
        if matched:
            return (months, days * 86_400_000_000 + total_us)
    return None


def _normalize_map(val: Any) -> dict[str, str | None] | None:
    """Normalizes a cetra or DuckDB map value into `dict[str, str | None]`."""
    if val is None:
        return None
    if isinstance(val, dict):
        if (
            set(val.keys()) == {"key", "value"}
            and isinstance(val["key"], list)
            and isinstance(val["value"], list)
            and len(val["key"]) == len(val["value"])
        ):
            return {
                str(k): None if v is None else str(v)
                for k, v in zip(val["key"], val["value"], strict=True)
            }
        return {str(k): None if v is None else str(v) for k, v in val.items()}
    return None


def normalize_cell(val: Any, col_type: str, *, unordered_arrays: bool = False) -> Any:
    """Normalizes one cell from either `cetra-query` or DuckDB for comparison."""
    if val is None:
        return None

    if isinstance(val, float) and not math.isfinite(val):
        return None

    if col_type == "boolean":
        return val

    if col_type == "integer":
        if isinstance(val, bool):
            return val
        if isinstance(val, int):
            return val
        if isinstance(val, Decimal) and val == val.to_integral_value():
            return int(val)
        return val

    if col_type == "float":
        if isinstance(val, bool):
            return val
        if isinstance(val, float | Decimal):
            fval = float(val)
            if not math.isfinite(fval):
                return None
            return 0.0 if fval == 0.0 else fval
        return val

    if col_type == "decimal":
        if isinstance(val, float):
            if not math.isfinite(val):
                return None
            return 0.0 if val == 0.0 else val
        if isinstance(val, Decimal):
            return val
        if isinstance(val, str):
            try:
                return Decimal(val)
            except InvalidOperation:
                return val
        return val

    if col_type == "timestamp":
        if isinstance(val, datetime.datetime):
            dt = (
                val.astimezone(datetime.UTC)
                if val.tzinfo is not None
                else val.replace(tzinfo=datetime.UTC)
            )
            epoch = datetime.datetime(1970, 1, 1, tzinfo=datetime.UTC)
            delta = dt - epoch
            return (delta.days * 86_400 + delta.seconds) * 1_000_000 + delta.microseconds
        if isinstance(val, str):
            parsed = _parse_iso_timestamp_us(val)
            if parsed is not None:
                return parsed
        return val

    if col_type == "date":
        if isinstance(val, datetime.datetime):
            return val.date().isoformat()
        if isinstance(val, datetime.date):
            return val.isoformat()
        return val

    if col_type == "interval":
        iv = _interval_canonical(val)
        return iv if iv is not None else val

    if col_type == "binary":
        if isinstance(val, bytes | bytearray | memoryview):
            return bytes(val).hex().lower()
        if isinstance(val, str):
            return val.lower()
        return val

    if col_type in ("string_list", "list"):
        if isinstance(val, list | tuple):
            elem_type = "string" if col_type == "string_list" else "unknown"
            items = [
                normalize_cell(item, elem_type, unordered_arrays=unordered_arrays) for item in val
            ]
            if unordered_arrays:
                items.sort(key=canonical_sort_key)
            return items
        return val

    if col_type == "string_map":
        normalized = _normalize_map(val)
        return normalized if normalized is not None else val

    if col_type == "string":
        return val

    # Generic fallback for nested list items with unknown element type
    if isinstance(val, datetime.datetime):
        return normalize_cell(val, "timestamp")
    if isinstance(val, datetime.date):
        return val.isoformat()
    if isinstance(val, bytes | bytearray | memoryview):
        return bytes(val).hex().lower()
    if isinstance(val, list | tuple):
        items = [normalize_cell(item, "unknown", unordered_arrays=unordered_arrays) for item in val]
        if unordered_arrays:
            items.sort(key=canonical_sort_key)
        return items
    if isinstance(val, float):
        return 0.0 if val == 0.0 else val
    return val


def _decimal_scale(val: Decimal) -> int:
    exp = val.as_tuple().exponent
    if isinstance(exp, int):
        return max(0, -exp)
    return 0


def cells_equal(a: Any, b: Any, col_type: str) -> bool:
    """Compares two already-normalized cells for semantic equivalence."""
    if a is None and b is None:
        return True
    if a is None or b is None:
        return False

    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a is b

    if col_type == "integer":
        if isinstance(a, int) and isinstance(b, int):
            return a == b
        if isinstance(a, int) and isinstance(b, float):
            return math.isfinite(b) and b.is_integer() and a == int(b)
        if isinstance(a, float) and isinstance(b, int):
            return math.isfinite(a) and a.is_integer() and int(a) == b
        return False

    if col_type == "decimal":
        if isinstance(a, Decimal) and isinstance(b, Decimal):
            return a == b
        if isinstance(a, Decimal) and isinstance(b, float):
            dec_scale = _decimal_scale(a)
            abs_tol = (
                max(FLOAT_ABS_TOL, 0.5 * (10.0 ** (-dec_scale)))
                if dec_scale >= 4
                else FLOAT_ABS_TOL
            )
            return math.isclose(float(a), b, rel_tol=FLOAT_REL_TOL, abs_tol=abs_tol)
        if isinstance(a, float) and isinstance(b, Decimal):
            dec_scale = _decimal_scale(b)
            abs_tol = (
                max(FLOAT_ABS_TOL, 0.5 * (10.0 ** (-dec_scale)))
                if dec_scale >= 4
                else FLOAT_ABS_TOL
            )
            return math.isclose(a, float(b), rel_tol=FLOAT_REL_TOL, abs_tol=abs_tol)
        return False

    if col_type == "float":
        if isinstance(a, float) and isinstance(b, float):
            return math.isclose(a, b, rel_tol=FLOAT_REL_TOL, abs_tol=FLOAT_ABS_TOL)
        return False

    if isinstance(a, float) and isinstance(b, float):
        return math.isclose(a, b, rel_tol=FLOAT_REL_TOL, abs_tol=FLOAT_ABS_TOL)

    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            return False
        elem_type = "string" if col_type == "string_list" else "unknown"
        return all(cells_equal(x, y, elem_type) for x, y in zip(a, b, strict=True))

    if isinstance(a, dict) and isinstance(b, dict):
        return a == b

    if type(a) is not type(b):
        return False

    return bool(a == b)


def canonical_sort_key(val: Any) -> tuple[int, Any]:
    """Total order key for sorting heterogeneous normalized cells / rows."""
    if val is None:
        return (0, 0)
    if isinstance(val, bool):
        return (1, int(val))
    if isinstance(val, int):
        return (2, float(val))
    if isinstance(val, float):
        rounded = 0.0 if abs(val) < FLOAT_ABS_TOL else float(f"{val:.7g}")
        return (2, rounded)
    if isinstance(val, Decimal):
        fval = float(val)
        rounded = 0.0 if abs(fval) < FLOAT_ABS_TOL else float(f"{fval:.7g}")
        return (2, rounded)
    if isinstance(val, str):
        return (3, val)
    if isinstance(val, list):
        return (4, tuple(canonical_sort_key(item) for item in val))
    if isinstance(val, dict):
        return (5, tuple(sorted((k, canonical_sort_key(v)) for k, v in val.items())))
    if isinstance(val, tuple):
        return (6, tuple(canonical_sort_key(item) for item in val))
    return (7, str(val))


def _rows_equal(row_a: list[Any], row_b: list[Any], col_types: list[str]) -> bool:
    return all(cells_equal(a, b, ct) for a, b, ct in zip(row_a, row_b, col_types, strict=True))


def compare_query_results(
    cetra_res: EngineResult,
    duckdb_res: EngineResult,
    *,
    ordered: bool,
    unordered_arrays: bool = False,
) -> str | None:
    """Compares `cetra_res` and `duckdb_res`.

    Returns `None` when equivalent, or a concise human-readable mismatch description.
    """
    if cetra_res.status == "leak":
        return (
            f"MEMORY/SPILL LEAK in cetra-query: reserved={cetra_res.reserved_bytes}B, "
            f"spilled={cetra_res.spilled_bytes}B ({cetra_res.error_message})"
        )

    if cetra_res.status == "error" and cetra_res.error_kind in FATAL_ERROR_KINDS:
        if cetra_res.error_kind == "internal":
            return f"QueryError::Internal returned by cetra-query: {cetra_res.error_message}"
        return (
            f"unexpected resource/lifecycle error ({cetra_res.error_kind}) "
            f"returned by cetra-query: {cetra_res.error_message}"
        )

    if (
        cetra_res.status == "error"
        and duckdb_res.status == "error"
        and cetra_res.error_kind in EXPECTED_ERROR_KINDS
    ):
        return None

    if cetra_res.status == "error":
        return (
            f"cetra-query errored ({cetra_res.error_kind}: {cetra_res.error_message}) "
            f"while DuckDB succeeded with {len(duckdb_res.rows)} rows"
        )

    if duckdb_res.status == "error":
        return (
            f"DuckDB errored ({duckdb_res.error_message}) while cetra-query succeeded "
            f"with {len(cetra_res.rows)} rows"
        )

    if cetra_res.truncated:
        return (
            f"cetra-query returned truncated=True (rows={len(cetra_res.rows)}, "
            f"duckdb_rows={len(duckdb_res.rows)}); unexpected truncation"
        )

    if len(cetra_res.columns) != len(duckdb_res.columns):
        return (
            f"column count mismatch: cetra={len(cetra_res.columns)} "
            f"vs duckdb={len(duckdb_res.columns)}"
        )

    if len(cetra_res.rows) != len(duckdb_res.rows):
        return f"row count mismatch: cetra={len(cetra_res.rows)} vs duckdb={len(duckdb_res.rows)}"

    col_types = [col_type for _, col_type in cetra_res.columns]
    norm_cetra = [
        [
            normalize_cell(cell, ct, unordered_arrays=unordered_arrays)
            for cell, ct in zip(row, col_types, strict=True)
        ]
        for row in cetra_res.rows
    ]
    norm_duck = [
        [
            normalize_cell(cell, ct, unordered_arrays=unordered_arrays)
            for cell, ct in zip(row, col_types, strict=True)
        ]
        for row in duckdb_res.rows
    ]

    if ordered:
        for idx, (ra, rb) in enumerate(zip(norm_cetra, norm_duck, strict=True)):
            if not _rows_equal(ra, rb, col_types):
                return f"ordered row {idx} mismatch:\n  cetra : {ra}\n  duckdb: {rb}"
        return None

    sorted_cetra = sorted(norm_cetra, key=lambda r: tuple(canonical_sort_key(c) for c in r))
    sorted_duck = sorted(norm_duck, key=lambda r: tuple(canonical_sort_key(c) for c in r))

    unmatched_cetra: list[list[Any]] = []
    unmatched_duck: list[list[Any]] = []
    for ra, rb in zip(sorted_cetra, sorted_duck, strict=True):
        if not _rows_equal(ra, rb, col_types):
            unmatched_cetra.append(ra)
            unmatched_duck.append(rb)

    if not unmatched_cetra:
        return None

    # Resolve tie-breaking jitter among nearly-equal float rows in unordered multisets.
    remaining_duck = list(unmatched_duck)
    still_unmatched: list[list[Any]] = []
    for ra in unmatched_cetra:
        found_idx: int | None = None
        for idx, rb in enumerate(remaining_duck):
            if _rows_equal(ra, rb, col_types):
                found_idx = idx
                break
        if found_idx is not None:
            remaining_duck.pop(found_idx)
        else:
            still_unmatched.append(ra)

    if not still_unmatched and not remaining_duck:
        return None

    sample_c = still_unmatched[0] if still_unmatched else None
    sample_d = remaining_duck[0] if remaining_duck else None
    return (
        f"unordered multiset mismatch ({len(still_unmatched)} differing rows):\n"
        f"  cetra : {sample_c}\n"
        f"  duckdb: {sample_d}"
    )


def duckdb_value_to_cetra_json(val: Any, cetra_wire_type: str) -> Any:
    """Converts a Python value returned by DuckDB into a `cetra-sql-diff` JSON wire cell."""
    if val is None:
        return None
    wire = cetra_wire_type.strip().upper()
    if wire.startswith("LIST(") and wire.endswith(")"):
        inner = cetra_wire_type.strip()[5:-1]
        return [duckdb_value_to_cetra_json(item, inner) for item in val]
    if wire == "MAP_UTF8":
        return _normalize_map(val)
    if wire == "BOOL":
        return bool(val)
    if wire == "I64":
        return int(val)
    if wire == "F64":
        fval = float(val)
        if math.isnan(fval):
            return "NaN"
        if math.isinf(fval):
            return "Infinity" if fval > 0 else "-Infinity"
        return fval
    if wire.startswith("DECIMAL("):
        if isinstance(val, Decimal):
            return format(val, "f")
        return str(val)
    if wire in ("TIMESTAMP", "TIMESTAMPTZ"):
        if isinstance(val, datetime.datetime):
            if wire == "TIMESTAMPTZ":
                dt = (
                    val.astimezone(datetime.UTC)
                    if val.tzinfo is not None
                    else val.replace(tzinfo=datetime.UTC)
                )
                return dt.strftime("%Y-%m-%dT%H:%M:%S.%fZ")
            dt_naive = (
                val.astimezone(datetime.UTC).replace(tzinfo=None) if val.tzinfo is not None else val
            )
            return dt_naive.strftime("%Y-%m-%dT%H:%M:%S.%f")
        return str(val)
    if wire == "DATE":
        if isinstance(val, datetime.datetime):
            return val.date().isoformat()
        if isinstance(val, datetime.date):
            return val.isoformat()
        return str(val)
    if wire == "INTERVAL":
        if isinstance(val, datetime.timedelta):
            days = val.days
            if days < 0 and (val.seconds > 0 or val.microseconds > 0):
                days += 1
                neg_us = (86_400 - val.seconds) * 1_000_000 - val.microseconds
                nanos = -neg_us * 1_000
            else:
                nanos = (val.seconds * 1_000_000 + val.microseconds) * 1_000
            return {"months": 0, "days": days, "nanos": nanos}
        if isinstance(val, dict) and {"months", "days"}.issubset(val.keys()):
            months = int(val.get("years", 0)) * 12 + int(val["months"])
            nanos = int(val.get("nanos", int(val.get("micros", 0)) * 1_000))
            return {"months": months, "days": int(val["days"]), "nanos": nanos}
        if hasattr(val, "months") and hasattr(val, "days"):
            months = int(getattr(val, "years", 0)) * 12 + int(val.months)
            hours = int(getattr(val, "hours", 0))
            minutes = int(getattr(val, "minutes", 0))
            seconds = int(getattr(val, "seconds", 0))
            micros = int(getattr(val, "microseconds", 0))
            nanos = (((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros) * 1_000
            return {"months": months, "days": int(val.days), "nanos": nanos}
        raise ValueError(f"unsupported DuckDB interval value: {val!r}")
    if wire in ("BINARY", "FIXEDBYTES8", "FIXEDBYTES16"):
        if isinstance(val, bytes | bytearray | memoryview):
            return bytes(val).hex().lower()
        return str(val)
    return str(val)
