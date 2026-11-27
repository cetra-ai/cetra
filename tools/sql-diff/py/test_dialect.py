"""Unit tests for DuckDB ↔ `cetra-query` dialect translation and result comparison."""

from __future__ import annotations

import datetime
import unittest
from dataclasses import dataclass
from decimal import Decimal

from dialect import (
    EngineResult,
    cetra_json_to_result,
    compare_query_results,
    duckdb_value_to_cetra_json,
    has_array_agg,
    has_top_level_order_by,
    normalize_cell,
    parse_drop_table_name,
    parse_slt_file,
    rewrite_slt_insert_for_duckdb,
    translate_create_table_for_duckdb,
    translate_slt_type_for_cetra_bundle,
    translate_slt_type_for_duckdb,
)


@dataclass(frozen=True)
class _FakeRelativeDelta:
    years: int = 0
    months: int = 0
    days: int = 0
    hours: int = 0
    minutes: int = 0
    seconds: int = 0
    microseconds: int = 0


class TestDdlAndSltParsing(unittest.TestCase):
    def test_ddl_type_translation(self) -> None:
        self.assertEqual(translate_slt_type_for_duckdb("I64"), "BIGINT")
        self.assertEqual(translate_slt_type_for_duckdb("F64"), "DOUBLE")
        self.assertEqual(translate_slt_type_for_duckdb("FIXEDBYTES8"), "BLOB")
        self.assertEqual(translate_slt_type_for_duckdb("FIXEDBYTES16"), "BLOB")
        self.assertEqual(translate_slt_type_for_duckdb("TIMESTAMP"), "TIMESTAMP_NS")
        self.assertEqual(translate_slt_type_for_duckdb("TIMESTAMPTZ"), "TIMESTAMPTZ")
        self.assertEqual(translate_slt_type_for_duckdb("VARCHAR[]"), "VARCHAR[]")
        self.assertEqual(translate_slt_type_for_duckdb("DECIMAL(10, 2)"), "DECIMAL(10, 2)")
        self.assertEqual(
            translate_slt_type_for_duckdb("MAP(VARCHAR, VARCHAR)"),
            "MAP(VARCHAR, VARCHAR)",
        )

        self.assertEqual(translate_slt_type_for_cetra_bundle("BIGINT"), "I64")
        self.assertEqual(translate_slt_type_for_cetra_bundle("DOUBLE"), "F64")
        self.assertEqual(translate_slt_type_for_cetra_bundle("VARCHAR[]"), "LIST(UTF8)")
        self.assertEqual(translate_slt_type_for_cetra_bundle("DECIMAL(10, 2)"), "DECIMAL(10,2)")
        self.assertEqual(translate_slt_type_for_cetra_bundle("MAP(VARCHAR, VARCHAR)"), "MAP_UTF8")

    def test_create_and_drop_table_translation(self) -> None:
        ddl = (
            "CREATE TABLE t_mix (id I64 NOT NULL, d DECIMAL(10, 2), "
            "ts TIMESTAMP, tid FIXEDBYTES16, m MAP(VARCHAR, VARCHAR), tags TEXT[])"
        )
        name, duck_ddl, cetra_cols = translate_create_table_for_duckdb(ddl)
        self.assertEqual(name, "t_mix")
        self.assertIn("id BIGINT NOT NULL", duck_ddl)
        self.assertIn("d DECIMAL(10, 2)", duck_ddl)
        self.assertIn("ts TIMESTAMP_NS", duck_ddl)
        self.assertIn("tid BLOB", duck_ddl)
        self.assertIn("m MAP(VARCHAR, VARCHAR)", duck_ddl)
        self.assertIn("tags VARCHAR[]", duck_ddl)
        self.assertEqual(
            cetra_cols,
            [
                ("id", "I64", False),
                ("d", "DECIMAL(10,2)", True),
                ("ts", "TIMESTAMP", True),
                ("tid", "FIXEDBYTES16", True),
                ("m", "MAP_UTF8", True),
                ("tags", "LIST(UTF8)", True),
            ],
        )
        self.assertEqual(parse_drop_table_name("DROP TABLE IF EXISTS T_MIX;"), "t_mix")
        self.assertIsNone(parse_drop_table_name("SELECT 1"))

    def test_slt_insert_rewrite_for_duckdb(self) -> None:
        sql = "INSERT INTO t VALUES (1, MAP('a', '1', 'b', NULL), MAP(), X'DEADBEEF')"
        rewritten = rewrite_slt_insert_for_duckdb(sql)
        self.assertIn("CAST(MAP(['a', 'b'], ['1', CAST(NULL AS VARCHAR)])", rewritten)
        self.assertIn("CAST(MAP([], []) AS MAP(VARCHAR, VARCHAR))", rewritten)
        self.assertIn("from_hex('DEADBEEF')", rewritten)

    def test_slt_directive_parsing(self) -> None:
        slt = (
            "# Regular comment\n"
            "statement ok\n"
            "CREATE TABLE t (id BIGINT NOT NULL)\n\n"
            "# duckdb: skip cetra-only encode\n"
            "query T nosort\n"
            "SELECT encode('hi', 'hex') AS c0\n"
            "----\n"
            "6869\n\n"
            "# unordered_arrays\n"
            "query T rowsort\n"
            "SELECT array_agg(id) AS c0 FROM t\n"
            "----\n"
            "[1]\n\n"
            "statement error division by zero\n"
            "SELECT 1 / 0 AS c0\n"
        )
        entries = parse_slt_file(slt)
        self.assertEqual(len(entries), 4)
        self.assertEqual(entries[0].kind, "statement_ok")
        self.assertIsNone(entries[0].duckdb_skip)
        self.assertEqual(entries[1].kind, "query")
        self.assertEqual(entries[1].duckdb_skip, "cetra-only encode")
        self.assertFalse(entries[1].unordered_arrays)
        self.assertEqual(entries[2].kind, "query")
        self.assertIsNone(entries[2].duckdb_skip)
        self.assertTrue(entries[2].unordered_arrays)
        self.assertEqual(entries[2].sort_mode, "rowsort")
        self.assertEqual(entries[3].kind, "statement_error")
        self.assertEqual(entries[3].expected_error, "division by zero")

    def test_slt_blank_line_resets_directives(self) -> None:
        slt = (
            "# duckdb: skip stale skip before blank line\n"
            "# unordered_arrays\n\n"
            "query I nosort\n"
            "SELECT 1 AS c0\n"
            "----\n"
            "1\n"
        )
        entries = parse_slt_file(slt)
        self.assertEqual(len(entries), 1)
        self.assertIsNone(entries[0].duckdb_skip)
        self.assertFalse(entries[0].unordered_arrays)

    def test_top_level_order_by_and_array_agg_detection(self) -> None:
        self.assertTrue(has_top_level_order_by("SELECT id FROM t ORDER BY id ASC NULLS LAST"))
        self.assertFalse(
            has_top_level_order_by("SELECT ROW_NUMBER() OVER (ORDER BY id) AS rn FROM t")
        )
        self.assertFalse(has_top_level_order_by("SELECT 'ORDER BY' AS s FROM t"))
        self.assertFalse(
            has_top_level_order_by("SELECT sub.id FROM (SELECT id FROM t ORDER BY id) sub")
        )
        self.assertTrue(has_array_agg("SELECT ARRAY_AGG(k_small) AS a FROM t"))
        self.assertFalse(has_array_agg("SELECT array_length(tags) AS n FROM t"))


