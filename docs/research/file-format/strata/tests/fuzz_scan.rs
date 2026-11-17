//! Randomised scan oracle test (M4F, from the M3F-M4 review). Ignored by default: run with
//! `cargo test --release -- --ignored fuzz_scan`. Seeds: `STRATA_FUZZ_SEEDS` (default 40).

#![allow(clippy::needless_range_loop, clippy::single_range_in_vec_init)]

use std::ops::Range;
use std::sync::Arc;

use arrow::array::*;
use arrow::buffer::Buffer;
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::compute::{
    CastOptions, and_kleene, cast_with_options, filter_record_batch, is_not_null, is_null, not,
    or_kleene,
};
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

fn blocks(rng: &mut Rng, n: usize) -> Vec<(usize, usize)> {
    // (start,len) blocks
    let mut out = vec![];
    let mut at = 0;
    while at < n {
        let len = match rng.below(4) {
            0 => 1 + rng.below(20),
            1 => 1 + rng.below(300),
            2 => 1 + rng.below(1500),
            _ => 1 + rng.below(4000),
        }
        .min(n - at);
        out.push((at, len));
        at += len;
    }
    out
}

fn pool_f64() -> Vec<Option<f64>> {
    vec![
        None,
        Some(f64::NAN),
        Some(-0.0),
        Some(0.0),
        Some(1.0),
        Some(-1.0),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(1e-300),
        Some(-1e-300),
        Some(0.1),
        Some(16777217.0),
        Some(1e300),
    ]
}

fn gen_col<T: Clone>(rng: &mut Rng, n: usize, pool: &[Option<T>], mix: bool) -> Vec<Option<T>> {
    let mut v = Vec::with_capacity(n);
    for (_, len) in blocks(rng, n) {
        // pick a sub-pool of 1..3 values
        let k = 1 + rng.below(3);
        let sub: Vec<Option<T>> = (0..k)
            .map(|_| pool[rng.below(pool.len())].clone())
            .collect();
        for _ in 0..len {
            if mix || sub.len() > 1 {
                v.push(sub[rng.below(sub.len())].clone());
            } else {
                v.push(sub[0].clone());
            }
        }
    }
    v
}

