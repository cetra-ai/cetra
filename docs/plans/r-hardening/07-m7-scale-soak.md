# R · M7 — Scale and soak

The seventh milestone of track R (`00-design.md` § M7, decisions D-R-11 and D-A1-12). It shows that the storage engine stays stable under sustained, realistic load. It also measures what the engine does at the scale the benchmarks were designed for.

It delivers three things:
- **Realistic payloads.** A generator shared by the benchmarks and the soak. It builds spans with the structure of the captured `fixtures/otlp/` cases (messages, tool calls, JSON arguments, system prompts, tool definitions, previews), sized as spec 03's capacity check assumes (LLM spans 10–50 KiB, others under 1 KiB). The text comes from a permissively licensed corpus, so compression ratios are realistic.
- **The pending 5M-trace `trace_bench` run.** It runs locally and against S3 (Versity Gateway on the workstation, then the Hetzner bucket), with realistic payloads. New rows go in `docs/research/storage/benchmarks.md`.
- **The soak.** `crates/storage/tests/soak.rs` drives `Storage` at a steady rate for hours. Score writes, deletions, retention and queries run at the same time, and background compaction stays on. Every minute it records one JSON line of resource and lag samples. At the end it applies pass/fail rules and a model-style check on a sample of traces. A 24 h dry run on the workstation comes first, then 72 h on a dedicated Hetzner AX52 in the bucket's region (D-R-11). The write-up, `docs/research/storage/soak.md`, has to be able to back a public "fast and solid" claim.

Sub-phases R7.1–R7.9, one commit each. R7.9 can produce several commits, one per bug found. R7.8 is run by the user on a server they rent; the implementer prepares it and records the results.

## Facts this plan relies on (checked 2026-11-14)

- **The workstation's disk is not rotational.** `benchmarks.md` § Machine says "disk `sda` (rotational)". That line comes from `machine()` in both benches, which prints the **first** row of `lsblk -d`. On this machine that row is `sda`, a 119 GiB exFAT card reader that holds no data. The working tree and the scratch directory are on **btrfs with `compress=zstd:1`, over LUKS, on an NVMe drive** (`nvme1n1`, model `CT4000P3PSSD8`). R7.1 fixes `machine()` and adds an erratum to `benchmarks.md`. btrfs compresses files transparently, so free space and physical write volume are not what the engine wrote. Every M7 data directory is therefore created with compression off (`chattr +m`, § Conventions).
- **The old ingest bench overstates compression.** Run 1's "about 19× Arrow-to-Parquet" comes from its pool of 16 random strings: every page sees the same 16 strings again. Random lowercase letters on their own compress about 1.7× with zstd level 3 in 1 MiB chunks (measured in the scratch directory with Python 3.14's `compression.zstd`). R7.3 records this as a finding.
- **Corpus measurements** (oasst2, the corpus R7.2 pins, downloaded and counted on 2026-11-15):
  - 135,174 messages, 64,513 of them English (37,958,567 bytes of text);
  - 40,495 English turns (an assistant message whose parent is an English prompter message);
  - English message length: median 227 bytes, p90 1,649, p99 3,370.

  The ratios below were measured on oasst1, oasst2's predecessor from the same project. oasst2 contains oasst1's messages and adds more, so it should give the same ratios:
  - plain corpus text: zstd-3 ratio 2.9×;
  - agent-style inputs, where each LLM call repeats the system prompt, the tools and the conversation so far, and spans of one trace sit next to each other: about 10×;
  - outputs: about 3.2×;
  - all together: about 7.9×.

  These numbers are the hypothesis R7.3 tests on real Parquet files. Spec 03 assumes 5–8×.
- **The scratch directory is at its quota.** `/tmp/claude-1000` holds 69 GiB and returns `Disk quota exceeded`. Benchmark and soak data must not go there (§ Conventions).
- **Hetzner Object Storage.** The endpoint is per location (`https://fsn1.your-objectstorage.com`, `nbg1`, `hel1`). SigV4 signs with the location code as the region (`fsn1`). Hetzner's own examples use virtual-hosted addressing. With `object_store`, a virtual-hosted endpoint must include the bucket name (`https://<bucket>.fsn1.your-objectstorage.com`). Path style is not confirmed by Hetzner; R7.8 tries it first. The benches hard-code region `us-east-1` and path style (`ingest_bench.rs::config`, `trace_bench.rs::config`, `delete_prefix`, `s3_listing`); R7.1 makes both configurable.

## Prerequisites

- **M1's R1.7.** It creates `docs/research/storage/hardening.md`, which R7.7 and R7.9 add a section to. If that file doesn't exist yet, stop and report.
- **Run numbers 8–18 are free.** `benchmarks.md` § Runs ends at run 7 (checked 2026-11-14). If another track has added runs since, continue from its last number and shift every number in this plan.
- **Docker or podman.** R7.3, R7.4 and R7.7 run Versity Gateway in a container. On this Fedora workstation, SELinux is enforcing: a bind mount needs the `:Z` suffix (`-v <dir>:/data:Z`), or Versity can't write to it.
- **N's M2 (`flush_now`, `collect_orphans`).** Track N's M2 adds `Storage::flush_now` and an operator `collect_orphans` GC. If it is merged when R7.6 starts, the settle step uses `flush_now` (R7.6). The soak never runs `collect_orphans`. Both that GC and the soak's orphan audit list objects and compare them with the catalog. If the GC ran during a soak, it would delete exactly the objects the audit is meant to report, so the soak runs without it and its audit stays the only orphan check.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Scope.** Edit only:
  - `crates/storage/tests/common/` (new);
  - `crates/storage/tests/bench_support.rs` (new);
  - `crates/storage/tests/ingest_bench.rs` and `crates/storage/tests/trace_bench.rs`;
  - `crates/storage/tests/soak.rs` and `crates/storage/tests/soak/` (new);
  - `crates/storage/Cargo.toml`, `[dev-dependencies]` only;
  - `justfile`;
  - `tools/soak/` (new). This is a deliberate addition to the track's edit scope; the coordinator adds it to `00-design.md` § Cross-cutting rules;
  - `AGENTS.md`, § Commands, § Tests and § Layout only (R7.7);
  - `docs/`.

  Nothing in M7 is user-visible (tests, tools and docs only), so no `CHANGELOG.md` line, unless an R7.9 fix changes behaviour. Such a fix adds a `Fixed` line.

  Don't touch `crates/storage/tests/s3.rs` or `crash.rs`; M4 owns the S3 tests. R7.9 may fix bugs anywhere in `crates/storage/src/` (D-R-2), and stops and reports instead when a fix would change a file format, a public API or a spec rule.
- **No production code changes before R7.9.** Everything an integration test needs is already public: `Storage::{open, append_spans, append_scores, query, register_deletion, deletion_status, apply_retention, health, read_cache_stats, compact, apply_deletions, collect_obsolete, collect_staged, shutdown}`, and the `reads` functions. Files per level, the lag of the oldest L0 file, staged objects and pending deletions are read from `catalog.db` through a read-only `sqlx` connection, as `tests/s3.rs` does (`SqliteConnectOptions::new().filename(..).read_only(true)`). If a read-only open fails on the `-shm` file while the storage is open, open it read-write and run `SELECT`s only, and say so in the report. Don't add a stats API to `Storage`.
- **Dependencies.** Two new dev-dependencies of `llmobs-storage`, both already declared in the root `[workspace.dependencies]`, so nothing changes there:
  - `flate2 = { workspace = true }` (1.1.11 in `Cargo.lock`): it reads the gzip corpus (R7.2);
  - `tracing-subscriber = { workspace = true }` (0.3.23 in `Cargo.lock`, features `env-filter`, `json`, `fmt`): the soak writes the storage's log events to a file (R7.5).

  `sha2`, `serde_json`, `sqlx`, `parquet`, `rustix`, `uuid` and `chrono` are normal dependencies of the crate, so integration tests can use them already. The plot tool (R7.7) is a separate uv project: Python `==3.13.*`, `matplotlib==3.11.2` (uploaded 2026-09-11), and dev `ruff==0.17.3` and `mypy==2.4.1`, the same pins as `tools/capture/py`.
- **Shared test code layout.** `crates/storage/tests/common/mod.rs` is the usual Cargo pattern: each test target that needs it declares `mod common;`.
  - **Visibility.** Every item in `common` is `pub(crate)`, never `pub`. The workspace lint `unreachable_pub` would flag `pub` items in a private module.
  - **Dead code.** Each target uses only part of `common`, so each `mod common;` line carries `#[allow(dead_code)] // each test target uses part of the shared helpers`. No `#[test]` lives in `common` (it would run once per target); its tests go in `tests/bench_support.rs`.
  - **Soak modules.** The soak's own modules live in `tests/soak/` and are declared in `tests/soak.rs` with `#[path = "soak/<name>.rs"] mod <name>;`. A plain `mod` would look for `tests/<name>.rs`, which Cargo treats as another test target.
  - **Allow lists.** Every new test target root (`bench_support.rs`, `soak.rs`) starts with the benches' list: `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::too_many_lines, clippy::print_stdout)]`. `print_stdout` is needed because helpers that print are not `#[test]` functions, so `allow-print-in-tests` doesn't cover them.
  - **Nothing unused in any commit.** Each sub-phase adds only items that something in the same commit uses: a test, or the bench or soak entry point. Clippy runs with `-D warnings`. The `#[allow(dead_code)]` on `mod common;` covers only the shared module.
