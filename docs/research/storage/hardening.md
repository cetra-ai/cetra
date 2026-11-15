# Storage hardening — findings

Findings of track R (storage hardening), one section per milestone. The coordinator merges the milestone sections into one document.

## M7 — Scale and soak

What M7 covers:

- realistic payloads for the benchmarks and the soak, from a permissively licensed corpus (R7.2);
- the ingest benchmark on those payloads, with the measured compression of the Parquet files (R7.3);
- the 5M-trace benchmark on those payloads, local and on S3 (R7.4);
- the soak: its driver, its per-minute samples, and its pass/fail rules with the sampled-trace oracle (R7.5, R7.6);
- the soak's plots and recipes (R7.7);
- the 24 h dry run, and the 72 h soak on a dedicated server, recorded in `soak.md` (R7.7, R7.8).

### Runs

| date | commit | run | machine | result | write-up |
|---|---|---|---|---|---|

None yet: the benchmark runs 8–14 and the soak runs are run by hand. See `benchmarks.md` § Track R M7 runs and `soak.md` § Runs.

### Findings

None yet.
