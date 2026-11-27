use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cetra_column::{Batch, DataType, Field, Interval, ScalarValue, Schema};
use cetra_query::exec::{BatchSource, ScanSource, ScanSpec};
use cetra_query::sql::sql_json;
use cetra_query::{FilterView, QueryError, compile_filter, eval_filter, filter_planning_schema};
use serde_json::Map;

use crate::common::{
    CompareOpts, FIXED_NOW_NS, LayoutVariant, assert_clean_ctx, assert_rows_equiv,
    build_batch_source, make_exec_ctx, make_table_catalog_and_sources, run_sql_optimized,
    run_sql_unoptimized,
};

const DEFAULT_CASES: usize = 64;
const DEFAULT_SEED: u64 = 0xCE7A_2026_0001;
const NS_PER_HOUR: i64 = 3_600_000_000_000;
const NS_PER_DAY: i64 = 24 * NS_PER_HOUR;

// ---------------------------------------------------------------------------
// Deterministic SplitMix64 PRNG & env configuration
// ---------------------------------------------------------------------------

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "SplitMix64::next_usize bound must be > 0");
        (self.next_u64() as usize) % bound
    }

    fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }

    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        &slice[self.next_usize(slice.len())]
    }

    fn range_i64(&mut self, lo: i64, hi_inclusive: i64) -> i64 {
        assert!(lo <= hi_inclusive);
        let span = (hi_inclusive as i128 - lo as i128 + 1) as u128;
        let offset = (u128::from(self.next_u64()) % span) as i128;
        (lo as i128 + offset) as i64
    }
}

fn metamorphic_cases() -> usize {
    std::env::var("CETRA_METAMORPHIC_CASES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CASES)
}

fn metamorphic_seed() -> u64 {
    std::env::var("CETRA_METAMORPHIC_SEED")
        .ok()
        .and_then(|s| {
            let trimmed = s.trim();
            if let Some(hex) = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
            {
                u64::from_str_radix(hex, 16).ok()
            } else {
                trimmed.parse::<u64>().ok()
            }
        })
        .unwrap_or(DEFAULT_SEED)
}

// ---------------------------------------------------------------------------
// Adversarial multi-table dataset (t_main, t_right, t_empty, t_one)
// ---------------------------------------------------------------------------

type TableSpec = (String, Arc<Schema>, Vec<Vec<ScalarValue>>);

fn main_schema() -> Arc<Schema> {
    Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("grp", DataType::Utf8, true),
        Field::new("k_small", DataType::I64, true),
        Field::new("k_edge", DataType::I64, true),
        Field::new("x", DataType::F64, true),
        Field::new(
            "dec",
            DataType::Decimal128 {
                precision: 10,
                scale: 2,
            },
            true,
        ),
        Field::new("flag", DataType::Bool, true),
        Field::new("ts", DataType::timestamp_utc(), true),
        Field::new("d", DataType::Date, true),
        Field::new("iv", DataType::Interval, true),
        Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        Field::new("meta", DataType::MapUtf8, true),
        Field::new("tid", DataType::FixedBytes16, true),
    ])
    .expect("valid t_main schema")
}

fn right_schema() -> Arc<Schema> {
    Schema::try_new_arc(vec![
        Field::new("rid", DataType::I64, false),
        Field::new("grp", DataType::Utf8, true),
        Field::new("k_small", DataType::I64, true),
        Field::new("val", DataType::I64, true),
        Field::new("flag", DataType::Bool, true),
    ])
    .expect("valid t_right schema")
}

fn tid_bytes_for_row(row: usize) -> [u8; 16] {
    [
        0x0a, 0xbc, 0xde, 0xf1, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x00, 0x00, 0x00,
        row as u8,
    ]
}

fn tid_hex_lower(row: usize) -> String {
    hex::encode(tid_bytes_for_row(row))
}

fn tid_hex_upper(row: usize) -> String {
    tid_hex_lower(row).to_ascii_uppercase()
}

