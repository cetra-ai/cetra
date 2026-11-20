# V · M3 — Trace linking: prompt columns, normalization, per-version metrics

Implementation plan for milestone M3 of track V. It has 4 phases, **V3.1 to V3.4**, one commit each, done in order. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. If this plan and the code disagree, **STOP and report** instead of guessing. Each phase ends with `just check` green.

**Prerequisites** (check before V3.1; each must print at least one line, else STOP):

```bash
grep -n "pub const SPANS_LIGHT_COLUMNS" crates/core/src/schema.rs
grep -n "const SCHEMA_VERSION: u16 = 1" crates/otel/src/normalize.rs
grep -n "Column::Tags | Column::Models" crates/storage/src/partials.rs
grep -n "pub const SPAN_LIST_COLUMNS" crates/storage/src/reads/traces.rs
grep -n "pub(crate) async fn get_prompt" crates/server/src/meta/prompts.rs
```

The last line needs M1 merged into this branch.

**Read first** (every phase): `AGENTS.md`; `docs/plans/v-prompts/00-design.md` § Decisions D-V-15, D-V-16, D-V-17; this file up to § V3.1, then your phase.

## What M3 is, and what it is not

M3 delivers:
- span columns `prompt_name` and `prompt_version` and the trace column `prompts` (D-V-15), through `cetra-core`, the storage engine, the SQL views, the schema catalog and the read API;
- normalization of `cetra.prompt.*` and Langfuse's `langfuse.observation.prompt.*` into them, with two new warnings and `schema_version` 2 (D-V-16);
- the storage read `prompt_metrics` and `GET /api/v1/projects/{project}/prompts/{prompt}/metrics` (D-V-17).

M3 does not add the SDK side (M2 writes the attributes), any web page or facet (M4), or the playground's reserved environment (M6).

**`cetra-core` ownership.** `cetra-core` is a shared contract (AGENTS.md § Parallel work). The coordinating session wrote this plan and grants V3.1 the `cetra-core` changes listed in it, nothing else.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-15 | Span columns `prompt_name` (Utf8, dictionary in Parquet, P) and `prompt_version` (Int64, P); trace column `prompts` (List(Utf8), distinct union of `name@version`, like `models`). `FORMAT_VERSION` stays 1 before the first release (D185). |
| D-V-16 | `cetra.prompt.*` first, then `langfuse.observation.prompt.*`; a bad version stays in `attributes` with a warning; prompts are not checked against the registry. |
| D-V-17 | Per-version metrics over linked `llm` spans in the window (default 7 days), with average numeric and boolean scores; default exclusions apply. |

Made precise by this plan (recorded in `docs/mvp-decisions.md` § Track V by V3.4, as amendments of D-V-15 to D-V-17):
- **Column order.** `prompt_name`, `prompt_version` come right after `model` in the spans schema (both in the light projection). `prompts` comes right after `models` in the traces schema.
- **Pairing.** The version is read only from the convention whose name won: a `cetra.prompt.name` takes `cetra.prompt.version`; a Langfuse name takes the Langfuse version. A version whose convention has no name stays in `attributes` with the warning `prompt_version_without_name`.
- **Bad versions.** A version that is not an integer ≥ 1 stays in `attributes` with the warning `invalid_prompt_version`; the name is still stored.
- **Name-only spans** (a fallback prompt, D-V-14) set `prompt_name` and contribute nothing to `prompts`.
- **`schema_version` 2.** Normalization now changes stored values (spec 01 § Normalization version), so `SCHEMA_VERSION` goes from 1 to 2 and the OTLP golden fixtures are re-blessed.
- **Scores per version.** A span score counts for the version of its span. A trace score counts for every version of the prompt that the trace's linked `llm` spans carry (a trace that used versions 2 and 3 counts its trace score once for each).
- **API.** `SpanSummary` and `SpanDetail` gain optional `prompt_name` and `prompt_version`; `TraceSummary` gains `prompts` (omitted when empty). Existing fixtures stay unchanged.

## Versions

No new dependency.

## Phase graph

Strictly in order: V3.1 (core and storage plumbing) → V3.2 (normalization) → V3.3 (metrics read) → V3.4 (server, docs).

## Conventions for every phase