fn make(seed: u64) -> (RecordBatch, Config) {
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let n = 1 + rng.below(9000);
    let mix = rng.below(2) == 0;
    let id = Int64Array::from_iter_values(0..n as i64);
    let f64s = Float64Array::from(gen_col(&mut rng, n, &pool_f64(), mix));
    let pf32: Vec<Option<f32>> = pool_f64()
        .into_iter()
        .map(|x| x.map(|v| v as f32))
        .collect();
    let f32s = Float32Array::from(gen_col(&mut rng, n, &pf32, mix));
    let pi: Vec<Option<i32>> = vec![
        None,
        Some(0),
        Some(-1),
        Some(1),
        Some(i32::MAX),
        Some(i32::MIN),
        Some(100),
        Some(-100),
        Some(7),
    ];
    let i32s = Int32Array::from(gen_col(&mut rng, n, &pi, mix));
    let pu: Vec<Option<u32>> = vec![
        None,
        Some(0),
        Some(1),
        Some(u32::MAX),
        Some(100),
        Some(1 << 31),
        Some(7),
    ];
    let u32s = UInt32Array::from(gen_col(&mut rng, n, &pu, mix));
    let pu8: Vec<Option<u8>> = vec![None, Some(0), Some(255), Some(128), Some(127), Some(7)];
    let u8s = UInt8Array::from(gen_col(&mut rng, n, &pu8, mix));
    let pi8: Vec<Option<i8>> = vec![None, Some(0), Some(-128), Some(127), Some(-1), Some(7)];
    let i8s = Int8Array::from(gen_col(&mut rng, n, &pi8, mix));
    let pu64: Vec<Option<u64>> = vec![None, Some(0), Some(u64::MAX), Some(1 << 63), Some(5)];
    let u64s = UInt64Array::from(gen_col(&mut rng, n, &pu64, mix));
    let pi64: Vec<Option<i64>> = vec![
        None,
        Some(0),
        Some(i64::MIN),
        Some(i64::MAX),
        Some(-5),
        Some(5),
    ];
    let i64s = Int64Array::from(gen_col(&mut rng, n, &pi64, mix));
    let pd: Vec<Option<i32>> = vec![None, Some(0), Some(-1), Some(19000), Some(i32::MAX)];
    let d32 = Date32Array::from(gen_col(&mut rng, n, &pd, mix));
    let pts: Vec<Option<i64>> = vec![
        None,
        Some(0),
        Some(-1),
        Some(1_700_000_000_000),
        Some(i64::MIN),
        Some(i64::MAX),
    ];
    let ts = TimestampMillisecondArray::from(gen_col(&mut rng, n, &pts, mix)).with_timezone("UTC");
    let pb: Vec<Option<bool>> = vec![None, Some(true), Some(false)];
    let bs = BooleanArray::from(gen_col(&mut rng, n, &pb, mix));
    let pf: Vec<Option<Vec<u8>>> = vec![
        None,
        Some(vec![0, 0, 0]),
        Some(vec![0, 0, 1]),
        Some(vec![255, 255, 255]),
        Some(vec![1, 2, 3]),
        Some(vec![0, 1, 0]),
    ];
    let fsb3 = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
        gen_col(&mut rng, n, &pf, mix).into_iter(),
        3,
    )
    .unwrap();
    let pf16: Vec<Option<Vec<u8>>> = vec![
        None,
        Some(vec![0; 16]),
        Some(vec![255; 16]),
        Some((0..16).collect()),
        Some((1..17).collect()),
        Some({
            let mut v = vec![0u8; 16];
            v[15] = 1;
            v
        }),
    ];
    let fsb16 = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
        gen_col(&mut rng, n, &pf16, mix).into_iter(),
        16,
    )
    .unwrap();
    let ps: Vec<Option<String>> = vec![
        None,
        Some("".into()),
        Some("a".into()),
        Some("b".into()),
        Some("ab".into()),
        Some("zzz".into()),
    ];
    let strs = StringArray::from(gen_col(&mut rng, n, &ps, mix));
    let _ = Buffer::from_vec(vec![0u8]);

    let fields = vec![
        Field::new("id", DataType::Int64, false),
        Field::new("f64", DataType::Float64, true),
        Field::new("f32", DataType::Float32, true),
        Field::new("i32", DataType::Int32, true),
        Field::new("u32", DataType::UInt32, true),
        Field::new("u8", DataType::UInt8, true),
        Field::new("i8", DataType::Int8, true),
        Field::new("u64", DataType::UInt64, true),
        Field::new("i64", DataType::Int64, true),
        Field::new("d32", DataType::Date32, true),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("b", DataType::Boolean, true),
        Field::new("fsb3", DataType::FixedSizeBinary(3), true),
        Field::new("fsb16", DataType::FixedSizeBinary(16), true),
        Field::new("s", DataType::Utf8, true),
    ];
    let schema = Arc::new(Schema::new(fields));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(id),
        Arc::new(f64s),
        Arc::new(f32s),
        Arc::new(i32s),
        Arc::new(u32s),
        Arc::new(u8s),
        Arc::new(i8s),
        Arc::new(u64s),
        Arc::new(i64s),
        Arc::new(d32),
        Arc::new(ts),
        Arc::new(bs),
        Arc::new(fsb3),
        Arc::new(fsb16),
        Arc::new(strs),
    ];
    let batch = RecordBatch::try_new(schema, cols).unwrap();
    let config = Config {
        chunk_max_rows: [256u32, 1000, 4096, 777][rng.below(4)],
        chunk_max_bytes: [512usize, 4096, 32768][rng.below(3)],
        ..Config::default()
    };
    (batch, config)
}