fn build_adversarial_tables() -> Vec<TableSpec> {
    let m_schema = main_schema();
    let r_schema = right_schema();

    let grp_pool: &[Option<&str>] = &[
        Some("alpha"),
        Some("beta"),
        Some("alpha"),
        Some("gamma"),
        Some(""),
        Some("a'b\"c"),
        Some("café"),
        Some("日本語"),
        Some("100%_\\test"),
        Some("ALPHA"),
        None,
    ];
    let k_small_pool: &[Option<i64>] = &[
        Some(0),
        Some(1),
        Some(-1),
        Some(5),
        Some(-5),
        Some(42),
        Some(-42),
        Some(100),
        Some(-250),
        Some(1000),
        Some(-1000),
        None,
    ];
    let k_edge_pool: &[Option<i64>] = &[
        Some(i64::MIN),
        Some(i64::MAX),
        Some(0),
        Some(-1),
        Some(1),
        Some(42),
        Some(-42),
        None,
    ];
    let x_pool: &[Option<f64>] = &[
        Some(-0.0),
        Some(0.0),
        Some(1.5),
        Some(-1.5),
        Some(3.25),
        Some(-10.75),
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        None,
    ];
    let dec_pool: &[Option<i128>] = &[
        Some(0),
        Some(100),
        Some(-100),
        Some(12345),
        Some(-99999),
        Some(50),
        None,
    ];
    let iv_pool: &[Option<Interval>] = &[
        Some(Interval {
            months: 0,
            days: 1,
            nanos: 0,
        }),
        Some(Interval {
            months: 0,
            days: 0,
            nanos: NS_PER_DAY,
        }),
        Some(Interval {
            months: 1,
            days: 0,
            nanos: 0,
        }),
        Some(Interval {
            months: 0,
            days: 30,
            nanos: 0,
        }),
        Some(Interval {
            months: 0,
            days: -1,
            nanos: 0,
        }),
        Some(Interval {
            months: 0,
            days: 0,
            nanos: 0,
        }),
        None,
    ];

    let mut main_rows = Vec::with_capacity(64);
    for i in 0..64usize {
        let id = ScalarValue::I64(i as i64);
        let grp = match grp_pool[i % grp_pool.len()] {
            Some(s) => ScalarValue::Utf8(s.to_owned()),
            None => ScalarValue::Null(DataType::Utf8),
        };
        let k_small = match k_small_pool[(i * 5 + 1) % k_small_pool.len()] {
            Some(v) => ScalarValue::I64(v),
            None => ScalarValue::Null(DataType::I64),
        };
        let k_edge = match k_edge_pool[i % k_edge_pool.len()] {
            Some(v) => ScalarValue::I64(v),
            None => ScalarValue::Null(DataType::I64),
        };
        let x = match x_pool[i % x_pool.len()] {
            Some(v) => ScalarValue::F64(v),
            None => ScalarValue::Null(DataType::F64),
        };
        let dec = match dec_pool[i % dec_pool.len()] {
            Some(v) => ScalarValue::Decimal128 {
                value: v,
                precision: 10,
                scale: 2,
            },
            None => ScalarValue::Null(DataType::Decimal128 {
                precision: 10,
                scale: 2,
            }),
        };
        let flag = match i % 3 {
            0 => ScalarValue::Bool(true),
            1 => ScalarValue::Bool(false),
            _ => ScalarValue::Null(DataType::Bool),
        };
        let ts = if i % 9 == 8 {
            ScalarValue::Null(DataType::timestamp_utc())
        } else {
            let delta = ((i as i64) - 32) * NS_PER_HOUR * 6;
            ScalarValue::TimestampNs {
                value: FIXED_NOW_NS + delta,
                utc: true,
            }
        };
        let d = if i % 8 == 7 {
            ScalarValue::Null(DataType::Date)
        } else {
            ScalarValue::Date(20_454 + (i as i32) - 32)
        };
        let iv = match iv_pool[i % iv_pool.len()] {
            Some(v) => ScalarValue::Interval(v),
            None => ScalarValue::Null(DataType::Interval),
        };
        let tags = match i % 6 {
            0 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            },
            1 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("alpha".to_owned()),
                    ScalarValue::Utf8("beta".to_owned()),
                ],
            },
            2 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("alpha".to_owned()),
                    ScalarValue::Null(DataType::Utf8),
                    ScalarValue::Utf8("alpha".to_owned()),
                ],
            },
            3 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![ScalarValue::Utf8("prod".to_owned())],
            },
            4 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("café".to_owned()),
                    ScalarValue::Utf8("".to_owned()),
                ],
            },
            _ => ScalarValue::Null(DataType::List(Box::new(DataType::Utf8))),
        };
        let meta = match i % 5 {
            0 => ScalarValue::MapUtf8(vec![]),
            1 => ScalarValue::MapUtf8(vec![
                ("env".to_owned(), Some("prod".to_owned())),
                ("json".to_owned(), Some(r#"{"a":1,"b":"ok"}"#.to_owned())),
                ("num".to_owned(), Some("42".to_owned())),
                ("region".to_owned(), Some("eu".to_owned())),
            ]),
            2 => ScalarValue::MapUtf8(vec![
                ("env".to_owned(), Some("staging".to_owned())),
                ("extra".to_owned(), Some("sparse".to_owned())),
                (
                    "json".to_owned(),
                    Some(r#"{"a":2,"b":"two","nested":{"k":"v"}}"#.to_owned()),
                ),
                ("num".to_owned(), Some("not-a-num".to_owned())),
            ]),
            3 => ScalarValue::MapUtf8(vec![
                ("env".to_owned(), None),
                ("json".to_owned(), Some(r#"{"a":null,"b":null}"#.to_owned())),
            ]),
            _ => ScalarValue::Null(DataType::MapUtf8),
        };
        let tid = if i % 7 == 6 {
            ScalarValue::Null(DataType::FixedBytes16)
        } else {
            ScalarValue::FixedBytes16(tid_bytes_for_row(i))
        };

        main_rows.push(vec![
            id, grp, k_small, k_edge, x, dec, flag, ts, d, iv, tags, meta, tid,
        ]);
    }

    let r_grp_pool: &[Option<&str>] = &[
        Some("alpha"),
        Some("beta"),
        Some("delta"),
        Some(""),
        Some("café"),
        None,
    ];
    let r_k_pool: &[Option<i64>] = &[
        Some(0),
        Some(1),
        Some(-1),
        Some(5),
        Some(42),
        Some(999),
        None,
    ];
    let mut right_rows = Vec::with_capacity(32);
    for i in 0..32usize {
        let rid = ScalarValue::I64(i as i64);
        let grp = match r_grp_pool[i % r_grp_pool.len()] {
            Some(s) => ScalarValue::Utf8(s.to_owned()),
            None => ScalarValue::Null(DataType::Utf8),
        };
        let k_small = match r_k_pool[(i * 2 + 1) % r_k_pool.len()] {
            Some(v) => ScalarValue::I64(v),
            None => ScalarValue::Null(DataType::I64),
        };
        let val = if i % 5 == 4 {
            ScalarValue::Null(DataType::I64)
        } else {
            ScalarValue::I64(((i as i64) - 16) * 25)
        };
        let flag = match i % 3 {
            0 => ScalarValue::Bool(true),
            1 => ScalarValue::Bool(false),
            _ => ScalarValue::Null(DataType::Bool),
        };
        right_rows.push(vec![rid, grp, k_small, val, flag]);
    }

    let one_rows = vec![main_rows[1].clone()];

    vec![
        ("t_main".to_owned(), Arc::clone(&m_schema), main_rows),
        ("t_right".to_owned(), r_schema, right_rows),
        ("t_empty".to_owned(), Arc::clone(&m_schema), Vec::new()),
        ("t_one".to_owned(), m_schema, one_rows),
    ]
}

// ---------------------------------------------------------------------------
// Seeded Shape B SQL generator
// ---------------------------------------------------------------------------

struct GeneratedQuery {
    sql: String,
    ordered: bool,
    unordered_arrays: bool,
    supports_limit_check: bool,
}

fn gen_infallible_predicate(rng: &mut SplitMix64, depth: usize, prefix: &str) -> String {
    if depth > 0 && rng.next_usize(3) == 0 {
        let lhs = gen_infallible_predicate(rng, depth - 1, prefix);
        let rhs = gen_infallible_predicate(rng, depth - 1, prefix);
        return match rng.next_usize(3) {
            0 => format!("(({lhs}) AND ({rhs}))"),
            1 => format!("(({lhs}) OR ({rhs}))"),
            _ => format!("(NOT ({lhs}))"),
        };
    }

    let c = |col: &str| -> String {
        if prefix.is_empty() {
            col.to_owned()
        } else {
            format!("{prefix}.{col}")
        }
    };

    match rng.next_usize(18) {
        0 => {
            let op = *rng.pick(&["=", "!=", "<", "<=", ">", ">="]);
            let rhs = rng.range_i64(-100, 100);
            format!("{} {op} {rhs}", c("k_small"))
        }
        1 => {
            let op = *rng.pick(&["=", "!=", "<", ">"]);
            let lit = *rng.pick(&["'alpha'", "'beta'", "''", "'café'", "'ALPHA'"]);
            format!("{} {op} {lit}", c("grp"))
        }
        2 => {
            let col = *rng.pick(&["grp", "k_small", "x", "dec", "flag", "ts", "d", "iv", "tid"]);
            if rng.next_bool() {
                format!("{} IS NULL", c(col))
            } else {
                format!("{} IS NOT NULL", c(col))
            }
        }
        3 => {
            let op = if rng.next_bool() {
                "IS DISTINCT FROM"
            } else {
                "IS NOT DISTINCT FROM"
            };
            match rng.next_usize(3) {
                0 => format!("{} {op} 'alpha'", c("grp")),
                1 => format!("{} {op} NULL", c("k_small")),
                _ => format!("{} {op} true", c("flag")),
            }
        }
        4 => {
            let kw = if rng.next_bool() { "LIKE" } else { "ILIKE" };
            let pat = *rng.pick(&["'alp%'", "'%\\_%'", "'%é'", "'%'", "''", "'a_pha'"]);
            format!("{} {kw} {pat} ESCAPE '\\'", c("grp"))
        }
        5 => {
            let neg = if rng.next_bool() { "NOT " } else { "" };
            if rng.next_bool() {
                format!("{} {neg}IN (0, 1, 42, NULL)", c("k_small"))
            } else {
                format!("{} {neg}IN ('alpha', 'beta', '')", c("grp"))
            }
        }
        6 => {
            let neg = if rng.next_bool() { "NOT " } else { "" };
            format!("{} {neg}BETWEEN -50 AND 50", c("k_small"))
        }
        7 => format!(
            "CASE WHEN {} THEN {} > 0 WHEN {} IS NULL THEN true ELSE {} = 'alpha' END",
            c("flag"),
            c("k_small"),
            c("k_small"),
            c("grp")
        ),
        8 => format!("COALESCE({}, false) = true", c("flag")),
        9 => format!("array_has({}, 'alpha')", c("tags")),
        10 => {
            let op = *rng.pick(&["=", "!="]);
            let val = *rng.pick(&["'prod'", "'staging'"]);
            format!("{}['env'] {op} {val}", c("meta"))
        }
        11 => format!("encode({}, 'hex') = '{}'", c("tid"), tid_hex_lower(0)),
        12 => {
            let op = *rng.pick(&["=", "!=", "<", ">="]);
            let rhs = *rng.pick(&["0.0", "-0.0", "1.5", "-1.5"]);
            format!("{} {op} {rhs}", c("x"))
        }
        13 => format!("isnan({}) OR isfinite({})", c("x"), c("x")),
        14 => format!("{} = INTERVAL '24 hours'", c("iv")),
        15 => format!("{} >= DATE '2026-01-01'", c("d")),
        16 => format!("{} >= TIMESTAMP '2026-01-01T00:00:00Z'", c("ts")),
        _ => format!("{} IS TRUE OR {} IS FALSE", c("flag"), c("flag")),
    }
}

fn gen_projection_filter_query(
    rng: &mut SplitMix64,
    include_uppercase_hex: bool,
) -> GeneratedQuery {
    let pred = if include_uppercase_hex && rng.next_usize(6) == 0 {
        let hex_lit = if rng.next_bool() {
            tid_hex_upper(0)
        } else {
            tid_hex_lower(1)
        };
        format!("encode(tid, 'hex') = '{hex_lit}' OR flag IS TRUE")
    } else {
        gen_infallible_predicate(rng, 2, "")
    };

    let expr_c1 = match rng.next_usize(10) {
        0 => "CASE WHEN k_small != 0 THEN 1000 / k_small ELSE 0 END".to_owned(),
        1 => "COALESCE(NULLIF(grp, 'alpha'), 'fallback')".to_owned(),
        2 => "GREATEST(k_small, -10, 5)".to_owned(),
        3 => "LEAST(k_small, 10, -5)".to_owned(),
        4 => "concat_ws(':', lower(grp), upper(left(grp, 2)), length(grp))".to_owned(),
        5 => "CASE WHEN regexp_like(grp, '^[a-z]+$') THEN regexp_extract(grp, '([a-z]{2})') ELSE 'other' END".to_owned(),
        6 => "COALESCE(json_get_text(meta['json'], 'b'), (meta['json'] ->> 'a'), 'none')".to_owned(),
        7 => "array_to_string(array_distinct(tags), '|', 'NULL')".to_owned(),
        8 => "COALESCE(array_length(map_keys(meta)), 0) + COALESCE(array_length(map_values(meta)), 0)".to_owned(),
        _ => "EXTRACT(HOUR FROM date_trunc('hour', ts))".to_owned(),
    };

    let expr_c2 = match rng.next_usize(8) {
        0 => "TRY_CAST(meta['num'] AS BIGINT)".to_owned(),
        1 => "array_element(tags, 1)".to_owned(),
        2 => "array_has_all(tags, ARRAY['alpha'])".to_owned(),
        3 => "json_extract_text(meta['json'], '$.a')".to_owned(),
        4 => "date_bin(INTERVAL '6 hours', ts)".to_owned(),
        5 => "dec + CAST('1.25' AS DECIMAL(10, 2))".to_owned(),
        6 => "encode(tid, 'hex')".to_owned(),
        _ => "CASE grp WHEN 'alpha' THEN 1 WHEN 'beta' THEN 2 ELSE 0 END".to_owned(),
    };

    let ordered = rng.next_bool();
    let (order_clause, supports_limit) = if ordered {
        let dir = if rng.next_bool() { "ASC" } else { "DESC" };
        let nulls = if rng.next_bool() {
            "NULLS FIRST"
        } else {
            "NULLS LAST"
        };
        let key = *rng.pick(&["k_small", "grp", "x", "dec", "ts", "iv"]);
        (
            format!(" ORDER BY {key} {dir} {nulls}, id ASC NULLS LAST"),
            true,
        )
    } else {
        (String::new(), false)
    };

    GeneratedQuery {
        sql: format!(
            "SELECT id AS c0, {expr_c1} AS c1, {expr_c2} AS c2 FROM t_main WHERE {pred}{order_clause}"
        ),
        ordered,
        unordered_arrays: false,
        supports_limit_check: supports_limit,
    }
}

fn gen_join_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let join_kind = *rng.pick(&[
        "INNER JOIN",
        "LEFT JOIN",
        "RIGHT JOIN",
        "FULL OUTER JOIN",
        "CROSS JOIN",
    ]);
    let right_tbl = match rng.next_usize(6) {
        0 => "t_empty",
        1 => "t_one",
        _ => "t_right",
    };

    let (r_id_col, r_val_col) = if right_tbl == "t_right" {
        ("r.rid", "r.val")
    } else {
        ("r.id", "r.k_small")
    };

    let on_clause = if join_kind == "CROSS JOIN" {
        String::new()
    } else {
        let equi = match rng.next_usize(3) {
            0 => "l.grp = r.grp",
            1 => "l.k_small = r.k_small",
            _ => "l.grp = r.grp AND l.k_small = r.k_small",
        };
        let residual = if rng.next_bool() {
            " AND l.flag IS DISTINCT FROM r.flag"
        } else {
            ""
        };
        format!(" ON {equi}{residual}")
    };

    let where_clause = if rng.next_bool() {
        let p = gen_infallible_predicate(rng, 1, "l");
        format!(" WHERE {p}")
    } else if join_kind == "CROSS JOIN" {
        // Keep cross-join cardinality small and fast.
        " WHERE l.id < 8".to_owned()
    } else {
        String::new()
    };

    let ordered = rng.next_bool();
    let (order_clause, supports_limit) = if ordered {
        let limit_part = if rng.next_bool() {
            format!(" LIMIT {}", 1 + rng.next_usize(20))
        } else {
            String::new()
        };
        (
            format!(" ORDER BY l.id ASC NULLS LAST, {r_id_col} ASC NULLS LAST{limit_part}"),
            true,
        )
    } else {
        (String::new(), false)
    };

    GeneratedQuery {
        sql: format!(
            "SELECT l.id AS c0, {r_id_col} AS c1, l.grp AS c2, COALESCE(l.k_small, 0) + COALESCE({r_val_col}, 0) AS c3 \
             FROM t_main AS l {join_kind} {right_tbl} AS r{on_clause}{where_clause}{order_clause}"
        ),
        ordered,
        unordered_arrays: false,
        supports_limit_check: supports_limit,
    }
}

fn gen_aggregate_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let table = match rng.next_usize(7) {
        0 => "t_empty",
        1 => "t_one",
        _ => "t_main",
    };
    let where_clause = if rng.next_bool() {
        format!(" WHERE {}", gen_infallible_predicate(rng, 1, ""))
    } else {
        String::new()
    };

    let variant = rng.next_usize(4);
    match variant {
        0 => {
            // Global aggregates (covers 0-row t_empty, 1-row t_one, and filtered t_main)
            GeneratedQuery {
                sql: format!(
                    "SELECT COUNT(*) AS c0, COUNT(k_small) AS c1, COUNT(DISTINCT grp) AS c2, \
                     SUM(k_small) AS c3, AVG(k_small) AS c4, MIN(k_small) AS c5, MAX(k_small) AS c6, \
                     BOOL_OR(flag) AS c7, BOOL_AND(flag) AS c8, \
                     percentile_cont(0.5) WITHIN GROUP (ORDER BY k_small) AS c9 \
                     FROM {table}{where_clause}"
                ),
                ordered: false,
                unordered_arrays: false,
                supports_limit_check: false,
            }
        }
        1 => {
            // Grouped with FILTER (WHERE ...) + HAVING
            let having = if rng.next_bool() {
                " HAVING COUNT(*) >= 1 AND COALESCE(SUM(k_small), 0) >= -5000"
            } else {
                ""
            };
            GeneratedQuery {
                sql: format!(
                    "SELECT grp AS c0, COUNT(*) AS c1, \
                     COUNT(*) FILTER (WHERE flag IS TRUE) AS c2, \
                     SUM(k_small) FILTER (WHERE k_small > 0) AS c3, \
                     MIN(iv) AS c4, MAX(ts) AS c5 \
                     FROM {table}{where_clause} GROUP BY grp{having} \
                     ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        2 => {
            // Grouped ARRAY_AGG (sets unordered_arrays = true)
            let distinct_kw = if rng.next_bool() { "DISTINCT " } else { "" };
            GeneratedQuery {
                sql: format!(
                    "SELECT flag AS c0, COUNT(DISTINCT k_small) AS c1, \
                     ARRAY_AGG({distinct_kw}grp) AS c2, SUM(dec) AS c3 \
                     FROM {table}{where_clause} GROUP BY flag \
                     ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: true,
                supports_limit_check: true,
            }
        }
        _ => {
            // Multi-key GROUP BY + expression key
            GeneratedQuery {
                sql: format!(
                    "SELECT COALESCE(grp, 'none') AS c0, flag AS c1, \
                     COUNT(*) AS c2, SUM(k_small) AS c3, BOOL_OR(flag) AS c4 \
                     FROM {table}{where_clause} GROUP BY COALESCE(grp, 'none'), flag \
                     ORDER BY c0 ASC NULLS LAST, c1 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
    }
}

fn gen_setop_or_distinct_query(rng: &mut SplitMix64) -> GeneratedQuery {
    if rng.next_usize(4) == 0 {
        // SELECT DISTINCT
        let ordered = rng.next_bool();
        let order_clause = if ordered {
            " ORDER BY c0 ASC NULLS LAST, c1 ASC NULLS LAST"
        } else {
            ""
        };
        return GeneratedQuery {
            sql: format!(
                "SELECT DISTINCT grp AS c0, flag AS c1 FROM t_main WHERE {}{order_clause}",
                gen_infallible_predicate(rng, 1, "")
            ),
            ordered,
            unordered_arrays: false,
            supports_limit_check: ordered,
        };
    }

    let op = *rng.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
    let rhs_tbl = *rng.pick(&["t_right", "t_empty", "t_one"]);
    let ordered = rng.next_bool();
    let (order_clause, supports_limit) = if ordered {
        let lim = if rng.next_bool() {
            format!(" LIMIT {}", 1 + rng.next_usize(25))
        } else {
            String::new()
        };
        (
            format!(" ORDER BY c0 ASC NULLS LAST, c1 ASC NULLS LAST{lim}"),
            true,
        )
    } else {
        (String::new(), false)
    };

    GeneratedQuery {
        sql: format!(
            "SELECT grp AS c0, k_small AS c1 FROM t_main \
             {op} \
             SELECT grp AS c0, k_small AS c1 FROM {rhs_tbl}{order_clause}"
        ),
        ordered,
        unordered_arrays: false,
        supports_limit_check: supports_limit,
    }
}

fn gen_window_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let part = match rng.next_usize(3) {
        0 => "PARTITION BY grp ",
        1 => "PARTITION BY flag ",
        _ => "",
    };
    let variant = rng.next_usize(4);
    match variant {
        0 => {
            // Row-order-dependent windows (strictly tie-broken on id ASC NULLS LAST)
            GeneratedQuery {
                sql: format!(
                    "SELECT id AS c0, grp AS c1, \
                     ROW_NUMBER() OVER ({part}ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST) AS c2, \
                     LAG(k_small, 1, -999) OVER ({part}ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST) AS c3, \
                     LEAD(k_small, 1, 999) OVER ({part}ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST) AS c4 \
                     FROM t_main ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        1 => {
            // Peer-group RANK / DENSE_RANK + RANGE frame on non-unique k_small
            GeneratedQuery {
                sql: format!(
                    "SELECT id AS c0, k_small AS c1, \
                     RANK() OVER ({part}ORDER BY k_small ASC NULLS LAST) AS c2, \
                     DENSE_RANK() OVER ({part}ORDER BY k_small ASC NULLS LAST) AS c3, \
                     SUM(k_small) OVER ({part}ORDER BY k_small ASC NULLS LAST RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c4 \
                     FROM t_main ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        2 => {
            // Bounded ROWS frame with FIRST_VALUE / LAST_VALUE / running SUM & COUNT
            GeneratedQuery {
                sql: format!(
                    "SELECT id AS c0, \
                     FIRST_VALUE(k_small) OVER ({part}ORDER BY k_small DESC NULLS FIRST, id ASC NULLS LAST ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING) AS c1, \
                     LAST_VALUE(k_small) OVER ({part}ORDER BY k_small DESC NULLS FIRST, id ASC NULLS LAST ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING) AS c2, \
                     SUM(k_small) OVER ({part}ORDER BY k_small DESC NULLS FIRST, id ASC NULLS LAST ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS c3, \
                     COUNT(*) OVER ({part}ORDER BY k_small DESC NULLS FIRST, id ASC NULLS LAST ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) AS c4 \
                     FROM t_main ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        _ => {
            // Multi-window query with two distinct partitions/orderings
            GeneratedQuery {
                sql: "SELECT id AS c0, \
                     ROW_NUMBER() OVER (PARTITION BY grp ORDER BY k_small ASC NULLS LAST, id ASC NULLS LAST) AS c1, \
                     SUM(k_small) OVER (PARTITION BY flag ORDER BY id ASC NULLS LAST ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS c2 \
                     FROM t_main ORDER BY c0 ASC NULLS LAST"
                    .to_owned(),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
    }
}

fn gen_order_limit_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let key = *rng.pick(&["k_small", "x", "grp", "dec", "ts", "d", "iv", "flag"]);
    let dir = if rng.next_bool() { "ASC" } else { "DESC" };
    let nulls = if rng.next_bool() {
        "NULLS FIRST"
    } else {
        "NULLS LAST"
    };
    let limit = *rng.pick(&[0usize, 1, 5, 12, 32, 64, 100]);
    let offset = *rng.pick(&[0usize, 1, 5, 20, 60, 100]);
    let where_clause = if rng.next_bool() {
        format!(" WHERE {}", gen_infallible_predicate(rng, 1, ""))
    } else {
        String::new()
    };

    GeneratedQuery {
        sql: format!(
            "SELECT id AS c0, grp AS c1, k_small AS c2, x AS c3 \
             FROM t_main{where_clause} \
             ORDER BY {key} {dir} {nulls}, id ASC NULLS LAST \
             LIMIT {limit} OFFSET {offset}"
        ),
        ordered: true,
        unordered_arrays: false,
        supports_limit_check: true,
    }
}

fn gen_subquery_or_cte_query(rng: &mut SplitMix64) -> GeneratedQuery {
    match rng.next_usize(5) {
        0 => GeneratedQuery {
            sql: "SELECT id AS c0, grp AS c1, k_small AS c2 FROM t_main \
                  WHERE k_small IN (SELECT k_small FROM t_right WHERE flag IS TRUE) \
                  ORDER BY c0 ASC NULLS LAST"
                .to_owned(),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
        1 => {
            // NOT IN both with and without NULLs on right
            let right_filter = if rng.next_bool() {
                " WHERE k_small IS NOT NULL"
            } else {
                ""
            };
            GeneratedQuery {
                sql: format!(
                    "SELECT id AS c0, k_small AS c1 FROM t_main \
                     WHERE k_small NOT IN (SELECT k_small FROM t_right{right_filter}) \
                     ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        2 => {
            let neg = if rng.next_bool() { "NOT " } else { "" };
            let sub_tbl = *rng.pick(&["t_right", "t_empty", "t_one"]);
            GeneratedQuery {
                sql: format!(
                    "SELECT id AS c0, grp AS c1 FROM t_main \
                     WHERE {neg}EXISTS (SELECT 1 FROM {sub_tbl} WHERE flag IS TRUE) \
                     ORDER BY c0 ASC NULLS LAST"
                ),
                ordered: true,
                unordered_arrays: false,
                supports_limit_check: true,
            }
        }
        3 => GeneratedQuery {
            sql: format!(
                "WITH filtered AS (SELECT id, grp, k_small FROM t_main WHERE {}), \
                 agg AS (SELECT grp, COUNT(*) AS cnt, SUM(k_small) AS tot FROM filtered GROUP BY grp) \
                 SELECT f.id AS c0, f.grp AS c1, a.cnt AS c2, a.tot AS c3 \
                 FROM filtered AS f LEFT JOIN agg AS a ON f.grp = a.grp \
                 ORDER BY c0 ASC NULLS LAST",
                gen_infallible_predicate(rng, 1, "")
            ),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
        _ => GeneratedQuery {
            sql: "WITH r_agg AS (SELECT grp, MAX(val) AS max_val FROM t_right GROUP BY grp) \
                  SELECT m.id AS c0, m.grp AS c1, r.max_val AS c2 \
                  FROM t_main AS m LEFT JOIN r_agg AS r ON m.grp = r.grp \
                  WHERE m.k_small IS NOT NULL \
                  ORDER BY c0 ASC NULLS LAST"
                .to_owned(),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
    }
}

fn generate_query(
    rng: &mut SplitMix64,
    case_idx: usize,
    include_uppercase_hex: bool,
) -> GeneratedQuery {
    // Round-robin across all 7 families for the first 14 cases so even small case counts cover everything.
    let family = if case_idx < 14 {
        case_idx % 7
    } else {
        rng.next_usize(7)
    };
    match family {
        0 => gen_projection_filter_query(rng, include_uppercase_hex),
        1 => gen_join_query(rng),
        2 => gen_aggregate_query(rng),
        3 => gen_setop_or_distinct_query(rng),
        4 => gen_window_query(rng),
        5 => gen_order_limit_query(rng),
        _ => gen_subquery_or_cte_query(rng),
    }
}

fn assert_not_internal(err: &QueryError, label: &str) {
    assert!(
        !matches!(err, QueryError::Internal(_)),
        "{label}: unexpected QueryError::Internal: {err:?}"
    );
}

fn compare_results(
    lhs: &Result<(Arc<Schema>, Vec<Vec<ScalarValue>>), QueryError>,
    rhs: &Result<(Arc<Schema>, Vec<Vec<ScalarValue>>), QueryError>,
    opts: &CompareOpts,
    allow_rhs_too_large: bool,
    label: &str,
) {
    match (lhs, rhs) {
        (Ok((ls, lr)), Ok((rs, rr))) => {
            assert_eq!(ls.as_ref(), rs.as_ref(), "{label}: schema mismatch");
            assert_rows_equiv(lr, rr, opts, label);
        }
        (Ok(_), Err(QueryError::TooLarge)) if allow_rhs_too_large => {}
        (Err(le), Err(re)) => {
            assert_not_internal(le, label);
            assert_not_internal(re, label);
            assert_eq!(
                std::mem::discriminant(le),
                std::mem::discriminant(re),
                "{label}: error variant mismatch (lhs={le:?}, rhs={re:?})"
            );
        }
        (Ok((_, lr)), Err(re)) => {
            assert_not_internal(re, label);
            panic!(
                "{label}: lhs succeeded ({} rows) but rhs failed with {re:?}",
                lr.len()
            );
        }
        (Err(le), Ok((_, rr))) => {
            assert_not_internal(le, label);
            panic!(
                "{label}: lhs failed with {le:?} but rhs succeeded ({} rows)",
                rr.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Axis A: Optimizer Equivalence (unoptimized vs optimized + TopK + sql_json)
// ---------------------------------------------------------------------------

#[test]
fn axis_a_optimizer_equivalence() {
    let seed = metamorphic_seed();
    let cases = metamorphic_cases();
    let tables = build_adversarial_tables();
    let (catalog, sources) = make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, seed);
    let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);

    // Targeted deterministic queries exercising hex-ID rewrite (lower + upper),
    // is_infallible preservation, projection/map pruning, and TopK.
    let mut queries = vec![
        GeneratedQuery {
            sql: format!(
                "SELECT id AS c0, encode(tid, 'hex') AS c1 FROM t_main \
                 WHERE encode(tid, 'hex') = '{}' ORDER BY c0 ASC NULLS LAST",
                tid_hex_lower(0)
            ),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
        GeneratedQuery {
            sql: format!(
                "SELECT id AS c0, encode(tid, 'hex') AS c1 FROM t_main \
                 WHERE encode(tid, 'hex') = '{}' ORDER BY c0 ASC NULLS LAST",
                tid_hex_upper(0)
            ),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
        GeneratedQuery {
            sql: format!(
                "SELECT id AS c0 FROM t_main \
                 WHERE encode(tid, 'hex') IN ('{}', '{}') ORDER BY c0 ASC NULLS LAST",
                tid_hex_lower(2),
                tid_hex_upper(3)
            ),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
        GeneratedQuery {
            sql: "SELECT id AS c0, k_small AS c1 FROM t_main \
                  WHERE false AND (1 / 0 = 1) ORDER BY c0 ASC NULLS LAST"
                .to_owned(),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: false,
        },
        GeneratedQuery {
            sql: "SELECT id AS c0, k_small AS c1 FROM t_main \
                  WHERE (k_small = 0) AND (100 / k_small > 1) ORDER BY c0 ASC NULLS LAST"
                .to_owned(),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: false,
        },
        GeneratedQuery {
            sql: "SELECT id AS c0, meta['env'] AS c1 FROM t_main \
                  WHERE meta['region'] = 'eu' ORDER BY c0 ASC NULLS LAST"
                .to_owned(),
            ordered: true,
            unordered_arrays: false,
            supports_limit_check: true,
        },
    ];

    let mut rng = SplitMix64::new(seed ^ 0xA11A_0001);
    for idx in 0..cases {
        queries.push(generate_query(&mut rng, idx, true));
    }

    for (case_idx, q) in queries.iter().enumerate() {
        let label = format!("axis_a seed={seed:#x} case={case_idx} sql={}", q.sql);
        let opts = CompareOpts {
            ordered: q.ordered,
            unordered_arrays: q.unordered_arrays,
            float_rel_tol: 1e-9,
            float_abs_tol: 1e-12,
        };

        let unopt = run_sql_unoptimized(&catalog, &sources, &exec.ctx, &q.sql, None);
        assert_clean_ctx(&exec.ctx);
        let opt = run_sql_optimized(&catalog, &sources, &exec.ctx, &q.sql, None);
        assert_clean_ctx(&exec.ctx);

        if case_idx == 4 {
            assert!(
                matches!(unopt, Err(QueryError::Failed(_))),
                "{label}: expected deterministic div-by-zero case 4 to return Err(QueryError::Failed(_)), got {unopt:?}"
            );
        } else {
            assert!(
                unopt.is_ok(),
                "{label}: expected baseline query to return Ok, got {:?}",
                unopt.as_ref().err()
            );
        }

        let baseline_ok = unopt.is_ok();
        compare_results(&unopt, &opt, &opts, false, &label);

        if q.supports_limit_check {
            let lim = 5usize;
            let unopt_lim = run_sql_unoptimized(&catalog, &sources, &exec.ctx, &q.sql, Some(lim));
            assert_clean_ctx(&exec.ctx);
            let opt_lim = run_sql_optimized(&catalog, &sources, &exec.ctx, &q.sql, Some(lim));
            assert_clean_ctx(&exec.ctx);
            compare_results(
                &unopt_lim,
                &opt_lim,
                &opts,
                false,
                &format!("{label} [limit={lim}]"),
            );
        }

        if baseline_ok {
            let json_res = sql_json(&catalog, &sources, &exec.ctx, &q.sql, &Map::new(), 1000);
            assert_clean_ctx(&exec.ctx);
            if let Err(err) = json_res {
                assert_not_internal(&err, &format!("{label} [sql_json]"));
                panic!("{label} [sql_json]: unexpected error: {err:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Axis B: Encoding & Selection Equivalence (FlatAll vs FragmentedMasked vs EncodedMixed)
// ---------------------------------------------------------------------------

#[test]
fn axis_b_encoding_and_selection_equivalence() {
    let seed = metamorphic_seed();
    let cases = metamorphic_cases();
    let tables = build_adversarial_tables();

    let (cat_flat, src_flat) =
        make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, seed);
    let (cat_frag, src_frag) =
        make_table_catalog_and_sources(&tables, LayoutVariant::FragmentedMasked, seed ^ 0xB1);
    let (cat_enc, src_enc) =
        make_table_catalog_and_sources(&tables, LayoutVariant::EncodedMixed, seed ^ 0xB2);
    let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);

    let mut rng = SplitMix64::new(seed ^ 0xB22B_0002);
    for case_idx in 0..cases {
        let q = generate_query(&mut rng, case_idx, false);
        let label = format!("axis_b seed={seed:#x} case={case_idx} sql={}", q.sql);
        let opts = CompareOpts {
            ordered: q.ordered,
            unordered_arrays: q.unordered_arrays,
            float_rel_tol: 1e-9,
            float_abs_tol: 1e-12,
        };

        let base = run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, &q.sql, None);
        assert_clean_ctx(&exec.ctx);
        assert!(
            base.is_ok(),
            "{label}: expected generated query baseline to return Ok, got {:?}",
            base.as_ref().err()
        );
        let frag = run_sql_optimized(&cat_frag, &src_frag, &exec.ctx, &q.sql, None);
        assert_clean_ctx(&exec.ctx);
        let enc = run_sql_optimized(&cat_enc, &src_enc, &exec.ctx, &q.sql, None);
        assert_clean_ctx(&exec.ctx);

        compare_results(&base, &frag, &opts, false, &format!("{label} [frag]"));
        compare_results(&base, &enc, &opts, false, &format!("{label} [enc]"));
    }
}

// ---------------------------------------------------------------------------
// Axis C: Parallelism & Forced-Spill Equivalence
// ---------------------------------------------------------------------------

fn build_spill_stress_tables() -> Vec<TableSpec> {
    let left_schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("grp", DataType::Utf8, true),
        Field::new("k_small", DataType::I64, true),
        Field::new("payload", DataType::Utf8, true),
    ])
    .expect("valid t_spill schema");

    let right_schema = Schema::try_new_arc(vec![
        Field::new("rid", DataType::I64, false),
        Field::new("grp", DataType::Utf8, true),
        Field::new("tag", DataType::Utf8, true),
    ])
    .expect("valid t_spill_r schema");

    let mut left_rows = Vec::with_capacity(600);
    for i in 0..600usize {
        let grp = if i.is_multiple_of(19) {
            ScalarValue::Null(DataType::Utf8)
        } else {
            ScalarValue::Utf8(format!("group-{:02}", i % 60))
        };
        let k_small = ScalarValue::I64(((i as i64) * 17) % 201 - 100);
        // ~150 bytes per distinct payload (550 distinct values -> ~82 KiB + table overhead > 64 KiB pool).
        let payload = ScalarValue::Utf8(format!(
            "p{:04}-{}-{}",
            i % 550,
            "abcdefghij".repeat(14),
            i % 13
        ));
        left_rows.push(vec![ScalarValue::I64(i as i64), grp, k_small, payload]);
    }

    // Build side of HashJoin (`BuildRight`): 550 rows × ~160B tag (~88 KiB > 64 KiB pool)
    // across 60 groups so Grace hash partitioning splits cleanly into <= 32 small partitions.
    let mut right_rows = Vec::with_capacity(550);
    for i in 0..550usize {
        let grp = if i.is_multiple_of(17) {
            ScalarValue::Null(DataType::Utf8)
        } else {
            ScalarValue::Utf8(format!("group-{:02}", i % 60))
        };
        let tag = ScalarValue::Utf8(format!("tag-{i:04}-{}", "xyz1234567".repeat(15)));
        right_rows.push(vec![ScalarValue::I64(i as i64), grp, tag]);
    }

    vec![
        ("t_spill".to_owned(), left_schema, left_rows),
        ("t_spill_r".to_owned(), right_schema, right_rows),
    ]
}

#[test]
fn axis_c_parallel_and_forced_spill_equivalence() {
    let seed = metamorphic_seed();
    let cases = metamorphic_cases();
    let tables = build_adversarial_tables();

    let (cat_base, src_base) =
        make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, seed);
    let (cat_multi, src_multi) =
        make_table_catalog_and_sources(&tables, LayoutVariant::MultiChunkFlat(8), seed ^ 0xC1);
    let (cat_enc, src_enc) =
        make_table_catalog_and_sources(&tables, LayoutVariant::EncodedMixed, seed ^ 0xC2);

    let ctx_base = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);
    let ctx_multi = make_exec_ctx(64 * 1024, 32 * 1024 * 1024, 4, 8);
    let ctx_enc = make_exec_ctx(64 * 1024, 32 * 1024 * 1024, 4, 8);

    let mut multi_ok = 0usize;
    let mut enc_ok = 0usize;
    let mut rng = SplitMix64::new(seed ^ 0xC33C_0003);
    for case_idx in 0..cases {
        let q = generate_query(&mut rng, case_idx, false);
        let label = format!("axis_c seed={seed:#x} case={case_idx} sql={}", q.sql);
        let opts = CompareOpts {
            ordered: q.ordered,
            unordered_arrays: q.unordered_arrays,
            float_rel_tol: 1e-9,
            float_abs_tol: 1e-12,
        };

        let base = run_sql_optimized(&cat_base, &src_base, &ctx_base.ctx, &q.sql, None);
        assert_clean_ctx(&ctx_base.ctx);
        assert!(
            base.is_ok(),
            "{label}: expected generated query baseline to return Ok, got {:?}",
            base.as_ref().err()
        );
        let multi = run_sql_optimized(&cat_multi, &src_multi, &ctx_multi.ctx, &q.sql, None);
        assert_clean_ctx(&ctx_multi.ctx);
        let enc = run_sql_optimized(&cat_enc, &src_enc, &ctx_enc.ctx, &q.sql, None);
        assert_clean_ctx(&ctx_enc.ctx);

        if multi.is_ok() {
            multi_ok += 1;
        }
        if enc.is_ok() {
            enc_ok += 1;
        }

        compare_results(&base, &multi, &opts, true, &format!("{label} [multi-4p]"));
        compare_results(&base, &enc, &opts, true, &format!("{label} [enc-4p]"));
    }

    assert!(
        multi_ok * 5 >= cases * 4,
        "axis_c seed={seed:#x}: too many random multi-4p queries hit TooLarge ({multi_ok}/{cases} succeeded)"
    );
    assert!(
        enc_ok * 5 >= cases * 4,
        "axis_c seed={seed:#x}: too many random enc-4p queries hit TooLarge ({enc_ok}/{cases} succeeded)"
    );

    // Dedicated spill-stress suite guaranteeing disk spills independently on
    // ExternalSort, HashAgg (COUNT(DISTINCT) + DISTINCT), and HashJoin (INNER + LEFT).
    // HashAgg and HashJoin queries intentionally omit ORDER BY so ExternalSort cannot mask spills.
    // Use small chunks (`MultiChunkFlat(32)`, ~3 KiB/batch) with 2 parallel workers in a 64 KiB
    // pool so ~100 KiB inputs reliably spill without starving scan/merge headroom.
    let spill_tables = build_spill_stress_tables();
    let (s_cat_base, s_src_base) =
        make_table_catalog_and_sources(&spill_tables, LayoutVariant::FlatAll, seed ^ 0xC9);
    let (s_cat_multi, s_src_multi) = make_table_catalog_and_sources(
        &spill_tables,
        LayoutVariant::MultiChunkFlat(32),
        seed ^ 0xCA,
    );
    let ctx_spill = make_exec_ctx(64 * 1024, 32 * 1024 * 1024, 2, 16);

    let spill_queries: &[(&str, &str, bool)] = &[
        (
            "ExternalSort",
            "SELECT id AS c0, grp AS c1, payload AS c2 FROM t_spill \
             ORDER BY payload DESC NULLS LAST, id ASC NULLS LAST",
            true,
        ),
        (
            "HashAgg(COUNT DISTINCT)",
            "SELECT grp AS c0, COUNT(*) AS c1, COUNT(DISTINCT payload) AS c2, \
             SUM(k_small) AS c3, MIN(payload) AS c4, MAX(payload) AS c5 \
             FROM t_spill GROUP BY grp",
            false,
        ),
        (
            "HashAgg(DISTINCT)",
            "SELECT DISTINCT grp AS c0, payload AS c1 FROM t_spill",
            false,
        ),
        (
            "HashJoin(INNER)",
            "SELECT l.id AS c0, r.rid AS c1, l.grp AS c2, r.tag AS c3 \
             FROM t_spill AS l INNER JOIN t_spill_r AS r ON l.grp = r.grp \
             WHERE l.id < 200",
            false,
        ),
        (
            "HashJoin(LEFT)",
            "SELECT l.id AS c0, r.rid AS c1, l.payload AS c2, r.tag AS c3 \
             FROM t_spill AS l LEFT JOIN t_spill_r AS r ON l.grp = r.grp \
             WHERE l.id < 200",
            false,
        ),
    ];

    for (idx, &(op_name, sql, ordered)) in spill_queries.iter().enumerate() {
        let label = format!("axis_c_spill seed={seed:#x} idx={idx} op={op_name} sql={sql}");
        let opts = CompareOpts {
            ordered,
            unordered_arrays: false,
            float_rel_tol: 1e-9,
            float_abs_tol: 1e-12,
        };
        let base = run_sql_optimized(&s_cat_base, &s_src_base, &ctx_base.ctx, sql, None);
        assert_clean_ctx(&ctx_base.ctx);
        assert!(
            base.is_ok(),
            "{label}: baseline query failed: {:?}",
            base.as_ref().err()
        );

        let spill_before = ctx_spill.spill_events.load(Ordering::Relaxed);
        let multi = run_sql_optimized(&s_cat_multi, &s_src_multi, &ctx_spill.ctx, sql, None);
        assert_clean_ctx(&ctx_spill.ctx);
        let spill_after = ctx_spill.spill_events.load(Ordering::Relaxed);

        assert!(
            multi.is_ok(),
            "{label}: expected spill-stress query to succeed without TooLarge, got {:?}",
            multi.as_ref().err()
        );
        assert!(
            spill_after > spill_before,
            "{label}: expected {op_name} to spill to disk (spill_before={spill_before}, spill_after={spill_after})"
        );
        compare_results(&base, &multi, &opts, false, &label);
    }
}

// ---------------------------------------------------------------------------
// Axis D: SQLancer TLP (Row TLP + Aggregate TLP + Grouped HAVING TLP)
// ---------------------------------------------------------------------------

fn gen_having_predicate(rng: &mut SplitMix64, depth: usize) -> String {
    if depth > 0 && rng.next_usize(3) == 0 {
        let lhs = gen_having_predicate(rng, depth - 1);
        let rhs = gen_having_predicate(rng, depth - 1);
        return match rng.next_usize(3) {
            0 => format!("(({lhs}) AND ({rhs}))"),
            1 => format!("(({lhs}) OR ({rhs}))"),
            _ => format!("(NOT ({lhs}))"),
        };
    }

    match rng.next_usize(8) {
        0 => format!("COUNT(*) > {}", rng.range_i64(0, 8)),
        1 => format!("SUM(k_small) >= {}", rng.range_i64(-500, 500)),
        2 => "SUM(k_small) IS NULL".to_owned(),
        3 => "MIN(k_small) IS DISTINCT FROM MAX(k_small)".to_owned(),
        4 => "BOOL_OR(flag)".to_owned(),
        5 => "BOOL_AND(flag)".to_owned(),
        6 => "COUNT(k_small) BETWEEN 1 AND 6".to_owned(),
        _ => "AVG(k_small) > 0.0".to_owned(),
    }
}

fn scalar_as_i64(v: &ScalarValue, label: &str) -> Option<i64> {
    match v {
        ScalarValue::I64(n) => Some(*n),
        ScalarValue::Null(_) => None,
        other => panic!("{label}: expected I64 or Null, got {other:?}"),
    }
}

#[test]
fn axis_d_sqlancer_tlp() {
    let seed = metamorphic_seed();
    let cases = metamorphic_cases();
    let tables = build_adversarial_tables();

    let (cat_flat, src_flat) =
        make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, seed);
    let (cat_enc, src_enc) =
        make_table_catalog_and_sources(&tables, LayoutVariant::EncodedMixed, seed ^ 0xD1);
    let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);

    let unordered_opts = CompareOpts {
        ordered: false,
        unordered_arrays: false,
        float_rel_tol: 1e-9,
        float_abs_tol: 1e-12,
    };

    // Baseline row set and global aggregates computed once.
    let base_rows_sql = "SELECT id AS c0, grp AS c1, k_small AS c2 FROM t_main";
    let (_, base_rows) = run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, base_rows_sql, None)
        .expect("TLP baseline rows query failed");
    assert_clean_ctx(&exec.ctx);

    let global_agg_sql = "SELECT COUNT(*) AS c0, SUM(k_small) AS c1, MIN(k_small) AS c2, MAX(k_small) AS c3 FROM t_main";
    let (_, global_agg_rows) =
        run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, global_agg_sql, None)
            .expect("TLP global agg query failed");
    assert_clean_ctx(&exec.ctx);
    assert_eq!(global_agg_rows.len(), 1);
    let g_cnt = scalar_as_i64(&global_agg_rows[0][0], "global cnt").expect("non-null COUNT(*)");
    let g_sum = scalar_as_i64(&global_agg_rows[0][1], "global sum");
    let g_min = scalar_as_i64(&global_agg_rows[0][2], "global min");
    let g_max = scalar_as_i64(&global_agg_rows[0][3], "global max");

    let base_grp_sql =
        "SELECT grp AS c0, COUNT(*) AS c1, SUM(k_small) AS c2 FROM t_main GROUP BY grp";
    let (_, base_grp_rows) = run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, base_grp_sql, None)
        .expect("TLP baseline group query failed");
    assert_clean_ctx(&exec.ctx);

    let mut rng = SplitMix64::new(seed ^ 0xD44D_0004);
    for case_idx in 0..cases {
        let p = gen_infallible_predicate(&mut rng, 2, "");
        let label = format!("axis_d seed={seed:#x} case={case_idx} pred={p}");

        // 1. Row TLP on BOTH FlatAll and EncodedMixed
        let row_tlp_sql = format!(
            "SELECT id AS c0, grp AS c1, k_small AS c2 FROM t_main WHERE ({p}) \
             UNION ALL \
             SELECT id AS c0, grp AS c1, k_small AS c2 FROM t_main WHERE NOT ({p}) \
             UNION ALL \
             SELECT id AS c0, grp AS c1, k_small AS c2 FROM t_main WHERE ({p}) IS NULL"
        );
        let (_, flat_tlp_rows) =
            run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, &row_tlp_sql, None)
                .unwrap_or_else(|e| panic!("{label} [row_tlp flat] sql={row_tlp_sql}: {e:?}"));
        assert_clean_ctx(&exec.ctx);
        assert_rows_equiv(
            &base_rows,
            &flat_tlp_rows,
            &unordered_opts,
            &format!("{label} [row_tlp flat] sql={row_tlp_sql}"),
        );

        let (_, enc_tlp_rows) =
            run_sql_optimized(&cat_enc, &src_enc, &exec.ctx, &row_tlp_sql, None)
                .unwrap_or_else(|e| panic!("{label} [row_tlp enc] sql={row_tlp_sql}: {e:?}"));
        assert_clean_ctx(&exec.ctx);
        assert_rows_equiv(
            &base_rows,
            &enc_tlp_rows,
            &unordered_opts,
            &format!("{label} [row_tlp enc] sql={row_tlp_sql}"),
        );

        // 2. Aggregate TLP (re-aggregate COUNT(*), SUM, MIN, MAX across the 3 partitions)
        let part_preds = [
            format!("({p})"),
            format!("NOT ({p})"),
            format!("({p}) IS NULL"),
        ];
        let mut p_cnt_sum: i64 = 0;
        let mut p_sum_acc: Option<i64> = None;
        let mut p_min_acc: Option<i64> = None;
        let mut p_max_acc: Option<i64> = None;

        for clause in &part_preds {
            let agg_sql = format!(
                "SELECT COUNT(*) AS c0, SUM(k_small) AS c1, MIN(k_small) AS c2, MAX(k_small) AS c3 \
                 FROM t_main WHERE {clause}"
            );
            let (_, rows) = run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, &agg_sql, None)
                .unwrap_or_else(|e| panic!("{label} [agg_tlp] sql={agg_sql}: {e:?}"));
            assert_clean_ctx(&exec.ctx);
            assert_eq!(rows.len(), 1, "{label} [agg_tlp] sql={agg_sql}");
            let c = scalar_as_i64(&rows[0][0], &label).expect("COUNT(*) must be non-null");
            p_cnt_sum += c;
            if let Some(s) = scalar_as_i64(&rows[0][1], &label) {
                p_sum_acc = Some(p_sum_acc.map_or(s, |acc| acc + s));
            }
            if let Some(mn) = scalar_as_i64(&rows[0][2], &label) {
                p_min_acc = Some(p_min_acc.map_or(mn, |acc| acc.min(mn)));
            }
            if let Some(mx) = scalar_as_i64(&rows[0][3], &label) {
                p_max_acc = Some(p_max_acc.map_or(mx, |acc| acc.max(mx)));
            }
        }
        assert_eq!(p_cnt_sum, g_cnt, "{label} [agg_tlp COUNT(*)]");
        assert_eq!(p_sum_acc, g_sum, "{label} [agg_tlp SUM(k_small)]");
        assert_eq!(p_min_acc, g_min, "{label} [agg_tlp MIN(k_small)]");
        assert_eq!(p_max_acc, g_max, "{label} [agg_tlp MAX(k_small)]");

        // 3. Grouped + HAVING TLP
        let hp = gen_having_predicate(&mut rng, 1);
        let having_tlp_sql = format!(
            "SELECT grp AS c0, COUNT(*) AS c1, SUM(k_small) AS c2 FROM t_main GROUP BY grp HAVING ({hp}) \
             UNION ALL \
             SELECT grp AS c0, COUNT(*) AS c1, SUM(k_small) AS c2 FROM t_main GROUP BY grp HAVING NOT ({hp}) \
             UNION ALL \
             SELECT grp AS c0, COUNT(*) AS c1, SUM(k_small) AS c2 FROM t_main GROUP BY grp HAVING ({hp}) IS NULL"
        );
        let (_, having_rows) =
            run_sql_optimized(&cat_flat, &src_flat, &exec.ctx, &having_tlp_sql, None)
                .unwrap_or_else(|e| panic!("{label} [having_tlp] sql={having_tlp_sql}: {e:?}"));
        assert_clean_ctx(&exec.ctx);
        assert_rows_equiv(
            &base_grp_rows,
            &having_rows,
            &unordered_opts,
            &format!("{label} [having_tlp] sql={having_tlp_sql}"),
        );
    }
}

// ---------------------------------------------------------------------------
// Axis E: Filter DSL Metamorphic & TLP (FlatAll vs FragmentedMasked vs EncodedMixed)
// ---------------------------------------------------------------------------

fn spans_filter_base_schema() -> Arc<Schema> {
    Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("environment", DataType::Utf8, true),
        Field::new("duration_ns", DataType::I64, false),
        Field::new("cost_total", DataType::F64, true),
        Field::new("has_root", DataType::Bool, false),
        Field::new("start_time", DataType::timestamp_utc(), false),
        Field::new("tags", DataType::List(Box::new(DataType::Utf8)), false),
        Field::new("metadata", DataType::MapUtf8, false),
        Field::new("attributes", DataType::MapUtf8, false),
    ])
    .expect("valid spans filter schema")
}

fn build_spans_filter_rows() -> Vec<Vec<ScalarValue>> {
    let names = [
        "llm.chat",
        "LLM.Complete",
        "tool_call",
        "retriever",
        "agent%step",
        "café-span",
    ];
    let statuses = ["ok", "error", "unset"];
    let envs: &[Option<&str>] = &[Some("prod"), Some("staging"), Some("eval"), Some(""), None];
    let costs: &[Option<f64>] = &[
        Some(0.0),
        Some(-0.0),
        Some(0.0025),
        Some(1.5),
        Some(f64::NAN),
        None,
    ];

    let mut rows = Vec::with_capacity(64);
    for i in 0..64usize {
        let id = ScalarValue::I64(i as i64);
        let trace_id = ScalarValue::Utf8(tid_hex_lower(i));
        let span_id = ScalarValue::Utf8(format!("{:016x}", 0x1000 + i));
        let name = ScalarValue::Utf8(names[i % names.len()].to_owned());
        let status = ScalarValue::Utf8(statuses[i % statuses.len()].to_owned());
        let environment = match envs[i % envs.len()] {
            Some(e) => ScalarValue::Utf8(e.to_owned()),
            None => ScalarValue::Null(DataType::Utf8),
        };
        let duration_ns = ScalarValue::I64(((i as i64) * 37 - 500) * 1_000);
        let cost_total = match costs[i % costs.len()] {
            Some(v) => ScalarValue::F64(v),
            None => ScalarValue::Null(DataType::F64),
        };
        let has_root = ScalarValue::Bool(i.is_multiple_of(2));
        let start_time = ScalarValue::TimestampNs {
            value: FIXED_NOW_NS + ((i as i64) - 32) * NS_PER_HOUR * 2,
            utc: true,
        };
        let tags = match i % 4 {
            0 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            },
            1 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("alpha".to_owned()),
                    ScalarValue::Utf8("beta".to_owned()),
                ],
            },
            2 => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![ScalarValue::Utf8("alpha".to_owned())],
            },
            _ => ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("gamma".to_owned()),
                    ScalarValue::Utf8("beta".to_owned()),
                ],
            },
        };
        let metadata = match i % 3 {
            0 => ScalarValue::MapUtf8(vec![
                ("channel".to_owned(), Some("email".to_owned())),
                ("tier".to_owned(), Some("1".to_owned())),
            ]),
            1 => ScalarValue::MapUtf8(vec![
                ("channel".to_owned(), Some("chat".to_owned())),
                ("tier".to_owned(), Some("not_int".to_owned())),
            ]),
            _ => ScalarValue::MapUtf8(vec![]),
        };
        let attributes = match i % 3 {
            0 => ScalarValue::MapUtf8(vec![
                ("env".to_owned(), Some("prod".to_owned())),
                ("num".to_owned(), Some("10".to_owned())),
            ]),
            1 => ScalarValue::MapUtf8(vec![
                ("env".to_owned(), Some("dev".to_owned())),
                ("num".to_owned(), None),
            ]),
            _ => ScalarValue::MapUtf8(vec![("env".to_owned(), Some("staging".to_owned()))]),
        };

        rows.push(vec![
            id,
            trace_id,
            span_id,
            name,
            status,
            environment,
            duration_ns,
            cost_total,
            has_root,
            start_time,
            tags,
            metadata,
            attributes,
        ]);
    }
    rows
}

fn gen_filter_dsl_pred(rng: &mut SplitMix64, depth: usize) -> String {
    if depth > 0 && rng.next_usize(3) == 0 {
        let lhs = gen_filter_dsl_pred(rng, depth - 1);
        let rhs = gen_filter_dsl_pred(rng, depth - 1);
        return match rng.next_usize(3) {
            0 => format!("(({lhs}) AND ({rhs}))"),
            1 => format!("(({lhs}) OR ({rhs}))"),
            _ => format!("(NOT ({lhs}))"),
        };
    }

    match rng.next_usize(18) {
        0 => {
            let op = *rng.pick(&["=", "!=", "<", "<=", ">", ">="]);
            let rhs = rng.range_i64(-300_000, 1_500_000);
            format!("duration_ns {op} {rhs}")
        }
        1 => {
            let op = *rng.pick(&["=", "!="]);
            let val = *rng.pick(&["'ok'", "'error'", "'unset'"]);
            format!("status {op} {val}")
        }
        2 => {
            if rng.next_bool() {
                "environment IS NULL".to_owned()
            } else {
                "environment IS NOT NULL".to_owned()
            }
        }
        3 => {
            let op = if rng.next_bool() {
                "IS DISTINCT FROM"
            } else {
                "IS NOT DISTINCT FROM"
            };
            let rhs = *rng.pick(&["'prod'", "'eval'", "NULL"]);
            format!("environment {op} {rhs}")
        }
        4 => {
            let neg = if rng.next_bool() { "NOT " } else { "" };
            format!("status {neg}IN ('ok', 'error')")
        }
        5 => {
            let neg = if rng.next_bool() { "NOT " } else { "" };
            format!("duration_ns {neg}BETWEEN -100000 AND 500000")
        }
        6 => {
            let kw = if rng.next_bool() { "LIKE" } else { "ILIKE" };
            let pat = *rng.pick(&["'llm%'", "'%chat%'", "'%\\%%'", "'%span'"]);
            format!("name {kw} {pat}")
        }
        7 => {
            let rhs = *rng.pick(&["['alpha']", "['alpha', 'beta']", "['gamma']"]);
            format!("tags @> {rhs}")
        }
        8 => {
            let tag = *rng.pick(&["'alpha'", "'beta'", "'gamma'"]);
            format!("array_has(tags, {tag})")
        }
        9 => format!("array_length(tags) >= {}", rng.next_usize(3)),
        10 => {
            let val = *rng.pick(&["'prod'", "'dev'", "'staging'"]);
            format!("attributes['env'] = {val}")
        }
        11 => "metadata['channel'] = 'email'".to_owned(),
        12 => "lower(name) = 'llm.complete' OR upper(status) = 'OK'".to_owned(),
        13 => "length(name) > 8 AND abs(duration_ns) >= 100000".to_owned(),
        14 => "coalesce(environment, 'prod') = 'prod'".to_owned(),
        15 => "start_time > now() - INTERVAL '1 day'".to_owned(),
        16 => "CAST(duration_ns AS DOUBLE) > 0.0 AND metadata['tier']::BIGINT = 1".to_owned(),
        _ => "cost_total >= 0.001 OR has_root".to_owned(),
    }
}

fn collect_source_batches(source: &BatchSource) -> Vec<Batch> {
    let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);
    let streams = source
        .open_partitions(&ScanSpec::default(), &exec.ctx)
        .expect("open_partitions failed");
    let mut batches = Vec::new();
    for mut stream in streams {
        let mut res = exec.ctx.pool.register("collect_filter_batches", false);
        while let Some(b) = stream
            .next_batch(&exec.ctx.eval.cancel, Some(&mut res))
            .expect("next_batch failed")
        {
            batches.push(b);
            res.free();
        }
    }
    assert_clean_ctx(&exec.ctx);
    batches
}

fn eval_filter_active_ids(
    expr: &cetra_query::Expr,
    batches: &[Batch],
    label: &str,
) -> BTreeSet<i64> {
    let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);
    let mut ids = BTreeSet::new();
    for batch in batches {
        let mut res = exec.ctx.pool.register("filter_eval", false);
        let mask = eval_filter(expr, batch, &exec.ctx.eval, Some(&mut res))
            .unwrap_or_else(|e| panic!("{label}: eval_filter failed: {e:?}"));
        let filtered = batch
            .filter_mask(&mask)
            .unwrap_or_else(|e| panic!("{label}: filter_mask failed: {e:?}"));
        drop(mask);
        res.free();

        let id_col = filtered
            .column_by_name("id")
            .unwrap_or_else(|| panic!("{label}: missing id column"));
        let active_indices = filtered
            .selection()
            .to_indices(filtered.raw_len())
            .unwrap_or_else(|e| panic!("{label}: to_indices failed: {e:?}"));
        for phys_row in active_indices {
            let sv = id_col
                .get_scalar(phys_row as usize)
                .unwrap_or_else(|e| panic!("{label}: get_scalar failed: {e:?}"));
            match sv {
                ScalarValue::I64(id) => {
                    assert!(ids.insert(id), "{label}: duplicate active id {id}");
                }
                other => panic!("{label}: expected I64 id, got {other:?}"),
            }
        }
    }
    assert_clean_ctx(&exec.ctx);
    ids
}

#[test]
fn axis_e_filter_dsl_metamorphic_and_tlp() {
    let seed = metamorphic_seed();
    let cases = metamorphic_cases();
    let schema = spans_filter_base_schema();
    let rows = build_spans_filter_rows();
    let all_ids: BTreeSet<i64> = (0..rows.len() as i64).collect();

    let src_flat = build_batch_source(&schema, &rows, LayoutVariant::FlatAll, seed);
    let src_frag = build_batch_source(&schema, &rows, LayoutVariant::FragmentedMasked, seed ^ 0xE1);
    let src_enc = build_batch_source(&schema, &rows, LayoutVariant::EncodedMixed, seed ^ 0xE2);

    let b_flat = collect_source_batches(&src_flat);
    let b_frag = collect_source_batches(&src_frag);
    let b_enc = collect_source_batches(&src_enc);

    let mut rng = SplitMix64::new(seed ^ 0xE55E_0005);
    for case_idx in 0..cases {
        let p = gen_filter_dsl_pred(&mut rng, 2);
        let q = gen_filter_dsl_pred(&mut rng, 1);
        let label = format!("axis_e seed={seed:#x} case={case_idx} filter={p}");

        let p_not = format!("NOT ({p})");
        let p_null = format!("({p}) IS NULL");
        let p_and_q = format!("({p}) AND ({q})");
        let p_or_q = format!("({p}) OR ({q})");

        let c_p = compile_filter(FilterView::Spans, &schema, &p)
            .unwrap_or_else(|e| panic!("{label}: compile P failed: {e:?}"));
        let c_not = compile_filter(FilterView::Spans, &schema, &p_not)
            .unwrap_or_else(|e| panic!("{label}: compile NOT P failed: {e:?}"));
        let c_null = compile_filter(FilterView::Spans, &schema, &p_null)
            .unwrap_or_else(|e| panic!("{label}: compile P IS NULL failed: {e:?}"));
        let c_q = compile_filter(FilterView::Spans, &schema, &q)
            .unwrap_or_else(|e| panic!("{label}: compile Q failed: {e:?}"));
        let c_and = compile_filter(FilterView::Spans, &schema, &p_and_q)
            .unwrap_or_else(|e| panic!("{label}: compile P AND Q failed: {e:?}"));
        let c_or = compile_filter(FilterView::Spans, &schema, &p_or_q)
            .unwrap_or_else(|e| panic!("{label}: compile P OR Q failed: {e:?}"));

        // 1. Encoding & selection equivalence across FlatAll, FragmentedMasked, EncodedMixed
        let ids_p_flat = eval_filter_active_ids(&c_p.expr, &b_flat, &format!("{label} [P flat]"));
        let ids_p_frag = eval_filter_active_ids(&c_p.expr, &b_frag, &format!("{label} [P frag]"));
        let ids_p_enc = eval_filter_active_ids(&c_p.expr, &b_enc, &format!("{label} [P enc]"));
        assert_eq!(
            ids_p_flat, ids_p_frag,
            "{label}: FlatAll != FragmentedMasked for P"
        );
        assert_eq!(
            ids_p_flat, ids_p_enc,
            "{label}: FlatAll != EncodedMixed for P"
        );

        let ids_not_flat =
            eval_filter_active_ids(&c_not.expr, &b_flat, &format!("{label} [NOT P flat]"));
        let ids_not_frag =
            eval_filter_active_ids(&c_not.expr, &b_frag, &format!("{label} [NOT P frag]"));
        let ids_not_enc =
            eval_filter_active_ids(&c_not.expr, &b_enc, &format!("{label} [NOT P enc]"));
        assert_eq!(
            ids_not_flat, ids_not_frag,
            "{label}: FlatAll != FragmentedMasked for NOT P"
        );
        assert_eq!(
            ids_not_flat, ids_not_enc,
            "{label}: FlatAll != EncodedMixed for NOT P"
        );

        let ids_null_flat =
            eval_filter_active_ids(&c_null.expr, &b_flat, &format!("{label} [P IS NULL flat]"));
        let ids_null_frag =
            eval_filter_active_ids(&c_null.expr, &b_frag, &format!("{label} [P IS NULL frag]"));
        let ids_null_enc =
            eval_filter_active_ids(&c_null.expr, &b_enc, &format!("{label} [P IS NULL enc]"));
        assert_eq!(
            ids_null_flat, ids_null_frag,
            "{label}: FlatAll != FragmentedMasked for P IS NULL"
        );
        assert_eq!(
            ids_null_flat, ids_null_enc,
            "{label}: FlatAll != EncodedMixed for P IS NULL"
        );

        // 2. Full 3VL TLP partition disjointness + 100% completeness
        assert!(
            ids_p_flat.is_disjoint(&ids_not_flat),
            "{label}: P and NOT P overlap"
        );
        assert!(
            ids_p_flat.is_disjoint(&ids_null_flat),
            "{label}: P and P IS NULL overlap"
        );
        assert!(
            ids_not_flat.is_disjoint(&ids_null_flat),
            "{label}: NOT P and P IS NULL overlap"
        );
        let mut union = ids_p_flat.clone();
        union.extend(&ids_not_flat);
        union.extend(&ids_null_flat);
        assert_eq!(
            union, all_ids,
            "{label}: TLP partitions do not cover all rows"
        );

        // 3. Exact 3VL boolean lattice identities:
        //    keep(P AND Q) == keep(P) ∩ keep(Q), keep(P OR Q) == keep(P) ∪ keep(Q)
        let ids_q_flat = eval_filter_active_ids(&c_q.expr, &b_flat, &format!("{label} [Q flat]"));
        let ids_and_enc =
            eval_filter_active_ids(&c_and.expr, &b_enc, &format!("{label} [P AND Q enc]"));
        let ids_or_enc =
            eval_filter_active_ids(&c_or.expr, &b_enc, &format!("{label} [P OR Q enc]"));

        let expected_and: BTreeSet<i64> = ids_p_flat.intersection(&ids_q_flat).copied().collect();
        let expected_or: BTreeSet<i64> = ids_p_flat.union(&ids_q_flat).copied().collect();
        assert_eq!(
            ids_and_enc, expected_and,
            "{label}: (P AND Q) mismatch vs Q={q}"
        );
        assert_eq!(
            ids_or_enc, expected_or,
            "{label}: (P OR Q) mismatch vs Q={q}"
        );
    }

    // 4. Score-function filter check with `filter_planning_schema` across all 3 layout variants
    let score_filter =
        "score('quality') >= 0.5 AND (score_label('grade') = 'A' OR has_score('quality'))";
    let compiled_score = compile_filter(FilterView::Spans, &schema, score_filter)
        .expect("compile score filter failed");
    let planning_schema = Arc::new(
        filter_planning_schema(&schema, &compiled_score.scores)
            .expect("filter_planning_schema failed"),
    );
    assert_eq!(planning_schema.as_ref(), &compiled_score.planning_schema);

    // Append 3 score columns per score name (2 score names: "quality", "grade").
    let score_rows: Vec<Vec<ScalarValue>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut r = row.clone();
            // __score_0_* ("quality")
            let q_present = !i.is_multiple_of(3);
            r.push(if q_present {
                ScalarValue::F64((i % 10) as f64 * 0.12)
            } else {
                ScalarValue::Null(DataType::F64)
            });
            r.push(ScalarValue::Null(DataType::Utf8));
            r.push(ScalarValue::Bool(q_present));
            // __score_1_* ("grade")
            let g_present = i.is_multiple_of(2);
            r.push(ScalarValue::Null(DataType::F64));
            r.push(if g_present {
                ScalarValue::Utf8(if i.is_multiple_of(4) { "A" } else { "B" }.to_owned())
            } else {
                ScalarValue::Null(DataType::Utf8)
            });
            r.push(ScalarValue::Bool(g_present));
            r
        })
        .collect();

    let s_flat = collect_source_batches(&build_batch_source(
        &planning_schema,
        &score_rows,
        LayoutVariant::FlatAll,
        seed ^ 0xE7,
    ));
    let s_frag = collect_source_batches(&build_batch_source(
        &planning_schema,
        &score_rows,
        LayoutVariant::FragmentedMasked,
        seed ^ 0xE8,
    ));
    let s_enc = collect_source_batches(&build_batch_source(
        &planning_schema,
        &score_rows,
        LayoutVariant::EncodedMixed,
        seed ^ 0xE9,
    ));

    let ids_s_flat = eval_filter_active_ids(&compiled_score.expr, &s_flat, "score filter flat");
    let ids_s_frag = eval_filter_active_ids(&compiled_score.expr, &s_frag, "score filter frag");
    let ids_s_enc = eval_filter_active_ids(&compiled_score.expr, &s_enc, "score filter enc");
    assert_eq!(
        ids_s_flat, ids_s_frag,
        "score filter FlatAll != FragmentedMasked"
    );
    assert_eq!(
        ids_s_flat, ids_s_enc,
        "score filter FlatAll != EncodedMixed"
    );
}

// ---------------------------------------------------------------------------
// Deep-expression & multi-CTE stress on an explicit 2 MiB stack thread
// ---------------------------------------------------------------------------

#[test]
fn deep_expression_runs_on_2mib_stack() {
    let handle = std::thread::Builder::new()
        .name("qe-2mib-stack".to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let tables = build_adversarial_tables();
            let (catalog, sources) =
                make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, DEFAULT_SEED);
            let exec = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);

            // 1. Deep left-deep boolean OR/AND spine (80 terms)
            let mut bool_chain = String::from("k_small = 0");
            for i in 1usize..80 {
                let op = if i.is_multiple_of(2) { "OR" } else { "AND" };
                let rhs = (i as i64) - 40;
                bool_chain.push_str(&format!(" {op} k_small != {rhs}"));
            }
            let bool_sql = format!(
                "SELECT id AS c0 FROM t_main WHERE {bool_chain} ORDER BY c0 ASC NULLS LAST"
            );

            // 2. Deep arithmetic spine (60 terms)
            let mut arith_chain = String::from("COALESCE(k_small, 0)");
            for i in 1usize..60 {
                let op = if i.is_multiple_of(2) { "+" } else { "-" };
                arith_chain.push_str(&format!(" {op} {}", i % 7));
            }
            let arith_sql = format!(
                "SELECT id AS c0, ({arith_chain}) AS c1 FROM t_main ORDER BY c0 ASC NULLS LAST"
            );

            // 3. Nested CASE expression (16 levels, within PARSER_RECURSION_LIMIT = 50)
            let mut case_expr = String::from("0");
            for i in (1..=16).rev() {
                case_expr = format!("CASE WHEN id = {i} THEN {i} ELSE ({case_expr}) END");
            }
            let case_sql = format!(
                "SELECT id AS c0, {case_expr} AS c1 FROM t_main ORDER BY c0 ASC NULLS LAST"
            );

            // 4. Multi-CTE pipeline (8 chained CTEs)
            let mut cte_parts = vec!["c0 AS (SELECT id, grp, k_small FROM t_main)".to_owned()];
            for i in 1..8 {
                let prev = i - 1;
                cte_parts.push(format!(
                    "c{i} AS (SELECT id, grp, COALESCE(k_small, 0) + 1 AS k_small FROM c{prev})"
                ));
            }
            let cte_sql = format!(
                "WITH {} SELECT id AS c0, k_small AS c1 FROM c7 ORDER BY c0 ASC NULLS LAST",
                cte_parts.join(", ")
            );

            // 5. Deep UNION ALL spine (10 branches)
            let union_branches: Vec<String> = (0..10)
                .map(|i| format!("SELECT id AS c0, k_small AS c1 FROM t_main WHERE id = {i}"))
                .collect();
            let union_sql = format!(
                "{} ORDER BY c0 ASC NULLS LAST",
                union_branches.join(" UNION ALL ")
            );

            let ordered_opts = CompareOpts {
                ordered: true,
                unordered_arrays: false,
                float_rel_tol: 1e-9,
                float_abs_tol: 1e-12,
            };

            for (name, sql) in [
                ("bool_chain", bool_sql),
                ("arith_chain", arith_sql),
                ("nested_case", case_sql),
                ("multi_cte", cte_sql),
                ("union_spine", union_sql),
            ] {
                let unopt = run_sql_unoptimized(&catalog, &sources, &exec.ctx, &sql, None);
                assert_clean_ctx(&exec.ctx);
                let opt = run_sql_optimized(&catalog, &sources, &exec.ctx, &sql, None);
                assert_clean_ctx(&exec.ctx);
                assert!(
                    unopt.is_ok(),
                    "2mib {name}: expected Ok, got {:?}",
                    unopt.as_ref().err()
                );
                compare_results(&unopt, &opt, &ordered_opts, false, &format!("2mib {name}"));
            }

            // 6. Over-limit recursion returns clean Invalid error (never stack overflow / Internal)
            let deep_parens = format!(
                "SELECT {}1{} AS c0 FROM t_one",
                "(".repeat(65),
                ")".repeat(65)
            );
            let err = run_sql_optimized(&catalog, &sources, &exec.ctx, &deep_parens, None)
                .expect_err("expected recursion limit rejection");
            assert_clean_ctx(&exec.ctx);
            assert!(
                matches!(err, QueryError::Invalid(_)),
                "expected QueryError::Invalid for over-nested expression, got {err:?}"
            );
        })
        .expect("failed to spawn 2 MiB stack thread");

    handle.join().expect("2 MiB stack thread panicked");
}