- AGENTS.md rules: no `unwrap`/`expect` outside tests without an invariant comment; never panic on input; logs never carry payload content (a prompt name is user text: never log it).
- **Machine load**: other sessions build on this machine. No cargo while iterating; compile and test once right before the commit with the focused commands of the phase, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`. Never kill a process you did not start.
- Snapshots: `INSTA_UPDATE=always cargo test -p <crate> <filter>` regenerates insta snapshots; review every `.snap` diff: it must show only the additions this plan names.
- Commit once per phase with the exact subject, no attribution lines.

---

## V3.1 — Columns through core and storage

**Commit subject:** `core, storage: add the prompt span columns and the prompts trace column`

### Read first

- `crates/core/src/span.rs` (the `Span` struct, `minimal_span`, `full_span`), `crates/core/src/schema.rs` (`mod spans`, `mod traces`, `span_fields`, `trace_fields`, `SPANS_LIGHT_COLUMNS`, the test `column_constants_match_the_schemas`, the schema snapshot test), `crates/core/src/batch.rs` (`SpanColumns`: fields, `new`, `append`, `finish`; the tests `mixed_span_batch_matches_the_schema_and_values`, `arrow_ipc_round_trip`), `crates/core/src/names.rs` (`mod attr`, the tests `catalog`, `branded_constants_start_with_the_product_name`, `ATTRIBUTES`, `the_attribute_reference_names_every_cetra_attribute`).
- `crates/storage/src/parquet.rs` (`SPANS_DICTIONARY_COLUMNS`), `crates/storage/src/partials.rs` (`enum Column`, `Column::ALL`, `name()`, `Acc::update`, `SPAN_COLUMNS`, `append_span`), `crates/storage/src/reads/traces.rs` (`SPAN_LIST_COLUMNS`), `crates/storage/src/views/catalog.rs`, `crates/storage/src/views/mod.rs` (test `view_columns_follow_the_spec`), `crates/storage/src/reads/facets.rs` (`TRACES_COLUMNS`, `SPANS_COLUMNS`), `crates/storage/src/test_support.rs` (the traces batch builder), `crates/storage/src/model_tests/{model.rs,observe.rs,check.rs}` (the `models` mirrors).
- `crates/server/src/api/convert.rs` (`SpanColumns`, `span_columns`, `SpanColumns::row`).

### Edit

| File | Change |
|---|---|
| `crates/core/src/schema.rs` | consts `spans::PROMPT_NAME`, `spans::PROMPT_VERSION`, `traces::PROMPTS`; `span_fields`, `trace_fields`, `SPANS_LIGHT_COLUMNS`, the name list in `column_constants_match_the_schemas` |
| `crates/core/src/span.rs` | two `Span` fields; `full_span` sets them |
| `crates/core/src/batch.rs` | builders, `new`, `append`, `finish`; the batch test asserts the two columns |
| `crates/core/src/names.rs` | `attr::PROMPT_NAME`, `attr::PROMPT_VERSION` and the three test lists |
| `crates/core/tests/snapshots/{spans_schema,traces_schema,span_full,names}.snap` | regenerated |
| every other `Span { … }` literal (list below) | `prompt_name: None, prompt_version: None` |
| `crates/server/src/api/convert.rs` | `SpanColumns` reads the two columns |
| `crates/storage/src/parquet.rs` | `spans::PROMPT_NAME` in `SPANS_DICTIONARY_COLUMNS` (schema order) |
| `crates/storage/src/partials.rs` | `Column::Prompts`, `ALL` (29 → 30), `name()`, the `str_set` arm, `SPAN_COLUMNS`, `append_span` |
| `crates/storage/src/reads/traces.rs` | `SPAN_LIST_COLUMNS` |
| `crates/storage/src/views/catalog.rs`, `views/mod.rs` | catalog entries and the view test |
| `crates/storage/src/reads/facets.rs` | `prompts` in the traces `lists`, `prompt_name` in the spans `values` |
| `crates/storage/src/test_support.rs`, `model_tests/model.rs`, `model_tests/observe.rs`, `model_tests/check.rs` | the `prompts` mirror |
| `fixtures/api/static/schema.json` | regenerated |
| `website/src/content/docs/docs/reference/attributes.md` | the two attributes |
| `website/src/content/docs/docs/reference/sql-schema.md` | regenerated (`just docs-gen`) |
| `docs/spec/01-data-model.md` | the three columns |

### Steps

1. **Constants** (`schema.rs`). In `mod spans`, after `MODEL`: `PROMPT_NAME => "prompt_name"`, `PROMPT_VERSION => "prompt_version"` (use the `columns!` macro's existing syntax). In `mod traces`, after `MODELS`: `PROMPTS => "prompts"`.
2. **Arrow fields.** In `span_fields()`, right after the `spans::MODEL` field: `field(spans::PROMPT_NAME, DataType::Utf8, true)`, `field(spans::PROMPT_VERSION, DataType::Int64, true)`. In `trace_fields()`, right after `traces::MODELS`: `field(traces::PROMPTS, utf8_list(), false)`. In `SPANS_LIGHT_COLUMNS`, the two names right after `spans::MODEL`. Update the explicit name list in `column_constants_match_the_schemas` at the same positions.
3. **`Span`** (`span.rs`), right after `response_model` (the struct has no `model` field; `model` is derived in the batch):
   ```rust
   /// Registry prompt this span used (`cetra.prompt.name`, D-V-15). Not checked against the registry.
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub prompt_name: Option<String>,
   /// Version of that prompt (`cetra.prompt.version`, ≥ 1).
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub prompt_version: Option<i64>,
   ```
   `minimal_span` sets both `None`; `full_span` sets `Some("support-reply".to_owned())` and `Some(2)`.
4. **Batch** (`batch.rs`): `prompt_name: StringBuilder` (capacity like `request_model`, from `span.prompt_name.as_deref()`), `prompt_version: Int64Builder::with_capacity(rows)`; `append`: `append_opt(&mut self.prompt_name, span.prompt_name.as_deref())`, `self.prompt_version.append_option(span.prompt_version)`; `finish`: at the schema position (right after `model`): `finish_string(self.prompt_name)`, `Arc::new(self.prompt_version.finish())`. Extend `mixed_span_batch_matches_the_schema_and_values` to give one span both values and assert them in the batch.
5. **Names** (`names.rs`), in `mod attr` (alphabetical with its neighbours if the module is ordered, else after `ENVIRONMENT`):
   ```rust
   /// The registry prompt a span used (D-V-15).
   pub const PROMPT_NAME: &str = crate::cetra_name!(".prompt.name");
   /// The version of that prompt (D-V-15).
   pub const PROMPT_VERSION: &str = crate::cetra_name!(".prompt.version");
   ```
   Add both to `catalog()` (rows like the other `attr` rows), to the `branded_constants_start_with_the_product_name` list, and to `ATTRIBUTES`. In `website/src/content/docs/docs/reference/attributes.md`, add two rows to the table of `cetra.*` span attributes (header `Attribute | Type | Set by | Meaning`): `` `cetra.prompt.name` | string | `context` and `compile` | The registry prompt the span used `` and `` `cetra.prompt.version` | integer | `context` and `compile` | Its version (≥ 1) `` (spell types as the page does). Never backtick anything else starting with `cetra.` on that page: its test requires every such token to be an attribute.
6. **Snapshots**: `INSTA_UPDATE=always cargo test -p cetra-core`; the diffs of `spans_schema.snap`, `traces_schema.snap`, `span_full.snap`, `names.snap` show only the additions.
7. **Other `Span` literals.** Add `prompt_name: None, prompt_version: None,` after `response_model` in each: `crates/eval/src/mapping/resolve.rs`, `crates/eval/src/mapping/transcript.rs`, `crates/eval/tests/live.rs`, `crates/eval/tests/runner.rs`, `crates/otel/src/normalize.rs` (the real one: `None` for now; V3.2 fills it), `crates/storage/src/query/traces.rs`, `crates/storage/src/wal/mod.rs`, `crates/storage/src/wal/record.rs`, `crates/storage/src/test_support.rs`, `crates/storage/src/anchor.rs`, `crates/storage/src/crash_tests.rs`, `crates/storage/src/parquet.rs`, `crates/storage/src/partials.rs`, `crates/storage/tests/common/payload.rs`, `crates/storage/tests/crash.rs`, `crates/storage/tests/s3.rs`, `crates/storage/tests/s3_chaos.rs`, `crates/storage/tests/trace_bench.rs`. Then `grep -rn "Span {" crates fuzz tools --include=*.rs` and add the fields to any literal the compiler still reports. A literal using `..` needs nothing.
8. **Server conversion** (`api/convert.rs`): `SpanColumns` gains `prompt_name` and `prompt_version` read with the same helpers as `request_model` (strings) and `input_tokens` (ints); `row()` fills `prompt_name`/`prompt_version`. If `span_columns()` requires every column to exist, the two are present in every spans batch from now on.
9. **Parquet** (`parquet.rs`): add `spans::PROMPT_NAME` to `SPANS_DICTIONARY_COLUMNS` at its schema position (right after `spans::MODEL` if `MODEL` is listed, else where schema order puts it). `prompts` is not dictionary-encoded.
10. **Trace merge** (`partials.rs`): `Column::Prompts` right after `Column::Models` in the enum and in `ALL` (length 30; also replace the test's literal `0usize..29` by `0..Column::ALL.len()` and update the comments that say 29 columns); `name()`: `Self::Prompts => traces::PROMPTS`; the merge arm becomes `Column::Tags | Column::Models | Column::Prompts => str_set(…)`; every other `match` on `Column` that lists `Models` gets `Prompts` beside it (grep `Column::Models`). `SPAN_COLUMNS` gains `spans::PROMPT_NAME` and `spans::PROMPT_VERSION`. In `append_span`, after the `Models` update:
    ```rust
    if let (Some(name), Some(version)) = (prompt_name, prompt_version) {
        acc.update(Column::Prompts, Val::StrList(vec![format!("{name}@{version}")]), &ctx);
    }
    ```
    where `prompt_name: Option<String>` and `prompt_version: Option<i64>` come from `value(readers, spans::PROMPT_NAME, row)?` and `value(readers, spans::PROMPT_VERSION, row)?`, matched as `Val::Str(text)` and `Val::I64(n)` (any other `Val`, the null case, gives `None`) the same way the function already extracts `model` and a token count. A span with no prompt contributes nothing. Add a unit test `prompts_are_a_distinct_union` to `partials.rs`' test module, built with its helpers `base_span` and `partials_from_spans`: spans `a@1`, `a@1`, `b@2` and a name-only span of one trace, split across two partials, merge to `["a@1", "b@2"]` (sorted).
11. **Test mirrors**: `test_support.rs` (traces builder: a `prompts: ListBuilder<StringBuilder>` built exactly like `models`, at its schema position), `model_tests/model.rs` (`pub(super) prompts: Vec<String>` beside `models`, computed like `models` from the model spans' `prompt_name@prompt_version`), `model_tests/observe.rs` (`prompts: string_list(batch, traces::PROMPTS, row)`), `model_tests/check.rs` (`compare("prompts", …)`). The model tests' span generators (`model_tests/shape.rs`) stay unchanged: they produce no prompt, so the `prompts` mirror is always empty, which still checks the column.
12. **Span list projection** (`reads/traces.rs`): `SPAN_LIST_COLUMNS` gains the two names at the same position as in `SPANS_LIGHT_COLUMNS`.
13. **Catalog** (`views/catalog.rs`): spans entries after `model`: `col("prompt_name", ColumnKind::String, true, "Registry prompt the span used (D-V-15).")`, `col("prompt_version", ColumnKind::Integer, true, "Version of that prompt.")` (use the integer kind's real name in that file); traces entry after `models`: `col("prompts", ColumnKind::StringList, false, "Registry prompts the trace used, as name@version (D-V-15).")`. Update `view_columns_follow_the_spec` in `views/mod.rs` at the same positions.
14. **Facets** (`reads/facets.rs`): traces `lists: &["tags", "models", "prompts"]`; spans `values` gains `"prompt_name"` after the model entry.
15. **Generated files**: `CETRA_API_EXAMPLES_WRITE=1 cargo test -p cetra-server --lib api_examples::examples_are_up_to_date` (rewrites `fixtures/api/static/schema.json` and nothing else; if other files change, STOP and report which), then `just docs-gen` (rewrites `sql-schema.md`).
16. **Spec 01**: spans table rows after `model`: `| \`prompt_name\` | Utf8 (dict), nullable | P | Registry prompt (\`cetra.prompt.name\`, D-V-15). |` and `| \`prompt_version\` | Int64, nullable | P | Its version, ≥ 1. |`; traces table row after `models`: `| \`prompts\` | List(Utf8) | distinct union of \`name@version\` (spans with both) |`.

