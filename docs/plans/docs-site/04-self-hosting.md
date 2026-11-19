# W4 — Self-hosting moves into the site; the configuration reference and its checks

**Commit subject:** `Move the self-hosting docs into the site and test the configuration reference`

**Goal:** `docs/self-hosting/` becomes the site's Self-hosting topic and is deleted; every pointer to it in code, deploy files and specs points at `https://cetra.sh/docs/self-hosting/<page>/`. A new configuration reference lists every key, and a Rust test proves it lists exactly the keys the loader accepts, with the right env aliases. A Node check proves every `CETRA_*` name anywhere in the site exists.

## Read first

- README § Decisions D-W-5, D-W-7, D-W-14; § Page list › Self-hosting topic; § Source of truth and drift.
- Every file in `docs/self-hosting/`.
- `crates/server/src/config.rs`: `ALIASES`, `ServerConfig` and every struct it holds, `load_from`, `map_figment`, `ConfigError`, `split_key`, `is_filtered`, `mod tests`.
- `crates/storage/src/config.rs`: `StorageConfig` and its sub-structs (every one has `#[serde(deny_unknown_fields)]`).
- `docs/spec/12-operations.md` § Configuration, § Security checklist, § Documentation deliverables; `docs/spec/03-storage.md` § 10 Resource needs.
- figment 0.10's `figment::error::Kind` (`UnknownField(String, &'static [&'static str])`, `InvalidType(Actual, String)`): `cargo doc -p figment --open` or the source under `~/.cargo/registry`.

**Check first:** `rg -l "docs/self-hosting" --glob '!docs/plans/**' --glob '!docs/reviews/**' --glob '!docs/research/**' --glob '!docs/mvp-decisions.md' --glob '!docs/roadmap.md'` lists exactly: `CHANGELOG.md`, `crates/cli/src/main.rs`, `crates/storage/src/model_tests/mod.rs`, `crates/storage/tests/s3.rs`, `crates/storage/tests/s3_probe.rs`, `deploy/compose/cetra.env.example`, `deploy/compose/compose.s3.yaml`, `deploy/compose/compose.yaml`, `deploy/helm/cetra/README.md`, `deploy/helm/cetra/templates/NOTES.txt`, `deploy/helm/cetra/values.yaml`, `docs/spec/03-storage.md`, `docs/spec/12-operations.md`. If the list differs, include the extra files in step 3 and say so in the report. Historical documents (plans, reviews, research, decisions, roadmap) keep their old paths: they record what was true then.

## Files

| File | Change |
|---|---|
| `docs/self-hosting/*.md` → `website/src/content/docs/docs/self-hosting/*.md` (`git mv`) | moved, frontmatter added, links rewritten |
| `website/src/content/docs/docs/self-hosting/configuration-reference.md` (new) | every key |
| `website/src/content/docs/docs/self-hosting/resources.md`, `security.md` (new) | from spec 03 § 10 and spec 12 § Security checklist |
| `website/astro.config.mjs` | the Self-hosting topic |
| `website/src/content/docs/docs/index.mdx`, `get-started/run-cetra.mdx` | links into the topic |
| `website/scripts/check-env-vars.mjs`, `check-env-vars.test.mjs` (new) | the env-var check |
| `website/package.json` | `check:content` runs it |
| `crates/server/src/config.rs` | the test `the_configuration_reference_matches_the_loader` |
| the files of the "check first" list | links to the site |
| `docs/spec/12-operations.md` | § Documentation deliverables and the pointers |
| `AGENTS.md` | § Source of truth: self-hosting docs are in the site |

## Steps

### 1. Move the pages

```bash
mkdir -p website/src/content/docs/docs/self-hosting
git mv docs/self-hosting/README.md website/src/content/docs/docs/self-hosting/index.md
for f in install docker-compose kubernetes configuration s3 backup monitoring upgrade; do
  git mv docs/self-hosting/$f.md website/src/content/docs/docs/self-hosting/$f.md
done
```

In each moved file:
- Replace the `# Title` line with frontmatter: `title:` (the old H1, sentence case; `README.md` becomes "Self-hosting"), `description:` (one sentence, under 160 characters).
- Rewrite every link: `install.md` → `/docs/self-hosting/install/`, `install.md#docker` → `/docs/self-hosting/install/#docker`, the same for each moved page; `../spec/12-operations.md#configuration` → `/docs/self-hosting/configuration-reference/`; any other link into `docs/` (specs, plans) is removed or replaced by a link to a site page (a doc page never cites a spec, D-W-5).
- Keep the content otherwise. Do not reword what is correct.
- `configuration.md`: its last sentence of the first paragraph ("The full list with defaults is in spec 12 …") points at `/docs/self-hosting/configuration-reference/`.

`git rm` nothing else: after the moves `docs/self-hosting/` is empty and disappears.

### 2. New pages

**`configuration-reference.md`** (title "Configuration reference"). One intro paragraph (sources and precedence, the `CETRA_` + `__` rule with an example, units for sizes and durations, unknown keys are errors), then one `##` section per top-level table (`Top level`, `[storage]`, `[ingest]`, `[media]`, `[eval]`, `[security]`, `[entitlements]`, `[orgs]`, `[auth]`, `[init]`, `[log]`), each with a table of exactly these columns:

```markdown
| Key | Environment | Type | Default | Description |
|---|---|---|---|---|
| `data_dir` | `CETRA_DATA_DIR` | path | `./cetra-data` | … |
| `storage.flush.max_age` | | duration | `"120s"` | … |
| `auth.oidc[].issuer` | | URL | | … |
```

Rules the test relies on:
- The first cell is the full dotted key in backticks, one key per row (never `a` / `b` in one cell). Elements of an array of tables are written `path[].field`.
- The second cell is empty or holds exactly one backticked name: the key's alias from `ALIASES` (`CETRA_DATA_DIR`). The split form (`CETRA_STORAGE__FLUSH__MAX_AGE`) is never written in this column: it exists for every key, as the intro says. Other ways to set a value (the `AWS_*` variables, `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`) go in the description.
- Defaults come from the code (`default_*` functions and `Default` impls), not from the spec. Where spec 12 and the code disagree, the code wins; list each disagreement in the report for the coordinator.
- No key appears that the loader refuses (spec 12 lists `telemetry.enabled` and `tls.cert` / `tls.key`; if the loader refuses them, they are not documented).

**`resources.md`** (title "Resources"): memory, disk and file-descriptor needs and the small-machine profile, from spec 03 § 10, as requirements and settings, with no throughput or latency figure (D-W-14). If spec 03 § 10 states a throughput figure, leave it out.

**`security.md`** (title "Security"): spec 12 § Security checklist rewritten as a checklist for an operator (TLS through a proxy, `trusted_proxies`, the private `metrics_addr`, secrets in env files, `security.allow_private_network_egress`, backups hold `secret.key`), each item linking to the page that explains it.

### 3. Pointers

Replace each `docs/self-hosting/<page>.md` in the "check first" files with `https://cetra.sh/docs/self-hosting/<page>/` (`README.md` → `https://cetra.sh/docs/self-hosting/`). In `deploy/helm/cetra/README.md` the relative links (`../../../docs/self-hosting/kubernetes.md`) become the same absolute URLs. In `crates/cli/src/main.rs`, the message at the `no running cetra serve answers on` error and its test's `assert!(text.contains(…))` both change to the URL. `CHANGELOG.md`'s header line gets the URL too.

In `docs/spec/12-operations.md` § Documentation deliverables, replace "Self-hosting (`docs/self-hosting/`: …; track N)" with "Self-hosting (`website/src/content/docs/docs/self-hosting/`, served at cetra.sh/docs/self-hosting/: …; tracks N and W)" and add at the end of the section: "The site and its drift checks are track W (`docs/plans/docs-site/`)." Update `docs/spec/03-storage.md`'s pointer the same way.

### 4. The Rust test

In `crates/server/src/config.rs` `mod tests`, add:

```rust
/// The configuration reference page (D-W-7): its keys are exactly the keys the loader
/// accepts, and its Environment column is exactly `ALIASES`.
const REFERENCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../website/src/content/docs/docs/self-hosting/configuration-reference.md"
));
```

and these helpers and test (adapt names to the module's style; keep the logic):

1. `documented() -> BTreeMap<String, Option<String>>`: for each line of `REFERENCE` starting with `` "| `" ``, split on `|`, take cell 1's text between the first pair of backticks as the key and cell 2's text between backticks, if any, as the env name. A key listed twice is a failure.
2. `probe(path: &[&str], array: bool) -> Probe`: build a TOML document that sets an unknown field `zz_probe = 1` inside the table at `path` (`[a.b]\nzz_probe = 1\n`, or `[[a.b]]\nzz_probe = 1\n` when `array`; the top level is just `zz_probe = 1\n`), call `load_from(Some(&toml), &[])`, and classify:
   - `Err(ConfigError::Figment(error))` where some part of the error (iterate `(*error).clone()`) has `kind == Kind::UnknownField(_, expected)` and `path` equal to `path` → `Probe::Table(expected.to_vec())`;
   - such an error with `Kind::InvalidType(Actual::Map, expected)` where `expected` contains `"sequence"` → `Probe::ArrayOfTables`;
   - `Ok(_)` → panic: "`<path>` accepts an unknown field: add `#[serde(deny_unknown_fields)]`" (a table must refuse unknown fields for this test, and for users);
   - anything else → `Probe::Leaf`.
3. `accepted() -> BTreeSet<String>`: walk from the top level. For each field `f` of a `Table`, probe `path + f`: a `Table` recurses; an `ArrayOfTables` probes again with `array = true` and recurses with the field written `f[]`; a `Leaf` is a key. `storage.data_dir` is not a key (the loader rejects it and copies the top-level one); drop it.
4. The test:

```rust
#[test]
fn the_configuration_reference_matches_the_loader() {
    let documented = documented();
    let accepted = accepted();
    let missing: Vec<_> = accepted.iter().filter(|key| !documented.contains_key(*key)).collect();
    let unknown: Vec<_> = documented.keys().filter(|key| !accepted.contains(*key)).collect();
    assert!(missing.is_empty(), "keys the loader accepts but the reference omits: {missing:?}");
    assert!(unknown.is_empty(), "keys the reference lists but the loader refuses: {unknown:?}");
    for (alias, key) in ALIASES {
        assert_eq!(
            documented.get(*key).and_then(Option::as_deref),
            Some(*alias),
            "{key} must list {alias} in the Environment column"
        );
    }
    for (key, env) in &documented {
        if let Some(env) = env {
            assert!(
                ALIASES.iter().any(|(alias, aliased)| alias == env && aliased == key),
                "{key}: {env} is not its alias in ALIASES"
            );
        }
    }
}
```

Notes for the implementer:
- `load_from` merges `storage.data_dir` itself; a probe of `storage` therefore sees `StorageConfig`'s fields. If a probe trips an earlier check (the `storage.data_dir` guard, a TOML parse error), the probe document is wrong: fix the document, not the classification.
- A field whose `deserialize_with` parses a string (`http_addr`, `smtp_url`) answers the map probe with a custom error: that is `Probe::Leaf`, as intended.
- If the walk finds a map-typed field (a `HashMap`), the `Ok` panic fires; stop and report the field instead of special-casing it.
- The test needs no new dependency (`figment` is already one).
- Write the page first, then run `cargo test -p cetra-server the_configuration_reference_matches_the_loader` and fix the page until it passes. Never change the loader to make the page pass.

### 5. `check-env-vars.mjs`

`website/scripts/check-env-vars.mjs` exports `checkSite(pages, codeText, configKeys)` and a `main`:
- `pages`: every `.md`/`.mdx` under `src/content/docs/`, with code fences included (examples count).
- `codeText`: the concatenated text of every file under `crates/*/src`, `sdks/python/src`, `sdks/typescript/src`, `deploy/` and `tools/packaging/` (read once; skip `node_modules`, `target`).
- `configKeys`: the first-cell keys of `configuration-reference.md`.
- For each match of `/\bCETRA_[A-Z0-9_]*[A-Z0-9]/g` in a page:
  - followed directly by `<` (a pattern such as `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`): the part before `<` must occur in `codeText`;
  - containing `__`: strip `CETRA_`, lower-case, replace `__` with `.`; the result must be in `configKeys` (the split form of a documented key);
  - otherwise: the name must occur in `codeText`.
- Findings: `path:line: CETRA_FOO is not read by the code`.

`check-env-vars.test.mjs` covers each branch with small strings. `website/package.json`: append `&& node scripts/check-env-vars.mjs` to `check:content`.

### 6. Sidebar and links

`astro.config.mjs`: add the topic after Docs:

```js
{
  label: "Self-hosting",
  link: "/docs/self-hosting/",
  icon: "setting",
  items: [
    { label: "Overview", link: "/docs/self-hosting/" },
    {
      label: "Install",
      items: [
        { label: "Install", link: "/docs/self-hosting/install/" },
        { label: "Docker Compose", link: "/docs/self-hosting/docker-compose/" },
        { label: "Kubernetes", link: "/docs/self-hosting/kubernetes/" },
      ],
    },
    {
      label: "Configure",
      items: [
        { label: "Configuration", link: "/docs/self-hosting/configuration/" },
        { label: "Configuration reference", link: "/docs/self-hosting/configuration-reference/" },
        { label: "Object storage (S3)", link: "/docs/self-hosting/s3/" },
        { label: "Security", link: "/docs/self-hosting/security/" },
        { label: "Resources", link: "/docs/self-hosting/resources/" },
      ],
    },
    {
      label: "Operate",
      items: [
        { label: "Backup and restore", link: "/docs/self-hosting/backup/" },
        { label: "Monitoring", link: "/docs/self-hosting/monitoring/" },
        { label: "Upgrade", link: "/docs/self-hosting/upgrade/" },
      ],
    },
  ],
},
```

`run-cetra.mdx`: the GitHub links of W3 become site links. `index.mdx`: a "Self-hosting" card.

### 7. `AGENTS.md`

§ Source of truth table: the `website/` row says it includes the self-hosting guides. § Tests: add "`config::tests::the_configuration_reference_matches_the_loader` checks the site's configuration reference against the loader: a new config key is documented in `website/src/content/docs/docs/self-hosting/configuration-reference.md` in the same PR."

### 8. CHANGELOG

Under `[Unreleased]` › `Changed`: "The self-hosting guides moved from `docs/self-hosting/` to cetra.sh/docs/self-hosting/."

## Tests

- `cargo test -p cetra-server the_configuration_reference_matches_the_loader` passes; deleting one row of the page makes it fail with that key in `missing` (try, revert); adding `| \`nope\` | | | | |` makes it fail in `unknown`.
- The CLI test that checked the backup path passes with the URL.
- `check-env-vars.test.mjs` passes; `just check-docs` passes with the moved pages (links validated).
- `just helm-check` and `just check-packaging-lint` pass (the deploy files changed only in comments and the README).

## Done when

- `docs/self-hosting/` no longer exists, and the "check first" command lists nothing.
- `just check` and `just check-packaging-lint` pass.
- The report lists every spec 12 key or default that differs from the code.
