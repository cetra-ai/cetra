# Track QE — Milestone 7 Implementation Plan (Atomic Workspace Cut-Over & DataFusion Removal)

**Status:** Approved (Neon R2: 0 Blockers, 0 Majors)
**Track / Milestone:** QE — M7 (`docs/plans/query-engine/design.md` § Milestones / M7)
**Depends on:** M1–M6 all merged and verified green on `qe-query-engine` (`cetra-column`, `cetra-query`, `reads_qe/`, `engine/sql.rs`, `engine/parquet_export.rs`, `file_format/`, WAL binary framing, `memtable` on `Batch`, compaction, deletion rewrite, media GC, `verify_file`).
**Leaves out:** Replacing Parquet storage files with Strata/Statam (separate follow-up track; `cetra-storage::file_format` retains a slim write-only `ArrowWriter` path until Statam clears its Stage 2 gate).

---

## Goal

Atomically switch the entire `cetra` workspace off Apache DataFusion and off Arrow across `cetra-core`, `cetra-otel`, `cetra-server`, `cetra-cli`, `web/`, `node-cli/`, OpenAPI, fixtures, specs, and docs:
- Delete `datafusion` and `datafusion-datasource` from `Cargo.toml`, `Cargo.lock`, `deny.toml`, and `.github/dependabot.yml`.
- Delete `arrow` from `cetra-core`, `cetra-otel`, `cetra-server`, and `cetra-cli`. Keep `parquet` at root `Cargo.toml` with `default-features = false`, zero `parquet` in `cetra-server`, zero runtime `parquet` in `cetra-cli` (dev-only `parquet` without `arrow` per `design.md:84, 91`), and zero `parquet/async`, `parquet/object_store`, `arrow/ipc_compression`, or `arrow/json` anywhere in the workspace.
- Promote `reads_qe/` -> `reads/` and `engine/sql.rs` -> `sql/` inside `cetra-storage`, delete all legacy DataFusion modules (`query/`, old `views/`, old `reads/`, old `sql/`, `filter/`, `query_text.rs`, `column_bridge.rs`, DF diff tests), and switch `cetra-storage::function_catalog()` to `cetra_query::sql::catalog::function_catalog()`.
- Adopt D-QE-15 unified `columns[].type` display names across `cetra-server`, `web/`, `node-cli/`, OpenAPI (`just openapi`), API fixtures (`just api-examples`), and generated docs/skills (`just docs-gen`).
- Verify full `just check`, `just deny`, `just check-release`, `just check-workflows`, `just e2e`, `just assert-clean`, and record compile-time & binary-size wins.

---

## Cross-Cutting Invariants

1. **Zero compatibility layers (`AGENTS.md`):** Delete every legacy DataFusion/Arrow path, bridge, and dual-engine flag (`CETRA_TRACE_BENCH_ENGINE` added in QE4.5, `column_bridge.rs`, legacy `reads/`, `views/`, `query/`, `sql/`, `filter/`, `query_text.rs`) in the same change.
2. **Dependency boundaries enforced after M7:**
   - `cetra-core`: depends on `cetra-column`; **ZERO** transitive `arrow`, `parquet`, `datafusion`, `tokio`.
   - `cetra-otel`: **ZERO** transitive `arrow`, `parquet`, `datafusion`.
   - `cetra-column` & `cetra-query`: **ZERO** transitive `arrow`, `parquet`, `datafusion`, `tokio`.
   - `cetra-storage`: **ZERO** `datafusion` / `datafusion-datasource`; `arrow` (`default-features = false`, no `ipc_compression`/`json`) and `parquet` (`["arrow", "zstd", "snap", "lz4", "flate2-zlib-rs"]`, NO `async` or `object_store`) isolated strictly inside `file_format/` (writer) and `engine/parquet_*.rs` (low-level zero-Arrow reader & exporter); `async-trait` stays for `object_store::ObjectStore`.
   - `cetra-server`: depends on `cetra-column`; **ZERO** direct `arrow`, `parquet`, `datafusion`, or `cetra-query` (note: `arrow`/`parquet` remain transitive via `cetra-storage` until Strata/Statam replaces Parquet storage; `cetra-storage` wraps `SqliteView` registration, `SqlColumn`/`SqlJson` conversion, `EngineError`, `collect_raw_spans`, and `read_parquet_summary_for_test` so `cetra-server` never imports `cetra-query`, `arrow`, or `parquet`).
   - `cetra-cli`: **ZERO** direct runtime `arrow`, `parquet`, `datafusion`, or `cetra-query`; keeps dev-only `parquet = { workspace = true }` (`default-features = false`, zero `arrow`) per `design.md:84, 91` for `SerializedFileReader` footer-only verification in `crates/cli/src/query.rs:1450`.
