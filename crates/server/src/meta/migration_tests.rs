//! The `meta.db` migration lint (spec 04 § Format versions, D-R-34). It mirrors the catalog's
//! lint (`crates/storage/src/catalog/db.rs`, D-R-31) for `migrations/meta`.

use std::borrow::Cow;
use std::path::Path;

use sqlx::Connection;

use super::{META_FORMAT_VERSION, MetaDb, connect, migrator};

/// Migrations from before the first public release that do more than add. Frozen at
/// that release: never append to it afterwards (a later one is format-changing).
const PRE_RELEASE_NON_ADDITIVE: &[i64] = &[8]; // 0008 drops orgs.limits (D-P-11)
/// Released migrations that changed the format; each came with a META_FORMAT_VERSION bump.
const FORMAT_CHANGING_MIGRATIONS: &[i64] = &[];

/// A migration that alters an existing table, with rows it must keep.
struct AlteringCase {
    /// The migration under test.
    version: i64,
    /// Rows as the migration before `version` left the tables. Run on a stamped database.
    seed: &'static [&'static str],
    /// Reads the rows back after the open, one text value per row.
    read: &'static str,
    /// What `read` returns, rows joined by newlines: the rows are intact and the new
    /// columns hold their default or NULL.
    expected: &'static str,
}

const ALTERING_CASES: &[AlteringCase] = &[
    AlteringCase {
        version: 8,
        seed: &[
            "INSERT INTO orgs (id, name, slug, limits, state, created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000a1', 'Acme', 'acme', '{}', 'active', 1, 2)",
        ],
        read: "SELECT id || '|' || name || '|' || slug || '|' || state || '|' || created_at
            || '|' || updated_at FROM orgs",
        expected: "018f6b4e-7c1a-7c1a-8c1a-0000000000a1|Acme|acme|active|1|2",
    },
    AlteringCase {
        version: 13,
        seed: &[
            "INSERT INTO users (id, email, created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000b1', 'kept@example.test', 1, 1)",
            "INSERT INTO orgs (id, name, slug, state, created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000b2', 'Acme', 'acme', 'active', 1, 2)",
            "INSERT INTO web_sessions (id, user_id, created_at, last_seen_at, expires_at, user_agent, ip)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000b3', '018f6b4e-7c1a-7c1a-8c1a-0000000000b1',
            1, 2, 3, 'agent', '192.0.2.1')",
        ],
        read: "SELECT id || '|' || user_id || '|' || created_at || '|' || last_seen_at || '|'
            || expires_at || '|' || user_agent || '|' || ip || '|' || COALESCE(sso_org_id, 'NULL')
            FROM web_sessions",
        expected: "018f6b4e-7c1a-7c1a-8c1a-0000000000b3|018f6b4e-7c1a-7c1a-8c1a-0000000000b1|1|2|3|agent|192.0.2.1|NULL",
    },
    AlteringCase {
        version: 15,
        seed: &[
            "INSERT INTO users (id, email, created_at, updated_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000c1', 'kept@example.test', 1, 1)",
            "INSERT INTO web_sessions (id, user_id, created_at, last_seen_at, expires_at)
            VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000c2', '018f6b4e-7c1a-7c1a-8c1a-0000000000c1',
            1, 2, 3)",
        ],
        read: "SELECT COALESCE(u.email_verified_via, 'NULL') || '|' || w.via_provider
            FROM web_sessions w JOIN users u ON u.id = w.user_id",
        expected: "NULL|0",
    },
];

/// The statements of a migration: `--` comments dropped, split on `;`, whitespace
/// collapsed, empty ones dropped. No migration has a `--` or a `;` inside a string literal.
fn statements(sql: &str) -> Vec<String> {
    let code: Vec<&str> = sql
        .lines()
        .map(|line| line.split("--").next().unwrap_or_default())
        .collect();
    code.join("\n")
        .split(';')
        .map(|statement| statement.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|statement| !statement.is_empty())
        .collect()
}

