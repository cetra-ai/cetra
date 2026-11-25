//! Randomised lookup and scan oracle test (M4F, from the M3F-M4 review). Ignored by default: run
//! with `cargo test --release -- --ignored fuzz_lookup`. Seeds: `STRATA_FUZZ_SEEDS` (default 40).

#![allow(clippy::needless_range_loop, clippy::single_range_in_vec_init)]

use std::ops::Range;
use std::sync::Arc;

use arrow::array::*;
use arrow::compute::kernels::cmp::{eq, gt, lt};
use arrow::compute::{and_kleene, filter_record_batch, not, or_kleene};
use arrow::datatypes::*;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use strata::*;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

const COLS: [&str; 8] = ["trace_id", "ts", "i", "q", "p", "attrs", "fl", "s"];

fn make(seed: u64) -> (RecordBatch, Config, Vec<Vec<u8>>) {
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    // groups
    let sizes = [
        1usize, 1, 2, 3, 5, 17, 100, 700, 1000, 4095, 4096, 4097, 6000,
    ];
    let mut n = 0;
    let mut ids: Vec<u8> = vec![];
    let mut keys = vec![];
    let mut g = 0u128;
    let target = 3000 + rng.below(12000);
    while n < target {
        let sz = sizes[rng.below(sizes.len())];
        g += 1 + rng.below(3) as u128;
        let key = g.to_be_bytes().to_vec();
        keys.push(key.clone());
        for _ in 0..sz {
            ids.extend_from_slice(&key);
        }
        n += sz;
    }
    let trace =
        FixedSizeBinaryArray::try_new(16, arrow::buffer::Buffer::from_vec(ids), None).unwrap();
    let ts = TimestampNanosecondArray::from_iter_values((0..n as i64).map(|i| i * 10))
        .with_timezone("UTC");
    let i = Int64Array::from_iter((0..n).map(|k| {
        if rng.below(5) == 0 {
            None
        } else {
            Some((k / 50) as i64)
        }
    }));
    let words = ["alpha", "beta", "gamma", "delta"];
    let q = StringArray::from_iter((0..n).map(|k| {
        if (k / 30) % 7 == 0 {
            None
        } else {
            Some(words[(k / 40) % 4])
        }
    }));
    let p = StringArray::from_iter((0..n).map(|k| {
        if rng.below(6) == 0 {
            None
        } else {
            let len = 280 + rng.below(300);
            let mut s = String::new();
            for _ in 0..len {
                s.push((b'a' + rng.below(4) as u8) as char);
            }
            if k % 97 == 0 {
                s.insert_str(rng.below(s.len()), "NEEDLE-xyz");
            }
            Some(s)
        }
    }));
    let fl = BooleanArray::from_iter((0..n).map(|k| Some(k % 3 == 0)));
    let s = StringArray::from_iter((0..n).map(|k| Some(format!("s{}", k % 11))));
    let mut attrs = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    for r in 0..n {
        if rng.below(13) == 0 {
            attrs.append(false).unwrap();
            continue;
        }
        let m = rng.below(6);
        for _ in 0..m {
            let key = format!("k{}", rng.below(5));
            attrs.keys().append_value(key);
            if rng.below(6) == 0 {
                attrs.values().append_null();
            } else {
                attrs
                    .values()
                    .append_value(format!("v{}", (r / 100 + rng.below(2)) % 5));
            }
        }
        attrs.append(true).unwrap();
    }
    let attrs = attrs.finish();
    let fields = vec![
        Field::new("trace_id", DataType::FixedSizeBinary(16), false),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("i", DataType::Int64, true),
        Field::new("q", DataType::Utf8, true),
        Field::new("p", DataType::Utf8, true),
        Field::new("attrs", attrs.data_type().clone(), true),
        Field::new("fl", DataType::Boolean, false),
        Field::new("s", DataType::Utf8, false),
    ];
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        vec![
            Arc::new(trace),
            Arc::new(ts),
            Arc::new(i),
            Arc::new(q),
            Arc::new(p),
            Arc::new(attrs),
            Arc::new(fl),
            Arc::new(s),
        ],
    )
    .unwrap();
    let config = Config {
        chunk_max_rows: [256u32, 1000, 4096][rng.below(3)],
        chunk_max_bytes: [1024usize, 8192, 32768][rng.below(3)],
        ..Config::default()
    };
    (batch, config, keys)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn map_mask(map: &MapArray, key: &str, lit: &Lit) -> BooleanArray {
    let keys = map.entries().column(0).as_string::<i32>();
    let vals = map.entries().column(1).as_string::<i32>();
    let o = map.value_offsets();
    let mut out = vec![];
    for r in 0..map.len() {
        if map.is_null(r) {
            out.push(None);
            continue;
        }
        let (a, b) = (o[r] as usize, o[r + 1] as usize);
        let hits: Vec<usize> = (a..b).filter(|&k| keys.value(k) == key).collect();
        let v = match (&lit, hits.as_slice()) {
            (Lit::Utf8(l), [h]) if vals.is_valid(*h) => Some(vals.value(*h) == l),
            _ => None,
        };
        out.push(v);
    }
    BooleanArray::from(out)
}