- **Data directories** for benchmarks and soaks live outside the scratch directory, in `~/llmobs-bench/`, created once with compression off:

  ```bash
  mkdir -p ~/llmobs-bench && chattr +m ~/llmobs-bench && lsattr -d ~/llmobs-bench   # shows 'm'
  ```

  New files under it inherit the flag. On a filesystem other than btrfs, skip `chattr`. Every run records `findmnt -T <dir>` (R7.1's `machine()` does).
- **Privacy and logging.**
  - No output, committed file or error message contains payload text from the corpus. Print ids, counts and sizes only.
  - No committed file contains `AWS_*` values, the bucket name, the S3 endpoint host of a rented bucket, the hostname, or a home directory path. R7.5's `Redactor` enforces this:
    - it redacts in a clone of each value before serializing (`run.json`, `summary.json`);
    - it scrubs `events.jsonl` before the file is committed (R7.7).

    The machine is named by `LLMOBS_SOAK_LABEL`.
  - `journalctl` prefixes each line with the hostname: always use `journalctl -o cat` for logs that are kept.
  - Credentials live only in the environment, or in a `chmod 600` env file outside the repository (R7.8).
- **`systemd-run --user`.** A user unit doesn't inherit the login shell's `PATH`, and `just`, `cargo` and `uv` live in `~/.cargo/bin` and `~/.local/bin`. Every `systemd-run` in this plan passes `--setenv=PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/bin:/bin`. The shell expands `$HOME` before `systemd-run` sees it, so write `$HOME/...`, never `%h`: systemd specifiers are not expanded in `-p` values passed on the command line.
- **Panics.** Test code may `unwrap`/`expect`. Every `expect` message says what was expected.
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - the numbers the sub-phase asks for;
  - anything left undone or unverified.

---

## R7.1 — Shared bench helpers; `machine()` and S3 settings fixed

Commit: `storage: share the benchmark helpers and describe the data disk correctly (R7.1)`

The two benches each carry copies of the RNG, `machine()`, `read_line`, `command`, `dir_bytes`, `delete_prefix` and the S3 builder. Move them into `common` once, fix `machine()`, and make the S3 region and addressing configurable. The benchmark results must not change otherwise: `trace_bench`'s data plan has to stay identical.

### Files

`crates/storage/tests/common/mod.rs`:

```rust
//! Helpers shared by the storage benchmarks and the soak (track R M7,
//! docs/plans/r-hardening/07-m7-scale-soak.md).

pub(crate) mod machine;
pub(crate) mod rng;
pub(crate) mod s3;
```

R7.2 adds `corpus` and `payload`, and R7.3 adds `parquet_stats`.

**`common/rng.rs`**. Move `Rng` from `trace_bench.rs` unchanged: `new`, `next`, `unit`, `below`, `normal`, `lognormal`. Add:

```rust
/// SplitMix64 finaliser: a well-mixed 64-bit value from any input.
pub(crate) fn mix(x: u64) -> u64;
impl Rng {
    /// An independent stream for `(a, b)`: `Rng::new(mix(a ^ mix(b)) | 1)`.
    /// The `| 1` keeps xorshift away from the all-zero state.
    pub(crate) fn stream(a: u64, b: u64) -> Self;
}
```

`mix(x)`: `x += 0x9E37_79B9_7F4A_7C15; z = x; z = (z ^ (z >> 30)) * 0xBF58_476D_1CE4_E5B9; z = (z ^ (z >> 27)) * 0x94D0_49BB_1331_11EB; z ^ (z >> 31)`, all with `wrapping_*`.

**`common/machine.rs`**:

```rust
/// Everything a benchmark or soak write-up names about the machine.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Machine {
    pub(crate) cpu: String,            // /proc/cpuinfo "model name"
    pub(crate) threads: usize,         // available_parallelism
    pub(crate) ram: String,            // /proc/meminfo "MemTotal:"
    pub(crate) kernel: String,         // uname -r
    /// Physical disk under `dir`: "<name> rota=<0|1> <model>".
    pub(crate) disk: String,
    /// `findmnt -n -o FSTYPE,OPTIONS -T <dir>`.
    pub(crate) filesystem: String,
    /// `/run/.containerenv` or `/.dockerenv` exists.
    pub(crate) container: bool,
    pub(crate) commit: String,         // git rev-parse HEAD, 12 chars
    /// `git status --porcelain` is not empty.
    pub(crate) dirty: bool,
    pub(crate) rustc: String,          // rustc -V
}

pub(crate) fn machine(dir: &Path) -> Machine;
impl std::fmt::Display for Machine { /* one line, the fields in this order */ }

/// "/dev/mapper/luks-x[/home]" → "/dev/mapper/luks-x": findmnt prints a btrfs subvolume in brackets.
pub(crate) fn strip_subvolume(source: &str) -> &str;
/// From `lsblk -s -n -o NAME,ROTA,MODEL,TYPE <source>` output, the first line with TYPE `disk`,
/// as "<name> rota=<r> <model>". Names may start with tree characters (`└─`); strip them.
pub(crate) fn physical_disk(lsblk_inverse: &str) -> Option<String>;
```

Model names contain spaces and may be blank, so `physical_disk` doesn't split on whitespace:
- `TYPE` is the line's **last** token, `NAME` the first (after the tree characters), and `ROTA` the second;
- `MODEL` is everything between `ROTA` and `TYPE`, trimmed;
- a blank model is written `unknown`.

`machine(dir)` runs `findmnt -n -o SOURCE -T <dir>`, applies `strip_subvolume`, then runs `lsblk -s -n -o NAME,ROTA,MODEL,TYPE <source>` (`-s` walks from the mapper device down to the disk) and `physical_disk`. Every command failure gives `"unknown"`, as today. Move `read_line`, `command` and `dir_bytes` here as `pub(crate)` functions.

**`common/s3.rs`**:

```rust
/// The S3 target of a benchmark (D119), from the environment.
#[derive(Debug, Clone)]
pub(crate) struct S3Env {
    pub(crate) endpoint: String,   // LLMOBS_TEST_S3_ENDPOINT
    pub(crate) bucket: String,     // LLMOBS_TEST_S3_BUCKET
    pub(crate) region: String,     // LLMOBS_TEST_S3_REGION, default "us-east-1"
    pub(crate) path_style: bool,   // LLMOBS_TEST_S3_PATH_STYLE, default true; "false" or "0" turn it off
}

impl S3Env {
    /// Pure parser: `lookup(name)` returns the variable's value. `None` when
    /// LLMOBS_TEST_S3_ENDPOINT is absent; panics when the endpoint is set but the bucket is not.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self>;
    /// `from_lookup(|name| std::env::var(name).ok())`.
    pub(crate) fn from_env() -> Option<Self>;
    /// The storage config for this target under `prefix`.
    pub(crate) fn config(&self, prefix: &str) -> (ObjectStoreUrl, S3Config);
    /// A store for listing and deleting, built like the storage's own.
    pub(crate) fn store(&self) -> Result<object_store::aws::AmazonS3, String>;
    pub(crate) async fn list(&self, prefix: &str) -> Result<Vec<object_store::ObjectMeta>, String>;
    pub(crate) async fn delete_prefix(&self, prefix: &str) -> Result<(), String>;
    /// What the benches print: "s3://<bucket>/<prefix> at <endpoint>, region <r>, path style <b>".
    /// The real bucket and endpoint appear only for a loopback endpoint
    /// (127.0.0.1, localhost); otherwise the literal placeholders `<bucket>` and `<endpoint>`.
    pub(crate) fn describe(&self, prefix: &str) -> String;
}
```

**No bucket or endpoint in bench output.** `trace_bench.rs` (the `store` line of its Markdown output) and `ingest_bench.rs` (its final `Store:` line and the `deleted prefix` line) print `S3Env::describe` instead of the bucket and endpoint. Both benches' output is pasted into `benchmarks.md`. The `Manifest` still stores `endpoint` and `bucket`, since dataset reuse compares them, but it lives in the data directory and is never committed. Test 6 below covers `describe`.

- `config`: `S3Config { region: Some(region), endpoint: Some(endpoint), force_path_style: path_style, allow_http: endpoint.starts_with("http://") }`.
- `store`: `AmazonS3Builder::from_env()` with the bucket, endpoint and region, `with_virtual_hosted_style_request(!path_style)` and `with_allow_http`.

### Changes to the benches

- `ingest_bench.rs` and `trace_bench.rs` declare `mod common;` (with the `allow` above) and use `common::{rng::Rng, machine::machine, s3::S3Env}`. Delete their copies of `Rng`, `read_line`, `command`, `machine`, `dir_bytes`, `delete_prefix`, `s3_bytes`/`s3_listing`, and their own `S3Target` structs. `Backend` in `trace_bench.rs` keeps its shape with `s3: Option<S3Env>`.
- Both print `Machine: {machine(&dir)}`.
- `trace_bench`'s `Manifest` keeps `endpoint` and `bucket`; nothing else changes in R7.1.

### Tests: `crates/storage/tests/bench_support.rs` (new, not ignored)

1. `rng_sequence_is_unchanged`. Before moving `Rng`, run the old code once (a scratch test or `cargo test` printout) and record the first three `next()` values of `Rng::new(1)` and `Rng::new(1).unit()`. Pin them as literals here: they prove `trace_bench`'s data plan did not change.
2. `rng_streams_differ`: `Rng::stream(1, 0)` and `Rng::stream(1, 1)` give different first values. `Rng::stream(0, 0).next() != 0`.
3. `strip_subvolume_handles_btrfs`: `"/dev/mapper/luks-c34c[/home]"` → `"/dev/mapper/luks-c34c"`; `"/dev/nvme0n1p2"` is unchanged.
4. `physical_disk_walks_to_the_disk`: input

   ```text
   luks-c34c         0                crypt
   └─nvme1n1p3       0                part
     └─nvme1n1       0 CT4000P3PSSD8  disk
   ```

   → `Some("nvme1n1 rota=0 CT4000P3PSSD8")`. An input with no `disk` line gives `None`.
5. `s3_env_reads_region_and_style`: calls `S3Env::from_lookup` with a closure over a `HashMap<&str, &str>`, never the process environment. The crate is edition 2024 with `unsafe_code = "forbid"`, and `std::env::set_var` is `unsafe` there. Check:
   - an empty map gives `None`;
   - endpoint and bucket alone give region `"us-east-1"` and `path_style == true`;
   - `LLMOBS_TEST_S3_REGION=fsn1` and `LLMOBS_TEST_S3_PATH_STYLE=false` give `"fsn1"` and `false`;
   - `PATH_STYLE=0` gives `false`.
6. `describe_hides_remote_targets`:
   - a `127.0.0.1` endpoint prints its bucket;
   - `https://fsn1.your-objectstorage.com` with bucket `acme-prod` prints neither `acme-prod` nor `fsn1.your-objectstorage.com`.
7. `physical_disk_reads_models_with_spaces`: a `disk` line whose model is `SAMSUNG MZVL21T0HCLR-00B00` keeps the whole model. A `disk` line with a blank model gives `"<name> rota=<r> unknown"`.

### Docs

`docs/research/storage/benchmarks.md` § Machine and environment, after the existing bullets, adds a paragraph **"Erratum (track R M7)."**:
- `sda` is a card reader;
- runs 1–7 wrote to btrfs (`compress=zstd:1`) over LUKS on NVMe `CT4000P3PSSD8`;
- the cause was `machine()` printing the first `lsblk -d` row, fixed in R7.1;
- btrfs compressed the files on disk a second time, which does not change the logical byte counts the runs report (`dir_bytes` sums file lengths).

Old rows are not rewritten.

### Commands

```bash
cargo test -p llmobs-storage --all-features --test bench_support
cargo test --release -p llmobs-storage --all-features --test ingest_bench --test trace_bench --no-run
```

Expected: 7 passed, and both benches build. Report the `machine()` line printed on the workstation.

---

## R7.2 — The corpus and the payload generator

Commit: `storage: add a realistic payload generator for benchmarks and the soak (R7.2)`

### The corpus (user's choice, 2026-11-15)

**OpenAssistant oasst2**, file `2023-11-05_oasst2_ready.messages.jsonl.gz`:
- Apache-2.0;
- 54,301,900 bytes;
- SHA-256 `a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751`. That is the file's LFS oid; a download on 2026-11-15 gave the same hash;
- pinned to dataset commit `179dd21fc55192153d94adb0e0ce8f69e222bf75`;
- URL `https://huggingface.co/datasets/OpenAssistant/oasst2/resolve/179dd21fc55192153d94adb0e0ce8f69e222bf75/2023-11-05_oasst2_ready.messages.jsonl.gz`.

It is downloaded at test time and never committed: nothing is redistributed, and committed results contain only counts. Only English messages are used (`lang == "en"`): 64,513 messages and 40,495 turns (§ Facts).

`justfile`, after `test:`:

```just
# Download the pinned text corpus of the storage benchmarks and soak (OpenAssistant oasst2, Apache-2.0; track R M7)
fetch-corpus:
    mkdir -p target/bench-corpus
    curl -fL --retry 3 -o target/bench-corpus/oasst2-ready-messages.jsonl.gz.part https://huggingface.co/datasets/OpenAssistant/oasst2/resolve/179dd21fc55192153d94adb0e0ce8f69e222bf75/2023-11-05_oasst2_ready.messages.jsonl.gz
    echo "a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751  target/bench-corpus/oasst2-ready-messages.jsonl.gz.part" | sha256sum --check --strict -
    mv target/bench-corpus/oasst2-ready-messages.jsonl.gz.part target/bench-corpus/oasst2-ready-messages.jsonl.gz
```

### `common/corpus.rs`

```rust
pub(crate) const CORPUS_SHA256: &str = "a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751";
pub(crate) const CORPUS_FILE: &str = "oasst2-ready-messages.jsonl.gz";

/// `LLMOBS_BENCH_CORPUS`, else `<CARGO_MANIFEST_DIR>/../../target/bench-corpus/<CORPUS_FILE>`.
pub(crate) fn corpus_path() -> PathBuf;

/// `Debug` is written by hand: counts only, never text.
pub(crate) struct Corpus {
    /// Message texts, each cut to at most `MAX_TEXT` bytes at a char boundary.
    texts: Vec<String>,
    /// (prompt index, answer index) into `texts`.
    turns: Vec<(u32, u32)>,
}

pub(crate) const MAX_TEXT: usize = 4 * 1024;

impl Corpus {
    /// Reads the pinned file: checks its SHA-256 first, then keeps English
    /// messages and pairs each assistant message with its prompter parent.
    pub(crate) fn load(path: &Path) -> Result<Self, String>;
    /// `load(&corpus_path())`, panicking with "run `just fetch-corpus`" when the file is missing.
    pub(crate) fn from_env() -> Self;
    /// For tests: texts paired as turns (0, 1), (2, 3), …
    pub(crate) fn from_texts(texts: Vec<String>) -> Self;
    pub(crate) fn text(&self, index: usize) -> &str;
    pub(crate) fn texts(&self) -> usize;
    pub(crate) fn turn(&self, index: usize) -> (&str, &str);
    pub(crate) fn turns(&self) -> usize;
    pub(crate) fn bytes(&self) -> usize;
}

/// `s` cut to at most `max` bytes at a char boundary.
pub(crate) fn cut(s: &str, max: usize) -> &str;
```

`load`:
1. Read the whole file and compare `sha2::Sha256` of the bytes, as lowercase hex, with `CORPUS_SHA256`; a mismatch is `Err("corpus SHA-256 mismatch")`.
2. Decompress with `flate2::read::GzDecoder` and read lines.
3. Parse each line as

   ```rust
   #[derive(serde::Deserialize)]
   struct Line { message_id: String, parent_id: Option<String>, text: String, role: String, lang: String }
   ```

   (serde ignores the other fields).
4. Keep `lang == "en"` and non-empty `text`, cut to `MAX_TEXT`.
5. Index kept messages by `message_id`. A turn is a kept `role == "assistant"` message whose `parent_id` names a kept `role == "prompter"` message. Keep turns in file order.
6. `Err` if fewer than 50,000 texts or 30,000 turns: a wrong file. The pinned file has 64,513 and 40,495.

**Hand-written `Debug`.** `Corpus`, `Payloads` and `Content` implement `Debug` by hand, never derived, so a failing assertion or `{:?}` can't print corpus text:
- `Corpus` prints its `texts`, `turns` and `bytes` counts;
- `Payloads` prints its app count and the corpus counts;
- `Content` prints, per field, `Some(<len> bytes)` or `None`, and the attribute count.

`load` prints nothing. Callers print `corpus: <texts> texts, <turns> turns, <bytes> bytes`.

### `common/payload.rs`

Everything is a pure function of a `trace_seed: u64` and the corpus: the same seed gives the same bytes. This lets `trace_bench` build any group of a trace without keeping state across groups.

```rust
pub(crate) const LLM_MIN_BYTES: usize = 10 * 1024;
pub(crate) const LLM_MAX_BYTES: usize = 50 * 1024;
pub(crate) const SMALL_MAX_BYTES: usize = 1024;
pub(crate) const APPS: usize = 16;
const APP_SEED: u64 = 0xA995;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppKind { Chat, Rag, Agent }

/// `Debug` by hand: kind, sizes and names only.
pub(crate) struct App {
    pub(crate) kind: AppKind,
    /// 1–6 KiB of corpus text joined with "\n\n".
    pub(crate) system: String,
    /// True: the system prompt is a `system` message in `input` (OpenInference,
    /// OpenLLMetry). False: it goes to `system_instructions` (GenAI).
    pub(crate) system_in_input: bool,
    /// `tool_definitions` JSON (spec 02 rule 7), or `None`.
    pub(crate) tools: Option<String>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) convention: Convention,
}

/// The content fields of one span. Everything else (ids, times, tokens,
/// cost, context) belongs to the caller.
#[derive(Clone, Default)] // `Debug` by hand (§ The corpus): sizes only
pub(crate) struct Content {
    pub(crate) input: Option<String>,
    pub(crate) input_format: Option<PayloadFormat>,
    pub(crate) output: Option<String>,
    pub(crate) output_format: Option<PayloadFormat>,
    pub(crate) input_preview: Option<String>,
    pub(crate) output_preview: Option<String>,
    pub(crate) system_instructions: Option<String>,
    pub(crate) tool_definitions: Option<String>,
    pub(crate) request_params: Option<String>,
    pub(crate) attributes: BTreeMap<String, String>,
}

impl Content {
    /// Sets the ten fields above on `span`, replacing whatever was there.
    pub(crate) fn apply(self, span: &mut Span);
    /// Bytes of every string field, attribute keys and values included.
    pub(crate) fn bytes(&self) -> usize;
}

/// `Debug` by hand: counts only.
pub(crate) struct Payloads { corpus: Corpus, apps: Vec<App> }

impl Payloads {
    pub(crate) fn new(corpus: Corpus) -> Self;
    pub(crate) fn corpus(&self) -> &Corpus;
    /// `apps[mix(trace_seed) % APPS]`.
    pub(crate) fn app(&self, trace_seed: u64) -> &App;
    /// The `calls` LLM calls of one trace, in order. Each call's input holds the
    /// previous call's whole input and output: a growing conversation.
    pub(crate) fn llm_calls(&self, trace_seed: u64, calls: usize) -> Vec<Content>;
    /// The root span: the first question as `text` input, the first answer as `text` output, both cut to 512 bytes.
    pub(crate) fn root(&self, trace_seed: u64) -> Content;
    /// The tool span between LLM calls `call` and `call + 1` of an agent trace.
    pub(crate) fn tool(&self, trace_seed: u64, call: usize) -> Content;
    /// The retriever span of a RAG trace.
    pub(crate) fn retriever(&self, trace_seed: u64) -> Content;
    /// A framework span (chain step, HTTP call): attributes only, 200–600 bytes.
    pub(crate) fn step(&self, trace_seed: u64, index: usize) -> Content;
}
```

**Apps** (`Payloads::new`), from `Rng::new(APP_SEED)`, for `a` in `0..APPS`:
- `kind = [Chat, Rag, Agent][a % 3]`.
- `system`: corpus texts picked with `rng.below(texts)`, joined with `"\n\n"` until at least `1024 + rng.below(5 * 1024)` bytes, then cut to that length.
- `system_in_input = a % 2 == 0`. `convention` is `Openinference` when true, `Genai` when false.
- `tools`:
  - `Agent` apps always have tools; other apps have them when `a % 4 == 1`;
  - 2–6 tools, `tool_names[k] = format!("tool_{a}_{k}")`;
  - each one, built with `serde_json::json!`: `{"type":"function","function":{"name":…, "description": cut(text, 300), "parameters":{"type":"object","properties":{"query":{"type":"string","description": cut(text, 120)},"limit":{"type":"integer"}},"required":["query"]}}}`;
  - the array is serialized once with `serde_json::to_string`.
- `(provider, model)`: cycle through `[("openai","gpt-4o-mini"), ("openai","gpt-4o"), ("anthropic","claude-sonnet-4"), ("gcp.gemini","gemini-2.5-flash")]`.

**Exchanges.** The conversation of a trace is a stream of exchanges from `Rng::stream(trace_seed, 0xC0)`. Exchange `j` draws `turn = rng.below(turns)` and renders as messages in the canonical form of spec 01 (§ Messages; `fixtures/otlp/openinference/openai-chat-tool/expected.json` shows it):
- `Chat`: `user(prompt)`, `assistant(answer)`.
- `Rag`: `user("Context:\n" + docs + "\n\nQuestion: " + prompt)`, `assistant(answer)`. `docs` is `3 + rng.below(4)` corpus texts, each cut to 1,536 bytes, joined with `"\n\n---\n\n"`.
- `Agent`: `user(prompt)`, then `assistant` with one `tool_call` part `{"type":"tool_call","id":"call_<j>","name": <tool_names[rng.below(n)]>,"arguments":{"query": cut(text, 120),"limit": 1 + rng.below(10)}}`, then `tool` with one `tool_call_response` part `{"type":"tool_call_response","id":"call_<j>","response":{"results":[…]}}` holding 1–3 `{"title": cut(text, 60), "snippet": cut(text, 200)}` entries, then `assistant(answer)`.

`user(t)` is `{"role":"user","parts":[{"type":"text","content":t}]}`, and `assistant(t)` the same with role `assistant`. Build messages as `serde_json::Value`, then serialize with `serde_json::to_string`.

**LLM calls** (`llm_calls(seed, calls)`):
- **Size target.** `T = LLM_MIN_BYTES × 5^u`, with `u = Rng::stream(seed, 0x51).unit()`: log-uniform over 10–50 KiB.
- **Call 0.**
  - Input: the system message (when `system_in_input`), then exchanges `0..p` rendered whole, then the *question part* of exchange `p`:
    - `Chat`/`Rag`: its user message;
    - `Agent`: its user message, tool call and tool response.
  - Output: the *answer part* of exchange `p`, its final assistant message, as a one-message array.
  - `p` is the smallest value (from 0) for which `input.len() + output.len()` plus the `system_instructions` and `tool_definitions` lengths reaches `T`.
- **Call `k`** uses `p + k` in place of `p`. Each call therefore contains the previous call's input and output: the prefix is shared, as in a real agent loop.
- **Fields of every call:**
  - `input_format` and `output_format` are `Messages`;
  - `system_instructions`: `[{"type":"text","content":system}]` when not `system_in_input`;
  - `tool_definitions = app.tools`;
  - `request_params = {"max_tokens":1024,"temperature":0.7}`;
  - attributes as an OpenInference LLM span has them (`llm.model_name`, `llm.system` = provider, `llm.invocation_parameters`, `input.mime_type` = `application/json`, `output.mime_type`) for `Openinference` apps, or `gen_ai.operation.name` = `chat` for `Genai` apps.
- **Previews** (spec 02 rule 9):
  - `input_preview`: the text parts of the last `user` message, joined with `\n` and cut to 512 Unicode scalar values. Count `chars()`, not bytes;
  - `output_preview`: the text parts of the output's last message, cut the same way.

**Small spans:**
- `root`: input `Text` = `cut(prompt of exchange 0, 512)`, output `Text` = `cut(answer of exchange 0, 512)`. The previews equal the values (rule 9, `text`).
- `tool(seed, call)`: input `Json` = the `arguments` object of exchange `p + call + 1` (the exchange whose question part ends LLM call `call + 1`) as compact JSON; output `Json` = its `response` object. `Agent` apps only; for other apps it returns `Content::default()`.
- `retriever(seed)`: input `Text` = `cut(prompt, 512)`; output `Json` = `[{"id":"doc-<n>","title": cut(text, 60),"score": <0..1 with 3 decimals>}, …]` for the docs of exchange `p`. Previews are the compact JSON cut to 512.
- `step(seed, index)`: no input or output. 3–8 attributes from `Rng::stream(seed, 0x5700 + index)`, keys from a fixed list (`http.method`, `http.route`, `http.status_code`, `db.system`, `db.operation`, `langgraph.node`, `langgraph.step`, `retry.count`, `cache.hit`), values short (route-like strings, numbers).

Every small span's `bytes()` stays under `SMALL_MAX_BYTES`. A `retriever` with 6 long titles can approach it; cut the titles until it fits.

**Whole traces.** These are used by `ingest_bench` and the soak. `trace_bench` keeps its own data plan and uses only the content functions (R7.4).

```rust
#[derive(Debug, Clone)]
pub(crate) struct TraceSummary {
    pub(crate) trace_id: TraceId,
    pub(crate) name: String,
    pub(crate) start_time: i64,
    pub(crate) end_time: i64,
    pub(crate) span_count: i64,
    pub(crate) error_count: i64,
    pub(crate) llm_count: i64,
    pub(crate) total_tokens: Option<i64>,
    pub(crate) cost_total: Option<f64>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<String>,
}

/// No `Debug`: its spans hold corpus text.
pub(crate) struct GeneratedTrace {
    /// Spans appended when the trace ends.
    pub(crate) first: Vec<Span>,
    /// Spans appended `delay` later (5% of traces), else empty.
    pub(crate) late: Vec<Span>,
    pub(crate) delay: Duration,
    pub(crate) summary: TraceSummary,
}

/// One trace that ends at `end` (ns). `ingest_time` is `end` on every span;
/// callers that append later overwrite it with the append time.
pub(crate) fn trace(
    payloads: &Payloads,
    project: ProjectId,
    trace_id: TraceId,
    trace_seed: u64,
    end: i64,
) -> GeneratedTrace;
```

`trace` draws from `Rng::stream(trace_seed, 0x7A)`:
- **Shape**, by `app(seed).kind`:
  - `Chat`: root (`Chain`, `"chat"`), 1 LLM call, 6 steps;
  - `Rag`: root (`Chain`, `"rag"`), 1 retriever (`Retriever`), 1 LLM call, 6 steps;
  - `Agent`: root (`Agent`, `"agent"`); then for each call `k` in `0..c` (`c = 2 + rng.below(3)`): `Llm`, then a `Tool` span when `k < c − 1`, then 3 steps.

  Spans are numbered in that order. Span 0 is the root, with no parent; the others have parent span 0. Span ids are `SpanId::from_bytes((i + 1).to_be_bytes())`.
- **Times:**
  - duration `D` = `rng.lognormal(2.0, 60.0)` seconds, clamped to `[0.05, 600]` s;
  - root span `[end − D, end)`;
  - span `i ≥ 1` starts at `end − D + i × D / (n + 1)` and lasts `D / (2(n + 1))`.

  No two spans share a `start_time`, which is a spec guarantee the merge relies on.
- **Context:**
  - `session_id`: `Some(format!("s-{}", rng.below(50_000)))` with probability 0.3, else `None`;
  - `user_id`: `Some(format!("u-{}", rng.below(20_000)))` with probability 0.6, else `None`;
  - `environment`: `"production"` with probability 0.9, else `"staging"`;
  - `release` from `["v1.0", "v1.1", "v2.0"]`;
  - `tags`: 0–3 from `["beta", "internal", "gpu", "batch", "realtime"]`;
  - `service_name`: `"app-<a>"`;
  - the same value on every span of the trace.
- **Status:** with probability 0.03, one random span has `SpanStatus::Error`. Every other span has `Ok`.
- **LLM fields:**
  - `provider`, `request_model` and `response_model` from the app;
  - `input_tokens = input bytes / 4` (plus `system_instructions` and tool bytes), `output_tokens = output bytes / 4`, `total_tokens` their sum;
  - `cost_total = total_tokens as f64 / 1_048_576.0`, a binary fraction, so sums are exact;
  - `conventions = vec![app.convention]`.

  Non-LLM spans leave tokens and cost `None`.
- **Late group:** with probability 0.05, the last third of the spans (`n / 3`, at least 1, never span 0) move to `late`. `delay` is uniform over 1–30 min with probability 0.8, else 1–3 h.
- **Summary:**
  - `name` is the root's;
  - `start_time`/`end_time`: min and max over all spans;
  - the counts over all spans, both groups;
  - `total_tokens`/`cost_total`: sums over the LLM spans.

### Tests (`tests/bench_support.rs`)

They use `Corpus::from_texts` with 200 synthetic texts: `format!("message {i}: ")` followed by words from a fixed list, of lengths 50–3,000 bytes. They never need the downloaded corpus.
1. `payloads_are_deterministic`: two `Payloads::new` on equal corpora give byte-equal `llm_calls(42, 3)`, `root(42)` and `step(42, 1)`.
2. `llm_calls_hit_the_size_band`: for seeds `0..200`, call 0's `bytes()` lies in `[LLM_MIN_BYTES, LLM_MAX_BYTES + 24 KiB]`. The slack is one exchange: six 1.5 KiB docs plus a 4 KiB prompt and a 4 KiB answer (`MAX_TEXT`), plus JSON overhead. Every later call is larger than the one before.
3. `later_calls_extend_the_conversation`: for an `Agent` app seed (search seeds until `app(seed).kind == Agent`), call 1's input with its closing `]` removed starts with call 0's input with its closing `]` removed.
4. `inputs_are_canonical_messages`: every `llm_calls` input and output parses as a JSON array whose elements have a `role` string and a `parts` array; every `tool_call` id has a matching `tool_call_response` id in the same input or in a later call.
5. `previews_follow_rule_9`: `input_preview` is the last user message's text cut to 512 chars, and has at most 512 chars.
6. `small_spans_stay_small`: `root`, `tool`, `retriever` and `step` for seeds `0..200` are each under `SMALL_MAX_BYTES`.
7. `trace_summary_matches_its_spans`: for seeds `0..100`:
   - `first.len() + late.len() == span_count`;
   - no two spans share a `start_time`;
   - the summary sums equal sums over the spans;
   - exactly one span has no parent;
   - context fields are equal on every span.
8. `corpus_from_texts_pairs_turns`: 4 texts give turns `(0, 1)`, `(2, 3)`.
9. `#[ignore]` `real_corpus_loads`: `Corpus::from_env()` loads. Print the counts and assert at least 64,000 texts and 40,000 turns. The § Facts counts were taken with `text` non-empty, before cutting to `MAX_TEXT`; the loader's own filter may differ slightly. Report the exact counts. Run it once by hand after `just fetch-corpus` and report the counts.

### Commands

```bash
just fetch-corpus
cargo test -p llmobs-storage --all-features --test bench_support
cargo test -p llmobs-storage --all-features --test bench_support -- --ignored --nocapture real_corpus_loads
```

Report the counts, and the mean `bytes()` of `llm_calls(seed, 1)[0]` over seeds `0..10,000` with the real corpus (a one-off printout, not committed).

---

## R7.3 — Ingest bench on realistic payloads; measured compression

Commit: `storage: run the ingest benchmark on realistic traces and report compression (R7.3)`

### `common/parquet_stats.rs`

```rust
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Sizes { pub(crate) uncompressed: u64, pub(crate) compressed: u64 }

/// Per level ("L0", "L1"), per leaf column path: sums over every row group of
/// the given files (paths relative to `objects_dir`; local object store only).
pub(crate) fn column_sizes(
    objects_dir: &Path,
    files: &[(u8, String)],
) -> Result<BTreeMap<String, BTreeMap<String, Sizes>>, String>;

/// `(level, path)` of every **active** `spans` file, read from `catalog.db`
/// (`SELECT level, path FROM data_files WHERE table_name = 'spans' AND state = 'active'`)
/// through a read-only `sqlx` connection.
pub(crate) async fn active_spans_files(data_dir: &Path) -> Result<Vec<(u8, String)>, String>;
```

Walking `objects/tables/spans` would also count obsolete files that GC has not deleted yet: the flushed L0 inputs of a compaction, for example. The bench therefore reads the file list from the catalog. It also runs `collect_obsolete(Duration::ZERO)` before measuring, so the directory and the catalog agree.

It opens each file with `parquet::file::serialized_reader::SerializedFileReader::new(File)`. Then, for every `metadata().row_groups()` and every `rg.columns()`, it adds `column_path().string()`, `uncompressed_size()` and `compressed_size()`. `uncompressed_size` is the encoded page size before zstd. `input`, `output`, `system_instructions` and `tool_definitions` have no dictionary (spec 03 §3), so for them it is the plain text plus 4 length bytes per value.

### `ingest_bench.rs`

- Delete `text`, `build_batch`, `POOL`, `LLM_IO_BYTES`, `ATTRIBUTE_BYTES` and `SPANS_PER_TRACE`.
- `ingest_benchmark` loads `Payloads::new(Corpus::from_env())` once (outside the timing) into an `Arc`.
- `write_until`:
  - builds batches of whole traces from `payload::trace`. Trace `i` (from the shared `AtomicU64`) gets `trace_id(i)`, `trace_seed = mix(i)` and `end = now_ns()`;
  - appends `first` and `late` immediately: the bench measures throughput, not lateness;
  - cuts a batch once it holds at least `BATCH_SPANS` (1,000) spans;
  - sets each span's `ingest_time` to the batch's `now`.
- `Counts` gets:
  - `payload_bytes`: the sum of `Content::bytes()` of the spans, recorded by the builder;
  - `gen_nanos`: time spent building spans and the `RecordBatch`, measured with `Instant`.
- **Output.** Keep every existing `println!`. Add:
  - `payload: <payload bytes per span> payload bytes per span, <llm share>% LLM spans, generation <gen share>% of writer time`, where `gen share = gen_nanos / (writers × ingest_secs)`;
  - local mode only, two `compression:` blocks:
    - one after the drain, before compaction: L0, the flush output;
    - one after the compaction phase and `collect_obsolete(Duration::ZERO)`: L1, plus any L0 left.

    Each comes from `column_sizes(dir/objects, &active_spans_files(dir))`, per level, with one line each for `input`, `output`, `system_instructions`, `tool_definitions`, `attributes` (the map's key and value leaves summed), and `all columns`: `<uncompressed> → <compressed> = <ratio>×`. Then `Arrow / file bytes = <ratio>×` over the active files of that level, the measure Run 1 reported.

  S3 mode prints the `Arrow / flushed bytes` ratio only.
- **Writer count.** If `gen share` is above 50%, the bench is bound by the generator, not the engine. Rerun with `LLMOBS_INGEST_BENCH_WRITERS=12` and then 16, keep the run with the most spans/s, and report all three.

### Runs (results in `benchmarks.md`)

```bash
RUST_MIN_STACK=33554432 cargo test --release -p llmobs-storage --all-features --test ingest_bench --no-run
RUST_MIN_STACK=33554432 LLMOBS_INGEST_BENCH_DIR=~/llmobs-bench/ingest-local LLMOBS_INGEST_BENCH_SECS=60 \
  cargo test --release -p llmobs-storage --all-features --test ingest_bench -- --ignored --nocapture
```

Then the same command against Versity Gateway on disk, not tmpfs: a 60 s run writes tens of GB of Arrow. The container runs under Docker or podman (§ Prerequisites); with podman, replace `docker` with `podman`. `:Z` relabels the directory for SELinux.

```bash
mkdir -p ~/llmobs-bench/vgw
docker run -d --name llmobs-s3 -p 7070:7070 -v ~/llmobs-bench/vgw:/data:Z -e ROOT_ACCESS_KEY=llmobs -e ROOT_SECRET_KEY=llmobs-secret -e VGW_BACKEND=posix -e VGW_BACKEND_ARGS=/data -e VGW_ARGS="--port :7070" versity/versitygw:v1.11.0
curl --aws-sigv4 "aws:amz:us-east-1:s3" --user llmobs:llmobs-secret -X PUT http://127.0.0.1:7070/llmobs-bench
export LLMOBS_TEST_S3_ENDPOINT=http://127.0.0.1:7070 LLMOBS_TEST_S3_BUCKET=llmobs-bench AWS_ACCESS_KEY_ID=llmobs AWS_SECRET_ACCESS_KEY=llmobs-secret
RUST_MIN_STACK=33554432 LLMOBS_INGEST_BENCH_DIR=~/llmobs-bench/ingest-s3 LLMOBS_INGEST_BENCH_SECS=60 cargo test --release -p llmobs-storage --all-features --test ingest_bench -- --ignored --nocapture
```

### `benchmarks.md`

Add a section `## Track R M7 runs (realistic payloads)` after § Runs' existing run 7, and before § Against the capacity check. It has:
- a short paragraph: the generator, the corpus (name, licence, SHA-256, `just fetch-corpus`), the shapes and size bands;
- its own machine block (R7.1's `machine()` line);
- **Run 8** (ingest, local) and **Run 9** (ingest, S3 Versity on disk), in Run 1's format, plus the payload and compression lines.

The run numbers follow the existing table; add rows 8 and 9 to the § Runs table.

§ Against the capacity check gets a new sub-list **"With realistic payloads (runs 8–9)"** under the existing bullets, with the same bullets measured again:
- ingest rate;
- span size: Arrow bytes and payload bytes per span, against the spec's 3 KiB;
- ingest MiB/s;
- compressed output MiB/s;
- **zstd ratio** per column and overall, against the spec's "roughly 5–8×".

The hypothesis to test, from the scratch measurement in § Facts: input about 10×, output about 3×, overall 5–8×.
- **Inside or above the range:** say so.
- **Below it:** it is a finding for R7.9's spec update. Do **not** tune the generator to reach the spec's number.

§ Findings adds:
- **(f) Payload pool.** Run 1's 19× ratio came from 16 reused strings; random letters alone compress about 1.7×, and runs 8–9 replace it.
- **(g)** The erratum of R7.1, cross-referenced.

### Report

Spans/s, payload bytes per span, LLM share, generator share and the compression table, for both runs.

---

## R7.4 — `trace_bench` on realistic payloads; the 5M-trace runs

Commit: `storage: fill trace_bench spans with realistic payloads (R7.4)`, and a docs commit `docs: record the 5M-trace benchmark runs (R7.4)`.

### Code (`trace_bench.rs`)

- **Data plan unchanged.** `trace_plan` and every draw from `rng` stay as they are. The payload uses its own streams, so times, costs, splits and late groups are identical to runs 3/7 at 1M and to the 5M shape S1 designed. `rng_sequence_is_unchanged` (R7.1) guards the stream.
- **Payloads.** `generate` loads `Payloads::new(Corpus::from_env())` once.
  - `build_spans(plan, group, payloads)`:
    - `trace_seed = mix(SEED ^ mix(index))`, with `index` the trace's position, stored in `TracePlan` as a new `index: usize` field;
    - the LLM call number of an LLM span is its rank among the trace's LLM spans by `span_index`;
    - `payloads.llm_calls(seed, llm_count)`, computed once per group, is indexed by that rank;
    - `root` for span 0, `step(seed, span_index)` for the others;
    - `Content::apply` on each span.
  - `SpanPlan::preview` goes away: the previews now come from the content.
  - Tokens, cost, models and status keep the plan's values. The generator's own token counts are not used here, because the query targets are defined on the plan's costs.
- **Manifest.**
  - Add `#[serde(default)] payload: Option<String>`, set to `format!("m7:{}", &CORPUS_SHA256[..12])`. `dataset()` reuses a stored dataset only when `payload` matches, in both the S3 and the local branch. The `(true, None, None)` branch (a catalog without a manifest) now regenerates instead of trusting the directory, since its payload is unknown.
  - Add `arrow_bytes: u64` and `payload_bytes: u64`, summed during generation, and print them with the dataset line.
- **First `max_bytes`, from a measurement.** With payloads, a trace is tens of KiB of Arrow instead of a few KiB. At 128 MiB the first generation would write thousands of spans files and then regenerate (Findings (c)).
  - Add `LLMOBS_TRACE_BENCH_MAX_BYTES` (a `ByteSize` string, e.g. `384MiB`). When set, it replaces the first attempt's 128 MiB; the scaling retry stays as it is.
  - Run 10 (1M) prints `arrow bytes per trace = arrow_bytes / traces`. From it, compute the value for Run 11:

    ```
    5M max_bytes = (5,000,000 × bytes per trace) / 1,000 target files × 0.8
    ```

    The 0.8 accounts for flushes that cut a partition early. Clamp the result to `[1 MiB, 1 GiB − 1]` (the validator's range), and pass it to runs 11 and 12. Report the computation.

### Runs

Preconditions:
- `free -g` shows at least 16 GiB available (the 5M plan holds about 5 GiB);
- `df -h ~/llmobs-bench` shows at least 400 GiB free (estimate below).

| Run | Command env | Notes |
|---|---|---|
| 10 | `LLMOBS_TRACE_BENCH_DIR=~/llmobs-bench/trace-1m LLMOBS_TRACE_BENCH_TRACES=1000000` | Validation at Run 7's size: same plan, new payloads. List-query p50s should be within Run 7's pass criterion (no more than 10 ms or 20% above Run 7's p50), since lists read `traces` partials. Q4 reads `spans` with payloads now and may be slower. |
| 11 | `LLMOBS_TRACE_BENCH_DIR=~/llmobs-bench/trace-5m` (default 5,000,000) | The pending 5M run, local disk. |
| 12–14 | `LLMOBS_TRACE_BENCH_DIR=~/llmobs-bench/trace-5m-s3`, the Versity environment of R7.3, `LLMOBS_TRACE_BENCH_CACHE=cold`, then `warm`, then `off` | 12 generates onto Versity, 13–14 reuse it. |

Each command is `RUST_MIN_STACK=33554432 cargo test --release -p llmobs-storage --all-features --test trace_bench -- --ignored --nocapture`. The variable is needed on the run line too: a test-binary rebuild triggered by `cargo test` would otherwise hit the rustc stack overflow `benchmarks.md` notes.

**Expected size.** The plan has 1–5 LLM spans per trace (mean 3) at about 25 KiB each:
- about 375 GB of Arrow at 5M traces;
- 50–75 GB of Parquet at 5–8×;
- WAL traffic of roughly a third of the Arrow bytes, as spec 03's capacity check says for LZ4 bodies, transient. Text-heavy columns may compress differently under LZ4; M7 doesn't measure WAL bytes written, so this stays an estimate;
- generation probably 1–2 h per attempt.

Start each 5M run in the background with its output in a log file (`… > ~/llmobs-bench/run-11.log 2>&1`), never as a foreground command.

**A missed target is a finding, not a failure of the sub-phase.** Record the table with `EXPLAIN ANALYZE` (the bench prints it), and add a § Findings entry. Fixing it is not part of M7 unless it is a bug (R7.9); a slow query that is not a bug is raised to the coordinator.

### `benchmarks.md`

- Runs 10–14 under § Track R M7 runs, each in Run 3's format: command, dataset line, table, read cache line, and the informational metrics line.
- Rows 10–14 in the § Runs table.
- A short "5M against 1M" paragraph comparing p50/p95 of runs 7, 10 and 11 per query.
- § Commands gets the M7 commands.

---

## R7.5 — The soak: driver, sampler, JSON lines

Commit: `storage: add the soak test driver and its per-minute samples (R7.5)`

### Layout

```
crates/storage/tests/soak.rs           the #[ignore] test `soak`, module declarations, unit tests
crates/storage/tests/soak/config.rs    SoakConfig from the environment
crates/storage/tests/soak/failures.rs  Failures: what went wrong, recorded by every task
crates/storage/tests/soak/redact.rs    Redactor: secrets out of every committed file
crates/storage/tests/soak/driver.rs    ingest, scores, deletions, retention, queries; the candidate ring
crates/storage/tests/soak/sample.rs    the per-minute Sample and how it is measured
```

R7.6 adds `soak/registry.rs` and `soak/rules.rs`.

`soak.rs` starts with the allow list of § Conventions, then `#[allow(dead_code)] mod common;`, then one `#[path = "soak/<name>.rs"] mod <name>;` per file above. R7.5 adds only what its own `soak` test and unit tests use (§ Conventions, nothing unused): the oracle registry, its checks and the rules wait for R7.6.

### `SoakConfig` (`config.rs`)

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SoakConfig {
    pub(crate) dir: PathBuf,              // LLMOBS_SOAK_DIR, required
    pub(crate) out: PathBuf,              // LLMOBS_SOAK_OUT, required
    pub(crate) label: String,             // LLMOBS_SOAK_LABEL, default "local"; names the run
    pub(crate) hours: f64,                // LLMOBS_SOAK_HOURS, default 24
    pub(crate) rate: u64,                 // LLMOBS_SOAK_RATE spans/s, default 2_000
    pub(crate) projects: usize,           // LLMOBS_SOAK_PROJECTS, default 4, 1..=8
    pub(crate) qps: f64,                  // LLMOBS_SOAK_QPS, default 1.0
    pub(crate) mix: Vec<(QueryKind, u32)>,// LLMOBS_SOAK_QUERY_MIX, default "list=30,filter=15,detail=25,sessions=10,metrics=10,scores=10"
    pub(crate) retention_days: u32,       // LLMOBS_SOAK_RETENTION_DAYS, default 0 (keep today only)
    pub(crate) sample_secs: u64,          // LLMOBS_SOAK_SAMPLE_SECS, default 60
    pub(crate) warmup_hours: f64,         // LLMOBS_SOAK_WARMUP_HOURS, default max(1, hours / 10): 2.4 h for 24 h, 7.2 h for 72 h
    pub(crate) max_rss_gib: f64,          // LLMOBS_SOAK_MAX_RSS_GIB, default 8
    pub(crate) max_l0_age_min: u64,       // LLMOBS_SOAK_MAX_L0_AGE_MIN, default 30
    pub(crate) keep: bool,                // LLMOBS_SOAK_KEEP=1: keep the S3 prefix after a pass
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) enum QueryKind { List, Filter, Detail, Sessions, Metrics, Scores }

pub(crate) fn parse_mix(s: &str) -> Result<Vec<(QueryKind, u32)>, String>;
impl SoakConfig {
    /// Pure parser, as `S3Env::from_lookup` (R7.1): unit tests pass a map.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String>;
    pub(crate) fn from_env() -> Result<Self, String>;
}

/// Makes `dir` ready: creates it with the marker file `.llmobs-soak` when
/// missing or empty; empties it when it holds the marker; refuses otherwise.
pub(crate) fn prepare_data_dir(dir: &Path) -> Result<(), String>;
```

**Directory guards.**
- **`dir`.** The soak deletes `dir` recursively, so a typo must not wipe real data. `prepare_data_dir` refuses a non-empty directory that has no `.llmobs-soak` marker. It also refuses a `dir` under `/tmp` (the scratch quota, § Facts).
- **`out`.** It must be missing or empty; the soak never deletes it.

**Retention boundary, lagged by 4 h.** The soak's watermark is `keep_from = (now − 4 h).date_naive() − retention_days` (UTC). With `retention_days = 0` it keeps today plus, until 04:00 UTC, yesterday.
- **Why the lag.** A boundary at midnight would drop yesterday before its day close was due (`close_delay`, 2 h, spec 03 §5). The `day_close` and `compaction_lag` rules would then never see a closed past day: they would be vacuous. With the lag, each day is closed (from 02:00) and checked (rule B, until about 02:30 plus slack) before retention drops it at 04:00.
- **Why 0.** It lets a 24 h run cross a retention boundary, and keeps the disk within about a day of data. It drives the same `apply_retention` path a project with `retention_days ≥ 1` uses.

Both scheduled runs use 0 (R7.7, R7.8). `soak.md` § Method states the formula.

**The storage config** is the production default (spec 03), plus only:
- `data_dir`;
- `object_store` and `s3`, from `S3Env::config(&format!("soak-{}", Uuid::now_v7()))` when `S3Env::from_env()` is `Some`.

WAL sync is `"always"`, `min_free_bytes` 2 GiB, compaction every 30 s, GC every 10 min, read cache 10% of the filesystem.

### `Failures` (`failures.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) enum FailureKind {
    /// `append_spans` / `append_scores` returned anything but `Backpressure`.
    Append,
    /// A query returned `QueryError` or `RunError`.
    Query,
    /// `apply_retention` failed.
    Retention,
    /// `register_deletion` or `deletion_status` failed.
    Deletion,
    /// A covered trace was still visible after `register_deletion` returned.
    DeletionVisible,
}

/// Shared by every task (`Arc<Failures>`). Keeps a count per kind and the first 20 messages.
#[derive(Debug, Default)]
pub(crate) struct Failures { /* std::sync::Mutex<BTreeMap<FailureKind, (u64, Vec<String>)>> */ }

impl Failures {
    /// `message` holds ids, kinds and the error's `Display`, never payload.
    pub(crate) fn record(&self, kind: FailureKind, message: String);
    pub(crate) fn counts(&self) -> BTreeMap<FailureKind, u64>;
    pub(crate) fn examples(&self) -> BTreeMap<FailureKind, Vec<String>>;
}
```

R7.6 adds the variant `Oracle` when it adds the check that records it.

### `Redactor` (`redact.rs`)

```rust
#[derive(Debug, Clone)]
pub(crate) struct Redactor { pairs: Vec<(String, &'static str)> }

impl Redactor {
    /// Secrets of this run, each with its placeholder: the S3 bucket → "<bucket>",
    /// the S3 endpoint host unless it is 127.0.0.1 or localhost → "<endpoint>",
    /// $HOME → "~", the hostname (/proc/sys/kernel/hostname) → "<host>".
    /// Empty values are skipped.
    pub(crate) fn for_run(s3: Option<&S3Env>) -> Self;
    /// Every occurrence replaced, longest secret first. A secret shorter than
    /// 4 bytes is refused at construction (`for_run` panics), and every match
    /// must sit on a boundary: the bytes before and after it are not
    /// `[A-Za-z0-9_-]`. A short bucket or host name then can't rewrite JSON keys
    /// or other words.
    pub(crate) fn apply(&self, text: &str) -> String;
}
```

Every committed file goes through it:
- **`run.json` and `summary.json`.** Build the JSON value, serialize it to a string, `apply`, then write. Values are never serialized unredacted to disk.
- **The storage config.** `run.json`'s `storage_config` is `apply(&format!("{config:#?}"))`. The `Debug` output names the bucket in `ObjectStoreUrl::S3`, and the paths name the home directory.
- **`events.jsonl`.** At the end of the run, it is read line by line and `apply` is written to `events.scrubbed.jsonl`. Only the scrubbed file is ever committed (R7.7). The unscrubbed file stays in `out` on the run machine.

### Projects

`project(i) = format!("018f6b4e-7c1a-7c1a-8c1a-{:012x}", 0x50a + i)`, parsed as `ProjectId`. Project `i` gets the share `2^(n−1−i) / (2^n − 1)` of the rate (8:4:2:1 for 4 projects): one big project and small ones, so small partitions get many small files.

### Sampling trigger and candidate ring (`driver.rs`)

```rust
/// Independent of the app choice (`mix(trace_seed) % APPS`): salted.
const SAMPLE_SALT: u64 = 0x5A3C_96E1_0F2B_7D48;
pub(crate) fn is_sampled(trace_seed: u64) -> bool { mix(trace_seed ^ SAMPLE_SALT) % 500 == 0 }

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) trace_id: TraceId,
    pub(crate) start: i64,
    pub(crate) user_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) sampled: bool,
}

/// Last 10,000 **complete** traces per project (all spans appended, the late group included).
#[derive(Debug, Default)]
pub(crate) struct Candidates { /* std::sync::Mutex<Vec<VecDeque<Candidate>>> */ }
```

`mix(trace_seed) % 500` would correlate with `mix(trace_seed) % 16`, the app index: only 4 of the 16 apps would ever be sampled. Hence the salt. In R7.5 `is_sampled` feeds the `sampled` flag, which the deletion task uses below, and the sample's `sampled_traces_total` counter.

### Driver tasks (`driver.rs`)

All tasks run on the test's runtime (`#[tokio::test(flavor = "multi_thread", worker_threads = 8)]`). They stop when a shared `tokio::sync::watch::Receiver<bool>` flips to `true`.

**A mutation gate.** `gate: Arc<tokio::sync::Mutex<()>>`:
- the deletion task holds it from just before `register_deletion` until its own bookkeeping is done;
- the retention task holds it from just before `apply_retention` until its own bookkeeping is done;
- R7.6's `check_sample` holds it for its whole run.

So a check never sees a deletion or retention half-recorded. Ingest, scores and queries don't take it.

- **Ingest, one task per project.**
  - **Pacing.** A `tokio::time::interval` of 100 ms, with `MissedTickBehavior::Delay`. Each tick adds `rate_i × 0.1` to a span budget. The budget counts **first-group spans only** (`first.len()`). Late groups arrive on their own schedule, outside the budget, so the configured rate is the rate of new spans and late spans come on top (about 5% of traces × a third of their spans).
  - **New traces.** While the budget covers the next trace's span count, it generates `payload::trace(…, end = now_ns())`. The trace id is `TraceId::from_bytes([project byte, 0…, counter as u64 big-endian])`, the seed `mix(counter ^ (i << 56))`.
  - **What gets appended.**
    - The trace's `first` spans go into the tick's batch.
    - Its `late` spans go into a `BinaryHeap<Reverse<(Instant, u64)>>` (with a side `HashMap<u64, Vec<Span>>`) due at `now + delay`. Every due late group is added to the batch.
    - A trace becomes a candidate (pushed to the ring) once its last group is acknowledged.
  - **Ingest time.** Each span's `ingest_time` is set to `now_ns()` just before the append.
  - **The append.** `append_spans`. `Backpressure` sleeps 10 ms and retries, counting retries and the time waited. Any other error goes to `failures.record(Append, …)` with its `Display` text (no payload: storage errors carry none), and the batch is dropped.
  - **Stop.** At stop, every pending late group is appended at once (counted).
- **Scores, one task.** Every second, for each project, it writes a score for each trace completed in the last second with probability 0.2, as one `append_scores` batch:
  - `ScoreTarget::Trace`, `name = "judge"`, `ScoreValue::Numeric(rng.unit())`, `source = ScoreSource::LlmJudge`, `created_at = updated_at = ingest_time = now`;
  - the trace's `session_id`/`user_id`;
  - `score_id = Uuid::now_v7()`.

  A further 2% of recent scores get a new version (`updated_at = now`), and 0.5% a `deleted = true` version. The task keeps the last 10,000 score ids per project in a ring for that. Errors are handled as in ingest.
- **Deletions, one task.**
  - **Every 10 minutes, a `Traces` deletion** for a project picked by rate share. It draws `k = 1 + rng.below(20)` candidates. At least `ceil(k / 2)` of them have `sampled == true`, when the ring has that many; the rest are any candidates. Without sampled ids, the oracle would never see a deleted trace.
  - **Every hour, a `User` deletion.** The user is taken from a random **sampled** candidate that has a user, or from any candidate with a user when none is sampled. The deletion carries:
    - that user's candidate trace ids (R7.6 adds every registry trace of the user);
    - the sessions of those traces, as `session_ids`.
  - **Registration.** Under the gate: `cutoff = now_ns()`, `id = Uuid::now_v7()`, `register_deletion`. Right after it returns, for up to 5 covered trace ids, `trace_detail` (context with `around = start`) must return `None`; otherwise `failures.record(DeletionVisible, …)`. R7.6 adds its registry update here, inside the gate.
  - **Status.** Every pending id's `deletion_status` is polled once a minute, and the time each one turns `Applied` is recorded. An error is a `Deletion` failure.
- **Retention, one task.** Every hour, and once at start, it calls `apply_retention(project, keep_from)` for every project, under the gate, with the lagged `keep_from` above. The task keeps the last `keep_from` it applied for the sampler. It records the `RetentionReport`, or a `Retention` failure.
- **Queries, one task.**
  - **Pacing.** It runs `qps` queries per second in sequence, each kind picked by the mix weights and each project by rate share.
  - **Context.** Every query builds its own `QueryContext::new(storage.query(project, opts).await?, None)`, with `timeout: 30 s`. "Last 1 h" is `TimeWindow::new(now − 1 h, now)`.
  - **The kinds:**
    - `List`: `trace_list`, window last 1 h, `include_eval: false`, `include_experiments: false`, `TraceSort::StartTime`, `SortOrder::Desc`, limit 50;
    - `Filter`: as `List` with filter `"cost_total > 0.015"`;
    - `Detail`: `trace_detail` of a random candidate, with `QueryOptions { around: Some(start) }`;
    - `Sessions`: `session_list`, window last 1 h, `SessionSort::StartTime`, `Desc`, limit 50;
    - `Metrics`: `metrics`, window last 24 h, `Bucket::Fixed(BucketWidth::H1)`, no filter or group, `include_eval: false`, `include_experiments: true`;
    - `Scores`: `score_list`, every filter `None`, window last 1 h, limit 50.
  - **Recording.** The latency of each call goes into a per-kind `Vec<f64>` that the sampler drains. An error is a `Query` failure with the kind and the error's `Display`.
  - **Timeouts.** A `RunError::Timeout` (most likely `Metrics`, whose 24 h window grows with the data) also lands here and fails the `queries` rule. R7.9 classifies it as capacity unless its cause is a bug. Don't raise the 30 s timeout to make a run pass.

### Sampler (`sample.rs`)

Every `sample_secs`, one `Sample` is serialized as a JSON line to `<out>/samples.jsonl` and the file is flushed. Counters are cumulative (`_total`). No field holds a path, a host or a bucket, so samples need no redaction.

```rust
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Sample {
    pub(crate) t: i64,                          // unix seconds
    pub(crate) elapsed_s: f64,
    // process
    pub(crate) rss_bytes: u64,                  // /proc/self/status VmRSS (kB × 1024)
    pub(crate) hwm_bytes: u64,                  // VmHWM
    pub(crate) threads: u64,                    // Threads
    pub(crate) fds: u64,                        // entries of /proc/self/fd
    pub(crate) cpu_s: f64,                      // /proc/self/stat utime + stime, / 100 (USER_HZ)
    pub(crate) load1: f64,                      // /proc/loadavg first field
    // ingest
    pub(crate) target_spans_total: u64,         // rate × elapsed (first-group spans)
    pub(crate) appended_spans_total: u64,       // first-group spans acknowledged
    pub(crate) late_spans_total: u64,           // late-group spans acknowledged, counted apart
    pub(crate) appended_scores_total: u64,
    pub(crate) sampled_traces_total: u64,
    pub(crate) backpressure_total: u64,
    pub(crate) backpressure_wait_ms_total: u64,
    pub(crate) failures_total: BTreeMap<String, u64>, // FailureKind name → count
    // storage
    pub(crate) memtable_bytes: u64,             // health()
    pub(crate) wal_failed: bool,                // health(): either table
    pub(crate) wal_bytes: u64,                  // sum of file sizes under dir/wal/
    pub(crate) files: BTreeMap<String, LevelFiles>, // "spans" | "traces" | "scores"
    pub(crate) partitions: u64,                 // distinct (table, project, day) with active files
    pub(crate) obsolete_files: u64,
    pub(crate) quarantined_files: u64,
    pub(crate) staged_objects: u64,
    pub(crate) l0_oldest_age_s: f64,            // spans L0 files of today's UTC day, 0 if none
    pub(crate) overdue_partitions: u64,         // rule B below
    pub(crate) retention_violations: u64,       // active files of a day before the project's keep_from
    pub(crate) pending_deletions: u64,
    pub(crate) oldest_pending_deletion_s: f64,
    pub(crate) objects_bytes: u64,              // local: file sizes under dir/objects/tables; S3: listing
    pub(crate) objects_files: u64,
    pub(crate) orphans: u64,                    // confirmed orphans, below
    pub(crate) free_bytes: u64,                 // rustix::fs::statvfs(dir): f_bavail × f_frsize
    pub(crate) tmp_bytes: u64,                  // dir/tmp/
    pub(crate) cache: Option<CacheSample>,      // read_cache_stats()
    // queries since the last sample
    pub(crate) queries: BTreeMap<String, QuerySample>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct LevelFiles { pub(crate) l0: u64, pub(crate) l1: u64, pub(crate) l0_bytes: u64, pub(crate) l1_bytes: u64 }
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CacheSample { pub(crate) hits: u64, pub(crate) misses: u64, pub(crate) bytes: u64, pub(crate) blocks: u64 }
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct QuerySample { pub(crate) n: u64, pub(crate) p50_ms: f64, pub(crate) p95_ms: f64, pub(crate) max_ms: f64, pub(crate) errors: u64 }
```

**Catalog reads.** One read-only connection is opened at start (§ Conventions). Each sample first **lists the objects**, then runs one transaction (`BEGIN`, the `SELECT`s below, `COMMIT`):
- `SELECT table_name, project_id, day, level, COUNT(*), SUM(size_bytes), MIN(created_at), MAX(created_at) FROM data_files WHERE state = 'active' GROUP BY 1, 2, 3, 4`;
- `SELECT state, COUNT(*) FROM data_files GROUP BY state`;
- `SELECT path FROM data_files` (every state), and `SELECT path FROM staged_objects`;
- `SELECT project_id, keep_from FROM retention_watermarks`;
- `SELECT COUNT(*), MIN(registered_at) FROM deletions WHERE status = 'pending'`.

Confirm in `crates/storage/src/catalog/` that `created_at` and `registered_at` are nanoseconds from `catalog::now_ns()`; if one is not, convert and say so in the report.

**Derived values:**
- `l0_oldest_age_s`: `now − MIN(created_at)` over level-0 `spans` rows whose `day` is today (UTC).
- `overdue_partitions` (rule B). A partition of `spans` whose day has ended is overdue when it has L0 files and `now − max(end of day, MAX(created_at) of its L0 files) > close_delay + 30 min`, with `close_delay` 2 h (spec 03 §5's day-close rule plus slack).
- `retention_violations`: active rows whose `day` is before their project's stored watermark (`retention_watermarks.keep_from`, the value the last `apply_retention` committed), not the lagged formula evaluated at sample time. Between hourly calls, a day may legitimately sit before the formula's value.
- **`orphans`, two-step.** Listing the objects before the catalog read means an object written after the listing can't look orphaned. But GC deletes an object, then its row: a listing taken before the delete and a catalog read taken after it still disagree, for an instant. So:
  1. **Suspects.** Every listed path (local: under `dir/objects/tables`, relative to `dir/objects`, skipping names that start with `.` and contain `.tmp-`; S3: under `<prefix>/tables/`, with `<prefix>/` stripped) that is in neither path set.
  2. **Confirmed.** A suspect that was also a suspect in the previous sample (set intersection), and whose object is older than 10 minutes: local `metadata().modified()`, S3 `ObjectMeta::last_modified`.

  `orphans` counts the confirmed ones. Keep the suspect set between samples.

  ```rust
  /// Returns (confirmed count, this sample's suspects).
  pub(crate) fn orphans(
      listed: &[(String, i64)],          // path, modification time (unix s)
      known: &HashSet<String>,           // data_files + staged_objects paths
      previous: &HashSet<String>,        // last sample's suspects
      now_s: i64,
  ) -> (u64, HashSet<String>);
  ```

**Process values.** Parse `/proc` with plain string splitting. A missing field is a test failure (`expect`): the soak runs on Linux only.

### Output files

- `<out>/run.json`, written at start through the `Redactor`:
  - `machine(&dir)` (R7.1);
  - the `SoakConfig` (paths come out as `~/…`);
  - the store: `"local"` or `{ "endpoint": …, "region": …, "bucket": …, "prefix": … }`, where the redactor turns the endpoint host and the bucket into placeholders;
  - the corpus file name and `CORPUS_SHA256`;
  - `storage_config`, the redacted `{:#?}` string;
  - the start time.
- `<out>/samples.jsonl`.
- `<out>/events.jsonl`. At start, `tracing_subscriber::fmt().json().with_max_level(tracing::Level::INFO).with_writer(std::sync::Mutex::new(file)).try_init()`. These are the storage's own log events, already free of payload content (`AGENTS.md`). At the end, `events.scrubbed.jsonl` is written (§ `Redactor`).
- `<out>/failures.json` at the end: the `Failures` counts and examples, through the redactor.

### End of the run in R7.5

1. Flip the stop signal and join every task. The ingest tasks append their pending late groups first.
2. Take one last sample, `shutdown()`.
3. Write `failures.json` and `events.scrubbed.jsonl`.
4. Fail the test (`panic!`) if any failure was recorded.

R7.6 replaces this with the full end of run.

### Smoke run

```bash
LLMOBS_SOAK_DIR=~/llmobs-bench/soak-smoke LLMOBS_SOAK_OUT=~/llmobs-bench/soak-smoke-out LLMOBS_SOAK_HOURS=0.25 LLMOBS_SOAK_SAMPLE_SECS=10 \
  cargo test --release -p llmobs-storage --all-features --test soak -- --ignored --nocapture --exact soak
```

Report:
- the line count of `samples.jsonl`;
- the achieved spans/s;
- the last sample;
- `failures.json`;
- that `grep -c` of your hostname, your home path and (S3) the bucket name in `run.json`, `failures.json` and `events.scrubbed.jsonl` is 0.

### Unit tests (`soak.rs`, not ignored)

- `parse_mix_reads_weights`: the default string parses to six kinds; `"list=0"` is an error (every weight ≥ 1), and so is an unknown kind.
- `config_defaults`: `SoakConfig::from_lookup` with only `LLMOBS_SOAK_DIR` and `LLMOBS_SOAK_OUT` gives the defaults above, and a `/tmp/x` dir is refused.
- `prepare_refuses_unmarked_dirs`: in a `tempfile::tempdir()` (allowed: the guard checks the configured path, not this helper's argument; test `prepare_data_dir` directly):
  - an unmarked directory holding a file is refused;
  - a marked one is emptied and keeps its marker;
  - a missing one is created with the marker.
- `proc_status_parses`: a fixed `/proc/self/status` excerpt gives the right RSS, HWM and threads (the parser takes the text).
- `orphans_need_two_samples_and_age`:
  - a path unknown in one sample only is not confirmed;
  - unknown in two samples but 5 minutes old, it is not confirmed;
  - unknown in two samples and 11 minutes old, it is;
  - `.x.parquet.tmp-0190…` is never a suspect.
- `sampling_is_independent_of_the_app`: over seeds `0..2,000,000`, every one of the 16 values of `mix(seed) % 16` has at least one sampled seed.
- `redactor_replaces_secrets`: bucket, endpoint host, `$HOME` and hostname are replaced in a text; `127.0.0.1` stays.

---

## R7.6 — Soak rules, the sampled-trace oracle, the final checks

Commit: `storage: add the soak pass/fail rules and its sampled-trace checks (R7.6)`

`soak.rs` gets two more module lines: `#[path = "soak/registry.rs"] mod registry;` and `#[path = "soak/rules.rs"] mod rules;`. `FailureKind` gets the variant `Oracle` (oracle mismatches).

### Registry (`registry.rs`)

```rust
/// Behind a std::sync::Mutex, shared by the ingest, deletion and retention tasks.
#[derive(Debug, Default)]
pub(crate) struct Registry { /* sampled traces by (project, trace id) */ }

#[derive(Debug, Clone)]
pub(crate) struct Sampled {
    pub(crate) project: usize,
    pub(crate) summary: TraceSummary,
    /// Last ingest time of its spans so far.
    pub(crate) max_ingest: i64,
    /// Every group appended.
    pub(crate) complete: bool,
    pub(crate) state: Expect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect { Visible, Deleted, Retained, Skip }
```

`TraceSummary` holds no payload; its derived `Debug` is fine.
- **Which traces.** The ingest task registers a trace when `is_sampled(trace_seed)` (R7.5) and none of its spans starts or ends within 1 h of a UTC midnight. That keeps the anchor rule (spec 03 § Trace anchor) out of the oracle: the trace's partition is its start day.
- **Coverage**, applied by the deletion task inside the gate.
  - **`Deleted`.** A deletion moves a sampled trace to `Deleted` only when its id is in the deletion's `trace_ids`, it is complete, and its `max_ingest ≤ cutoff`. Storage hides `traces` partials by trace id only (`deletion.rs`, the tombstone filter). A user-matching span row is hidden, but the trace row stays, so `trace_detail` still returns `Some` for a user-matching trace whose id isn't listed.
  - **`User` deletions list their traces.** The deletion task adds every registry trace of that user to the deletion's `trace_ids`, besides the candidates, as the server resolves a user deletion (D-M-3). Those traces then follow the rule above.
  - **`Skip`.** A sampled trace that has that user but is not in `trace_ids` (it was not yet registered, or it is incomplete) is set to `Skip`. So is an incomplete trace that is in `trace_ids`.
- **Retention**, applied by the retention task inside the gate, moves `Visible` traces whose start day is before the `keep_from` just applied (the lagged one) to `Retained`.

### Oracle check

```rust
/// Holds `gate` for its whole run. Compares storage with the registry for up
/// to `live` visible, `gone` deleted and `gone` retained sampled traces (seeded
/// random picks), complete and older than 5 minutes. Returns the mismatches,
/// each "<trace hex>: <field> expected <e> got <g>".
pub(crate) async fn check_sample(
    storage: &Storage,
    registry: &std::sync::Mutex<Registry>,
    gate: &tokio::sync::Mutex<()>,
    live: usize,
    gone: usize,
    seed: u64,
) -> Vec<String>;
```

Its picks are cloned out of the registry under the gate, before any query. The `std::sync::Mutex` is never held across an `.await`.

**Retry.** A read that fails with `QueryError::Busy` or `RunError::Timeout`, or whose context can't be built, is retried once after 1 s. A second failure is an `Oracle` failure with the error's `Display`, not a mismatch.
- **`Visible`.** `trace_detail` with `around = start` returns `Some`. Its `trace` row matches the summary:
  - `name`;
  - `start_time`/`end_time` (`TimestampNanosecond` columns);
  - `span_count`, `error_count`, `llm_count`, `total_tokens`;
  - `cost_total`, within 1e-9;
  - `session_id`, `user_id`.

  `spans` holds `span_count` rows. Column names are the constants in `llmobs_core::schema::traces`.
- **`Deleted` and `Retained`.** `trace_detail` returns `None`.
- The soak's traces have no re-sends and no ties, and their context is constant, so these values are exact before compaction too (spec 01 § Order).

It runs every hour (200 live, 50 + 50 gone) as a task of its own, and at the end (2,000 live, every deleted and retained trace up to 500 each). Each mismatch is a `failures.record(Oracle, …)`.

### End of the run (`soak` test), replacing R7.5's

1. **Stop.** Flip the stop signal and join every task. The ingest tasks append their pending late groups first. Take one last sample.
2. **Restart.** `shutdown()`, timed (`shutdown_s`). Drop. `Storage::open` again, timed (`reopen_s`): this is WAL replay plus anchor rebuild under a soak-sized catalog.
3. **Check.** `check_sample` at full size.
4. **Settle.** Background compaction is still running, and the rows replayed from the WAL in step 2 are in the memtable, so a single pass is not enough. Repeat the round below until one round changes nothing (no compaction job, no new file, every deletion applied), at most 3 rounds:
   1. **Flush.** One path, chosen when R7.6 starts by `grep -rn "pub async fn flush_now" crates/storage/src/lib.rs`, and named in the report:
      - **found:** call `storage.flush_now()`;
      - **not found:** wait `flush.max_age` + 10 s (130 s), so the timed flush writes the replayed rows.

      Don't write both paths. Afterwards `health().memtable_bytes` must be 0.
   2. **Explicit passes at `now + 3 days`** (`compact` and `apply_deletions` take `now` as a parameter):
      - `compact` until a pass reports `jobs == 0 && failed == 0`, at most 50 passes. Earlier passes may report a failed job: it is retried, and the job may have been deferred for disk space or lost a race with the background task. If the 50th pass still has jobs or failures, the `settled` rule fails with the last report;
      - `apply_deletions` until every registered deletion reads `Applied`, at most 20 passes.
   3. **GC.** `collect_obsolete(Duration::ZERO)`, then `collect_staged(Duration::ZERO)`.
5. **Settled state.** Take one more sample. It must show:
   - `staged_objects == 0` and `overdue_partitions == 0`;
   - no `spans` partition with an L0 file;
   - **no suspect orphan at all.** Nothing writes any more, so the two-sample rule doesn't apply here: every listed path must be in the catalog.
6. **Check again.** `check_sample` again.
7. **Clean up.** `shutdown()`. In S3 mode, when every rule passed and `keep` is false, `S3Env::delete_prefix(prefix)`.
8. **Write and fail.** Write `<out>/summary.json` and `failures.json` through the redactor, then `events.scrubbed.jsonl`, and print the summary. The test fails (`panic!`) when `passed` is false, after writing everything.

### Rules (`rules.rs`, pure functions over `&[Sample]`, the failure counts and the run config)

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) enum Verdict {
    Pass,
    Fail(String),
    /// Not enough data to decide: listed in summary.json, does not fail the run.
    Skipped(String),
    /// Computed and reported, but the run is too short for the rule to be meaningful; does not fail the run.
    Info(String),
}
```

**`passed` is true when no rule is `Fail`.** `Skipped` and `Info` rules are listed in `summary.json` with their detail, and in `soak.md`'s rules table. A claim can rely only on rules that read `Pass`.

**Growth warm-up.** `growth_warmup_h = max(warmup_hours, (retention_days + 1) × 24)`, the time disk and file counts need to reach their daily cycle.
- **Enforced** when `hours ≥ growth_warmup_h + 24`. With `retention_days = 0`, that means runs of 48 h or more: the 72 h run.
- **Otherwise informational:** the growth rules are evaluated after `warmup_hours` and reported as `Info`. That covers the 24 h dry run and every smoke.

**Growth rule.** For a series of `(elapsed_s, value)` after the warm-up:
- split it into 4 windows of equal duration and take each window's 95th percentile, `q1 … q4`;
- the metric **grows** when `q1 < q2 < q3 < q4` (strictly) and `q4 − q1 > rel × q1 + abs`;
- fewer than 8 samples per window gives `Skipped`.

```rust
pub(crate) fn grows(series: &[(f64, f64)], rel: f64, abs: f64) -> Verdict;
pub(crate) fn evaluate(samples: &[Sample], failures: &BTreeMap<FailureKind, u64>, config: &SoakConfig, extra: &EndChecks) -> Vec<(String, Verdict)>;
```

`EndChecks` holds step 5's results and the deletion lag list.

**Ingest-rate windows.** These are independent of the growth warm-up: the soak must keep up from the start.
- Consecutive windows of 1 hour, starting 15 minutes after the start, for runs of at least 2 hours; of 10 minutes for shorter runs.
- Each window's `Δappended_spans_total / Δtarget_spans_total` must be at least 0.99. Both counters cover first-group spans only; `late_spans_total` is reported, never in the ratio. Otherwise a burst of due late groups would hide a shortfall, or the late groups pending at stop would inflate the ratio.
- A trailing window shorter than half the window length is ignored.

| Rule | Check | Bound | Kind |
|---|---|---|---|
| `rss_growth` | `grows(rss_bytes)` | rel 0.10, abs 256 MiB | growth |
| `fd_growth` | `grows(fds)` | rel 0.10, abs 64 | growth |
| `thread_growth` | `grows(threads)` | rel 0.10, abs 8 | growth |
| `wal_growth` | `grows(wal_bytes)` | rel 0.5, abs 2 × `wal.segment_bytes` | growth |
| `memtable_growth` | `grows(memtable_bytes)` | rel 0.5, abs `flush.max_bytes` | growth |
| `l0_growth` | `grows(files.spans.l0)` | rel 0.5, abs `compaction.l0_files` | growth |
| `query_growth_<kind>` | `grows(p95_ms)` per kind (`metrics`'s 24 h window fills in the first day, which the warm-up covers) | rel 0.5, abs 50 ms | growth |
| `rss_max` | max `rss_bytes` | ≤ `max_rss_gib` | harness |
| `ingest_rate` | windows above | ≥ 0.99 | harness |
| `compaction_lag` | max `l0_oldest_age_s` after `warmup_hours` | ≤ `max_l0_age_min` | harness |
| `day_close` | `overdue_partitions` | 0 at every sample | harness |
| `retention` | `retention_violations` | 0 at every sample | harness |
| `deletion_lag` | max `oldest_pending_deletion_s`; every registered deletion applied by the end | ≤ `deletion.max_delay` (24 h, spec 03 §7) | harness |
| `orphans` | `orphans` | 0 at every sample | harness |
| `wal_failed` | any sample | never | harness |
| `appends` | `Append` failures | 0 | harness |
| `queries` | `Query` failures | 0 | harness |
| `deletions` | `Deletion` and `DeletionVisible` failures | 0 | harness |
| `retention_calls` | `Retention` failures | 0 | harness |
| `log_errors` | `"level":"ERROR"` lines in `events.jsonl` | 0 | harness |
| `oracle` | `Oracle` failures | 0 | harness |
| `settled` | step 5's conditions | all hold | harness |

Growth rules follow the warm-up rule above (`Pass`/`Fail`, or `Info`). Harness rules are always enforced.

Disk usage is **not** a growth rule. With retention by day, it rises for `retention_days + 1` days and then follows a daily sawtooth; leaks show up as `orphans` and `retention` instead. `objects_bytes` and `free_bytes` are plotted and reported.

`summary.json`:
- `{ "label", "passed", "rules": [{ "name", "verdict", "detail" }], "stats" }`;
- `stats` holds:
  - achieved spans/s, payload bytes per span, scores written;
  - deletions registered and applied, with lag p50/p95/max;
  - retention reports;
  - `shutdown_s`, `reopen_s`;
  - per query kind: count, p50, p95 and max after `warmup_hours`;
  - post-warm-up min/median/p95/max of RSS, fds, WAL bytes, memtable bytes, L0 count, `l0_oldest_age_s`, `objects_bytes`;
  - the ERROR count and the WARN count by `fields.message` (top 20).

### Unit tests (`soak.rs`)

- `flat_noise_passes`: 1,000 samples of `100 ± 5` (seeded) → `Pass`.
- `steady_growth_fails`: `100 + 0.05 × i` over 1,000 samples → `Fail`.
- `daily_sawtooth_passes`: 72 h at one sample per minute, a sawtooth from 50 to 100 with a 24 h period → `Pass`.
- `short_series_is_skipped`: 20 samples → `Skipped`.
- `short_runs_make_growth_informational`: `evaluate` with `hours = 24` and `retention_days = 0` gives `Info` for every growth rule, even on steadily growing RSS, and `passed` stays true when the harness rules pass.
- `ingest_rate_rule_flags_a_shortfall`:
  - a 1-hour window at 0.95 of target fails;
  - a trailing 20-minute window at 0.5 is ignored;
  - a window at 0.95 of first-group spans plus enough `late_spans_total` to exceed 1.0 combined still fails.
- `coverage_marks_user_traces`. A `User` deletion of user `u`:
  - a complete sampled trace of `u` whose id is in `trace_ids` becomes `Deleted`;
  - a complete sampled trace of `u` whose id is **not** in `trace_ids` becomes `Skip`, never `Deleted`;
  - an incomplete one becomes `Skip`;
  - a trace of another user stays `Visible`.

  Add a second case: the deletion task's id list for `u` contains every registry trace of `u`.

### Run

A 30-minute smoke (`LLMOBS_SOAK_HOURS=0.5`, `LLMOBS_SOAK_WARMUP_HOURS=0.1`, `LLMOBS_SOAK_SAMPLE_SECS=10`). Its growth rules are `Info` by construction, and every harness rule must `Pass`. The settle step includes a 130 s flush wait unless `flush_now` exists. Report `summary.json`.

---

## R7.7 — Plots, recipes, the write-up format, and the 24 h dry run

Commits: `tools: add the soak plot script and recipes (R7.7)`, then `docs: record the 24 h soak dry run (R7.7)`.

### `tools/soak/` (uv project)

- `pyproject.toml`: `name = "llmobs-soak-plot"`, `requires-python = "==3.13.*"`, `dependencies = ["matplotlib==3.11.2"]`, `[dependency-groups] dev = ["ruff==0.17.3", "mypy==2.4.1"]`, `[tool.uv] package = false` and `exclude-newer = "7 days"` (D74/D117, as `tools/capture/py`), plus ruff (`target-version = "py313"`, `line-length = 100`, the same `select` list as capture) and `mypy` `strict = true`.
- `.python-version` with `3.13`.
- `uv.lock`, committed (`uv lock` inside `tools/soak`).
- `plot.py`:
  - **Arguments.** `python plot.py <out-dir> [--warmup-hours H]`, defaulting to `run.json`'s `warmup_hours`.
  - **Input.** It reads `samples.jsonl` or `samples.jsonl.gz`.
  - **Plots.** It writes PNGs (`matplotlib.use("Agg")`, 1200×500 px) to `<out-dir>/plots/`, with x in hours and the warm-up shaded:
    - `memory.png`: RSS and HWM, GiB;
    - `fds_threads.png`;
    - `ingest.png`: achieved spans/s per sample interval, target line, backpressure waits;
    - `wal_memtable.png`;
    - `files.png`: L0 and L1 counts per table;
    - `lag.png`: `l0_oldest_age_s` in minutes, `oldest_pending_deletion_s` in hours, the bound lines;
    - `disk.png`: `objects_bytes`, `free_bytes`, `tmp_bytes`;
    - `cache.png`: hit ratio per interval, S3 runs only;
    - `queries.png`: p95 per kind in ms, log scale.
  - **Table.** It prints a Markdown table of post-warm-up min/median/p95/max for the plotted series, for pasting into `soak.md`.
  - **Robustness.** It tolerates `cache: null` and missing query kinds.

`justfile`:

```just
# Storage soak (D-R-11). Needs LLMOBS_SOAK_DIR and LLMOBS_SOAK_OUT; see crates/storage/tests/soak.rs
soak:
    RUST_MIN_STACK=33554432 cargo test --release -p llmobs-storage --all-features --test soak -- --ignored --nocapture --exact soak

# Plots of a soak run's samples into <out>/plots
soak-plot out:
    cd tools/soak && uv run --locked python plot.py {{absolute_path(out)}}

# Lint and type-check the soak plot script
check-soak-plot:
    cd tools/soak && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy plot.py
```

`check-soak-plot` is not part of `just check`, as `check-capture` isn't. Run it before this commit.

### `AGENTS.md`

- **§ Commands table**, one row each:
  - `just fetch-corpus`: download the pinned benchmark corpus (oasst2) into `target/bench-corpus`;
  - `just soak`: the storage soak (D-R-11), configured by `LLMOBS_SOAK_*`;
  - `just soak-plot <out>`: plots of a soak run;
  - `just check-soak-plot`: lint and type-check `tools/soak`.
- **§ Tests**, one bullet. `LLMOBS_BENCH_CORPUS` overrides the corpus path. `LLMOBS_TEST_S3_REGION` (default `us-east-1`) and `LLMOBS_TEST_S3_PATH_STYLE` (default `true`) complete `LLMOBS_TEST_S3_ENDPOINT` for the benches and the soak. `LLMOBS_SOAK_*` configure the soak (`crates/storage/tests/soak.rs`).
- **§ Layout**, under `tools/`: `tools/soak/  soak plots (uv)`.

### `docs/research/storage/soak.md` (new): the write-up format

This structure lets every public claim point at a run, a number and its raw data.

```markdown
# Storage soak

What the soak does (one paragraph), the decision (D-R-11) and the plan (docs/plans/r-hardening/07-m7-scale-soak.md).

## Method
- Workload: rate, projects and shares, trace shapes and payload sizes, corpus (name, licence, SHA-256), scores, deletions, retention, query mix.
- Storage config: production defaults (spec 03); the only overrides.
- Samples: the fields, every minute.
- Rules: the table of R7.6, with bounds.

## Runs
| id | date | commit | machine | store | hours | rate | result | data |
(one row per run; `data` links docs/research/storage/soak/<id>/)

## <id>: <one-line description>
- Machine: the R7.1 line (CPU, threads, RAM, disk model, rota, filesystem and mount options, kernel, container).
- Command: the exact environment (credentials elided) and `just soak`.
- Result: the rules table from summary.json (name, verdict, detail); `Info` and `Skipped` rules are shown as such, never as passes.
- Key numbers: the plot script's table, plus shutdown/reopen time, deletion lag, query p95s.
- Plots: the nine PNGs, inline.
- Caveats: anything that limits what the run shows (shared machine, loopback S3, run shorter than retention + 2 days, …).
- Deviations from the plan.

## Claims
| claim | evidence (run id, metric, value) | caveats |
Only claims a run supports, each naming the run's rate, machine and duration. For example: "on a Hetzner AX52, 2,000 spans/s for 72 h with RSS within X GiB and no growth (rule rss_growth: Pass)". A claim never states a rate above the one the run sustained, and never rests on an `Info` or `Skipped` rule.
```

Raw data per run is committed under `docs/research/storage/soak/<id>/`:
- `run.json`, `summary.json` and `failures.json` (already redacted by the soak);
- `samples.jsonl.gz` (`gzip -9`);
- `events.jsonl.gz`, compressed from **`events.scrubbed.jsonl`**, never from `events.jsonl`;
- `plots/*.png`.

`events.jsonl.gz` is committed when it is under 2 MiB; otherwise its counts in `summary.json` stand in for it, and the file stays on the run machine. The run id is `<YYYYMMDD>-<label>-<hours>h`, e.g. `20261120-ws-24h`. No file names the host.

**Before committing**, grep for the hostname, the home directory, the bucket name and the endpoint host. The count must be 0 in:
- every file of the run directory, decompressed;
- `docs/research/storage/benchmarks.md`;
- any bench or soak log quoted in the commit.

This applies to every M7 docs commit, R7.8's included. `journalctl` output is only kept as `journalctl -o cat`, which has no hostname prefix.

### `docs/research/storage/hardening.md`

Add `## M7 — Scale and soak`, with:
- what M7 covers, in one list;
- `### Runs`: date, commit, run, machine, result, linking `soak.md` and `benchmarks.md`;
- `### Findings`: "None yet." until R7.9.

### The 24 h dry run (D-R-11)

On the workstation, in S3 mode against Versity Gateway on disk (R7.3's container, with `:Z`), so the configuration matches the Hetzner run. `LLMOBS_SOAK_RETENTION_DAYS=0` makes the run cross one UTC midnight with retention dropping the previous day.

```bash
export LLMOBS_TEST_S3_ENDPOINT=http://127.0.0.1:7070 LLMOBS_TEST_S3_BUCKET=llmobs-bench AWS_ACCESS_KEY_ID=llmobs AWS_SECRET_ACCESS_KEY=llmobs-secret
export LLMOBS_SOAK_DIR=$HOME/llmobs-bench/soak-ws LLMOBS_SOAK_OUT=$HOME/llmobs-bench/soak-ws-out LLMOBS_SOAK_LABEL=ws \
  LLMOBS_SOAK_HOURS=24 LLMOBS_SOAK_RETENTION_DAYS=0
systemd-run --user --unit llmobs-soak-ws --same-dir -p LimitNOFILE=65536 \
  --setenv=PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/bin:/bin \
  --setenv=LLMOBS_TEST_S3_ENDPOINT --setenv=LLMOBS_TEST_S3_BUCKET --setenv=AWS_ACCESS_KEY_ID --setenv=AWS_SECRET_ACCESS_KEY \
  --setenv=LLMOBS_SOAK_DIR --setenv=LLMOBS_SOAK_OUT --setenv=LLMOBS_SOAK_LABEL --setenv=LLMOBS_SOAK_HOURS \
  --setenv=LLMOBS_SOAK_RETENTION_DAYS just soak
journalctl --user -o cat -u llmobs-soak-ws -f
```

`--setenv=NAME` without a value copies the variable from the caller's environment.
- **What it can prove.** A 24 h run with retention 0 is shorter than the growth warm-up plus a day (R7.6), so its growth rules are `Info`. The dry run checks the harness rules (lag, retention, deletions, orphans, errors, oracle) and the tooling; growth is enforced by the 72 h run.
- **Estimates.** At 2,000 spans/s and about 4–5 KiB per span, expect about 0.7–0.8 TB of Arrow per day, and 100–150 GB per day in the bucket at 5–8×. With retention 0, at most about a day of that is kept.
- **Caveat.** The workstation is shared (the dry run's own `load1` samples show it), so latency and RSS are noisier than on a dedicated server. A failed rule there is still investigated, in R7.9.
- **After the run.** `just soak-plot ~/llmobs-bench/soak-ws-out`. Copy the committed files into `docs/research/storage/soak/<id>/` and write the run's section of `soak.md`.

The docs commit includes the run. **A failed rule is a finding for R7.9**: record it in `hardening.md` and commit the run anyway.

---

## R7.8 — Hetzner: the bucket benchmarks and the 72 h soak (user-run)

Commit: `docs: record the Hetzner benchmark and 72 h soak runs (R7.8)`

The user rents the server and the bucket and runs the commands. The implementer copies this checklist into `soak.md` § Running on a rented server (R7.7), then records the results. Nothing of the server's address, the bucket name or the keys enters the repository.

### The machine: a dedicated Hetzner AX52 (user's choice of a dedicated AX server, 2026-11-15)

**AX52**: AMD Ryzen 7 7700 (8 cores, 16 threads), 64 GB DDR5, 2 × 1 TB Gen4 NVMe, bare metal, monthly billing (Hetzner's product page, checked 2026-11-15; confirm the configuration and price on the order page).

Why it rather than the AX42 (Ryzen 7 PRO 8700GE, 64 GB, 2 × 512 GB NVMe):
- **One disk for the data alone.** The AX52's two 1 TB disks are used without RAID: one for the OS, one dedicated to the benchmark and soak data. The soak then measures a disk nothing else writes to. The AX42's 512 GB disks would need the OS on the data disk, or leave about 450 GB for data. The 5M local benchmark copy (about 50–75 GB), the read cache (10% of the filesystem) and a 5,000 spans/s soak day (about 250–400 GB with retention 0) don't fit beside each other there.
- **Same core count as spec 03's target box** (8 cores). Its memory is larger (64 GB against 32 GiB), so the soak runs inside a 32 GiB memory limit (`-p MemoryMax=32G`, step 10). The run then also shows the engine lives within the target box's memory.

### Provisioning checklist

1. **Order** the AX52 in the location of the bucket's region: Falkenstein for `fsn1`, Nuremberg for `nbg1`, Helsinki for `hel1`. A dedicated server ships in the Rescue System.
2. **Install the OS with `installimage`** from the Rescue System (no cloud-init on dedicated servers):
   - image: Ubuntu 24.04 LTS;
   - `SWRAID 0`: no software RAID;
   - `DRIVE1 /dev/nvme0n1` only, with the second disk left out of the install;
   - partitions: the default `/boot` and swap lines removed (`PART swap` deleted: the soak must not swap), and `PART / ext4 all`;
   - hostname: anything; it never enters a committed file.

   Reboot, log in, and record `uname -r`.
3. **The data disk.**

   ```bash
   mkfs.ext4 -L llmobs-data /dev/nvme1n1
   mkdir -p /data && echo 'LABEL=llmobs-data /data ext4 defaults,noatime 0 2' >> /etc/fstab && mount /data
   chown <user>: /data
   ```

   ext4 without compression, `noatime`, nothing else on it. The R7.1 `machine()` line records the disk model and `rota=0`. Next to it, `soak.md` names the disk type from Hetzner's product page ("Gen4 NVMe"), because `lsblk`'s model string doesn't say it.
4. **A user and checks.**
   - Create a non-root user. `timedatectl` shows a synchronised clock: partitions and compaction use the wall clock. `swapon --show` is empty.
   - `loginctl enable-linger <user>`, so the user's units survive the SSH logout.
5. **Packages.**
   - `apt-get install -y build-essential pkg-config git curl tmux gzip podman`; podman only if the optional local Versity check is wanted;
   - as the user: rustup, then `rustup show` in the clone installs the pinned toolchain;
   - `cargo install just`;
   - `uv` (official installer), for plots on the server, or copy the samples back and plot locally.
6. **Code.**

   ```bash
   git clone <repo> && cd llm-observer && git checkout <commit>
   just fetch-corpus
   RUST_MIN_STACK=33554432 cargo test --release -p llmobs-storage --all-features --test soak --test trace_bench --test ingest_bench --no-run
   ```

7. **Credentials.**
   - Create the bucket and an access key pair in the Hetzner console.
   - Put them in `~/.config/llmobs-soak.env`, mode 600, with nothing else in it:
     - `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`;
     - `LLMOBS_TEST_S3_ENDPOINT=https://<location>.your-objectstorage.com`;
     - `LLMOBS_TEST_S3_BUCKET`;
     - `LLMOBS_TEST_S3_REGION=<location>`.
   - Never type the keys on a command line (shell history).
8. **Addressing check, and the multipart lifecycle rule.**
   - Run `set -a; . ~/.config/llmobs-soak.env; set +a` and a 60 s `ingest_bench` with `LLMOBS_INGEST_BENCH_DIR=/data/ingest-hz` (Run 15).
   - **If it fails with a signature or bucket-not-found error,** set `LLMOBS_TEST_S3_PATH_STYLE=false` and `LLMOBS_TEST_S3_ENDPOINT=https://<bucket>.<location>.your-objectstorage.com` (object_store expects the bucket in a virtual-hosted endpoint), and retry.
   - **Record which addressing worked.** It goes into `hardening.md` and, through the coordinator, into the self-hosting docs.
   - **Lifecycle rule.** Add Hetzner's rule that aborts incomplete multipart uploads after 1 day (M4 recommends it), so a crashed run cannot leave billed parts.
9. **Bucket benchmarks, before the soak** (runs 16–18): `trace_bench` 5M with `LLMOBS_TRACE_BENCH_DIR=/data/trace-5m-hz` and `LLMOBS_TRACE_BENCH_MAX_BYTES` from R7.4, with `LLMOBS_TRACE_BENCH_CACHE=cold`, then `warm`, then `off`, each in the background:

   ```bash
   systemd-run --user --unit tb-cold --same-dir -p EnvironmentFile=$HOME/.config/llmobs-soak.env \
     --setenv=PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/bin:/bin --setenv=RUST_MIN_STACK=33554432 \
     --setenv=LLMOBS_TRACE_BENCH_DIR=/data/trace-5m-hz --setenv=LLMOBS_TRACE_BENCH_MAX_BYTES=<from R7.4> \
     --setenv=LLMOBS_TRACE_BENCH_CACHE=cold \
     cargo test --release -p llmobs-storage --all-features --test trace_bench -- --ignored --nocapture
   journalctl --user -o cat -u tb-cold > /data/run-16.log
   ```

   - This is the first measurement over a real network S3: cold and off numbers include real round trips (`benchmarks.md` Findings (e)).
   - After run 18, delete the `trace-bench` prefix (console or `rclone purge`) and `/data/trace-5m-hz`, so neither counts against the soak.
10. **The 72 h soak.**

    ```bash
    systemd-run --user --unit llmobs-soak --same-dir -p EnvironmentFile=$HOME/.config/llmobs-soak.env \
      -p LimitNOFILE=65536 -p MemoryMax=32G \
      --setenv=PATH=$HOME/.cargo/bin:$HOME/.local/bin:/usr/bin:/bin \
      --setenv=LLMOBS_SOAK_DIR=/data/soak --setenv=LLMOBS_SOAK_OUT=/data/soak-out \
      --setenv=LLMOBS_SOAK_LABEL=hz-ax52 --setenv=LLMOBS_SOAK_HOURS=72 \
      --setenv=LLMOBS_SOAK_RETENTION_DAYS=0 --setenv=LLMOBS_SOAK_RATE=2000 just soak
    ```

    - **Rate.** 2,000 spans/s is the default and the recommendation. The user may choose about 5,000 spans/s (spec 03's "an order of magnitude below 50k" for the target box) for a stronger claim: about 2.5× the bucket volume (roughly 250–400 GB per day). Whatever rate is used, the claims state it (R7.7).
    - **Check the memory limit applies.**
      - `cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers` must list `memory`. Ubuntu 24.04 delegates it to user managers by default; if it is missing, `MemoryMax` is silently ignored.
      - After the start, `systemctl --user show llmobs-soak -p MemoryMax` shows `34359738368`.
      - **Fallback:** a system unit, `sudo systemd-run --unit llmobs-soak --uid=<user> --working-directory=$PWD -p MemoryMax=32G …` with the same `--setenv`s and `EnvironmentFile`. It is read with `journalctl -o cat -u llmobs-soak`, without `--user`.
    - **`MemoryMax=32G`** holds the run to the target box's memory. An OOM kill is a result (rule `rss_max` fails first at 8 GiB by default).
    - **No `Restart=`.** A crash is a result, not something to hide.
    - **User units.** The unit needs a user systemd instance: either log in by SSH first, or rely on the linger of step 4.
    - **Check-ins:** `journalctl --user -o cat -u llmobs-soak -n 50`, `tail -1 /data/soak-out/samples.jsonl`, `just soak-plot /data/soak-out` at any time.
    - tmux is the fallback if user units are unavailable.
11. **Results back.**
    - `rsync -av <server>:/data/soak-out/ ~/llmobs-bench/soak-hz-out/`, and the bench logs the same way.
    - Then plot, run the grep check of R7.7, and copy the committed files into `docs/research/storage/soak/<id>/`.
12. **Teardown.**
    - Check the soak deleted its prefix (it does on a pass; otherwise delete it after inspection).
    - `aws s3api list-multipart-uploads` (or the console) shows none.
    - Cancel the server: the month is already billed, so cancel after the reruns R7.9 may need.
    - Rotate or delete the access key.

### Records

- `benchmarks.md`:
  - Run 15 (ingest, Hetzner bucket);
  - runs 16–18 (trace_bench 5M, Hetzner, cold/warm/off), with the machine line and a caveat paragraph: dedicated AX52, the bucket in the same location, a 32 GiB memory limit on the soak only;
  - § Against the capacity check: "Sustained use needs S3 and a bigger box" is now measured by the soak; link it.
  - **Before pasting.** The benches print placeholders for a remote bucket (R7.1's `describe`). Still run R7.7's grep check over `benchmarks.md` and the run logs (`/data/run-*.log`) before committing. Commands quoted in `benchmarks.md` show the endpoint as `https://<location>.your-objectstorage.com` and never the bucket.
- `soak.md`: the 72 h run section and its row; § Claims filled from the two soak runs.
- `hardening.md`: the runs.

---

## R7.9 — Findings and fixes

Commit per bug: `storage: <fix in imperative> (R7.9)`. Final docs commit: `docs: close M7 with its findings (R7.9)`.

1. **Collect the findings** of R7.3–R7.8: failed soak rules, benchmark misses that are bugs, errors in `events.jsonl`, oracle mismatches, orphans.
2. **For each one**, follow M1's R1.8 procedure:
   1. **Reduce it to a test in the crate.** A soak-only failure (e.g. RSS growth) gets a reproduction in the shortest form that shows it:
      - an ignored integration test or a unit test;
      - for example, a soak run with a higher rate and shorter hours that fails the same rule within 30 minutes;
      - or an M1 model case (`run_case(&[...])` in `model_tests/mod.rs`) when the reads disagree.
   2. **Classify it:**
      - **Generator or harness bug** (the soak expected something the spec doesn't promise): fix the soak, cite the spec line.
      - **Storage bug:** find the cause in `crates/storage/src`. If the fix is local (one module, no format, public API or spec change), fix it in the same commit as its regression test. Otherwise stop and report the case, the cause and the change it needs.
      - **Capacity, not a bug** (the rate can't be sustained, a query target missed by design): no code change. Record it, and raise it to the coordinator with the numbers.
   3. **Log it.** A row in `hardening.md` § M7 Findings: id `M7-<n>`, the run and rule (or test name), the cause, the classification, and the fix commit.
3. **Rerun** each run whose rule failed because of a storage bug, after its fix. For the 72 h run, ask the user. Record the rerun as a new row; never edit the old one.
4. **Spec 03 § Capacity sanity check.** After its three bullets, add one paragraph headed "Measured (track R M7)" with:
   - the realistic ingest rate and bytes per span (runs 8–9);
   - the measured zstd ratio (overall, and input/output);
   - the sustained soak rate on 8 cores within a 32 GiB memory limit (the target box's size), and its RSS (the 72 h run);

   each linking `docs/research/storage/benchmarks.md` or `soak.md`. If the ratio is outside 5–8×, change the "roughly 5–8×" sentence to the measured range in the same commit, and add it to the report as a spec change.
5. **Roadmap.** Update track R's row in `docs/roadmap.md`: "M7 done" (with any capacity items raised).

If a storage cause is not found within the code paths the failing rule touches, stop and report. Don't guess.

---

## What not to build in M7

- **A stats API on `Storage`.** The soak reads `catalog.db` read-only and `/proc`; the server's metrics are another track's concern.
- **Media in the soak.** `put_media` and the media GC are not exercised. Media lives under `media/`, which the orphan audit skips because it only walks `tables/`.
- **Faults during the soak:** crashes, restarts under load, S3 errors and full disks are M2, M4 and M5. The soak's one restart is the measured shutdown and reopen at the end.
- **Operator GC during the soak.** N's `collect_orphans` never runs while a soak runs (§ Prerequisites).
- **Simulated time.** The soak runs on the wall clock: partitions, day close and retention happen in real time. A faster simulated clock would need clock seams the storage doesn't have (D-R-13).
- **Reusing M1's model.** `model_tests` is crate-private and models small fixed shapes. The soak's oracle is its own registry of generated summaries, exact because the generator stays inside the spec's guarantees.
- **A CI job.** The soak and the 5M benchmark run by hand; nothing joins `just check`, and M3's nightly workflow runs no soak or bench job (user decision, 2026-11-15).
- **Committing the corpus**, or any file holding its text.
- **Tuning the generator to match spec numbers.** Sizes follow the spec's bands; ratios are measured, not targeted.
- **Migrating `tests/s3.rs` to `common`:** it belongs to M4.