fn lit_for(rng: &mut Rng, batch: &RecordBatch, col: usize) -> Lit {
    // take a value of the column mostly, else perturbed
    let arr = batch.column(col);
    let row = rng.below(arr.len());
    if rng.below(12) == 0 {
        return Lit::Null;
    }
    if arr.is_null(row) {
        return match arr.data_type() {
            DataType::Boolean => Lit::Bool(true),
            DataType::Utf8 => Lit::Utf8("a".into()),
            DataType::FixedSizeBinary(w) => Lit::Bytes(vec![0; *w as usize]),
            DataType::Float32 | DataType::Float64 => Lit::Float(0.0),
            _ => Lit::Int(0),
        };
    }
    let d = rng.below(3) as i64 - 1;
    match arr.data_type() {
        DataType::Boolean => Lit::Bool(arr.as_boolean().value(row)),
        DataType::Utf8 => {
            let s = arr.as_string::<i32>().value(row).to_string();
            if rng.below(4) == 0 {
                Lit::Utf8(format!("{s}a"))
            } else {
                Lit::Utf8(s)
            }
        }
        DataType::FixedSizeBinary(_) => {
            let mut b = arr.as_fixed_size_binary().value(row).to_vec();
            if rng.below(3) == 0 {
                let l = b.len();
                b[l - 1] = b[l - 1].wrapping_add(1);
            }
            Lit::Bytes(b)
        }
        DataType::Float64 => {
            let v = arr.as_primitive::<Float64Type>().value(row);
            Lit::Float(if rng.below(5) == 0 { v + 1e-17 } else { v })
        }
        DataType::Float32 => {
            let v = arr.as_primitive::<Float32Type>().value(row) as f64;
            match rng.below(4) {
                0 => Lit::Float(v * (1.0 + 1e-9)),
                1 => Lit::Float(v),
                2 => Lit::Float(0.1),
                _ => Lit::Int(16777217),
            }
        }
        DataType::Int32 => Lit::Int(arr.as_primitive::<Int32Type>().value(row) as i64 + d),
        DataType::Int8 => Lit::Int(arr.as_primitive::<Int8Type>().value(row) as i64 + d),
        DataType::Int64 => Lit::Int(arr.as_primitive::<Int64Type>().value(row).saturating_add(d)),
        DataType::UInt32 => Lit::Int(arr.as_primitive::<UInt32Type>().value(row) as i64 + d),
        DataType::UInt8 => Lit::Int(arr.as_primitive::<UInt8Type>().value(row) as i64 + d),
        DataType::UInt64 => Lit::UInt(
            arr.as_primitive::<UInt64Type>()
                .value(row)
                .saturating_add_signed(d),
        ),
        DataType::Date32 => Lit::Int(arr.as_primitive::<Date32Type>().value(row) as i64 + d),
        DataType::Timestamp(_, _) => Lit::Int(
            arr.as_primitive::<TimestampMillisecondType>()
                .value(row)
                .saturating_add(d),
        ),
        _ => Lit::Null,
    }
}

fn gen_pred(rng: &mut Rng, batch: &RecordBatch, depth: usize) -> Predicate {
    let ncols = batch.num_columns();
    let kind = rng.below(if depth == 0 { 8 } else { 12 });
    let ops = [
        CmpOp::Eq,
        CmpOp::NotEq,
        CmpOp::Lt,
        CmpOp::LtEq,
        CmpOp::Gt,
        CmpOp::GtEq,
    ];
    match kind {
        0..=4 => {
            let c = rng.below(ncols);
            let l = lit_for(rng, batch, c);
            Predicate::Cmp(c, ops[rng.below(6)], l)
        }
        5 => {
            let c = rng.below(ncols);
            let k = rng.below(4);
            Predicate::In(c, (0..k).map(|_| lit_for(rng, batch, c)).collect())
        }
        6 => Predicate::IsNull(rng.below(ncols)),
        7 => Predicate::IsNotNull(rng.below(ncols)),
        8 | 9 => Predicate::And(
            (0..rng.below(4))
                .map(|_| gen_pred(rng, batch, depth - 1))
                .collect(),
        ),
        10 => Predicate::Or(
            (0..rng.below(4))
                .map(|_| gen_pred(rng, batch, depth - 1))
                .collect(),
        ),
        _ => Predicate::Not(Box::new(gen_pred(rng, batch, depth - 1))),
    }
}

fn lit_arr(dt: &DataType, lit: &Lit) -> Option<ArrayRef> {
    let o = CastOptions {
        safe: false,
        ..Default::default()
    };
    match lit {
        Lit::Null => Some(new_null_array(dt, 1)),
        Lit::Bool(b) => Some(Arc::new(BooleanArray::from(vec![*b]))),
        Lit::Int(v) => {
            cast_with_options(&(Arc::new(Int64Array::from(vec![*v])) as ArrayRef), dt, &o).ok()
        }
        Lit::UInt(v) => {
            cast_with_options(&(Arc::new(UInt64Array::from(vec![*v])) as ArrayRef), dt, &o).ok()
        }
        Lit::Float(v) => cast_with_options(
            &(Arc::new(Float64Array::from(vec![*v])) as ArrayRef),
            dt,
            &o,
        )
        .ok(),
        Lit::Utf8(s) => Some(Arc::new(StringArray::from(vec![s.as_str()]))),
        Lit::Bytes(b) => match dt {
            DataType::FixedSizeBinary(_) => Some(Arc::new(
                FixedSizeBinaryArray::try_from_iter(std::iter::once(b.as_slice())).unwrap(),
            )),
            _ => None,
        },
    }
}