fn gen_pred(rng: &mut Rng, depth: usize) -> Predicate {
    let kind = rng.below(if depth == 0 { 6 } else { 9 });
    match kind {
        0 => Predicate::Cmp(
            2,
            [CmpOp::Eq, CmpOp::Lt, CmpOp::Gt][rng.below(3)],
            Lit::Int(rng.below(300) as i64),
        ),
        1 => Predicate::Cmp(
            3,
            CmpOp::Eq,
            Lit::Utf8(["alpha", "beta", "gamma", "delta", "zeta"][rng.below(5)].into()),
        ),
        2 => Predicate::Contains(
            4,
            if rng.below(4) == 0 {
                vec![]
            } else {
                [
                    b"NEEDLE-xyz".to_vec(),
                    b"abca".to_vec(),
                    b"NEEDLE-xyzq".to_vec(),
                    b"E-x".to_vec(),
                    b"zz".to_vec(),
                    b"ab".to_vec(),
                    b"a".to_vec(),
                ][rng.below(7)]
                .clone()
            },
        ),
        3 => Predicate::MapKeyEq(
            5,
            format!("k{}", rng.below(6)),
            if rng.below(8) == 0 {
                Lit::Null
            } else {
                Lit::Utf8(format!("v{}", rng.below(6)))
            },
        ),
        4 => Predicate::Cmp(
            1,
            [CmpOp::Lt, CmpOp::Gt][rng.below(2)],
            Lit::Int(rng.below(150000) as i64),
        ),
        5 => Predicate::IsNull(3),
        6 => Predicate::And(
            (0..rng.below(4))
                .map(|_| gen_pred(rng, depth - 1))
                .collect(),
        ),
        7 => Predicate::Or(
            (0..rng.below(4))
                .map(|_| gen_pred(rng, depth - 1))
                .collect(),
        ),
        _ => Predicate::Not(Box::new(gen_pred(rng, depth - 1))),
    }
}

fn oracle(p: &Predicate, full: &RecordBatch) -> BooleanArray {
    let n = full.num_rows();
    match p {
        Predicate::And(t) => t.iter().fold(BooleanArray::from(vec![true; n]), |a, x| {
            and_kleene(&a, &oracle(x, full)).unwrap()
        }),
        Predicate::Or(t) => t.iter().fold(BooleanArray::from(vec![false; n]), |a, x| {
            or_kleene(&a, &oracle(x, full)).unwrap()
        }),
        Predicate::Not(x) => not(&oracle(x, full)).unwrap(),
        Predicate::Cmp(c, op, l) => {
            let col = full.column(*c);
            let a: &dyn Array = col.as_ref();
            match (col.data_type(), l) {
                (DataType::Int64, Lit::Int(v)) => {
                    let s = Scalar::new(Int64Array::from(vec![*v]));
                    match op {
                        CmpOp::Eq => eq(&a, &s),
                        CmpOp::Lt => lt(&a, &s),
                        _ => gt(&a, &s),
                    }
                    .unwrap()
                }
                (DataType::Timestamp(_, _), Lit::Int(v)) => {
                    let s =
                        Scalar::new(TimestampNanosecondArray::from(vec![*v]).with_timezone("UTC"));
                    match op {
                        CmpOp::Eq => eq(&a, &s),
                        CmpOp::Lt => lt(&a, &s),
                        _ => gt(&a, &s),
                    }
                    .unwrap()
                }
                (DataType::Utf8, Lit::Utf8(v)) => {
                    let s = Scalar::new(StringArray::from(vec![v.as_str()]));
                    eq(&a, &s).unwrap()
                }
                _ => panic!(),
            }
        }
        Predicate::IsNull(c) => arrow::compute::is_null(full.column(*c).as_ref()).unwrap(),
        Predicate::Contains(c, nd) => {
            let col = full.column(*c).as_string::<i32>();
            BooleanArray::from(
                (0..n)
                    .map(|r| {
                        col.is_valid(r)
                            && (nd.is_empty()
                                || col
                                    .value(r)
                                    .as_bytes()
                                    .windows(nd.len())
                                    .any(|w| w == nd.as_slice()))
                    })
                    .collect::<Vec<_>>(),
            )
        }
        Predicate::MapKeyEq(c, k, l) => map_mask(
            full.column(*c).as_any().downcast_ref::<MapArray>().unwrap(),
            k,
            l,
        ),
        _ => panic!(),
    }
}