/// Whether a statement only adds to the schema: it creates a table or an index, or adds a
/// column that has a default or is nullable.
fn is_additive(statement: &str) -> bool {
    let upper = statement.to_ascii_uppercase();
    if upper.starts_with("CREATE TABLE ")
        || upper.starts_with("CREATE INDEX ")
        || upper.starts_with("CREATE UNIQUE INDEX ")
    {
        return true;
    }
    let words: Vec<&str> = upper.split_whitespace().collect();
    match words.as_slice() {
        // ALTER TABLE <name> ADD COLUMN <column> <type> ...
        ["ALTER", "TABLE", _, "ADD", "COLUMN", _, ..] => {
            let definition = &words[6..];
            let has_default = definition.contains(&"DEFAULT");
            let not_null = definition
                .windows(2)
                .any(|pair| pair[0] == "NOT" && pair[1] == "NULL");
            has_default || !not_null
        }
        _ => false,
    }
}

/// Creates `path` with the migrations before `version` applied, stamps it as a released
/// database (`user_version` is the format version), and runs `seed` on it.
async fn released_database_before(path: &Path, version: i64, seed: &[&'static str]) {
    let mut before = migrator();
    let index = before
        .migrations
        .iter()
        .position(|migration| migration.version == version)
        .unwrap();
    before.migrations = Cow::Owned(before.migrations[..index].to_vec());
    let mut connection = connect(path, true).await.unwrap();
    before.run(&mut connection).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {META_FORMAT_VERSION}"
    )))
    .execute(&mut connection)
    .await
    .unwrap();
    for statement in seed {
        sqlx::query(*statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    connection.close().await.unwrap();
}

#[test]
fn migrations_only_add() {
    for migration in migrator().migrations.iter() {
        if FORMAT_CHANGING_MIGRATIONS.contains(&migration.version)
            || PRE_RELEASE_NON_ADDITIVE.contains(&migration.version)
        {
            continue;
        }
        for statement in statements(migration.sql.as_str()) {
            assert!(
                is_additive(&statement),
                "migration {:04} is not additive: a meta.db format change. Bump META_FORMAT_VERSION (spec 04 § Format versions), then list the migration in FORMAT_CHANGING_MIGRATIONS.",
                migration.version,
            );
        }
    }
}

#[tokio::test]
async fn migrations_that_alter_a_table_keep_its_rows() {
    for case in ALTERING_CASES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        released_database_before(&path, case.version, case.seed).await;
        let db = MetaDb::open(dir.path()).await.unwrap();
        let rows: Vec<String> = sqlx::query_scalar(case.read)
            .fetch_all(db.reader())
            .await
            .unwrap();
        assert_eq!(
            rows.join("\n"),
            case.expected,
            "migration {:04}",
            case.version
        );
        db.close().await;
    }
}

#[test]
fn every_altering_migration_has_a_case() {
    let mut altering = Vec::new();
    for migration in migrator().migrations.iter() {
        let alters = statements(migration.sql.as_str())
            .iter()
            .any(|statement| statement.to_ascii_uppercase().starts_with("ALTER TABLE "));
        if alters {
            altering.push(migration.version);
            assert!(
                ALTERING_CASES
                    .iter()
                    .any(|case| case.version == migration.version),
                "migration {:04} alters a table: add a case to migrations_that_alter_a_table_keep_its_rows",
                migration.version,
            );
        }
    }
    for case in ALTERING_CASES {
        assert!(
            altering.contains(&case.version),
            "migration {:04} has a case but does not alter a table",
            case.version,
        );
    }
}

#[test]
fn pre_release_exceptions_predate_the_first_release() {
    let releases =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/storage/golden/RELEASES");
    let text = std::fs::read_to_string(&releases).unwrap_or_default();
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'));
    let Some(first) = first else {
        println!("no released server golden directory yet");
        return;
    };
    let expected = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/server/golden")
        .join(first)
        .join("expected.json");
    if !expected.is_file() {
        println!("no released server golden directory yet");
        return;
    }
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&expected).unwrap()).unwrap();
    let schema = value["meta_schema"].as_i64().unwrap();
    for version in PRE_RELEASE_NON_ADDITIVE {
        assert!(
            *version <= schema,
            "migration {version:04} is a pre-release exception and must predate the first release (meta_schema {schema})",
        );
    }
}
