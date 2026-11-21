# Strata fuzzing

cargo-fuzz targets for Strata's decoders, like the repository's `fuzz/` (nightly only, its own workspace). The entry points are in [src/fuzz_entry.rs](../src/fuzz_entry.rs):

| Target | Input | What it reads |
|---|---|---|
| `file` | a whole Strata file | open, every column, full scans with and without views, a `Contains` scan of each payload column, a key lookup |
| `narrow_chunk` | a field choice, a block entry, a dictionary, a chunk body | `decode_chunk` and `decode_chunk_view` |
| `payload_values` | a payload store, its decoded frames, rows of chunk ids | `build_payload_values`, as `Utf8`, `Utf8View` or `Binary` |

Every array a target decodes must pass arrow's `validate_full`, which checks offsets, views and UTF-8. Two string decoders build arrays without arrow's UTF-8 check (`unsafe`, after one `simdutf8` pass), so a wrong shortcut shows up as a crash here.

Under `cfg(fuzzing)` the segment and hot-tail checksums always match, so mutated files reach the decoders.

## Running

```bash
cargo install cargo-fuzz --version 0.13.2 --locked
cd docs/research/file-format/strata/fuzz
cp ../Cargo.lock Cargo.lock
RUST_MIN_STACK=33554432 cargo +nightly-2026-11-03 fuzz run --target x86_64-unknown-linux-gnu file corpus-new/file corpus/file -- -max_total_time=600 -rss_limit_mb=4096 -timeout=30 -max_len=262144
```

New inputs go to `corpus-new/<target>` (ignored). To refresh the committed corpus, merge with `-merge=1` into an empty directory, then keep the smallest files up to 2 MiB and 500 files per target, as the repository's `just fuzz-cmin` does.

- `cargo test --lib fuzz_entry` replays every corpus and regression input on stable.
- `cargo test --lib -- --ignored write_seed_corpus` writes the seeds again: the test files, every chunk of their narrow columns, and their payload columns.

A crash is fixed with its input in `regressions/<target>/` and a named unit test.

## Findings

Two 10-minute rounds per target: 5.9 million inputs in the second round, with no crash after these fixes.

| Input | What happened | Fix | Test |
|---|---|---|---|
| `regressions/file/hot-schema-unknown-type` | A hot schema with an unknown Arrow type id: arrow's IPC reader panics (`unimplemented!`). About 40 other schema errors panic too (bit widths, units, child counts). | Both IPC reads go through `format::read_ipc_stream`, which runs arrow's reader under `catch_unwind`, as the repository's spec 03 §10 does for Parquet and Arrow decoding. | `file::tests::a_hot_schema_that_makes_arrow_panic_is_corrupt` |
| `regressions/narrow_chunk/ipc-declared-length` | An `Ipc` chunk of a few bytes that declares a huge message: arrow allocates and zeroes the declared metadata and body lengths before reading them (1.6 GB, 27 s). | `format::ipc_messages_fit` walks the stream's framing first; every message must fit in the bytes there. | `format::tests::an_ipc_stream_that_declares_more_than_it_holds_is_rejected` |

The payload target found nothing in 6.4 million inputs over both rounds, including the unchecked `Utf8View` path.