### Tests

- `cetra-core`: the updated snapshot tests, `column_constants_match_the_schemas`, `light_columns_exist_in_table_order`, the extended batch test, `arrow_ipc_round_trip`, the names tests.
- `cetra-storage`: `prompts_are_a_distinct_union`; `dictionary_lists_match_the_schemas`; `span_list_columns_follow_the_light_projection`; `view_columns_follow_the_spec`; the model tests (`cargo test -p cetra-storage model_tests` with the default case count) pass with the mirrors.
- A storage round-trip: in the test module that already appends spans and reads a trace back (find the test that checks `models` on a read trace, e.g. in `reads/traces.rs` tests), add `prompt_columns_round_trip`: append two spans of one trace with `prompt_name = "p"`, versions 1 and 2, flush, and read: the spans carry the values, the trace's `prompts` is `["p@1", "p@2"]`, and the filter `prompts @> ['p@2']` on the traces list returns the trace while `['p@3']` does not.

### Done when

`cargo test -p cetra-core`, `cargo test -p cetra-storage --lib`, `cargo test -p cetra-server --lib api_examples` pass; `just docs-gen` leaves no diff; `just check` is green.

---

## V3.2 — Normalization of the prompt attributes

**Commit subject:** `otel: normalize cetra.prompt.* and Langfuse prompt attributes`

