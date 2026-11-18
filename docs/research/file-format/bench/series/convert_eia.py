"""Converts an EIA bulk file (one JSON series per line) into the layout the
columnar-format-benchmarks scripts read: <out>/<DS>/year=YYYY/data.parquet with
series_id (dictionary), period (EIA string), period_norm (string), value (f64).

Rows keep the file order: series by series, each series newest first, as EIA ships them.
Observations whose value is not a number are dropped.
"""
import json, sys, zipfile
from pathlib import Path

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq


def norm(p):
    if len(p) == 4:
        return f"{p}-01-01"
    if len(p) == 6 and p[4] == "Q":
        return f"{p[:4]}-Q{p[5]}-01"
    if len(p) == 6:
        return f"{p[:4]}-{p[4:6]}-01"
    if len(p) >= 8:
        return f"{p[:4]}-{p[4:6]}-{p[6:8]}"
    raise ValueError(f"unknown period format of length {len(p)}")


def main(zip_path, ds, out):
    chunks, sid, per, pn, val = [], [], [], [], []
    series = dropped = 0

    def flush():
        if sid:
            chunks.append(pa.table({
                "series_id": pa.array(sid, pa.string()),
                "period": pa.array(per, pa.string()),
                "period_norm": pa.array(pn, pa.string()),
                "value": pa.array(val, pa.float64()),
            }))
            for l in (sid, per, pn, val):
                l.clear()

    with zipfile.ZipFile(zip_path) as z, z.open(z.namelist()[0]) as f:
        for line in f:
            rec = json.loads(line)
            data = rec.get("data")
            if not data or "series_id" not in rec:
                continue
            series += 1
            s = rec["series_id"]
            for p, v in data:
                if not isinstance(v, (int, float)) or isinstance(v, bool):
                    dropped += 1
                    continue
                sid.append(s); per.append(p); pn.append(norm(p)); val.append(float(v))
            if len(sid) >= 2_000_000:
                flush()
    flush()
    t = pa.concat_tables(chunks).combine_chunks()
    print(f"{ds}: {series:,} series, {t.num_rows:,} rows, {dropped:,} non-numeric values dropped", flush=True)
    t = t.set_column(0, "series_id", pc.dictionary_encode(t.column("series_id")))
    years = pc.utf8_slice_codeunits(t.column("period"), 0, 4)
    for y in sorted(set(years.to_pylist())):
        part = t.filter(pc.equal(years, y))
        d = Path(out) / ds / f"year={y}"
        d.mkdir(parents=True, exist_ok=True)
        pq.write_table(part, d / "data.parquet", compression="zstd")
    # One three-column file per dataset for format_benchmark.py and flight_e2e.py.
    pq.write_table(t.select(["series_id", "period", "value"]), Path(out) / f"{ds}.parquet", compression="zstd")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], sys.argv[3])