fn oracle(p: &Predicate, batch: &RecordBatch) -> Option<BooleanArray> {
    let n = batch.num_rows();
    Some(match p {
        Predicate::And(t) => {
            let mut acc = BooleanArray::from(vec![true; n]);
            for x in t {
                acc = and_kleene(&acc, &oracle(x, batch)?).unwrap();
            }
            acc
        }
        Predicate::Or(t) => {
            let mut acc = BooleanArray::from(vec![false; n]);
            for x in t {
                acc = or_kleene(&acc, &oracle(x, batch)?).unwrap();
            }
            acc
        }
        Predicate::Not(x) => not(&oracle(x, batch)?).unwrap(),
        Predicate::Cmp(c, op, l) => {
            let col = batch.column(*c);
            let la = lit_arr(col.data_type(), l)?;
            let s = Scalar::new(la);
            let a: &dyn Array = col.as_ref();
            match op {
                CmpOp::Eq => eq(&a, &s),
                CmpOp::NotEq => neq(&a, &s),
                CmpOp::Lt => lt(&a, &s),
                CmpOp::LtEq => lt_eq(&a, &s),
                CmpOp::Gt => gt(&a, &s),
                CmpOp::GtEq => gt_eq(&a, &s),
            }
            .ok()?
        }
        Predicate::In(c, ls) => {
            let col = batch.column(*c);
            let mut acc = BooleanArray::from(vec![false; n]);
            for l in ls {
                let la = lit_arr(col.data_type(), l)?;
                let s = Scalar::new(la);
                let a: &dyn Array = col.as_ref();
                acc = or_kleene(&acc, &eq(&a, &s).ok()?).unwrap();
            }
            acc
        }
        Predicate::IsNull(c) => is_null(batch.column(*c).as_ref()).unwrap(),
        Predicate::IsNotNull(c) => is_not_null(batch.column(*c).as_ref()).unwrap(),
        _ => return None,
    })
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
#[ignore]
fn fuzz_scan() {
    let seeds: u64 = std::env::var("STRATA_FUZZ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in 1..=seeds {
        let (batch, config) = make(seed);
        let mut w = StrataWriter::new(batch.schema(), &["id"], config).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("f.strata");
        let n = batch.num_rows();
        let file = rt().block_on(async {
            store.put(&path, PutPayload::from(bytes)).await.unwrap();
            StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap()
        });
        let mut rng = Rng(seed * 7919 + 13);
        for q in 0..60 {
            let p = gen_pred(&mut rng, &batch, 3);
            let want_mask = oracle(&p, &batch);
            // random ranges
            let ranges: Option<Vec<Range<u32>>> = if rng.below(3) == 0 {
                let mut r = vec![];
                let mut at = 0usize;
                while at < n && r.len() < 5 {
                    let s = at + rng.below(n / 3 + 1);
                    if s >= n {
                        break;
                    }
                    let e = (s + 1 + rng.below(n / 2 + 1)).min(n);
                    r.push(s as u32..e as u32);
                    at = e + 1;
                }
                Some(r)
            } else {
                None
            };
            let limit = if rng.below(4) == 0 {
                Some(rng.below(50))
            } else {
                None
            };
            let projection = vec![0usize, 1 + rng.below(14)];
            let req = ScanRequest {
                projection: projection.clone(),
                predicate: Some(p.clone()),
                row_ranges: ranges.clone(),
                limit,
                kind: if rng.below(2) == 0 {
                    ScanKind::Scan
                } else {
                    ScanKind::Point
                },
                views: false,
            };
            let got = rt().block_on(file.scan(&req));
            match (&got, &want_mask) {
                (Err(_), None) => continue,
                (Err(e), Some(_)) => {
                    panic!("seed {seed} q {q}: scan error {e:?} but oracle ok for {p:?}")
                }
                (Ok(_), None) => panic!("seed {seed} q {q}: oracle err but scan ok {p:?}"),
                (Ok(batches), Some(mask)) => {
                    let mut keep = vec![false; n];
                    let mut cnt = 0;
                    for i in 0..n {
                        let inr = ranges.as_ref().is_none_or(|rs| {
                            rs.iter()
                                .any(|r| (r.start as usize..r.end as usize).contains(&i))
                        });
                        if inr && mask.is_valid(i) && mask.value(i) {
                            if limit.is_none_or(|l| cnt < l) {
                                keep[i] = true;
                            }
                            cnt += 1;
                        }
                    }
                    let want = filter_record_batch(
                        &batch.project(&projection).unwrap(),
                        &BooleanArray::from(keep),
                    )
                    .unwrap();
                    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
                    assert_eq!(
                        total,
                        want.num_rows(),
                        "seed {seed} q {q} rows for {p:?} ranges {ranges:?} limit {limit:?}"
                    );
                    if total > 0 {
                        let got = arrow::compute::concat_batches(&want.schema(), batches).unwrap();
                        for c in 0..2 {
                            assert_eq!(
                                got.column(c).as_ref(),
                                want.column(c).as_ref(),
                                "seed {seed} q {q} col {c} {p:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
