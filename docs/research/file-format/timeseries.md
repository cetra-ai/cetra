# Strata on time-series data: the columnar-format-benchmarks tests

[balicat/columnar-format-benchmarks](https://github.com/balicat/columnar-format-benchmarks) compares Parquet, Vortex 0.75 and Arrow IPC on about 73M rows of energy time series (`series_id | period | value`). This note runs the same tests on `ubuntu-vm`:

- their Python scripts, unchanged, on Parquet, Vortex and Arrow IPC;
- the same tests on Strata, Parquet and Vortex through `fmtbench series`, in Rust.

Their S3 tests were not run.

**Summary.** The first run found four gaps in Strata, set out under [Why Strata lost here](#why-strata-lost-here). Two of them are now fixed:
- **String zones.** Chunks of strings stored without a dictionary now have min/max zones.
- **Row count.** DataFusion now gets the row count of a scan.

**After the fixes:**
- **Queries:**
  - `count(*)` is the fastest of all formats, at 0.3 ms.
  - A one-series query on sorted data is as fast as Parquet on PET (2.8 against 3.4 ms). On ELEC it is 2.5× Parquet (10.1 against 4.0 ms), against 110× before.
  - Unsorted, Strata now beats Parquet on that query (137 against 202 ms on ELEC; 30 against 87 ms on PET), but stays behind Vortex (51 and 18 ms).
- **Unchanged, from the two gaps still open:**
  - **Size:** unsorted, Strata is 1–11% bigger than Parquet and 28–32% bigger than vx_compact. Sorted, it is 37% (ELEC) to 137% (PET) bigger than the smallest file.
  - **Full reads:** 1.3–5× slower than Parquet, and 2.5–4.5× slower than the faster Vortex variant.
  - **Aggregates:** 1.5–2× slower.

Strata was designed around LLM traces, with an id sort key and text payloads; none of that is used here.

## Data

Their dataset is not public. The datasets `PET` and `ELEC` it is built from are the US Energy Information Administration's public bulk files:
- `https://api.eia.gov/bulk/PET.zip`: 55 MB, downloaded on 2026-11-18;
- `https://api.eia.gov/bulk/ELEC.zip`: 302 MB.

[`bench/series/convert_eia.py`](bench/series/convert_eia.py) converts each file into the layout their `schema_opt_bench.py` reads: `<DS>/year=YYYY/data.parquet` with `series_id` (dictionary), `period` (EIA string), `period_norm` and `value` (f64). Rows keep EIA's order, series by series with the newest observation first. Non-numeric values are dropped.

| Dataset | Rows | Series | Dropped values |
|---|---|---|---|
| PET | 13,901,002 | 174,940 | 3,391,224 |
| ELEC | 61,121,330 | 784,932 | 353,148 |

That is 75M rows in all, close to their 73M. It is not their data: compare ratios between formats, not absolute numbers.

## Setup

- **Machine.** `ubuntu-vm`, 12 vCPUs, 94 GB of RAM, local disk, warm OS cache. Theirs was a Minisforum MS-02 (Core Ultra 5, NVMe).
- **Their scripts.** Python 3.13 with the versions their README names: pyarrow 24.0.0, Vortex 0.75.0, DuckDB 1.5.6, DataFusion 54.1.0 and Polars 1.40.1. The latest DuckDB (2.0) removed `Relation.fetchone()`, which breaks `format_benchmark.py`.
- **Their runs.**
  - `format_benchmark.py --runs 3` on `PET.parquet` and `ELEC.parquet`;
  - `schema_opt_bench.py`, with only its data path changed;
  - `flight_e2e.py` on ELEC with 5 runs. pyarrow cannot sort a dictionary column, so this script reads a copy of ELEC with `series_id` as plain strings.
- **`fmtbench series <data> <out> 5`** ([`bench/src/series.rs`](bench/src/series.rs)) builds the four schema variants of `schema_opt_bench.py` in Rust. It writes each variant as one file per format:
  - `pq` is arrow-rs with pyarrow's settings: zstd level 1, dictionaries on, row groups of 1 Mi rows;
  - `vx_default` and `vx_compact` are Vortex from the same checkout as the rest of the research;
  - `st` is Strata with `Config::default()`.

  Each file is then read through DataFusion 55 with 12 partitions: a full `SELECT *`, then the three queries of `format_benchmark.py`:
  - `SELECT count(*)`;
  - an average per period (`GROUP BY period`);
  - every row of one series (`WHERE series_id = '…'`), over 20 series spread across the dataset, timed per series.

  Times are the median and minimum of 5 runs after a warm-up; `read cpu` is the median process CPU time.
- **Strata's row order.** A Strata file is normally sorted by its sort key, and the first sort-key column must be an id, an integer or a timestamp; `series_id` is a string. So the writer now takes an empty sort key and keeps the rows in the order given, as the other formats do (prototype plan § Row order). The sorted variant `v3_d32sort` adds a key filter on `series_id`.

## Their scripts, reproduced

**`format_benchmark.py`**, median of 3 runs, ms. "Project" reads a random sample of 10 columns; with 3 columns, that is all of them.

| | Format | MB | Write | Read | Project |
|---|---|---|---|---|---|
| PET | arrow_ipc_raw | 311.9 | 753 | 150 | 66 |
| | arrow_ipc_zstd | 68.5 | 1,139 | 272 | 208 |
| | parquet_zstd | **38.6** | 1,283 | 319 | 301 |
| | vortex | 62.7 | 3,247 | 540 | 363 |
| ELEC | arrow_ipc_raw | 1,367.0 | 32,333 | 387 | 252 |
| | arrow_ipc_zstd | 387.1 | 33,504 | 1,204 | 902 |
| | parquet_zstd | 403.9 | 13,478 | 5,659 | 5,658 |
| | vortex | **331.0** | 10,386 | 1,221 | 430 |

Query engines on the source Parquet, ms:

| | Engine | scan_count | point_series | agg_by_time |
|---|---|---|---|---|
| PET | DuckDB | 1.0 | 30.6 | 56.9 |
| | DataFusion | 1.1 | 121.4 | 59.5 |
| ELEC | DuckDB | 1.2 | 148.2 | 185.1 |
| | DataFusion | 1.3 | 1,249.3 | 249.1 |

**`flight_e2e.py`** on ELEC, ms, median of 5 (time to first batch / total):

| Query | arrow_mem | arrow_idx | parquet | vortex |
|---|---|---|---|---|
| full scan | 52 / 2,257 | – | 1,567 / 2,651 | 384 / 1,205 |
| one series (35 rows) | 76 / 76 | **0.6 / 0.6** | 520 / 520 | 24 / 24 |

**`schema_opt_bench.py`**, MB and read ms:

| | Variant | arrow_ipc_raw | arrow_ipc_zstd | parquet_zstd | vortex |
|---|---|---|---|---|---|
| PET | v0_today | 506.5 / 91 | 122.7 / 233 | 47.7 / 310 | 114.3 / 237 |
| | v1_3col | 311.9 / 60 | 76.9 / 220 | 45.8 / 273 | 96.8 / 160 |
| | v2_date32 | 227.2 / 45 | 37.5 / 223 | 45.8 / 238 | 96.4 / 149 |
| | v3_d32sort | 227.2 / 47 | **29.2** / 186 | 38.5 / 217 | 62.0 / 87 |
| ELEC | v0_today | 2,222.8 / 380 | 606.4 / 1,405 | 423.1 / 6,557 | 475.5 / 901 |
| | v1_3col | 1,367.0 / 250 | 409.9 / 1,179 | 420.9 / 6,328 | 443.3 / 769 |
| | v2_date32 | 1,007.4 / 191 | 236.1 / 1,255 | 420.6 / 6,092 | 477.6 / 730 |
| | v3_d32sort | 1,007.4 / 391 | **130.6** / 916 | 343.9 / 6,086 | 354.7 / 461 |

**Against their published findings:**
- **Reproduced:**
  - Sorting by `(series_id, period)` with `date32` dates shrinks every format; the sorted `date32` Arrow IPC zstd file is the smallest of all, ahead of Parquet.
  - The indexed in-RAM baseline answers one series in 0.6 ms, far ahead of reading a file per request.
  - Vortex reads one series about 20× faster than Parquet through pyarrow.
- **Not reproduced:** Vortex reading faster than raw Arrow IPC. Here raw IPC is faster.
- **pyarrow's Parquet reads of ELEC take about 6 s.** ELEC's `series_id` dictionary has 785k values, more than one dictionary page holds. The writer falls back to plain encoding, and pyarrow rebuilds the dictionary on read. arrow-rs reads the same shape in 0.7–0.9 s (below), so this comes from pyarrow, not from Parquet.

## Strata, Parquet and Vortex in Rust (`fmtbench series`)

**Size, MB**

| | Variant | st | pq | vx_compact | vx_default |
|---|---|---|---|---|---|
| PET | v0_today | 48.6 | 42.4 | **38.3** | 104.7 |
| | v1_3col | 45.1 | 40.5 | **35.2** | 87.1 |
| | v2_date32 | 45.1 | 40.5 | **37.2** | 85.9 |
| | v3_d32sort | 49.6 | 29.0 | **20.9** | 51.6 |
| ELEC | v0_today | 276.7 | 270.5 | **213.1** | 416.2 |
| | v1_3col | 270.8 | 268.2 | **205.7** | 384.0 |
| | v2_date32 | 271.8 | 267.9 | **218.4** | 380.1 |
| | v3_d32sort | 215.4 | **157.1** | 162.8 | 302.1 |

**Write time, ms**

| | Variant | st | pq | vx_compact | vx_default |
|---|---|---|---|---|---|
| PET | v1_3col | 1,906 | 1,205 | 1,051 | **880** |
| | v3_d32sort | 1,946 | 849 | 740 | **597** |
| ELEC | v1_3col | 9,013 | 5,919 | 5,919 | **3,860** |
| | v3_d32sort | 9,766 | 3,966 | 3,087 | **2,464** |

**Full read into Arrow (`SELECT *`), min ms, with median CPU s**

| | Variant | st | pq | vx_compact | vx_default |
|---|---|---|---|---|---|
| PET | v1_3col | 185 (2.1) | 144 (1.2) | 104 (1.2) | **75** (0.8) |
| | v3_d32sort | 161 (1.7) | **39** (0.3) | 52 (0.6) | 49 (0.5) |
| ELEC | v1_3col | 1,035 (11.9) | 727 (8.0) | 387 (4.6) | **351** (4.2) |
| | v3_d32sort | 958 (10.6) | **189** (1.5) | 230 (2.7) | 214 (2.5) |

**Queries, min ms** (`point` is per series, with the KiB it read). From the run after the two fixes, with Strata's first run beside it.

| | Variant | Query | st | st, first run | pq | vx_compact | vx_default |
|---|---|---|---|---|---|---|---|
| PET | v2_date32 | count | **0.30** | 5.20 | 0.34 | 0.43 | 0.46 |
| | v2_date32 | agg | 72.2 | 73.0 | 46.2 | 53.0 | **37.3** |
| | v1_3col | point | 30.4 / 21,783 KiB | 91.5 / 22,506 | 86.8 / 14,609 | 36.4 / 48,675 | **17.7** / 110,875 |
| | v3_d32sort | point | **2.8** / 7 KiB | 85.2 / 1,915 | 3.4 / 329 | 5.7 / 6,927 | 3.8 / 6,391 |
| ELEC | v2_date32 | count | **0.31** | 19.84 | 0.34 | 0.43 | 0.42 |
| | v2_date32 | agg | 311.5 | 316.8 | 183.7 | 224.3 | **151.9** |
| | v1_3col | point | 137.6 / 66,810 KiB | 635.7 / 85,085 | 201.6 / 52,230 | 90.1 / 205,601 | **51.4** / 371,624 |
| | v3_d32sort | point | 10.1 / 18 KiB | 575.8 / 15,009 | **4.0** / 736 | 9.8 / 16,117 | 8.5 / 20,050 |

Unsorted rows are still grouped by series (EIA ships them series by series), so the zones rule out most chunks there too.

The size, write and full-read tables are from the first run; the second run matched them within the noise (see [Raw output](#raw-output)). The tables leave out `v0_today` and some variants; the full output is under [Raw output](#raw-output).

## Why Strata lost here

1. **No chunk statistics on strings (fixed).**
   - **The gap.** Zones (chunk min/max) existed for numbers and dictionary codes only. `series_id` has 175k distinct values on PET and 785k on ELEC, above the 65,535-value dictionary limit (`Config::dict_max_values`). So it was stored without a dictionary: no zone, and no chunk postings either.
   - **Its cost.** `WHERE series_id = '…'` decoded the whole `series_id` column on every query. In a profile of Strata on sorted PET, where the point queries take most of the time, 36% of the samples were in decoding chunks and 27% in evaluating the filter. Parquet's page statistics prune all but one page; Strata's key filter only tells whether a value is somewhere in the file.
   - **The fix.** A `Varbin` chunk (Utf8 or Binary without a dictionary) now has a zone in one of two forms:
     - **Plain form:** the first 16 bytes of its lowest and highest value, zero-padded. Padding keeps the byte order, so comparisons prune when taken as non-strict.
     - **Shared-prefix form** (new flag `SHARED_PREFIX`), when those values share a prefix of 6 bytes or more: the prefix length, a 4-byte hash of the prefix, and the next 11 bytes of each bound. Only equality prunes with it; a hash collision only keeps a chunk.

     The second form is what fixed ELEC: its sorted ids share prefixes such as `ELEC.PLANT.AVG_HEAT.1391-`, longer than 16 bytes. With the plain form alone, a sorted ELEC point query still took 238 ms.
2. **`count(*)` scanned a column (fixed).** `StrataExec` did not report a row count to DataFusion, so a count decoded a column: 5–20 ms against 0.35 ms. It now reports the row count from the postscripts. The count is exact for a plain scan, so DataFusion answers `count(*)` without a read; with a predicate, a lookup or a limit it is only an estimate.
3. **Sorted data barely compresses better (open).** Sorting shrank Parquet by 28% (PET) and 41% (ELEC), and Vortex compact by 44% and 25%. Strata got 10% bigger on PET and 21% smaller on ELEC:
   - Its narrow encodings have no run-length encoding for repeated strings without a dictionary, and no delta encoding for dates.
   - Its chunks are small (2,048 rows), each with its own zstd frame.
   - It has nothing like ALP for floats; Vortex uses ALP.
4. **Reads use more CPU (open).** A full read costs Strata 1.5–7× Parquet's CPU. Every 2,048-row chunk is a separate zstd frame and its own decode call, and nothing is skipped when a column is constant across a chunk.

The sorted ELEC point query remains 2.5× Parquet's time, though it reads 18 KiB against 736: zone checks run over about 30,000 block-table entries per column. That cost has not been profiled.

Strata's strengths do not apply to this data:
- no text payloads, so no deduplication and no trigram search;
- no id sort key, so no trace lookup;
- a high-cardinality string column used as the key.

Gaps 1 and 2 also mattered for cetra:
- **Gap 1:** a filter on a high-cardinality string column such as `user_id` or `session_id` decoded the whole column. Values inside map columns still get no zone.
- **Gap 2:** a count in the UI scanned a column.

## Raw output

**After the fixes.** `fmtbench series ~/cfb/data ~/cfb/st-out 5 PET ELEC` took 580 s, with a peak RSS of 15.9 GB:

```
ds    variant     format            MB  write ms  read med  read min  read cpu     count   agg med   agg min point med point min point KiB
PET   v0_today    pq              42.4      1586     166.8     165.4    1242.7      0.33      58.5      55.3    89.122    88.768   14609.4
PET   v0_today    vx_default     104.7       985      96.6      94.8    1052.3      0.41      61.3      61.0    17.697    17.355  109945.0
PET   v0_today    vx_compact      38.3      1387     147.2     145.1    1627.1      0.43      84.9      81.8    36.721    36.518   48290.1
PET   v0_today    st              48.6      2913     236.8     216.8    2562.3      0.30      94.9      94.5    30.873    30.296   21783.3
PET   v1_3col     pq              40.5      1220     156.0     142.6    1179.2      0.33      57.7      54.0    88.857    86.825   14609.4
PET   v1_3col     vx_default      87.1       922      75.9      75.1     816.6      0.45      62.8      61.5    17.938    17.668  110875.3
PET   v1_3col     vx_compact      35.2      1115     105.4     103.2    1159.4      0.45      83.1      80.5    36.855    36.350   48675.4
PET   v1_3col     st              45.1      2043     197.7     187.9    2195.1      0.30      95.0      94.1    31.163    30.443   21783.3
PET   v2_date32   pq              40.5      1096     152.3     146.3    1087.1      0.34      52.0      46.2    89.569    87.645   14611.4
PET   v2_date32   vx_default      85.9       806      64.3      62.9     675.3      0.46      38.3      37.3    18.002    17.645  109139.3
PET   v2_date32   vx_compact      37.2      1006      82.9      81.6     904.4      0.43      54.6      53.0    35.904    35.200   52655.6
PET   v2_date32   st              45.1      1511     178.0     176.5    2028.4      0.30      73.4      72.2    30.862    29.979   21790.6
PET   v3_d32sort  pq              29.0       851      44.6      38.8     272.0      0.35      45.8      45.0     3.756     3.383     328.7
PET   v3_d32sort  vx_default      51.6       585      49.5      47.8     529.9      0.42      39.2      38.8     4.186     3.817    6390.7
PET   v3_d32sort  vx_compact      20.9       771      53.6      53.5     562.9      0.46      51.4      49.9     5.844     5.666    6927.0
PET   v3_d32sort  st              49.6      2045     169.8     159.9    1675.2      0.30      78.2      75.8     3.427     2.799       7.4
ELEC  v0_today    pq             270.5      7410     865.7     823.9    9024.9      0.38     236.0     229.0   205.033   201.430   52230.1
ELEC  v0_today    vx_default     416.2      4394     478.9     452.1    5016.5      0.39     263.2     255.0    51.596    49.968  352529.6
ELEC  v0_today    vx_compact     213.1      6861     539.1     482.3    6279.3      0.39     299.7     297.0    89.809    88.807  202413.2
ELEC  v0_today    st             276.7     12889    1368.9    1303.8   15823.0      0.29     429.5     416.5   138.914   136.492   66810.4
ELEC  v1_3col     pq             268.2      5808     801.9     725.0    8041.6      0.36     243.2     225.3   202.749   201.637   52230.1
ELEC  v1_3col     vx_default     384.0      4111     372.6     353.5    4172.4      0.43     254.4     249.4    52.615    51.432  371623.6
ELEC  v1_3col     vx_compact     205.7      5899     394.1     378.7    4532.5      0.40     297.2     290.6    90.348    90.088  205600.5
ELEC  v1_3col     st             270.8      9497    1064.8    1054.6   12096.4      0.30     434.7     399.2   139.113   137.619   66810.4
ELEC  v2_date32   pq             267.9      5276     741.5     694.0    7609.6      0.34     196.5     183.7   201.965   200.915   52227.6
ELEC  v2_date32   vx_default     380.1      3665     305.5     290.9    3404.6      0.42     170.1     151.9    53.223    51.097  372296.3
ELEC  v2_date32   vx_compact     218.4      5307     357.7     350.4    4175.9      0.43     233.4     224.3    90.561    90.299  218821.6
ELEC  v2_date32   st             271.8      7066    1055.2    1004.1   11332.7      0.31     327.3     311.5   139.096   136.947   67058.1
ELEC  v3_d32sort  pq             157.1      3932     189.2     175.8    1500.8      0.35     179.1     175.2     4.342     3.955     736.1
ELEC  v3_d32sort  vx_default     302.1      2617     219.6     215.0    2472.6      0.44     173.0     160.7     8.544     8.500   20050.4
ELEC  v3_d32sort  vx_compact     162.8      3289     249.2     231.5    2795.1      0.42     227.7     220.5    10.271     9.796   16117.0
ELEC  v3_d32sort  st             215.4      8733    1046.2     986.9   11523.9      0.29     287.2     279.5    10.499    10.059      17.6
```

**First run, before the fixes**, 951 s:

```
ds    variant     format            MB  write ms  read med  read min  read cpu     count   agg med   agg min point med point min point KiB
PET   v0_today    pq              42.4      1595     167.0     162.9    1247.2      0.33      58.9      55.6    87.894    84.456   14609.4
PET   v0_today    vx_default     104.7       927      96.1      94.9    1051.7      0.42      64.3      60.1    17.688    17.597  109735.2
PET   v0_today    vx_compact      38.3      1314     147.1     144.7    1643.4      0.44      84.5      80.1    37.509    37.382   48817.0
PET   v0_today    st              48.6      2786     228.9     212.0    2519.4      5.36     102.4      93.4    95.993    95.920   22505.5
PET   v1_3col     pq              40.5      1205     152.9     143.9    1156.8      0.34      60.2      59.3    87.419    84.936   14609.4
PET   v1_3col     vx_default      87.1       880      76.7      75.2     816.5      0.42      61.7      60.1    18.320    17.775  110624.2
PET   v1_3col     vx_compact      35.2      1051     105.2     104.2    1155.8      0.43      82.9      80.3    38.391    37.736   48112.9
PET   v1_3col     st              45.1      1906     186.1     184.5    2101.7      5.33      93.8      93.6    93.940    91.515   22505.5
PET   v2_date32   pq              40.5      1052     155.5     145.5    1111.3      0.34      51.1      48.4    87.371    86.556   14611.4
PET   v2_date32   vx_default      85.9       789      63.7      63.1     667.3      0.41      40.0      37.5    18.999    18.418  109112.1
PET   v2_date32   vx_compact      37.2      1047      83.1      82.4     897.4      0.44      55.3      52.8    36.925    36.059   52100.4
PET   v2_date32   st              45.1      1371     177.2     172.0    1974.1      5.20      73.4      73.0    95.929    94.632   22512.8
PET   v3_d32sort  pq              29.0       849      43.6      39.3     289.5      0.35      45.0      42.8     3.691     3.647     328.7
PET   v3_d32sort  vx_default      51.6       597      51.2      48.6     545.2      0.45      38.6      36.6     4.582     4.406    7260.0
PET   v3_d32sort  vx_compact      20.9       740      53.1      51.8     572.0      0.43      51.7      50.7     6.183     5.814    7195.9
PET   v3_d32sort  st              49.6      1946     161.8     161.0    1711.0      5.45      77.3      76.1    86.669    85.153    1914.8
ELEC  v0_today    pq             270.5      7359     899.4     848.4    9242.2      0.36     248.6     228.6   196.688   193.928   52230.1
ELEC  v0_today    vx_default     416.2      4261     452.4     437.6    5119.0      0.40     261.7     247.0    50.880    50.334  350642.9
ELEC  v0_today    vx_compact     213.1      6852     504.4     475.2    5812.0      0.45     301.0     288.5    91.368    90.325  200706.8
ELEC  v0_today    st             276.7     12459    1380.6    1250.1   15468.5     19.77     418.4     404.0   635.728   632.030   85085.1
ELEC  v1_3col     pq             268.2      5919     790.6     727.1    7961.9      0.37     239.3     235.8   196.211   195.449   52230.1
ELEC  v1_3col     vx_default     384.0      3860     373.1     350.9    4194.1      0.43     263.6     261.7    52.616    51.277  367775.4
ELEC  v1_3col     vx_compact     205.7      5919     402.5     386.6    4633.7      0.41     307.5     288.4    92.540    91.863  206049.5
ELEC  v1_3col     st             270.8      9013    1049.6    1035.3   11946.6     19.90     421.9     391.3   636.279   635.697   85085.1
ELEC  v2_date32   pq             267.9      5294     745.5     685.0    7590.0      0.35     190.4     186.6   198.429   196.840   52227.6
ELEC  v2_date32   vx_default     380.1      3584     307.6     289.9    3422.9      0.41     167.6     158.4    52.977    52.677  373446.7
ELEC  v2_date32   vx_compact     218.4      5464     377.5     348.4    4297.5      0.44     238.5     224.7    93.809    92.810  219881.3
ELEC  v2_date32   st             271.8      6471    1032.5     977.8   11685.8     19.84     332.6     316.8   639.539   636.583   85332.7
ELEC  v3_d32sort  pq             157.1      3966     206.6     188.9    1490.1      0.36     181.7     167.9     5.383     5.232     736.1
ELEC  v3_d32sort  vx_default     302.1      2464     218.6     214.3    2513.3      0.46     168.5     166.7     8.403     8.092   17825.3
ELEC  v3_d32sort  vx_compact     162.8      3087     238.0     230.2    2745.4      0.44     222.8     219.7    10.969    10.580   19980.0
ELEC  v3_d32sort  st             215.4      9766    1036.8     957.6   10601.6     19.85     305.7     281.7   577.578   575.806   15008.5
```

`read cpu` is in ms. Nothing else was running on the VM when the run started.