class TestNormalizationAndComparison(unittest.TestCase):
    def test_nan_inf_none_and_float_tolerance(self) -> None:
        cetra = EngineResult(
            status="ok",
            columns=[("a", "float"), ("b", "float"), ("c", "float"), ("d", "float")],
            rows=[[None, None, None, 1.0000000001]],
        )
        duck = EngineResult(
            status="ok",
            columns=[("a", "DOUBLE"), ("b", "DOUBLE"), ("c", "DOUBLE"), ("d", "DOUBLE")],
            rows=[[float("nan"), float("inf"), float("-inf"), 1.0]],
        )
        self.assertIsNone(compare_query_results(cetra, duck, ordered=True))

        duck_far = EngineResult(
            status="ok",
            columns=[("a", "DOUBLE"), ("b", "DOUBLE"), ("c", "DOUBLE"), ("d", "DOUBLE")],
            rows=[[float("nan"), float("inf"), float("-inf"), 1.01]],
        )
        self.assertIsNotNone(compare_query_results(cetra, duck_far, ordered=True))

    def test_decimal_exact_and_float_avg_equality(self) -> None:
        cetra = EngineResult(
            status="ok",
            columns=[("s", "decimal"), ("a", "decimal")],
            rows=[["12.30", "4.500000"]],
        )
        duck = EngineResult(
            status="ok",
            columns=[("s", "DECIMAL"), ("a", "DOUBLE")],
            rows=[[Decimal("12.3"), 4.5]],
        )
        self.assertIsNone(compare_query_results(cetra, duck, ordered=True))

        # Exact Decimal vs Decimal: off-by-1 integer and off-by-1-cent must fail.
        cetra_int_dec = EngineResult(status="ok", columns=[("d", "decimal")], rows=[["10"]])
        duck_int_dec = EngineResult(status="ok", columns=[("d", "DECIMAL")], rows=[[Decimal("11")]])
        self.assertIsNotNone(compare_query_results(cetra_int_dec, duck_int_dec, ordered=True))

        cetra_cent = EngineResult(status="ok", columns=[("d", "decimal")], rows=[["1.00"]])
        duck_cent = EngineResult(status="ok", columns=[("d", "DECIMAL")], rows=[[Decimal("1.01")]])
        self.assertIsNotNone(compare_query_results(cetra_cent, duck_cent, ordered=True))

        # AVG(Decimal) scale >= 4 vs DuckDB float accepts half-ULP rounding at dec_scale.
        cetra_avg = EngineResult(status="ok", columns=[("q", "decimal")], rows=[["0.333333"]])
        duck_avg = EngineResult(status="ok", columns=[("q", "DOUBLE")], rows=[[1.0 / 3.0]])
        self.assertIsNone(compare_query_results(cetra_avg, duck_avg, ordered=True))

        # Low-scale Decimal vs float must not use a loose 10**(-scale) tolerance.
        cetra_low = EngineResult(status="ok", columns=[("q", "decimal")], rows=[["1.00"]])
        duck_low = EngineResult(status="ok", columns=[("q", "DOUBLE")], rows=[[1.004]])
        self.assertIsNotNone(compare_query_results(cetra_low, duck_low, ordered=True))

    def test_null_column_does_not_mask_non_null_duckdb_value(self) -> None:
        cetra = EngineResult(status="ok", columns=[("x", "null")], rows=[[None]])
        duck_non_null = EngineResult(status="ok", columns=[("x", "INTEGER")], rows=[[42]])
        duck_null = EngineResult(status="ok", columns=[("x", "INTEGER")], rows=[[None]])
        self.assertIsNotNone(compare_query_results(cetra, duck_non_null, ordered=True))
        self.assertIsNone(compare_query_results(cetra, duck_null, ordered=True))

    def test_strict_type_coercions_catch_mismatches(self) -> None:
        # String vs int must fail.
        cetra_s = EngineResult(status="ok", columns=[("c", "string")], rows=[["42"]])
        duck_i = EngineResult(status="ok", columns=[("c", "BIGINT")], rows=[[42]])
        self.assertIsNotNone(compare_query_results(cetra_s, duck_i, ordered=True))

        # Float vs int must fail, while float vs DuckDB Decimal literal passes.
        cetra_f = EngineResult(status="ok", columns=[("c", "float")], rows=[[42.0]])
        self.assertIsNotNone(compare_query_results(cetra_f, duck_i, ordered=True))
        duck_dec_lit = EngineResult(
            status="ok", columns=[("c", "DECIMAL")], rows=[[Decimal("42.0")]]
        )
        self.assertIsNone(compare_query_results(cetra_f, duck_dec_lit, ordered=True))

        # Decimal vs int must fail.
        cetra_d = EngineResult(status="ok", columns=[("c", "decimal")], rows=[["42.00"]])
        self.assertIsNotNone(compare_query_results(cetra_d, duck_i, ordered=True))

        # Integer vs DuckDB exact integral float (e.g. floor(7) -> 7.0) passes; fractional fails.
        cetra_int = EngineResult(status="ok", columns=[("c", "integer")], rows=[[7]])
        duck_f_exact = EngineResult(status="ok", columns=[("c", "DOUBLE")], rows=[[7.0]])
        duck_f_frac = EngineResult(status="ok", columns=[("c", "DOUBLE")], rows=[[7.5]])
        self.assertIsNone(compare_query_results(cetra_int, duck_f_exact, ordered=True))
        self.assertIsNotNone(compare_query_results(cetra_int, duck_f_frac, ordered=True))

    def test_temporal_interval_binary_and_map_normalization(self) -> None:
        ts_c = normalize_cell("2025-01-01T12:30:45.125000000Z", "timestamp")
        ts_d = normalize_cell(
            datetime.datetime(2025, 1, 1, 12, 30, 45, 125_000, tzinfo=datetime.UTC),
            "timestamp",
        )
        self.assertEqual(ts_c, ts_d)

        date_c = normalize_cell("2024-02-29", "date")
        date_d = normalize_cell(datetime.date(2024, 2, 29), "date")
        self.assertEqual(date_c, date_d)

        iv_c = normalize_cell("1 days 2 hours 30.500000000 secs", "interval")
        iv_d = normalize_cell(
            datetime.timedelta(days=1, hours=2, seconds=30, microseconds=500_000),
            "interval",
        )
        self.assertEqual(iv_c, iv_d)

        # 1 month must NOT equal 30 days.
        self.assertNotEqual(
            normalize_cell("1 mons", "interval"),
            normalize_cell("30 days", "interval"),
        )
        # Years on relativedelta-like objects fold into months.
        self.assertEqual(
            normalize_cell(_FakeRelativeDelta(years=1, months=2, days=3), "interval"),
            normalize_cell("14 mons 3 days", "interval"),
        )
        # Negative nanos truncate toward zero (not floor division).
        self.assertEqual(
            normalize_cell({"months": 0, "days": 0, "nanos": -1_500}, "interval"),
            (0, -1),
        )

        bin_c = normalize_cell("deadbeef", "binary")
        bin_d = normalize_cell(b"\xde\xad\xbe\xef", "binary")
        self.assertEqual(bin_c, bin_d)

        map_c = normalize_cell({"a": "1", "b": None}, "string_map")
        map_d = normalize_cell({"key": ["a", "b"], "value": ["1", None]}, "string_map")
        self.assertEqual(map_c, map_d)

    def test_unordered_multiset_and_unordered_arrays(self) -> None:
        cetra = EngineResult(
            status="ok",
            columns=[("k", "integer"), ("arr", "string_list"), ("v", "float")],
            rows=[
                [2, ["b", "a"], 1.0 + 1e-11],
                [1, ["z", None, "x"], 2.0],
            ],
        )
        duck = EngineResult(
            status="ok",
            columns=[("k", "BIGINT"), ("arr", "VARCHAR[]"), ("v", "DOUBLE")],
            rows=[
                [1, ["x", "z", None], 2.0],
                [2, ["a", "b"], 1.0],
            ],
        )
        self.assertIsNotNone(
            compare_query_results(cetra, duck, ordered=True, unordered_arrays=True)
        )
        self.assertIsNone(compare_query_results(cetra, duck, ordered=False, unordered_arrays=True))
        self.assertIsNotNone(
            compare_query_results(cetra, duck, ordered=False, unordered_arrays=False)
        )

    def test_error_class_internal_and_leak_handling(self) -> None:
        err_c = EngineResult(
            status="error",
            columns=[],
            rows=[],
            error_kind="query_failed",
            error_message="division by zero",
        )
        err_d = EngineResult(
            status="error",
            columns=[],
            rows=[],
            error_message="Division by zero",
        )
        ok_d = EngineResult(status="ok", columns=[("c0", "BIGINT")], rows=[[1]])

        self.assertIsNone(compare_query_results(err_c, err_d, ordered=False))
        self.assertIsNotNone(compare_query_results(err_c, ok_d, ordered=False))

        internal_c = EngineResult(
            status="error",
            columns=[],
            rows=[],
            error_kind="internal",
            error_message="panic",
        )
        diff_internal = compare_query_results(internal_c, err_d, ordered=False)
        self.assertIsNotNone(diff_internal)
        self.assertIn("Internal", str(diff_internal))

        for fatal_kind in ("too_large", "cancelled", "busy"):
            fatal_c = EngineResult(
                status="error",
                columns=[],
                rows=[],
                error_kind=fatal_kind,
                error_message="resource limit",
            )
            diff_fatal = compare_query_results(fatal_c, err_d, ordered=False)
            self.assertIsNotNone(diff_fatal)
            self.assertIn(fatal_kind, str(diff_fatal))

        leak_c = cetra_json_to_result(
            {
                "status": "leak",
                "id": "q0",
                "reserved_bytes": 64,
                "spilled_bytes": 0,
                "outcome": "ok",
            }
        )
        diff_leak = compare_query_results(leak_c, ok_d, ordered=False)
        self.assertIsNotNone(diff_leak)
        self.assertIn("LEAK", str(diff_leak))

    def test_missing_json_column_key_and_truncated_rejection(self) -> None:
        malformed = cetra_json_to_result(
            {
                "status": "ok",
                "id": "q0",
                "columns": [{"name": "a", "type": "integer"}, {"name": "b", "type": "integer"}],
                "rows": [{"a": 1}],
                "truncated": False,
            }
        )
        self.assertEqual(malformed.status, "error")
        self.assertEqual(malformed.error_kind, "internal")
        self.assertIn("'b'", str(malformed.error_message))

        trunc_c = EngineResult(
            status="ok",
            columns=[("c0", "integer")],
            rows=[[1]],
            truncated=True,
        )
        ok_d = EngineResult(status="ok", columns=[("c0", "BIGINT")], rows=[[1]])
        diff_trunc = compare_query_results(trunc_c, ok_d, ordered=True)
        self.assertIsNotNone(diff_trunc)
        self.assertIn("truncated=True", str(diff_trunc))

    def test_duckdb_value_to_cetra_json(self) -> None:
        self.assertEqual(duckdb_value_to_cetra_json(float("nan"), "F64"), "NaN")
        self.assertEqual(duckdb_value_to_cetra_json(float("-inf"), "F64"), "-Infinity")
        self.assertEqual(duckdb_value_to_cetra_json(Decimal("1E-4"), "DECIMAL(10,4)"), "0.0001")
        self.assertEqual(duckdb_value_to_cetra_json(b"\x01\xff", "FIXEDBYTES8"), "01ff")
        self.assertEqual(
            duckdb_value_to_cetra_json(
                datetime.timedelta(days=2, seconds=3, microseconds=4),
                "INTERVAL",
            ),
            {"months": 0, "days": 2, "nanos": 3_000_004_000},
        )
        self.assertEqual(
            duckdb_value_to_cetra_json(datetime.timedelta(hours=-2), "INTERVAL"),
            {"months": 0, "days": 0, "nanos": -7_200_000_000_000},
        )
        self.assertEqual(
            duckdb_value_to_cetra_json(
                datetime.timedelta(days=-1, hours=-2, microseconds=-500),
                "INTERVAL",
            ),
            {"months": 0, "days": -1, "nanos": -7_200_000_500_000},
        )
        self.assertEqual(
            duckdb_value_to_cetra_json(
                _FakeRelativeDelta(years=2, months=3, days=4, hours=1),
                "INTERVAL",
            ),
            {"months": 27, "days": 4, "nanos": 3_600_000_000_000},
        )


if __name__ == "__main__":
    unittest.main()