3. **Slim Arrow write-only path inside `cetra-storage::file_format/`:**
   - Because `cetra_core::schema::{spans,traces,scores}_schema()` now return `Arc<cetra_column::Schema>` (widening `spans.schema_version` and `traces.partial_count` to `I64`), `file_format/` defines private on-disk Arrow schemas (`arrow_spans_schema`, `arrow_traces_schema`, `arrow_scores_schema` with `UInt16` `schema_version`, `Int32` `partial_count`, and Arrow child names `"item"`, `"entries"`, `"keys"`, `"values"`) plus a slim write-only `batch_to_record_batch` helper extracted from `column_bridge.rs`, allowing the 2,865-line bidirectional `column_bridge.rs` to be deleted completely.
4. **No double memory admission on SQL or reads:**
   - `Storage::query(...)` admits a `ScopedPool` once inside `ProjectQuery`. Both `EngineContext` (`reads/`) and `SqlEngineContext` (`sql/`) take `pool: Option<Arc<ScopedPool>>` and reuse `ProjectQuery`'s admitted pool instead of calling `admit_until`/`admit_for_export` a second time; `sql_export` clones `Arc<ScopedPool>` into the returned export stream so the admission reservation lives until stream drop.
5. **Safety, privacy & hygiene (`AGENTS.md`):** `#![forbid(unsafe_code)]`, zero `unwrap()`/`expect()` outside tests, zero `#![allow(...)]`, privacy-safe `fmt::Debug` and errors.

---

## Execution Order (Atomic Single-PR Cut-Over, Staged in Clean Commits on Branch `qe-query-engine`)

Because M7 breaks cross-crate signatures (`cetra-core` -> `cetra-storage` -> `cetra-server` -> `cetra-cli`), stages **QE7.1–QE7.3** execute sequentially on one cut-over branch `qe7-cutover`, while **QE7.4 (`web/` + `node-cli/`)** runs in parallel on branch `qe7-web` (100% file-disjoint!). Both branches merge into `qe-query-engine`, followed by **QE7.5** (docs, specs, `AGENTS.md`, Neon cold review, and full verification).