### Read first

- `crates/otel/src/context.rs` (the `Source` tables, `ContextFields`, `fill`, `link()`, `expand_param_object`, test helpers `string_attr`, `int_attr`, `double_attr`), `crates/otel/src/fields.rs` (`first_string`, `first_i64`, `Resolved`), `crates/otel/src/value.rs` (`as_i64`), `crates/otel/src/warn.rs` (`Warning`, `code()`, the test `codes_and_order_are_alphabetical`), `crates/otel/src/normalize.rs` (`SCHEMA_VERSION`, `accept_span`, the `Span` literal), `crates/otel/src/truncate.rs` (`truncate_columns`).
- `fixtures/otlp/README.md` § Hand-written case.
- `docs/spec/02-ingestion.md` (the context fields table and rule 18's warning list).

### Edit

| File | Change |
|---|---|
| `crates/otel/src/context.rs` | prompt fields and their reader |
| `crates/otel/src/warn.rs` | two warnings |
| `crates/otel/src/normalize.rs` | `SCHEMA_VERSION = 2`, the span fields, its test's `1` |
| `crates/otel/src/truncate.rs` | `prompt_name` in `truncate_columns` (15 → 16) |
| `fixtures/otlp/**/expected.json` | re-blessed (`schema_version` 2) |
| `fixtures/otlp/cetra/prompt-link/`, `fixtures/otlp/langfuse/prompt-link/`, `fixtures/otlp/edge/prompt-version-invalid/` | new hand-written cases |
| `docs/spec/02-ingestion.md` | the fields and warnings |
| `crates/cli/tests/snapshots/e2e__sdk-*.snap`, `crates/eval/src/snapshots/judge_trace_*.snap`, `crates/server/tests/snapshots/span_detail_full.snap` | regenerated: their spans pass through `normalize` (the last one loads `fixtures/otlp/genai/chat-cache-tokens/expected.json`), so `schema_version` becomes 2 |

### Steps

1. **Warnings** (`warn.rs`): add `InvalidPromptVersion` (code `invalid_prompt_version`) and `PromptVersionWithoutName` (code `prompt_version_without_name`) in alphabetical position by code (`invalid_number` < `invalid_prompt_version` < `max_depth` < `prompt_version_without_name`), in the enum, in `code()`, and in both lists of `codes_and_order_are_alphabetical`.
2. **Reader** (`context.rs`). Constants:
   ```rust
   /// (name key, version key), in precedence order (D-V-16).
   const PROMPT_SOURCES: &[(&str, &str)] = &[
       (attr::PROMPT_NAME, attr::PROMPT_VERSION),
       ("langfuse.observation.prompt.name", "langfuse.observation.prompt.version"),
   ];
   ```
   `ContextFields` gains `prompt_name: Option<String>` and `prompt_version: Option<i64>`. A new private `fn prompt(bag: &mut AttrBag, warnings: &mut Warnings) -> (Option<String>, Option<i64>)` (use the real type names of the bag and the warnings that `fill` takes), called from `fill`:
   1. For each `(name_key, version_key)` in order: read `name_key` from the span bag as a string that is not empty after `trim()`; the first that has one wins (an all-space name is skipped and its key left in `attributes`). Consume it (`bag.take`). Conventions are detected elsewhere from key prefixes; nothing to record here.
   2. With a winning name (stored as read, not trimmed): read its `version_key` with `bag.get`. Absent → version `None`. Present: `value::as_i64(value)` returns `Result<Option<i64>, _>`; `Ok(Some(v))` with `v >= 1` → `bag.take(version_key)`, version `Some(v)`; `Ok(Some(v))` with `v < 1`, `Ok(None)` or `Err(_)` → leave the key, add `Warning::InvalidPromptVersion`, version `None`.
   3. Without any name: for each `version_key`, if the key is present, add `Warning::PromptVersionWithoutName` once (not per key) and leave the keys.
   4. The other convention's keys are never read and stay in `attributes`.
   Prompt keys are span attributes only (never resource attributes).
3. **Normalize**: the `Span` literal takes `fields.prompt_name` and `fields.prompt_version`; `SCHEMA_VERSION = 2`; fix the test that asserts `1`.
4. **Truncation**: in `truncate_columns`, add `("prompt_name", &mut span.prompt_name)` to the optional string columns and change the array length literal from 15 to 16.
5. **Hard-coded `1`s**: the only assertion on the normalizer's output is in `normalize.rs`' own test (`assert_eq!(…schema_version, 1)` → `2`). The `"schema_version": 1` lines in `otel/src/payload.rs`, `otel/src/truncate.rs`, `otel/tests/fixtures.rs`, `server/src/api/traces.rs`, `server/src/ingest/environment.rs` and `server/src/routes/traces.rs` are test inputs and stay 1. If another test fails on the number, STOP and report it.
6. **New fixture cases** (hand-written, `fixtures/otlp/README.md` § Hand-written case: `case.json`, `input.json`, then `just fixtures-bless` writes `expected.json`; review it):
   - `cetra/prompt-link`: one `llm` span (an OTel GenAI chat span is fine) with `cetra.prompt.name = "support-reply"` (string) and `cetra.prompt.version = 2` (int), plus a `langfuse.observation.prompt.name = "other"`: expected `prompt_name = "support-reply"`, `prompt_version = 2`, and the Langfuse key left in `attributes`.
   - `langfuse/prompt-link`: `langfuse.observation.prompt.name = "summarize"`, `langfuse.observation.prompt.version = "3"` (a string): `prompt_name = "summarize"`, `prompt_version = 3`, both keys consumed.
   - Every `case.json` holds `description`, `origin: "hand_written"` and `spec_refs` (at least one, e.g. `["02 § Context fields"]`), as the README requires; every span starts at or after `2025-12-31T23:59:50Z` and ends before `2026-01-01T00:00:00Z`; run `just fixtures-fmt` after writing `input.json`.
   - `edge/prompt-version-invalid`: two spans: one with `cetra.prompt.name = "p"`, `cetra.prompt.version = 0` → `prompt_name = "p"`, no version, the version left in `attributes`, warning `invalid_prompt_version`; one with only `cetra.prompt.version = 4` → no prompt, the key left, warning `prompt_version_without_name`.
7. **Re-bless**: `just fixtures-bless`; `git diff --stat fixtures/otlp` shows the three new cases and, in every other `expected.json`, only `"schema_version": 1` → `2`. Check with `git diff fixtures/otlp | grep '^[-+] ' | grep -v schema_version | grep -v '^+++\|^---'`: the output must come only from the three new case directories. Otherwise STOP.
8. **Snapshots of normalized spans**: `INSTA_UPDATE=always cargo test -p cetra-eval`, `INSTA_UPDATE=always cargo test -p cetra-cli --test e2e` and `INSTA_UPDATE=always cargo test -p cetra-server --lib api::traces`; `git diff crates/cli/tests/snapshots crates/eval/src/snapshots crates/server/tests/snapshots` must show only `"schema_version": 1` → `2` (or the snapshot's spelling of it). Anything else: STOP.
9. **Spec 02**: add `prompt_name`/`prompt_version` to the context-fields table with their sources in precedence order and the pairing rule; add the two warning codes to rule 18's list: `invalid_prompt_version` right after `invalid_number`, `prompt_version_without_name` between `max_depth` and `reserved_environment`.

### Tests (`context.rs`)

The existing `filled` helper drops its warnings: add `filled_with_warnings` beside it that returns them too, and assert with `Warnings::contains`.

1. `prompt_from_cetra` — name string + version int → both set, keys consumed.
2. `prompt_version_as_string_and_double` — `"7"` and `7.0` both give 7.
3. `cetra_name_wins_and_pairs_its_version` — cetra name without version and Langfuse name + version → name from cetra, version `None`, Langfuse keys left.
4. `langfuse_pair` — Langfuse name + version only → both set.
5. `invalid_versions` — `0`, `-1`, `1.5`, `"x"`, `true` each: name set, version `None`, key left, one `InvalidPromptVersion`.
6. `version_without_name` — cetra version and Langfuse version, no name: nothing set, both keys left, exactly one `PromptVersionWithoutName`.
7. `empty_name_is_absent` — `cetra.prompt.name = ""` and, separately, `"  "` → no prompt; the key stays in `attributes`.
8. `resource_keys_are_ignored` — `cetra.prompt.name` as a resource attribute → no prompt.

### Done when

`cargo test -p cetra-otel` (including the golden test) and the server tests touched pass; `just fixtures-check` passes; `just check` is green.

---

## V3.3 — The per-version metrics read

**Commit subject:** `storage: add the per-version prompt metrics read`

### Read first

- `crates/storage/src/reads/alerts.rs` (`alert_value`, `trace_id_plan`, how it applies the default exclusions through a trace-id semi-join and its timeout), `crates/storage/src/reads/mod.rs` (`scoped_traces`, `ListScope`, the module list), `crates/storage/src/reads/columns.rs` (`percentile`, `require`, `i64_at`, `f64_at`, `string_at`, `internal`), `crates/storage/src/reads/experiments.rs` (`means_of`, the `texts`/`ints`/`floats` cast helpers), `crates/storage/src/views/sessions.rs` (`exact_cost_sum`), `crates/storage/src/views/mod.rs` (`TimeWindow`), `crates/storage/src/lib.rs` (the `pub use reads::{…}` list).

### Edit

| File | Change |
|---|---|
| `crates/storage/src/reads/prompts.rs` | new |
| `crates/storage/src/reads/mod.rs` | `mod prompts;` and its `pub use` |
| `crates/storage/src/lib.rs` | re-export |

### Types and function (exactly these)

```rust
/// Most versions one request may name.
pub const MAX_PROMPT_VERSIONS: usize = 50;

/// What to aggregate (D-V-17).
#[derive(Debug, Clone)]
pub struct PromptMetricsRequest<'a> {
    /// The prompt name, compared with `spans.prompt_name` exactly.
    pub prompt: &'a str,
    /// Span start time window.
    pub window: TimeWindow,
    /// Only these versions, when non-empty (at most [`MAX_PROMPT_VERSIONS`]).
    pub versions: &'a [i64],
    /// Whole-read timeout.
    pub timeout: Duration,
}

/// One version's numbers. Averages and percentiles are `None` when no span had the value.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptVersionMetrics {
    pub version: i64,
    /// Linked `llm` spans.
    pub span_count: i64,
    /// Distinct traces of those spans.
    pub trace_count: i64,
    /// Spans with status `error`.
    pub error_count: i64,
    /// Exact sum of `cost_total` (USD).
    pub cost_total: f64,
    /// `cost_total` average over spans that have one.
    pub cost_avg: Option<f64>,
    /// Approximate percentiles of `duration_ns`, in nanoseconds.
    pub latency_p50_ns: Option<i64>,
    pub latency_p95_ns: Option<i64>,
    pub input_tokens_avg: Option<f64>,
    pub output_tokens_avg: Option<f64>,
    /// Numeric and boolean scores, by name (ascending).
    pub scores: Vec<PromptScoreMean>,
}

/// One score name's mean for a version.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptScoreMean {
    pub name: String,
    pub mean: f64,
    pub count: i64,
}

/// Per-version metrics, newest version first.
pub async fn prompt_metrics(ctx: &QueryContext, request: PromptMetricsRequest<'_>)
    -> Result<Vec<PromptVersionMetrics>, RunError>;
```

### Steps (inside `tokio::time::timeout(request.timeout, …)`, `Elapsed` → `RunError::Timeout`)

1. `request.versions.len() > MAX_PROMPT_VERSIONS` → `RunError::InvalidRequest("at most 50 versions")`.
2. **Linked spans**: the `spans` view filtered by `window.predicate(spans::START_TIME)`, `span_kind = 'llm'`, `prompt_name = <prompt>`, `prompt_version IS NOT NULL`, `col(spans::PROMPT_VERSION).in_list(versions.iter().map(|v| lit(*v)).collect(), false)` when the list is non-empty, and the default exclusions exactly as `alert_value` applies them: `in_subquery(col(spans::TRACE_ID), Arc::new(trace_id_plan(ctx, request.window, DAY_NS, true, None, false).await?))`. Make `trace_id_plan` and `DAY_NS` in `reads/alerts.rs` `pub(super)` (a visibility change only) and import them; do not copy them.
3. **Span aggregates**: group the linked spans by `prompt_version`: `count(1)` span_count, `count(DISTINCT trace_id)` trace_count, `sum(CASE WHEN status = 'error' THEN 1 ELSE 0 END)` error_count, `exact_cost_sum(cost_total)`, `avg(cost_total)`, `percentile(duration_ns, 0.5)` and `0.95` (read with `f64_at` and convert with `.map(|v| v.round() as i64)`), `avg(input_tokens)`, `avg(output_tokens)`. Read the result batches with the `reads/columns.rs` helpers (`require`, `i64_at`, `f64_at`, `string_at`; `f64_at` accepts Int64 and Float64), which handle the views' types; do not use the private helpers of `experiments.rs`. Rows with no span do not appear (a version with no linked span in the window is absent).
4. **Score means**: from the linked spans, `pairs` = distinct `trace_id AS pt, span_id AS ps, prompt_version AS pv` and `trace_versions` = distinct `trace_id AS tt, prompt_version AS tv` (alias every right-side column so no join has two `trace_id`s). From the `scores` view: live rows with `data_type IN ('numeric','boolean')`, `value_num IS NOT NULL`, `created_at >= window.from` (no upper bound: a score may come later than the span). Span scores (`target = 'span'`) `.join(pairs, JoinType::Inner, &["trace_id", "span_id"], &["pt", "ps"], None)` then project `pv AS version, name, value_num`; trace scores (`target = 'trace'`) join `trace_versions` on `trace_id` = `tt` and project `tv AS version, name, value_num`. Both branches project the same three columns in the same order (cast `version` to Int64 and `value_num` to Float64 in both), then `.union(…)`, group by `(version, name)`: `avg(value_num)`, `count(lit(1_i64))`. Session scores are ignored.
5. Assemble per version (newest first), scores sorted by name; `cost_total` is `0.0` when the exact sum is null.

### Tests (`reads/prompts.rs`)

Use the helpers `reads/alerts.rs`' tests use: `crate::test_support::{span_row, spans_table, traces_table, score_row, live_scores_table}` and that module's `context(spans, scores)`, `T0` and `MIN` (copy `context` into this test module if it is private). `score_row` builds a trace-target score; for span scores set `target` to the span variant of the score target enum after building (`ScoreTarget::Span { … }` or the real name you find). Fixture: prompt `p`, versions 1 and 2, in one day:
- trace A: `llm` spans `p@1` (cost 0.01, 100 ms, input 10, output 5) and `p@1` (cost 0.03, 300 ms, status error); a trace score `quality = 1.0`;
- trace B: `llm` span `p@2` (cost 0.02, 200 ms), a span score `quality = 0.5` on it, a trace score `quality = 0.0`;
- trace C: `llm` spans `p@1` and `p@2`; a trace score `helpful = 1.0` (boolean);
- trace D: a `chain` span `p@1` (not `llm`: ignored);
- trace E: an `llm` span `p@1` in environment `cetra-eval` (excluded);
- trace F: an `llm` span `q@1` (another prompt);
- a span `p@1` outside the window.

1. `aggregates_per_version` — version 2: span_count 2 (B, C), trace_count 2; version 1: span_count 3 (A×2, C), trace_count 2, error_count 1, cost_total 0.04 + C's cost, tokens averages over spans that have them; newest first.
2. `scores_per_version` — version 1: `helpful` mean 1.0 count 1 (C), `quality` mean 1.0 count 1 (A); version 2: `helpful` 1.0 (C), `quality` mean 0.25 count 2 (B's span score 0.5 and trace score 0.0).
3. `version_filter` — `versions: [2]` returns version 2 only; 51 versions → `InvalidRequest`.
4. `percentiles_are_close` — 100 spans of `p@1` with durations 1..=100 ms: p50 within 45–55 ms, p95 within 90–100 ms (approximate percentiles).
5. `unknown_prompt_is_empty` — `prompt: "nope"` → `[]`.

### Done when

`cargo test -p cetra-storage --lib reads::prompts` passes; `just check` is green.

---

## V3.4 — API fields, the metrics route, docs

**Commit subject:** `server: expose prompt links and the per-version metrics route`

### Read first

- `crates/server/src/api/traces.rs` (`SpanSummary`, `SpanDetail`, `TraceSummary`, their `From` impls), `crates/server/src/api/convert.rs` (`trace_summaries`, `list_strings`), `crates/server/src/routes/facets.rs` (a storage-backed GET with `Deadline`, `open_query`), `crates/server/src/routes/read.rs` (`Deadline`, `open_query`, `window`), `crates/server/src/routes/prompts.rs` (M1: `find`, the route table), `crates/server/src/api/prompts.rs`, `crates/server/src/api/facets.rs` (`TimeParam`), the OpenAPI block `prompt_paths` and `tests/openapi.rs`, `api_examples.rs`.

### Edit

| File | Change |
|---|---|
| `crates/server/src/api/traces.rs` | `prompt_name`, `prompt_version` on `SpanSummary` and `SpanDetail`; `prompts` on `TraceSummary`; the `From` impls; the `summary()` test helper |
| `crates/server/src/api/sessions.rs` | the `TraceSummary` literal |
| `crates/server/src/api/convert.rs` | `trace_summaries` reads `traces::PROMPTS`; `span_batch` (the span list rows) reads `spans::PROMPT_NAME` and `spans::PROMPT_VERSION` |
| `crates/server/src/api/prompts.rs` | `PromptMetricsParams`, `PromptMetrics`, `PromptVersionMetrics`, `PromptScoreMean` |
| `crates/server/src/routes/prompts.rs` | `GET /prompts/{prompt}/metrics` |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | the operation and the new fields |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/…` | the metrics example |
| `docs/spec/05-query.md`, `docs/spec/09-api.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md` | text |

### Steps

1. **API fields.** `SpanSummary` and `SpanDetail`: `#[serde(default, skip_serializing_if = "Option::is_none")] pub prompt_name: Option<String>` and `… pub prompt_version: Option<i64>`, after `model`, filled in both `From` impls and in `convert.rs`' `span_batch`, which builds list rows column by column (read `spans::PROMPT_NAME` with its string helper and `spans::PROMPT_VERSION` with its integer helper, as it reads `provider` and `input_tokens`). `TraceSummary`: `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub prompts: Vec<String>` after `models`, filled in `trace_summaries` like `models`. The server's snapshots do not change in this phase (fields are omitted when absent; V3.2 already regenerated `span_detail_full.snap`); if one does, STOP. Two struct literals need the new fields: `crates/server/src/api/sessions.rs` (a `TraceSummary { … }`: `prompts: Vec::new()`) and the `summary()` test helper in `crates/server/src/api/traces.rs` (a `SpanSummary { … }`: `prompt_name: None, prompt_version: None`); then `grep -rn "TraceSummary {\|SpanSummary {\|SpanDetail {" crates --include=*.rs` and fix any other literal the compiler reports.
2. **Types** (`api/prompts.rs`):
   ```rust
   pub struct PromptMetricsParams {   // Deserialize, IntoParams, deny_unknown_fields
       /// Window start (RFC 3339); default: `to` minus 7 days.
       #[serde(default)] pub from: Option<TimeParam>,
       /// Window end; default: now.
       #[serde(default)] pub to: Option<TimeParam>,
       /// Comma-separated version numbers (1–50 distinct); default: every version.
       #[serde(default)] pub versions: Option<String>,
   }
   pub struct PromptMetrics {          // Serialize, Deserialize, ToSchema
       pub from: i64 /* time */, pub to: i64 /* time */,
       pub versions: Vec<PromptVersionMetrics>,
   }
   pub struct PromptVersionMetrics {
       pub version: i64, pub span_count: i64, pub trace_count: i64, pub error_count: i64,
       pub cost_total: f64,
       /* Option fields with skip_serializing_if: */ cost_avg: Option<f64>, latency_p50_ms: Option<f64>,
       latency_p95_ms: Option<f64>, input_tokens_avg: Option<f64>, output_tokens_avg: Option<f64>,
       pub scores: Vec<PromptScoreMean>,
   }
   pub struct PromptScoreMean { pub name: String, pub mean: f64, pub count: i64 }
   ```
   Latencies are converted from nanoseconds to milliseconds (`ns as f64 / 1e6`). Use the time serde attributes for `from`/`to`; use `TimeParam` from `api/common.rs` (`use super::common::TimeParam`) exactly as `api/facets.rs` does.
3. **Route** `GET /api/v1/projects/{project}/prompts/{prompt}/metrics`, `Access::Project { perm: ProjectPerm::View, session_only: false }`, handler `prompt_metrics`:
   1. `find` the prompt (404 when absent).
   2. `to` = param or `state.clock.now_ns()`; `from` = param or `to − 7 days`. `from >= to` → 400 field `from`: "must be before to". `to − from > 90 days` → 400 field `from`: "the window is at most 90 days".
   3. `versions`: split on `,`, trim, each a positive integer (400 field `versions`: "must be comma-separated positive versions"), distinct, 1–50 (400 "must name 1 to 50 versions").
   4. Import the storage types by path (`cetra_storage::PromptMetricsRequest`, and the result types as `cetra_storage::PromptVersionMetrics as StoredVersionMetrics`) since `api/prompts.rs` defines types of the same names. `Deadline::start()`, `open_query(&state, access.project.org_id, access.project.id, None)`, `TimeWindow::new(from, to)` (map its error to 400 as `routes/read.rs`'s `window` does), `cetra_storage::prompt_metrics(&ctx, PromptMetricsRequest { prompt: &prompt.name, window, versions: &versions, timeout: deadline.remaining()? })`; `?` maps `RunError` as everywhere.
   5. Answer `200 PromptMetrics`.
4. **OpenAPI**: operation `get_prompt_metrics` (tag `prompts`, summary "Get per-version prompt metrics", description `PROMPT_READ` plus "Default window: the last 7 days; at most 90 days (D-V-17)."), query params from `PromptMetricsParams`, responses 200 `PromptMetrics`; `400 401 403 404 422 429`. Add it to `OPERATIONS`, `QUERY_OR_BODY` and `READ_OPERATIONS` (where `get_facets` is); schemas `PromptMetrics`, `PromptVersionMetrics`, `PromptScoreMean`. `just openapi`.
5. **Example**: in `api_examples.rs`, after the prompt seed, fetch `/prompts/support-reply/metrics?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z` into `responses/prompts/support-reply/metrics.json` (the example project has no linked spans: `versions` is `[]`; that is fine and documents the shape), with a response round-trip entry `get_prompt_metrics` → `PromptMetrics`. `just api-examples`.
6. **Route tests** (`routes/prompts.rs`): `metrics_route` — append, the way `routes/traces.rs`' tests do (around its line 480: build a `Span` from JSON and append it), two `llm` spans with `prompt_name = "support-reply"`, versions 1 and 2, starting at 2026-01-01T00:10:00Z; every metrics request in the test passes explicit `from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z` (never rely on the clock); create the prompt `support-reply` via the API; GET metrics → two versions, newest first, span counts 1 each; `?versions=1` → one; `?versions=0` → 400; `from` after `to` → 400; unknown prompt → 404; `read_key` → 200; `ingest_key` → 403. And `trace_summary_lists_prompts` — the traces list for that trace has `prompts == ["support-reply@1", "support-reply@2"]` and both the span detail and the spans list (`GET …/spans`) rows carry `prompt_name`/`prompt_version`.
7. **Docs**:
   - `docs/spec/05-query.md`: the traces facet lists gain `prompts`, the spans values gain `prompt_name` (where the facet columns are listed); one sentence that `prompts @> ['name@version']` filters traces by prompt version.
   - `docs/spec/09-api.md` § Prompts: the metrics row.
   - `docs/mvp-decisions.md` § Track V, and the same three decisions in `docs/plans/v-prompts/00-design.md`:
     - append to D-V-15: "Columns sit after `model` (spans) and `models` (traces); a name-only span contributes nothing to `prompts`; `schema_version` 2."
     - in D-V-16, replace "A version without a name is dropped with a warning." by "The version is read only from the convention whose name won; a bad version, or a version without a name, stays in `attributes` with the warning `invalid_prompt_version` or `prompt_version_without_name`."
     - append to D-V-17: "The route answers counts (span, trace, error) rather than rates; a span score counts for its span's version; a trace score counts for every version its linked `llm` spans carry; the window is at most 90 days and the data window (D175) hides older spans; versions 1–50. Widget breakdowns do not offer prompts yet."
   - `CHANGELOG.md` `### Added`: `- Prompt links on spans and traces (D-V-15, D-V-16): cetra.prompt.name/version and Langfuse's prompt attributes fill prompt_name, prompt_version and the traces' prompts list (filter with prompts @> ['name@version']); GET /prompts/{name}/metrics gives per-version cost, latency, errors and scores (D-V-17). Spans are now normalized with schema_version 2.`
   - `docs/roadmap.md` V row: `M1 (registry API) and M3 (trace linking) implemented.` (keep M2 if it is already listed).

### Done when

`cargo test -p cetra-server --lib routes::prompts`, `--test openapi`, `--test api_examples` pass; `just openapi`, `just api-examples` and `just docs-gen` leave no diff; `just check` is green.