async fn open(bytes: Vec<u8>) -> StrataFile {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("f.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    StrataFile::open(store, path, None, &Config::default())
        .await
        .unwrap()
}

#[test]
#[ignore]
fn fuzz_lookup() {
    let seeds: u64 = std::env::var("STRATA_FUZZ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in 1..=seeds {
        let (batch, config, keys) = make(seed);
        let mut w = StrataWriter::new(batch.schema(), &["trace_id", "ts"], config).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let n = batch.num_rows();
        let mut rng = Rng(seed * 31 + 5);
        let file = rt().block_on(open(bytes.clone()));
        let mut cols = vec![];
        for c in 0..batch.num_columns() {
            cols.push(rt().block_on(read_column(&file, c)).unwrap());
        }
        let full = RecordBatch::try_new(batch.schema(), cols).unwrap();
        // lookups, cold and warm
        for q in 0..25 {
            let file2;
            let f = if rng.below(3) == 0 {
                file2 = rt().block_on(open(bytes.clone()));
                &file2
            } else {
                &file
            };
            let key = if rng.below(8) == 0 {
                (u128::MAX / 3).to_be_bytes().to_vec()
            } else {
                keys[rng.below(keys.len())].clone()
            };
            let np = 1 + rng.below(5);
            let proj: Vec<usize> = (0..np).map(|_| rng.below(8)).collect();
            let got = rt().block_on(f.lookup_scan(&key, &proj, false)).unwrap();
            let keep = BooleanArray::from(
                (0..n)
                    .map(|r| full.column(0).as_fixed_size_binary().value(r) == key.as_slice())
                    .collect::<Vec<_>>(),
            );
            let want = filter_record_batch(&full.project(&proj).unwrap(), &keep).unwrap();
            let total: usize = got.iter().map(|b| b.num_rows()).sum();
            assert_eq!(
                total,
                want.num_rows(),
                "seed {seed} lookup {q} key {key:?} proj {proj:?}"
            );
            if total > 0 {
                let g = arrow::compute::concat_batches(&want.schema(), &got).unwrap();
                for c in 0..np {
                    assert_eq!(
                        g.column(c).as_ref(),
                        want.column(c).as_ref(),
                        "seed {seed} lookup {q} col {c}"
                    );
                }
            }
        }
        // scans
        for q in 0..25 {
            let p = gen_pred(&mut rng, 3);
            let mask = oracle(&p, &full);
            let ranges: Option<Vec<Range<u32>>> = if rng.below(3) == 0 {
                let a = rng.below(n);
                let b = (a + 1 + rng.below(n - a)).min(n);
                let c = (b + rng.below(5)).min(n);
                let d = (c + 1 + rng.below(n / 2)).min(n);
                let mut r = vec![a as u32..b as u32];
                if d > c {
                    r.push(c as u32..d as u32);
                }
                Some(r)
            } else {
                None
            };
            let limit = if rng.below(4) == 0 {
                Some(1 + rng.below(40))
            } else {
                None
            };
            let np = 1 + rng.below(4);
            let proj: Vec<usize> = (0..np).map(|_| rng.below(8)).collect();
            let req = ScanRequest {
                projection: proj.clone(),
                predicate: Some(p.clone()),
                row_ranges: ranges.clone(),
                limit,
                kind: ScanKind::Scan,
                views: false,
                top: None,
                row_filter: None,
            };
            let f = if rng.below(2) == 0 {
                &file
            } else {
                &rt().block_on(open(bytes.clone()))
            };
            let got = rt()
                .block_on(f.scan(&req))
                .unwrap_or_else(|e| panic!("seed {seed} q {q} {p:?}: {e:?}"));
            let mut keep = vec![false; n];
            let mut cnt = 0;
            for r in 0..n {
                let inr = ranges.as_ref().is_none_or(|rs| {
                    rs.iter()
                        .any(|x| (x.start as usize..x.end as usize).contains(&r))
                });
                if inr && mask.is_valid(r) && mask.value(r) {
                    if limit.is_none_or(|l| cnt < l) {
                        keep[r] = true;
                    }
                    cnt += 1;
                }
            }
            let want =
                filter_record_batch(&full.project(&proj).unwrap(), &BooleanArray::from(keep))
                    .unwrap();
            let total: usize = got.iter().map(|b| b.num_rows()).sum();
            assert_eq!(
                total,
                want.num_rows(),
                "seed {seed} q {q} {p:?} ranges {ranges:?} limit {limit:?} proj {proj:?}"
            );
            if total > 0 {
                let g = arrow::compute::concat_batches(&want.schema(), &got).unwrap();
                for c in 0..np {
                    assert_eq!(
                        g.column(c).as_ref(),
                        want.column(c).as_ref(),
                        "seed {seed} q {q} col {c} {p:?}"
                    );
                }
            }
        }
        let _ = COLS;
    }
}