```text
QE7.1 `cetra-core` + `cetra-otel` off Arrow (`spans_to_batch`/`scores_to_batch`, `cetra_column::Schema`)
  └── QE7.2 `cetra-storage` cut-over + `fixtures/api/static/schema.json`
            (delete `query/`, `views/`, `reads/`, `sql/`, `filter/`, `query_text.rs`, `column_bridge.rs`;
            promote `reads_qe/` -> `reads/`, `engine/sql.rs` -> `sql/`; slim Arrow writer in `file_format/`;
            native `Storage`/`ProjectQuery`/`QueryContext`/`EngineError`/`disk`/`memory`/`objects`/`verify`/
            `unwind`/`golden_tests`/`model_tests`/`tests/*`; drop `datafusion` from Cargo.toml + deny.toml)
        └── QE7.3 `cetra-server` + `cetra-cli` cut-over (drop `arrow` & `datafusion` from `cetra-server`;
                  D-QE-15 `SqlColumnInfo` doc + test assertions; `convert.rs`, `sql_views.rs`, `issues/errors.rs`,
                  `routes/otlp.rs` privacy-safe `stored_rows` + `spans_where`, `eval/trace_load.rs`, `http/error.rs`;
                  `cli/src/query.rs` Shape B `resource_attributes` test & zero-Arrow `SerializedFileReader`;
                  `just openapi` + `just api-examples`)
              │
QE7.4 (PARALLEL with QE7.1–QE7.3!) `web/` + `node-cli/` D-QE-15 `columns[].type` update
              │
              └── QE7.5 Specs, docs, `AGENTS.md`, `just docs-gen`, `just fuzz-lock`,
                        Neon cold review of cut-over, full `just check` + `just deny` + `just check-release` +
                        `just check-workflows` + `just e2e` + compile-time & binary-size report!
```

---

### Stage QE7.1 — `cetra-core` & `cetra-otel` Off Arrow

**Files:**
- `crates/core/Cargo.toml`, `crates/core/src/{lib.rs, error.rs, schema.rs, batch.rs}`
- `crates/core/tests/snapshots/{spans,traces,scores}_schema.snap`
- `crates/otel/src/fuzz_entry.rs`

**Deliverables:**
1. **`crates/core/Cargo.toml`**: replace `arrow` with `cetra-column = { workspace = true }`; remove `parquet`, `bytes`, `arrow/ipc` from `[dev-dependencies]`; update the crate description comment (`Cargo.toml:3`) off Arrow/DataFusion.
2. **`crates/core/src/error.rs`**: `CoreError::Column(#[from] cetra_column::ColumnError)` replacing `Arrow(ArrowError)`.
3. **`crates/core/src/schema.rs`**:
   - `spans_schema()`, `traces_schema()`, `scores_schema()` return `Arc<cetra_column::Schema>` (`schema_version` & `partial_count` as `I64`, `events` as `Events`, `links` as `Links`, `tags`/`media_refs`/`truncated`/`conventions`/`models`/`prompts` as `List(Utf8)`, maps as `MapUtf8`, IDs as `FixedBytes16`/`FixedBytes8`, timestamps as `TimestampNs { utc: true }`).
   - Delete Arrow-only `event_fields`/`link_fields`/`event_struct`/`link_struct`; KEEP `pub mod event_field` and `pub mod link_field`.
   - Update `render(&Schema)` and bless `crates/core/tests/snapshots/{spans,traces,scores}_schema.snap`.
4. **`crates/core/src/batch.rs`**:
   - Replace `spans_to_record_batch`/`scores_to_record_batch` with `spans_to_batch(&[Span]) -> Result<Batch, CoreError>` and `scores_to_batch(&[Score]) -> Result<Batch, CoreError>`.
   - Rewrite unit tests onto `cetra_column::Batch`; replace `arrow_ipc_round_trip` with `cetra_column::frame::{encode_batch, decode_batch}` round-trip; drop `parquet_round_trip`.
5. **`crates/otel/src/fuzz_entry.rs`**: update `spans_to_record_batch` -> `spans_to_batch`.
6. Verify: `cargo test -p cetra-core -p cetra-otel && cargo clippy -p cetra-core -p cetra-otel --all-targets -- -D warnings && cargo tree -p cetra-core -p cetra-otel | rg "arrow|parquet|datafusion|tokio"` (0 matches!).

---

### Stage QE7.2 — `cetra-storage` Cut-Over, `schema.json` Update & DataFusion Removal from `Cargo.toml` / `deny.toml`

**Files:**
- `Cargo.toml`, `deny.toml`, `.github/dependabot.yml`, `crates/storage/Cargo.toml`
- `crates/storage/src/**`, `crates/storage/tests/**`, `fixtures/api/static/schema.json`

**Deliverables:**
1. **Dependencies (`Cargo.toml`, `crates/storage/Cargo.toml`, `deny.toml`, `.github/dependabot.yml`):**
   - Root `Cargo.toml`: delete `datafusion` & `datafusion-datasource`; set `parquet = { version = "59.2", default-features = false }`; update comments on `lz4_flex`, `sqlparser`, `csv`, `async-trait` (keep `async-trait` for `object_store::ObjectStore`), and `[profile.dev.package."*"]`.
   - `crates/storage/Cargo.toml`: drop `datafusion` & `datafusion-datasource`; set `arrow = { workspace = true }` (no `ipc_compression`/`json`); set `parquet = { workspace = true, features = ["arrow", "zstd", "snap", "lz4", "flate2-zlib-rs"] }` (no `async` or `object_store`).
   - `deny.toml`:
     - Remove `datafusion`, `datafusion-datasource`, `arrow-csv` (`:74`), and `arrow-json` (`:77`) from `bans.deny` (keep active `arrow*`/`parquet`/`object_store` pins).
     - Remove `RUSTSEC-2026-0555` (`:20`, `chrono-tz` — only pulled by `datafusion`, would fail `just deny` / nightly CI as unmatched!); update `:19` comment (`chrono` is a direct workspace dep).
     - Verify `just deny-policy` AND `just deny-advisories` are clean!
   - `.github/dependabot.yml`: rename `datafusion-family` -> `arrow-parquet-family`, drop `"datafusion"` / `"datafusion-*"`.
2. **Delete legacy DF modules & bridges, promote QE modules:**
   - Delete `crates/storage/src/{query/, filter/, query_text.rs, column_bridge.rs, query_diff_tests.rs, engine/diff_tests.rs, engine/sql_diff_tests.rs}` (preserving any non-differential unique unit tests) and remove `pub mod query;` from `lib.rs` (plus M1–M6 scaffolding `#[allow(dead_code)]` on `lib.rs:48, 56, 59, 78`).
   - Extract all public request/response/constant types from legacy `reads/` before deleting it, then promote `reads_qe/` -> `reads/` (replacing `RecordBatch` in `Page`/`TraceDetailData`/`TraceSpansData`/`SessionDetailData`/`UserDetailData`/`UserExtras` with `cetra_column::Batch`, and dropping `pub mod qe_bench` once `reads/` is the sole implementation).
   - Promote `engine/sql.rs` -> `sql/` backed by `cetra_query::sql` + `cetra_query::encode` + `engine/parquet_export.rs`:
     - Give `SqlEngineContext` `pool: Option<Arc<ScopedPool>>` so it reuses `ProjectQuery`'s admitted pool without double-admitting (and clones `Arc<ScopedPool>` into the `sql_export` stream so the reservation stays alive until stream drop).
     - Keep `cetra_storage::SqlColumn { name: String, data_type: String }` and `cetra_storage::SqlJson` mapped explicitly from `cetra_query`'s `SqlColumn` (`col.type_name().to_owned()`) and `SqlJsonResult` so `cetra-server` never imports `cetra-query`.
     - Preserve public exports `EXPORT_MAX_CHUNK_BYTES`, `EXPORT_MAX_ROWS`, `ExportFormat`, `SQL_MAX_RESULT_BYTES`, `SQL_MAX_ROWS`, `SqlColumn`, `SqlJson`, `SqlRequest`.
   - Replace `crates/storage/src/views/` with slim `catalog.rs` (`ColumnInfo`, `ColumnKind`, `FunctionInfo`, `ViewColumns`, `ViewName`, `view_catalog`, `TimeWindow`, `SqliteView`):
     - Switch `function_catalog()` to map `cetra_query::sql::catalog::function_catalog()` (`&'static [cetra_query::sql::catalog::FunctionInfo]`) into `Vec<cetra_storage::FunctionInfo>`.
     - Migrate `views/catalog.rs` tests off `ctx.session().table(...)` / `arrow::datatypes::DataType` onto `cetra_column::Schema`.
     - **Update `fixtures/api/static/schema.json` in QE7.2** to match the new Shape B `function_catalog()` so `views::catalog::tests::function_catalog_matches_the_fixture` passes immediately in QE7.2!
   - Move private on-disk Arrow schemas + slim write-only `batch_to_record_batch` into `crates/storage/src/file_format/`; fold any remaining constants/helpers from `parquet.rs` into `file_format/` and delete `parquet.rs` (removing its `#![allow(dead_code)]`); remove `arrow_schema_to_column` everywhere (use `cetra_core::schema::{spans,traces,scores}_schema()` directly).
3. **Native `Storage`, `ProjectQuery`, `QueryContext`, errors & all remaining storage modules:**
   - `lib.rs`:
     - Drop `Storage.runtime`, `Storage.memory`, `build_runtime`, and DF compaction `RuntimeEnv`.
     - `Storage::append_spans` / `append_scores` take `cetra_column::Batch`.
     - `Storage::query` admits once via `self.qe_runtime.admission.admit(...)` -> `ProjectQuery` (with `#[doc(hidden)] pub async fn collect_raw_spans` for `cetra-server`'s `ingest/fixtures_tests.rs` & `routes/otlp.rs`).
     - `QueryContext` wraps `ProjectQuery` + `Option<TimeWindow>` + interior-mutable `extra_tables: RwLock<HashMap<SqliteView, (Arc<Schema>, Vec<Batch>)>>` so `register_sqlite_view(&self, view: SqliteView, schema: Arc<Schema>, batches: Vec<Batch>)` keeps its `&self` signature (constructing `BatchSource` internally so `cetra-server` stays `cetra-query`-free!).
     - Expose `#[doc(hidden)] pub fn read_parquet_summary_for_test(bytes: bytes::Bytes) -> Result<(Vec<String>, usize), String>` via low-level `SerializedFileReader`.
     - Update `lib.rs:3214-3287` (`used_disk_space()` / `storage.memory.used()` -> `qe_runtime.spill.spilled_bytes()` / `qe_runtime.admission.used()`) and `lib.rs:3299-3313` (`open_removes_leftover_spill_directories` expects `entries.is_empty()`, not `"datafusion-"`).
     - Update stale DF/Arrow comments in `lib.rs`, `config.rs`, `fs.rs`.
   - `error.rs` & `query_error.rs`:
     - Define opaque privacy-safe `EngineError` (wrapping internal `cetra_query::QueryError` or message) in `cetra-storage` so `QueryError::Internal(EngineError)`, `RunError::Internal(EngineError)`, `MediaError::Query(EngineError)`, and `CompactionError` never expose `DataFusionError`, `ArrowError`, or `cetra_query::QueryError` to `cetra-server`. Provide a `#[doc(hidden)] pub fn engine_error_for_test(msg: &str) -> EngineError` for `cetra-server` tests (`http/error.rs:757`, `ingest/mod.rs:1690`).
     - Map `ArrowError` inside `file_format/` into `ParquetWriteError::Parquet(Box::new(ParquetError::ArrowError(...)))` (or `ParquetWriteError::Encode`) and switch `MemtableError`/`WalError`/`AnchorError`/`PartialsError` to `ColumnError`.
     - Delete all `DataFusionError` / `ArrowError` / `query_err_to_df` / `from_column` bridges.
   - Clean up remaining DataFusion/Arrow references across ALL storage modules:
     - `deletion.rs` (delete legacy DataFusion `PhysicalExpr` tombstone builders at lines 16–20, 249–555).
     - `compaction/{job.rs, merge.rs, slice.rs}` (drop test `SessionContext`, `RuntimeEnv`, `DataFusionError`, `parquet::write_file` references).
     - `media.rs` (drop `RuntimeEnv` & `QueryError::DataFusion`).
     - `file_format/mod.rs` (drop `DataFusionError::ParquetError`, `column_bridge`, `parquet::write_file`).
     - `disk.rs`, `memory.rs`, `objects.rs`, `verify.rs`, `unwind.rs`, `test_support.rs`, `fuzz_entry.rs`.
     - `read_cache.rs` & `golden_tests/build.rs` (migrate `crate::disk::test_spill` off `Arc<DiskManager>` onto `SpillManager` / `TempDir`).
     - `golden_tests/{mod,dump}.rs` (replace `arrow::json::ArrayWriter` oracle test at `dump.rs:965-1150` with direct `cetra_column::Batch` JSON assertions so `arrow/json` can be dropped!).
     - `crash_tests.rs`, `flush.rs`, `gc.rs`, `snapshot.rs`, `s3_fault_tests.rs` (replace test `SessionContext` setup).
     - `model_tests/**` (including `model_tests/check.rs:1447` and `model_tests/resources/disk_full.rs:886-955` switching off `"datafusion-"` / `disk_manager.used_disk_space()` / `storage.memory.used()` to `qe_runtime.spill.spilled_bytes()` and `qe_runtime.admission.used()`).
   - `crates/storage/tests/{ingest_bench,trace_bench,s3_chaos,it/crash,it/s3,it/soak/driver,it/soak/registry,it/bench_support}.rs`:
     - Migrate off Arrow/DataFusion.
     - In `trace_bench.rs`: remove dual-engine `CETRA_TRACE_BENCH_ENGINE` toggle (QE is sole engine); expose minimal `#[doc(hidden)]` range-mode toggle and `sql_json` / `EXPLAIN` helpers on `QueryContext` to replace `ProjectQuery.optimizer_rules` (`no_rule`), `ctx.session().sql(...)` (`cost_threshold`), and `EXPLAIN ANALYZE` (`explain_analyze`) so `trace_bench` compiles cleanly and satisfies `design.md:246`.
4. Verify: `cargo test -p cetra-storage --lib && cargo test -p cetra-storage --test it && cargo clippy -p cetra-storage --all-targets --all-features --test ingest_bench --test trace_bench --test s3_chaos --test s3_probe -- -D warnings`.

---

### Stage QE7.3 — `cetra-server` & `cetra-cli` Cut-Over, OpenAPI & API Fixtures

**Files:**
- `crates/server/Cargo.toml`, `crates/server/src/**`
- `crates/cli/Cargo.toml`, `crates/cli/src/{main.rs, query.rs}`
- `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/static/sql-result.json`, `fixtures/api/responses/assistant-conversations/01900000-0000-7000-8000-000000001a01.json`

**Deliverables:**
1. **`crates/server/Cargo.toml`**: replace `arrow` with `cetra-column.workspace = true`; remove `datafusion.workspace = true` (zero direct `arrow`, `parquet`, `datafusion`, `cetra-query`).
2. **`crates/server/src/`**:
   - `api/convert.rs`: rewrite `Batch` readers natively on `cetra_column::Batch` (all 4 encodings + `Selection`, `schema_version` `I64 -> u16`, native `EventsData`/`LinksData`, delete `Utf8View` workaround).
   - `api/sql.rs:42`: update `SqlColumnInfo::data_type` doc comment to D-QE-15 vocabulary.
   - `datasets/sql_views.rs`: build `cetra_column::Batch` directly (preserving `MAX_VIEW_ROWS = 1_000_000` and `MAX_VIEW_TEXT_BYTES = 256 MiB`) and register via `ctx.register_sqlite_view(view, schema, batches)` (`&self`).
   - `routes/otlp.rs`:
     - Rewrite `stored_rows` (`:452-465`, `:932-934`) to inspect decoded spans (`convert::spans`) or read `environment`/`warnings` columns directly (since `Batch::fmt::Debug` is privacy-safe and never prints cell strings!).
     - Migrate `spans_where` (`:1374-1385`) off `SessionContext::sql` onto `QueryContext::sql_json` or `collect_raw_spans`.
   - `eval/trace_load.rs:146`, `http/error.rs:318, 757`, `ingest/mod.rs:1690`: migrate `QueryError::DataFusion` -> `QueryError::Internal(EngineError)` and replace `DataFusionError::Internal` test construction with `cetra_storage::error::engine_error_for_test`.
   - `mcp/sql.rs:591, 715`, `routes/sql.rs:784`, `test_support/mod.rs:756`: update D-QE-15 `columns[].type` assertions (`"Utf8"`/`"VARCHAR"` -> `"string"`, `"Int64"`/`"BIGINT"` -> `"integer"`); update `mcp/mod.rs:156` comment.
   - `issues/errors.rs`, `deletion/mod.rs`, `ingest/{grpc,fixtures_tests}.rs`, `routes/{read,sql,datasets,online_rules,traces}.rs`, `scores.rs`, `backup.rs`, `test_support/mod.rs` (`raw_span_count` off `SessionContext`).
   - `routes/sql.rs:1491`: use `cetra_storage::read_parquet_summary_for_test(bytes)` (zero `parquet` in `cetra-server`).
3. **`crates/cli/`**:
   - `crates/cli/Cargo.toml`: keep dev-only `parquet = { workspace = true }` (`default-features = false`, zero `arrow`, per `design.md:84, 91`).
   - `crates/cli/src/main.rs:232`: update comment.
   - `crates/cli/src/query.rs`: update lines 627, 975 (`"integer"`/`"string"`), line 1363 (replace non-Shape-B `named_struct('k', 'v')` with a Shape B query selecting `resource_attributes` — which is non-empty `{"service.name":"conformance","telemetry.distro.name":"cetra"}` in `TestServer::start()`!), and line 1450 (migrate off `ParquetRecordBatchReaderBuilder` onto footer-only `parquet::file::reader::SerializedFileReader`).
4. **Regenerate OpenAPI & API fixtures:**
   - Run `just openapi` and `just api-examples` (regenerating `openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/static/sql-result.json`, and `assistant-conversations/...1a01.json`, and verifying `static/schema.json` matches!).
5. Verify: `cargo test -p cetra-server -p cetra-cli && cargo clippy --workspace --all-targets --all-features -- -D warnings`.

---

### Stage QE7.4 — `web/` & `node-cli/` D-QE-15 Type Vocabulary *(runs in parallel with QE7.1–QE7.3!)*

**Files:**
- `web/src/lib/sql/types.ts` + `web/src/lib/sql/{types,chart,sort,format}.test.ts`
- `web/src/components/sql/{result-chart,result-grid}.test.tsx` + `web/src/components/sql/result-grid.tsx` (comments at `:51, 92`)
- `web/src/routes/$org/$project/sql/index.test.tsx`
- `web/src/components/data-table/data-table.test.tsx`
- `web/src/lib/assistant/types.test.ts`
- `node-cli/src/{query,table}.test.ts`

**Deliverables:**
1. Update `web/src/lib/sql/types.ts` to exact D-QE-15 vocabulary (`string`, `integer`, `float`, `decimal`, `boolean`, `timestamp`, `date`, `interval`, `binary`, `string_list`, `string_map`, `list`, `json`, `null`) with explicit helper coverage:
   - `isNumericType`: `integer | float | decimal`
   - `isTimestampType`: `timestamp`
   - `isDateType`: `date`
   - `isTextType`: `string`
   - `isBooleanType`: `boolean`
   - `isNestedType`: `string_list | string_map | list | json`
2. Update all 10 web test files and 2 `node-cli` test files from Arrow/SQL type strings (`Utf8`, `Int64`, `Float64`, `BIGINT`, `Decimal128(...)`, `Date32`, `Boolean`) to D-QE-15 display names (preserving SQL syntax tests in `format-sql.test.ts`) and clean up stale DataFusion/Arrow comments in `result-grid.tsx`.
3. Verify: `just check-web && just check-cli-js` (after merging with QE7.3's updated `fixtures/api/static/{schema,sql-result}.json`).

---

### Stage QE7.5 — Specs, Docs, `AGENTS.md`, `just docs-gen`, Neon Review & Full Verification

**Files:**
- `docs/spec/{README.md, 01-data-model.md, 03-storage.md, 05-query.md, 07-alerts.md, 09-api.md, 12-operations.md}`
- `docs/mvp-decisions.md`, `docs/roadmap.md`, `docs/plans/query-engine/design.md`, `AGENTS.md`
- `website/src/content/docs/docs/tracing/sql-console.mdx`, `website/src/content/docs/docs/self-hosting/configuration-reference.md`
- Generated via `just docs-gen`: `website/src/content/docs/docs/reference/sql-schema.md`, `skills/cetra/references/*`

**Deliverables:**
1. Update all specs, `docs/mvp-decisions.md` (`D-QE-1`–`D-QE-15`), `docs/roadmap.md` (Track QE **Done** M1–M7), `AGENTS.md` (project summary, deps, updated crate layout & dependency graph `core -> column`, `storage -> core, column, query`, `server -> core, column, otel, storage, eval, token, saml`), and `website/` docs; run `just docs-gen` and `just fuzz-lock`.
2. Verify dependency tree invariants:
   - `cargo tree | rg datafusion` -> **0 matches!**
   - `cargo tree -p cetra-core -p cetra-otel -p cetra-column -p cetra-query | rg "arrow|parquet|datafusion"` -> **0 matches!**
   - `cargo tree --depth 1 --edges normal -p cetra-server -p cetra-cli | rg "arrow|parquet|datafusion|cetra-query"` -> **0 matches!**
   - `cargo tree --depth 1 -p cetra-server | rg "arrow|parquet|datafusion|cetra-query"` -> **0 matches!**
3. Neon cold code review of the M7 cut-over + fix all findings.
4. Full verification suite:
   - `just check` (Rust, web, Python SDK, TypeScript SDK, node-cli, docs site)
   - `just deny` (full supply-chain + advisories check, catching any unmatched ignore!)
   - `just check-release` (release build + embedded UI check)
   - `just check-workflows`
   - `just e2e`
   - `just assert-clean`
   - Storage resource & stress verification where available locally: `resources::memory` and `resources::disk_full` (`#[ignore]`d — run via `just resources-memory` / `just resources`), short `just stress 15` smoke (noting `just lazyfs` requires FUSE `user_allow_other` and `just soak` requires benchmark corpus/S3).
   - Record before/after crate count, compile time, and release binary size!