// ---------------------------------------------------------------------------
// Dedicated `approx_percentile_cont` parallel accuracy check (1% relative tol)
// ---------------------------------------------------------------------------

#[test]
fn approx_percentile_cont_parallel_within_relative_tolerance() {
    let seed = metamorphic_seed();
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("grp", DataType::Utf8, true),
        Field::new("val", DataType::F64, true),
    ])
    .expect("valid approx_percentile schema");

    let mut rng = SplitMix64::new(seed ^ 0x9999_0001);
    let mut rows = Vec::with_capacity(400);
    for i in 0..400usize {
        let grp = match i % 4 {
            0 => ScalarValue::Utf8("fast".to_owned()),
            1 => ScalarValue::Utf8("slow".to_owned()),
            2 => ScalarValue::Utf8("mixed".to_owned()),
            _ => ScalarValue::Null(DataType::Utf8),
        };
        let val = if i.is_multiple_of(25) {
            ScalarValue::Null(DataType::F64)
        } else {
            let raw = rng.range_i64(-5000, 10000) as f64 / 10.0;
            ScalarValue::F64(raw)
        };
        rows.push(vec![ScalarValue::I64(i as i64), grp, val]);
    }

    let tables = vec![("t_pct".to_owned(), schema, rows)];
    let (cat_1p, src_1p) = make_table_catalog_and_sources(&tables, LayoutVariant::FlatAll, seed);
    let (cat_4p, src_4p) =
        make_table_catalog_and_sources(&tables, LayoutVariant::MultiChunkFlat(8), seed ^ 0x91);
    let (cat_enc, src_enc) =
        make_table_catalog_and_sources(&tables, LayoutVariant::EncodedMixed, seed ^ 0x92);

    let ctx_1p = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 1, 64);
    let ctx_4p = make_exec_ctx(32 * 1024 * 1024, 32 * 1024 * 1024, 4, 16);

    let pct_opts = CompareOpts {
        ordered: true,
        unordered_arrays: false,
        float_rel_tol: 0.01,
        float_abs_tol: 1e-6,
    };

    let queries = [
        "SELECT approx_percentile_cont(val, 0.0) AS c0, \
                approx_percentile_cont(val, 0.1) AS c1, \
                approx_percentile_cont(val, 0.5) AS c2, \
                approx_percentile_cont(val, 0.9) AS c3, \
                approx_percentile_cont(val, 0.95) AS c4, \
                approx_percentile_cont(val, 1.0) AS c5 \
         FROM t_pct",
        "SELECT grp AS c0, \
                approx_percentile_cont(val, 0.5) AS c1, \
                approx_percentile_cont(val, 0.95) AS c2 \
         FROM t_pct GROUP BY grp ORDER BY c0 ASC NULLS LAST",
    ];

    for sql in queries {
        let (_, r_1p) = run_sql_optimized(&cat_1p, &src_1p, &ctx_1p.ctx, sql, None)
            .unwrap_or_else(|e| panic!("approx_percentile 1p failed: {e:?}"));
        assert_clean_ctx(&ctx_1p.ctx);

        let (_, r_4p) = run_sql_optimized(&cat_4p, &src_4p, &ctx_4p.ctx, sql, None)
            .unwrap_or_else(|e| panic!("approx_percentile 4p failed: {e:?}"));
        assert_clean_ctx(&ctx_4p.ctx);

        let (_, r_enc) = run_sql_optimized(&cat_enc, &src_enc, &ctx_4p.ctx, sql, None)
            .unwrap_or_else(|e| panic!("approx_percentile enc-4p failed: {e:?}"));
        assert_clean_ctx(&ctx_4p.ctx);

        assert_rows_equiv(
            &r_1p,
            &r_4p,
            &pct_opts,
            &format!("approx_percentile 1p vs 4p sql={sql}"),
        );
        assert_rows_equiv(
            &r_1p,
            &r_enc,
            &pct_opts,
            &format!("approx_percentile 1p vs enc-4p sql={sql}"),
        );

        // Sanity check: confirm every row has at least one finite F64 output (not NaN/Inf)
        assert!(!r_1p.is_empty(), "approx_percentile_cont returned 0 rows");
        for row in &r_1p {
            let mut finite_count = 0usize;
            for cell in row {
                if let ScalarValue::F64(f) = cell {
                    assert!(
                        f.is_finite(),
                        "non-finite approx_percentile_cont output: {f}"
                    );
                    finite_count += 1;
                }
            }
            assert!(
                finite_count > 0,
                "expected at least one finite F64 output in approx_percentile_cont row"
            );
        }
    }
}
