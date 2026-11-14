# B9 — Pre-migration copies

Read [00-design.md](00-design.md) first, especially § Design › Pre-migration copies (D-N-22).

**Goal:** before the migrations of `meta.db` or `catalog.db` change an existing database, the server
copies it to `<name>.pre-<binary version>` with `VACUUM INTO`. Afterwards, the other `<name>.pre-*`
copies are deleted, so only the copy from the last upgrade remains.

**Edit only:** `crates/server/src/meta/mod.rs`, `crates/storage/src/catalog/db.rs`,
`crates/storage/src/error.rs` (one variant), `docs/spec/12-operations.md` (§ Upgrades sentence).

**Commit:** `Copy the databases before migrating them`

## Read first

- `crates/server/src/meta/mod.rs`: `MetaDb::open`, `connect`, `migrator`, `expected_version`,
  `applied_version`.
- `crates/storage/src/catalog/db.rs`: `open_catalog_db`.

## Changes

### One rule, implemented in each crate

The crates do not depend on each other, so each gets its own private function, about 30 lines.

```text
fn pre_migration_copy(connection, dir, name, applied, expected):
    if applied == 0 (no migrations table: a new database) or applied >= expected: return
    target = dir / "{name}.pre-{CARGO_PKG_VERSION}"
    if target exists: keep it and skip the copy (a previous start of this version made it; an
        upgrade that failed half-way must not replace the good copy with a half-migrated one)
    else:
        tmp = target + ".tmp"; remove a leftover tmp
        VACUUM INTO ?  with tmp bound as a parameter (never interpolated: paths may contain quotes),
                       on the same connection, before any migration runs
        rename tmp to target
    for every entry in dir whose file name starts with "{name}.pre-" and is not target: remove it
        (other versions and stale .tmp files)
    log info: "copied {name} before migrating" with applied and expected (no paths)
```

### `meta/mod.rs`

In `MetaDb::open`, after `connect(&path, true)` and before `migrator().run(...)`:

```rust
let applied = applied_version(&mut writer).await?;
pre_migration_copy(&mut writer, data_dir, "meta.db", applied, expected_version()).await?;
```

Errors map to two new variants: `MetaError::PreMigrationIo(std::io::Error)` and
`MetaError::PreMigrationDb(sqlx::Error)`. Neither uses `#[from]`: `MetaError` already has
`From<sqlx::Error>`. Map explicitly. A failed copy stops the
start: an upgrade without its safety copy is not run.

### `catalog/db.rs`

In `open_catalog_db`, between `connect` and the migrator:
1. Compute `applied` the same way: the `_sqlx_migrations` lookup. Copy the small helper here.
2. Compute `expected` as the largest version of `sqlx::migrate!("./migrations/catalog")`.
3. Call the same rule with `"catalog.db"`.

Errors map to `CatalogError::PreMigrationCopy`. Add the variant in `error.rs`.

### `docs/spec/12-operations.md` § Upgrades

The sentence about the automatic pre-migration copies currently says "(not built)" and "are track N's
M2". Make it: "Before migrating an existing `meta.db` or `catalog.db`, the server keeps a copy named
`meta.db.pre-<version>` / `catalog.db.pre-<version>` (by `VACUUM INTO`), and only the copy from the
last upgrade (D-N-22)."

## Tests

**`meta/mod.rs`:**
- `opening_an_older_meta_db_keeps_a_pre_migration_copy`:
  1. Create a `meta.db` migrated only to the first migration. `Migrator::run` cannot apply only
     the first N, so build it by hand on a fresh file:
     - create sqlx's `_sqlx_migrations` table with the same columns sqlx uses (see the table
       `crates/server/src/admin.rs` tests create);
     - execute `migrator().migrations[0].sql`;
     - insert its row with the **real** `version`, `description` and `checksum` from
       `migrations[0]`, `success = 1`, and any `execution_time`.

     Then `MetaDb::open` sees version 1 < expected and migrates the rest.
  2. Add an old `meta.db.pre-0.0.0-old` file.
  3. Call `MetaDb::open`.
  4. `meta.db.pre-<CARGO_PKG_VERSION>` exists and has the first migration's version; the old copy is
     gone.
- `a_new_or_current_meta_db_gets_no_copy`: open a fresh directory twice; no `meta.db.pre-*` file.
- `an_existing_pre_migration_copy_is_kept`:
  1. Before opening an older `meta.db`, create `meta.db.pre-<CARGO_PKG_VERSION>` with known
     content.
  2. After `MetaDb::open`, its content is unchanged.

**`catalog/db.rs`:**
- `opening_an_older_catalog_keeps_a_pre_migration_copy`: the same recipe with the catalog migrator's
  first migration (`0001_catalog.sql`).
- `a_new_or_current_catalog_gets_no_copy`.
- `an_existing_pre_migration_copy_is_kept` (catalog).

## Verify

```bash
cargo test -p llmobs-server --all-features pre_migration
cargo test -p llmobs-storage --all-features pre_migration
just check
```

## Done when

- [ ] Both databases are copied before an actual migration, and never otherwise.
- [ ] Only the last copy remains.
- [ ] The six tests pass, and spec 12 is updated.
- [ ] `just check` passes. One commit: `Copy the databases before migrating them`.
