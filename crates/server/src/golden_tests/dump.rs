//! What a restored golden directory must answer (`expected.json`, D-R-38): every row of `meta.db`,
//! the decoded reads of the store functions by natural key, the sealed secrets, the sign-in checks
//! and the encryption check. Computed from a restored copy of the backup.

use std::path::Path;

use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row, TypeInfo, ValueRef};

use super::{
    GENERATOR, INGEST_KEY, INVITE_TOKEN, OWNER_PASSWORD, PERSONAL_TOKEN, PUBLIC_KEY, SESSION_TOKEN,
};
use crate::auth::password::Hasher;
use crate::auth::secret::hash_token;
use crate::clock::Clock;
use crate::config::PublicUrl;
use crate::crypto::{self, Aad, SecretBox};
use crate::ee::org_sso::store as sso;
use crate::meta::orgs::OrgRow;
use crate::meta::projects::ProjectRow;
use crate::meta::{self, MetaDb};
use crate::meta::{
    agent_contexts, alerts, annotation_queues, api_keys, assistant, backfills, connections,
    dashboards, datasets, deletions, entitlements, eval_jobs, evaluators, experiments, identities,
    instance_settings, invites, issue_links, issues, members, model_prices, online_rules, orgs,
    password_resets, pending_traces, personal_tokens, projects, prompts, rule_spend, saved_views,
    score_definitions, usage, users, web_sessions,
};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The column that names a row of a sealed column's table (its natural key). Every entry of
/// `crypto::ENCRYPTED_COLUMNS` needs one: a new sealed column fails `secret_values` until it is
/// listed here and has a row in the scenario.
fn natural_key(table: &str, column: &str) -> &'static str {
    match (table, column) {
        ("llm_connections", _) | ("evaluators", _) | ("notification_channels", _) => "name",
        ("org_sso_providers", _) => "display_name",
        _ => panic!(
            "sealed column {table}.{column} has no natural key in golden_tests/dump.rs: add it"
        ),
    }
}

/// What a restored golden directory must answer, as JSON (expected.json). `meta` is an open copy
/// of the restored `meta.db`; `data_dir` is the restored directory.
pub(super) async fn dump(meta: &MetaDb, secrets: &SecretBox, data_dir: &Path) -> Value {
    let pool = meta.reader();
    let (rows, columns) = rows(data_dir).await;
    let mut expected = Map::new();
    expected.insert("generator".to_owned(), json!(GENERATOR));
    expected.insert(
        "workspace_version".to_owned(),
        json!(env!("CARGO_PKG_VERSION")),
    );
    expected.insert("meta_format".to_owned(), json!(meta::META_FORMAT_VERSION));
    expected.insert("meta_schema".to_owned(), json!(meta::expected_version()));
    expected.insert("rows".to_owned(), rows);
    expected.insert("columns".to_owned(), columns);
    expected.insert("reads".to_owned(), reads(meta).await);
    expected.insert("secrets".to_owned(), secret_values(data_dir, secrets).await);
    expected.insert("auth".to_owned(), auth(meta).await);
    expected.insert(
        "credentials".to_owned(),
        json!({
            "password": OWNER_PASSWORD,
            "sk": INGEST_KEY,
            "pk": PUBLIC_KEY,
            "pat": PERSONAL_TOKEN,
            "session": SESSION_TOKEN,
        }),
    );
    let check = instance_settings::get_setting(pool, instance_settings::ENCRYPTION_CHECK)
        .await
        .expect("read the encryption check")
        .expect("the encryption check is recorded");
    expected.insert("encryption_check".to_owned(), json!(check));
    Value::Object(expected)
}

/// Every table of the restored `meta.db` except the migrations, with all rows in column order
/// (`ORDER BY` every column), and the column names of each table. BLOBs are lowercase hex.
async fn rows(data_dir: &Path) -> (Value, Value) {
    let mut connection = SqliteConnectOptions::new()
        .filename(data_dir.join("meta.db"))
        .connect()
        .await
        .expect("open the restored meta.db");
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' ORDER BY name",
    )
    .fetch_all(&mut connection)
    .await
    .expect("list the tables");
    let mut rows = Map::new();
    let mut columns = Map::new();
    for name in names {
        let info = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA table_info(\"{name}\")"
        )))
        .fetch_all(&mut connection)
        .await
        .expect("read the columns");
        let column_names: Vec<String> = info
            .iter()
            .map(|row| row.get::<String, _>("name"))
            .collect();
        let order: Vec<String> = (1..=column_names.len()).map(|n| n.to_string()).collect();
        let query = if order.is_empty() {
            format!("SELECT * FROM \"{name}\"")
        } else {
            format!("SELECT * FROM \"{name}\" ORDER BY {}", order.join(", "))
        };
        let fetched = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut connection)
            .await
            .expect("read the rows");
        let table_rows: Vec<Value> = fetched
            .iter()
            .map(|row| {
                Value::Array(
                    (0..row.columns().len())
                        .map(|index| cell(row, index))
                        .collect(),
                )
            })
            .collect();
        rows.insert(name.clone(), Value::Array(table_rows));
        columns.insert(name, json!(column_names));
    }
    connection.close().await.ok();
    (Value::Object(rows), Value::Object(columns))
}

/// One cell as JSON: NULL, integer, real, text, or lowercase hex for a BLOB.
pub(super) fn cell(row: &sqlx::sqlite::SqliteRow, index: usize) -> Value {
    let raw = row.try_get_raw(index).expect("a cell");
    if raw.is_null() {
        return Value::Null;
    }
    let name = raw.type_info().name().to_owned();
    match name.as_str() {
        "INTEGER" => json!(row.get::<i64, _>(index)),
        "REAL" => json!(row.get::<f64, _>(index)),
        "BLOB" => json!(hex::encode(row.get::<Vec<u8>, _>(index))),
        _ => json!(row.get::<String, _>(index)),
    }
}

/// What the reads of `dump` need to name ids by natural keys.
struct World<'a> {
    pool: &'a SqlitePool,
    org: OrgRow,
    /// Project slug by id (the projects the server still lists for the org).
    slugs: BTreeMap<String, String>,
    /// User email by id.
    emails: BTreeMap<Uuid, String>,
    /// `golden-p1`, which holds the project-level scenario rows.
    p1: ProjectRow,
}

/// A server enum or struct as the JSON its `Serialize` impl writes.
fn ser<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("a server value serializes")
}

/// The JSON of a stored JSON text column (a config, a mapping): parsed, so that key order and
/// spacing of the stored text do not matter.
fn json_text(text: &str) -> Value {
    serde_json::from_str(text).expect("a stored JSON column parses")
}

/// A `Debug`-named enum of the server as a lowercase word (for enums with no `Serialize`).
fn word<T: std::fmt::Debug>(value: &T) -> Value {
    json!(format!("{value:?}").to_lowercase())
}

/// The decoded reads of the store functions, keyed by natural keys (never by generated ids or
/// times). Each section names the function that produced it. A table with no read function in
/// the server has only `rows`:
///
/// - `email_verifications`: only `use_verification`, which consumes the row;
/// - `usage_notices`: only written (`notices.rs`) and counted in tests;
/// - `alert_rule_channels`, `deletion_traces`, `deletion_sessions`, `deletion_projects`,
///   `dataset_versions`, `dataset_items`, `experiment_items`: read through their parent's
///   section (`alert_rules`, `deletions`, `datasets`, `experiments`);
/// - `prompt_versions`, `prompt_labels`, `prompt_label_events`: read through `prompts`;
///   `prompt_settings`: the `prompts` section's protected labels;
/// - `annotation_queue_definitions`, `annotation_queue_items`: read through `annotation_queues`;
/// - `assistant_messages`: read through `assistant`; `org_assistant_settings`: its `settings`;
/// - `issue_events`, `issue_candidates`, `issue_source_days`, `issue_refits`,
///   `issue_source_channels`: read through `issues` (candidates only as counts by status);
/// - `issue_notifications`: only written (`issue_links::notify_source`, which also queues
///   deliveries) and counted in tests.
async fn reads(meta: &MetaDb) -> Value {
    let pool = meta.reader();
    let org = orgs::org_by_slug(pool, "golden-org")
        .await
        .expect("read the org")
        .expect("the golden org exists");
    let projects = projects::projects_of_org(pool, org.id)
        .await
        .expect("read the projects");
    let slugs = projects
        .iter()
        .map(|project| (project.id.to_string(), project.slug.clone()))
        .collect();
    let p1 = projects
        .iter()
        .find(|project| project.slug == "golden-p1")
        .expect("golden-p1 exists")
        .clone();
    let mut emails = BTreeMap::new();
    for email in OWNER_AND_MEMBER {
        let user = users::user_by_email(pool, email)
            .await
            .expect("read a user")
            .expect("the golden user exists");
        emails.insert(user.id, (*email).to_owned());
    }
    let world = World {
        pool,
        org,
        slugs,
        emails,
        p1,
    };
    let mut out = Map::new();
    for (table, value) in [
        ("users", users_section(&world).await),
        ("identities", identities_section(&world).await),
        ("orgs", orgs_section(&world)),
        ("org_members", members_section(&world).await),
        ("projects", projects_section(&projects)),
        (
            "project_members",
            overrides_section(&world, &projects).await,
        ),
        ("invites", invites_section(&world).await),
        ("password_resets", resets_section(&world).await),
        ("api_keys", keys_section(&world).await),
        ("personal_tokens", tokens_section(&world).await),
        ("web_sessions", sessions_section(&world).await),
        ("instance_settings", settings_section(&world).await),
        ("llm_connections", connections_section(&world).await),
        ("evaluators", evaluators_section(&world).await),
        ("online_rules", rules_section(&world).await),
        ("score_definitions", definitions_section(&world).await),
        ("saved_views", views_section(&world).await),
        ("notification_channels", channels_section(&world).await),
        ("alert_rules", alert_rules_section(&world).await),
        ("deletions", deletions_section(&world).await),
        ("eval_jobs", jobs_section(&world).await),
        ("backfills", backfills_section(&world).await),
        ("rule_spend", spend_section(&world).await),
        ("pending_traces", pending_section(&world).await),
        ("org_sso", sso_section(&world).await),
        ("org_entitlements", entitlements_section(&world).await),
        ("org_window_ends", window_section(&world).await),
        ("usage_daily", usage_section(&world).await),
        ("model_prices", prices_section(&world).await),
        ("datasets", datasets_section(&world).await),
        ("experiments", experiments_section(&world).await),
        ("dashboards", dashboards_section(&world).await),
        ("prompts", prompts_section(&world).await),
        ("agent_contexts", agent_contexts_section(&world).await),
        ("annotation_queues", queues_section(&world).await),
        ("assistant", assistant_section(&world).await),
        ("issues", issues_section(&world).await),
    ] {
        out.insert(table.to_owned(), value);
    }
    Value::Object(out)
}

const OWNER_AND_MEMBER: &[&str] = &["golden-owner@example.com", "golden-member@example.com"];

/// users: `users::user_by_email`.
async fn users_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for email in OWNER_AND_MEMBER {
        let user = users::user_by_email(world.pool, email)
            .await
            .expect("read a user")
            .expect("the golden user exists");
        map.insert(
            (*email).to_owned(),
            json!({
                "name": user.name,
                "email_verified": user.email_verified_at.is_some(),
                "email_verified_via": user.email_verified_via.map(users::EmailVerifiedVia::as_str),
                "has_password": user.password_hash.is_some(),
                "is_instance_admin": user.is_instance_admin,
                "disabled": user.disabled_at.is_some(),
            }),
        );
    }
    Value::Object(map)
}

/// identities: `identities::user_identities` (the providers of a user), by email.
async fn identities_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for (id, email) in &world.emails {
        let providers = identities::user_identities(world.pool, *id)
            .await
            .expect("read the identities");
        map.insert(email.clone(), json!(providers));
    }
    Value::Object(map)
}

/// orgs: `orgs::org_by_slug`.
fn orgs_section(world: &World<'_>) -> Value {
    json!({ "golden-org": { "name": world.org.name, "state": world.org.state } })
}

/// org_members: `members::org_members` (role by email).
async fn members_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for member in members::org_members(world.pool, world.org.id)
        .await
        .expect("read the members")
    {
        map.insert(member.user.email.clone(), ser(&member.role));
    }
    Value::Object(map)
}

/// projects: `projects::projects_of_org`, by slug.
fn projects_section(projects: &[ProjectRow]) -> Value {
    let mut map = Map::new();
    for project in projects {
        map.insert(
            project.slug.clone(),
            json!({
                "name": project.name,
                "state": project.state,
                "retention_days": project.retention_days,
                "trace_completion_timeout_s": project.trace_completion_timeout_s,
            }),
        );
    }
    Value::Object(map)
}

/// project_members: `members::project_overrides` (override by project slug, then email).
async fn overrides_section(world: &World<'_>, projects: &[ProjectRow]) -> Value {
    let mut map = Map::new();
    for project in projects {
        let overrides = members::project_overrides(world.pool, project.id)
            .await
            .expect("read the overrides");
        if overrides.is_empty() {
            continue;
        }
        let mut by_email = Map::new();
        for (user, role) in overrides {
            by_email.insert(world.emails[&user].clone(), word(&role));
        }
        map.insert(project.slug.clone(), Value::Object(by_email));
    }
    Value::Object(map)
}

/// invites: `invites::pending_invites` (role and inviter by invitee email).
async fn invites_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for pending in invites::pending_invites(world.pool, world.org.id, Clock::System.now_ns())
        .await
        .expect("read the invites")
    {
        map.insert(
            pending.invite.email.clone(),
            json!({
                "role": ser(&pending.invite.role),
                "invited_by": pending.invited_by.map(|inviter| inviter.email),
                "accepted": pending.invite.accepted_at.is_some(),
            }),
        );
    }
    let by_hash = invites::invite_by_hash(
        world.pool,
        &hash_token(INVITE_TOKEN),
        Clock::System.now_ns(),
    )
    .await
    .expect("read the invite by its token")
    .map(|(invite, org)| json!({ "email": invite.email, "org": org.slug }));
    json!({ "pending": map, "by_token": by_hash })
}

/// password_resets: `password_resets::valid_reset` (the user a live token resets), by the
/// plaintext's hash.
async fn resets_section(world: &World<'_>) -> Value {
    let user = password_resets::valid_reset(
        world.pool,
        &hash_token("golden-reset-token"),
        Clock::System.now_ns(),
    )
    .await
    .expect("read the reset");
    json!({ "golden-reset-token": user.map(|id| world.emails[&id].clone()) })
}

/// api_keys: `api_keys::api_key_by_hash`, by the plaintext's hash (`sk`, `pk`).
async fn keys_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for (label, plain) in [("sk", INGEST_KEY), ("pk", PUBLIC_KEY)] {
        let key = api_keys::api_key_by_hash(world.pool, &hash_token(plain))
            .await
            .expect("read a key")
            .expect("the golden key exists");
        map.insert(
            label.to_owned(),
            json!({
                "project": world.slugs[&key.key.project_id.to_string()],
                "kind": key.key.kind,
                "scopes": key.key.scopes.iter().map(word).collect::<Vec<_>>(),
                "name": key.key.name,
                "prefix": key.key.prefix,
                "active": key.active,
            }),
        );
    }
    Value::Object(map)
}

/// personal_tokens: `personal_tokens::token_by_hash`, by the plaintext's hash.
async fn tokens_section(world: &World<'_>) -> Value {
    let token = personal_tokens::token_by_hash(world.pool, &hash_token(PERSONAL_TOKEN))
        .await
        .expect("read a token")
        .expect("the golden token exists");
    json!({ "owner": {
        "user": world.emails[&token.user_id],
        "name": token.name,
        "prefix": token.prefix,
        "scopes": token.scopes.iter().map(word).collect::<Vec<_>>(),
        "expires": token.expires_at.is_some(),
    }})
}

/// web_sessions: `web_sessions::session_user`, by the cookie's hash.
async fn sessions_section(world: &World<'_>) -> Value {
    let session = web_sessions::session_user(
        world.pool,
        &hash_token(SESSION_TOKEN),
        Clock::System.now_ns(),
    )
    .await
    .expect("read a session")
    .expect("the golden session is live");
    json!({ "golden-owner@example.com": session.user.email })
}

/// instance_settings: `instance_settings::get_setting`.
async fn settings_section(world: &World<'_>) -> Value {
    let setting = instance_settings::get_setting(world.pool, "golden_setting")
        .await
        .expect("read a setting");
    json!({ "golden_setting": setting })
}

/// llm_connections: `connections::list_connections`, by name. The sealed values are in `secrets`.
async fn connections_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for row in connections::list_connections(world.pool, world.org.id)
        .await
        .expect("read the connections")
    {
        map.insert(
            row.name.clone(),
            json!({
                "adapter": row.adapter,
                "base_url": row.base_url,
                "api_key_hint": row.api_key_hint,
                "has_secret": !row.secret_enc.is_empty(),
                "has_extra_headers": row.extra_headers_enc.is_some(),
                "extra_header_names": row.extra_header_names,
                "default_model": row.default_model,
                "structured_output": row.structured_output,
                "max_concurrency": row.max_concurrency,
                "requests_per_minute": row.requests_per_minute,
                "status": row.status,
                "last_error": row.last_error,
                "last_tested_at": row.last_tested_at,
                "last_test_ok": row.last_test_ok,
            }),
        );
    }
    Value::Object(map)
}

/// The names of the golden connections and evaluators, by id.
async fn evaluator_names(world: &World<'_>) -> (BTreeMap<Uuid, String>, BTreeMap<Uuid, String>) {
    let connection_names = connections::list_connections(world.pool, world.org.id)
        .await
        .expect("read the connections")
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect();
    let evaluator_names = evaluators::list_evaluators(world.pool, world.p1.id)
        .await
        .expect("read the evaluators")
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect();
    (connection_names, evaluator_names)
}

/// evaluators (with evaluator_versions): `evaluators::list_evaluators` and
/// `list_evaluator_versions`, by name. The sealed webhook document is in `secrets`.
async fn evaluators_section(world: &World<'_>) -> Value {
    let (connection_names, _) = evaluator_names(world).await;
    let mut map = Map::new();
    for row in evaluators::list_evaluators(world.pool, world.p1.id)
        .await
        .expect("read the evaluators")
    {
        // The judge's connection id is generated: `connection` below names it.
        let mut config = json_text(&row.config);
        if let Some(object) = config.as_object_mut() {
            object.remove("connection_id");
        }
        let versions: Vec<Value> =
            evaluators::list_evaluator_versions(world.pool, world.p1.id, row.id, None, 100)
                .await
                .expect("read the evaluator versions")
                .into_iter()
                .map(|version| {
                    let mut config = json_text(&version.config);
                    if let Some(object) = config.as_object_mut() {
                        object.remove("connection_id");
                    }
                    json!({
                        "version": version.version,
                        "name": version.name,
                        "kind": version.kind,
                        "config": config,
                        "output_score": json_text(&version.output_score),
                        "created_by": version.created_by.map(|id| world.emails.get(&id).cloned()),
                    })
                })
                .collect();
        map.insert(
            row.name.clone(),
            json!({
                "kind": row.kind,
                "config": config,
                "output_score": json_text(&row.output_score),
                "connection": row.connection_id.map(|id| connection_names[&id].clone()),
                "has_secret": row.secret_enc.is_some(),
                "version": row.version,
                "versions": versions,
            }),
        );
    }
    Value::Object(map)
}

/// online_rules: `online_rules::list_rules`, by name.
async fn rules_section(world: &World<'_>) -> Value {
    let (_, evaluator_names) = evaluator_names(world).await;
    let mut map = Map::new();
    for row in online_rules::list_rules(world.pool, world.p1.id)
        .await
        .expect("read the rules")
    {
        map.insert(
            row.name.clone(),
            json!({
                "evaluator": evaluator_names[&row.evaluator_id],
                "target": row.target,
                "filter": row.filter,
                "variable_mapping": json_text(&row.variable_mapping),
                "sample_rate": row.sample_rate,
                "budget_usd": row.budget_usd,
                "budget_period": row.budget_period,
                "enabled": row.enabled,
                "paused_reason": row.paused_reason,
                "paused_detail": row.paused_detail,
            }),
        );
    }
    Value::Object(map)
}

/// score_definitions: `score_definitions::list_definitions` (archived included), by name.
async fn definitions_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for row in score_definitions::list_definitions(world.pool, world.p1.id, true)
        .await
        .expect("read the score definitions")
    {
        map.insert(
            row.name.clone(),
            json!({
                "data_type": ser(&row.data_type),
                "categories": ser(&row.categories),
                "min_value": row.min_value,
                "max_value": row.max_value,
                "description": row.description,
                "direction": ser(&row.direction),
                "archived": row.archived_at.is_some(),
            }),
        );
    }
    Value::Object(map)
}

/// saved_views: `saved_views::list_visible` as the owner, by name.
async fn views_section(world: &World<'_>) -> Value {
    let owner = world
        .emails
        .iter()
        .find(|(_, email)| email.as_str() == "golden-owner@example.com")
        .map(|(id, _)| *id)
        .expect("the owner is known");
    let page = saved_views::list_visible(
        world.pool,
        world.p1.id,
        saved_views::Viewer::User(owner),
        None,
        100,
        None,
    )
    .await
    .expect("read the saved views");
    let mut map = Map::new();
    for row in page.rows {
        map.insert(
            row.name.clone(),
            json!({
                "kind": ser(&row.kind),
                "state": row.state,
                "owner": row.owner_user_id.map(|id| world.emails[&id].clone()),
                "shared": row.shared,
                "is_default": row.is_default,
            }),
        );
    }
    Value::Object(map)
}

/// notification_channels: `alerts::list_channels`, by name. The sealed config is in `secrets`.
async fn channels_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for row in alerts::list_channels(world.pool, world.p1.id)
        .await
        .expect("read the channels")
    {
        map.insert(
            row.name.clone(),
            json!({ "kind": ser(&row.kind), "target_hint": row.target_hint }),
        );
    }
    Value::Object(map)
}

/// alert_rules (with alert_rule_channels, alert_events and notification_deliveries):
/// `alerts::list_rules`, `alerts::list_events` and `alerts::deliveries_for_events`, by rule name.
async fn alert_rules_section(world: &World<'_>) -> Value {
    let channel_names: BTreeMap<Uuid, String> = alerts::list_channels(world.pool, world.p1.id)
        .await
        .expect("read the channels")
        .into_iter()
        .map(|row| (row.id, row.name))
        .collect();
    let mut map = Map::new();
    for rule in alerts::list_rules(world.pool, world.p1.id)
        .await
        .expect("read the alert rules")
    {
        let events = alerts::list_events(world.pool, rule.id, None, 100)
            .await
            .expect("read the alert events");
        let event_ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let deliveries = alerts::deliveries_for_events(world.pool, &event_ids)
            .await
            .expect("read the deliveries");
        let events: Vec<Value> = events
            .iter()
            .map(|event| {
                json!({
                    "kind": ser(&event.kind),
                    "value": event.value,
                    "count": event.count,
                    "threshold": event.threshold,
                    "details": ser(&event.details),
                    "deliveries": deliveries
                        .iter()
                        .filter(|delivery| delivery.event_id == event.id)
                        .map(|delivery| json!({
                            "channel": delivery.channel_name,
                            "channel_kind": ser(&delivery.channel_kind),
                            "status": ser(&delivery.status),
                            "attempts": delivery.attempts,
                            "has_next_attempt": delivery.next_attempt_at.is_some(),
                            "last_error": delivery.last_error,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut channels: Vec<String> = rule
            .channel_ids
            .iter()
            .map(|id| channel_names[id].clone())
            .collect();
        channels.sort();
        map.insert(
            rule.name.clone(),
            json!({
                "condition": ser(&rule.condition),
                "channels": channels,
                "enabled": rule.enabled,
                "cooldown_s": rule.cooldown_s,
                "notify_while_firing": rule.notify_while_firing,
                "state": ser(&rule.state),
                "clear_streak": rule.clear_streak,
                "last_value": rule.last_value,
                "last_count": rule.last_count,
                "last_error": rule.last_error,
                "events": events,
            }),
        );
    }
    Value::Object(map)
}

/// deletions (with deletion_traces, deletion_sessions, deletion_projects):
/// `deletions::all_deletions`, `scope_traces`, `scope_sessions`, `scope_projects`, by kind (the
/// scenario has one deletion of each). The `project` deletion on `golden-p2` may be applied by
/// the deletion worker before the backup: its state is whatever the backup holds, and the check
/// compares a restore with that.
async fn deletions_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for listed in deletions::all_deletions(world.pool)
        .await
        .expect("read the deletions")
    {
        let row = listed.expect("a golden deletion decodes");
        let traces = deletions::scope_traces(world.pool, row.id)
            .await
            .expect("read the traces of a deletion");
        let sessions = deletions::scope_sessions(world.pool, row.id)
            .await
            .expect("read the sessions of a deletion");
        let projects = deletions::scope_projects(world.pool, row.id)
            .await
            .expect("read the projects of a deletion");
        map.insert(
            row.kind.as_str().to_owned(),
            json!({
                "project": row.project_id.and_then(|id| world.slugs.get(&id.to_string()).cloned()),
                "state": row.state.as_str(),
                "user_id": row.user_id,
                "request": ser(&row.request),
                "actor_kind": row.actor_kind.as_str(),
                "actor": world.emails.get(&row.actor_id),
                "trace_count": row.trace_count,
                "session_count": row.session_count,
                "handed_over": row.handed_over_at.is_some(),
                "applied": row.applied_at.is_some(),
                "error": row.error.map(deletions::FailReason::as_str),
                "traces": traces.iter().map(cetra_core::ids::TraceId::to_hex).collect::<Vec<_>>(),
                "sessions": sessions,
                "projects": projects
                    .iter()
                    .map(|id| world.slugs.get(&id.to_string()).cloned())
                    .collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(map)
}

/// eval_jobs: `eval_jobs::list_jobs`, by job kind (the scenario has one of each).
async fn jobs_section(world: &World<'_>) -> Value {
    let (_, evaluator_names) = evaluator_names(world).await;
    let mut map = Map::new();
    for row in eval_jobs::list_jobs(
        world.pool,
        world.p1.id,
        &eval_jobs::JobFilter::default(),
        None,
        100,
    )
    .await
    .expect("read the jobs")
    {
        map.insert(
            row.kind.clone(),
            json!({
                "priority": row.priority,
                "evaluator": evaluator_names[&row.evaluator_id],
                "has_rule": row.rule_id.is_some(),
                "trace_id": row.trace_id.to_hex(),
                "span_id": row.span_id.map(|span| span.to_hex()),
                "status": row.status,
                "attempts": row.attempts,
                "claims": row.claims,
                "load_retries": row.load_retries,
                "interruptions": row.interruptions,
                "error_class": row.error_class,
                "error": row.error,
                "judge_trace_id": row.judge_trace_id.map(|trace| trace.to_hex()),
                "has_score": row.score_id.is_some(),
                "evaluator_version": row.evaluator_version,
            }),
        );
    }
    Value::Object(map)
}

/// backfills: `backfills::list_backfills`, by rule name.
async fn backfills_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for rule in online_rules::list_rules(world.pool, world.p1.id)
        .await
        .expect("read the rules")
    {
        let rows = backfills::list_backfills(world.pool, rule.id)
            .await
            .expect("read the backfills");
        map.insert(
            rule.name.clone(),
            Value::Array(
                rows.iter()
                    .map(|row| {
                        json!({
                            "max_targets": row.max_targets,
                            "sample_rate": row.sample_rate,
                            "status": row.status,
                            "matched": row.matched,
                            "enqueued": row.enqueued,
                            "error": row.error,
                            "finished": row.finished_at.is_some(),
                        })
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(map)
}

/// rule_spend: `rule_spend::spent_between` over every day, by rule name.
async fn spend_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for rule in online_rules::list_rules(world.pool, world.p1.id)
        .await
        .expect("read the rules")
    {
        let spent = rule_spend::spent_between(world.pool, rule.id, 0, i64::MAX)
            .await
            .expect("read the spend");
        map.insert(rule.name.clone(), json!(spent));
    }
    Value::Object(map)
}

/// pending_traces: `pending_traces::load_pending` (rows of active projects of active orgs), by
/// project slug.
async fn pending_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for row in pending_traces::load_pending(world.pool)
        .await
        .expect("read the pending traces")
    {
        let slug = world.slugs[&row.project_id.to_string()].clone();
        let entry = map.entry(slug).or_insert_with(|| json!([]));
        entry
            .as_array_mut()
            .expect("an array")
            .push(json!({ "trace_id": row.trace_id.to_hex(), "root_ended": row.root_ended }));
    }
    Value::Object(map)
}

/// org_sso_providers and org_sso_domains: `org_sso::store::provider` and `domains`, by org slug.
/// The sealed client secret is in `secrets`.
async fn sso_section(world: &World<'_>) -> Value {
    let provider = sso::provider(world.pool, world.org.id)
        .await
        .expect("read the provider")
        .map(|row| {
            json!({
                "display_name": row.display_name,
                "issuer": row.issuer,
                "client_id": row.client_id,
                "has_secret": !row.client_secret_enc.is_empty(),
                "enabled": row.enabled,
                "enforced": row.enforced,
                "jit_role": ser(&row.jit_role),
                "has_logged_in": row.last_login_at.is_some(),
            })
        });
    let mut domains = Map::new();
    for row in sso::domains(world.pool, world.org.id)
        .await
        .expect("read the domains")
    {
        domains.insert(
            row.domain.clone(),
            json!({
                "token": row.token,
                "verified": row.verified_at.is_some(),
                "checked": row.last_checked_at.is_some(),
            }),
        );
    }
    json!({ "golden-org": { "provider": provider, "domains": domains } })
}

/// org_entitlements: `meta::entitlements::all_tokens`, each verified by
/// `entitlements::tokens::grant_from_token` (what the token grants), by org slug.
async fn entitlements_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for stored in entitlements::all_tokens(world.pool)
        .await
        .expect("read the tokens")
    {
        let (org, grant) = crate::entitlements::tokens::grant_from_token(
            &stored.token,
            PublicUrl::default().origin(),
            &crate::entitlements::Limits::default(),
        )
        .expect("the golden token verifies");
        assert_eq!(org, stored.org_id, "the token is for its row's org");
        let limits = grant.limits;
        map.insert(
            "golden-org".to_owned(),
            json!({
                "issued_at": grant.issued_at,
                "expires_at": grant.expires_at,
                "grace_ends_at": grant.grace_ends_at,
                "plan": { "id": grant.plan.id, "name": grant.plan.name, "add_ons": grant.plan.add_ons },
                "features": grant.features.to_vec().iter().map(|feature| feature.name()).collect::<Vec<_>>(),
                "limits": {
                    "units_included": limits.units_included,
                    "units_cap": limits.units_cap,
                    "ingest_spans_per_sec": limits.ingest_spans_per_sec,
                    "ingest_burst": limits.ingest_burst,
                    "data_access_days": limits.data_access_days,
                    "max_seats": limits.max_seats,
                    "max_projects": limits.max_projects,
                    "max_alert_rules": limits.max_alert_rules,
                    "max_active_online_rules": limits.max_active_online_rules,
                },
            }),
        );
    }
    Value::Object(map)
}

/// org_window_ends: `meta::entitlements::window_ends_since` (the data-access days of each window
/// that ended, newest first), by org slug. The end times are generated and stay in `rows`.
async fn window_section(world: &World<'_>) -> Value {
    let windows = entitlements::window_ends_since(world.pool, world.org.id, 0)
        .await
        .expect("read the windows");
    json!({ "golden-org": windows.iter().map(|(_, days)| *days).collect::<Vec<_>>() })
}

/// usage_daily: `meta::usage::org_days` (per day, summed over the projects), by org slug.
async fn usage_section(world: &World<'_>) -> Value {
    let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("a valid date");
    let days = usage::org_days(world.pool, world.org.id, day, day)
        .await
        .expect("read the usage");
    json!({ "golden-org": ser(&days) })
}

/// dashboards: `dashboards::list` and `get` (custom, by name) and `get_overview` (the
/// project's Overview, under `overview`).
async fn dashboards_section(world: &World<'_>) -> Value {
    let row_json = |row: dashboards::DashboardRow| {
        json!({
            "description": row.info.description,
            "version": row.info.version,
            "created_by": row.info.created_by.map(|id| world.emails.get(&id).cloned()),
            "updated_by": row.info.updated_by.map(|id| world.emails.get(&id).cloned()),
            "definition": ser(&row.definition),
        })
    };
    let mut custom = Map::new();
    let page = dashboards::list(world.pool, world.p1.id, 100, None)
        .await
        .expect("read the dashboards");
    for info in page.rows {
        let row = dashboards::get(world.pool, world.p1.id, info.id)
            .await
            .expect("read a dashboard")
            .expect("a listed dashboard exists");
        custom.insert(info.name.clone(), row_json(row));
    }
    let overview = dashboards::get_overview(world.pool, world.p1.id)
        .await
        .expect("read the overview")
        .map(|row| {
            let name = row.info.name.clone();
            let mut value = row_json(row);
            value["name"] = json!(name);
            value
        });
    json!({ "custom": custom, "overview": overview })
}

/// model_prices: `model_prices::list_all`, by model name. Ids and times stay in `rows`.
async fn prices_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for record in model_prices::list_all(world.pool)
        .await
        .expect("read the prices")
    {
        map.insert(
            record.row.model_name.clone(),
            json!({
                "org": world.org.id == record.org_id,
                "project": record.project_id.and_then(|id| world.slugs.get(&id.to_string()).cloned()),
                "price": ser(&record.row),
            }),
        );
    }
    Value::Object(map)
}

/// datasets (with dataset_versions and dataset_items): `datasets::list_datasets`,
/// `list_versions` and `list_items` at the current version, by dataset name.
async fn datasets_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for dataset in datasets::list_datasets(world.pool, world.p1.id, None)
        .await
        .expect("read the datasets")
    {
        let versions = datasets::list_versions(world.pool, dataset.id, None, 100)
            .await
            .expect("read the versions");
        let items = datasets::list_items(
            world.pool,
            dataset.id,
            dataset.current_version,
            None,
            100,
            None,
        )
        .await
        .expect("read the items");
        map.insert(
            dataset.name.clone(),
            json!({
                "description": dataset.description,
                "input_schema": dataset.input_schema,
                "expected_schema": dataset.expected_schema,
                "current_version": dataset.current_version,
                "item_count": dataset.item_count,
                "experiment_count": dataset.experiment_count,
                "versions": versions.iter().map(|version| json!({
                    "version": version.version,
                    "description": version.description,
                    "created_by": version.created_by,
                })).collect::<Vec<_>>(),
                "items": items.iter().map(|item| json!({
                    "input": item.input,
                    "expected_output": item.expected_output,
                    "metadata": item.metadata,
                    "has_source_trace": item.source_trace_id.is_some(),
                    "valid_from_version": item.valid_from_version,
                    "valid_to_version": item.valid_to_version,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(map)
}

/// experiments (with experiment_items): `experiments::list` and `experiments::results_page`, by
/// experiment name.
async fn experiments_section(world: &World<'_>) -> Value {
    let (rows, _) = experiments::list(world.pool, world.p1.id, None, None, 100, None)
        .await
        .expect("read the experiments");
    let mut map = Map::new();
    for experiment in rows {
        let results = experiments::results_page(world.pool, experiment.id, None, false, 100)
            .await
            .expect("read the results");
        map.insert(
            experiment.name.clone(),
            json!({
                "dataset": experiment.dataset_name,
                "dataset_version": experiment.dataset_version,
                "description": experiment.description,
                "metadata": json_text(&experiment.metadata),
                "repetitions": experiment.repetitions,
                "item_count": experiment.item_count,
                "result_count": experiment.result_count,
                "status": experiment.status,
                "error": experiment.error,
                "created_by": experiment.created_by,
                "results": results.iter().map(|result| json!({
                    "repetition": result.repetition,
                    "input": json_text(&result.input),
                    "expected_output": result.expected_output.as_deref().map(json_text),
                    "has_trace": result.trace_id.is_some(),
                    "output": result.output.as_deref().map(json_text),
                    "error": result.error,
                    "latency_ns": result.latency_ns,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(map)
}

/// The plaintext of every sealed value, keyed `<table>.<column>.<natural key>`, opened with the
/// golden key and the row's own AAD.
pub(super) async fn secret_values(data_dir: &Path, secrets: &SecretBox) -> Value {
    let mut connection = SqliteConnectOptions::new()
        .filename(data_dir.join("meta.db"))
        .connect()
        .await
        .expect("open the restored meta.db");
    let mut out = Map::new();
    for (table, column) in crypto::ENCRYPTED_COLUMNS {
        let key = natural_key(table, column);
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, {key} AS natural_key, {column} AS sealed FROM {table} WHERE {column} IS NOT NULL"
        )))
        .fetch_all(&mut connection)
        .await
        .expect("read the sealed column");
        assert!(
            !rows.is_empty(),
            "sealed column {table}.{column} has no row in the golden scenario: add one in golden_tests/build.rs"
        );
        for row in rows {
            let id: String = row.get("id");
            let natural: String = row.get("natural_key");
            let sealed: Vec<u8> = row.get("sealed");
            let id = Uuid::parse_str(&id).expect("a row id");
            let plain = secrets
                .open(
                    &sealed,
                    Aad {
                        table,
                        column,
                        row_id: &id,
                    },
                )
                .expect("a golden secret opens with the golden key");
            out.insert(
                format!("{table}.{column}.{natural}"),
                json!(String::from_utf8(plain).expect("a golden secret is text")),
            );
        }
    }
    connection.close().await.ok();
    Value::Object(out)
}

/// What each recorded credential resolves to through the server's own verification functions.
async fn auth(meta: &MetaDb) -> Value {
    let pool = meta.reader();
    let hasher = Hasher::new().await.expect("the password hasher");
    let owner = users::user_by_email(pool, "golden-owner@example.com")
        .await
        .expect("read the owner")
        .expect("the owner exists");
    let password = hasher
        .verify(OWNER_PASSWORD, owner.password_hash.as_deref())
        .await
        .expect("verify the password");
    let project_of = |id| async move {
        projects::project_by_id(pool, id)
            .await
            .expect("read a project")
            .map(|project| project.slug)
    };
    let sk = api_keys::api_key_by_hash(pool, &hash_token(INGEST_KEY))
        .await
        .expect("read the key")
        .expect("the key exists");
    let pk = api_keys::api_key_by_hash(pool, &hash_token(PUBLIC_KEY))
        .await
        .expect("read the key")
        .expect("the key exists");
    let pat = personal_tokens::token_by_hash(pool, &hash_token(PERSONAL_TOKEN))
        .await
        .expect("read the token")
        .expect("the token exists");
    let pat_user = users::user_by_id(pool, pat.user_id)
        .await
        .expect("read the token's user")
        .expect("the token's user exists");
    let session =
        web_sessions::session_user(pool, &hash_token(SESSION_TOKEN), Clock::System.now_ns())
            .await
            .expect("read the session")
            .expect("the session is live");
    json!({
        "password": password.ok,
        "sk": project_of(sk.key.project_id).await,
        "pk": project_of(pk.key.project_id).await,
        "pat": pat_user.email,
        "session": session.user.email,
    })
}

/// The email of a user id, when the user is a golden one.
fn email_of(world: &World<'_>, id: Uuid) -> Option<String> {
    world.emails.get(&id).cloned()
}

/// prompts (with prompt_versions, prompt_labels, prompt_label_events and prompt_settings):
/// `prompts::list_prompts`, `list_versions`, `list_label_events` and `protected_labels`, by
/// prompt name.
async fn prompts_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for prompt in prompts::list_prompts(world.pool, world.p1.id, &prompts::PromptFilter::default())
        .await
        .expect("read the prompts")
    {
        let versions = prompts::list_versions(world.pool, prompt.id, None, 100)
            .await
            .expect("read the versions");
        let events = prompts::list_label_events(world.pool, prompt.id, None, 100)
            .await
            .expect("read the label events");
        map.insert(
            prompt.name.clone(),
            json!({
                "type": prompt.prompt_type.as_str(),
                "description": prompt.description,
                "tags": prompt.tags,
                "latest_version": prompt.latest_version,
                "created_by": prompt.created_by,
                "labels": prompt.labels.iter().map(|label| json!({
                    "label": label.label,
                    "version": label.version,
                    "updated_by": label.updated_by,
                })).collect::<Vec<_>>(),
                "versions": versions.iter().map(|version| json!({
                    "version": version.version,
                    "template": json_text(&version.template),
                    "config": json_text(&version.config),
                    "commit_message": version.commit_message,
                    "labels": version.labels,
                    "created_by": version.created_by,
                })).collect::<Vec<_>>(),
                "label_events": events.iter().map(|event| json!({
                    "label": event.label,
                    "from_version": event.from_version,
                    "to_version": event.to_version,
                    "actor": event.actor,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    let protected = prompts::protected_labels(world.pool, world.p1.id)
        .await
        .expect("read the protected labels");
    json!({ "prompts": map, "protected_labels": protected })
}

/// agent_contexts: `agent_contexts::load_contexts`, by project slug, agent and session.
async fn agent_contexts_section(world: &World<'_>) -> Value {
    let mut map = Map::new();
    for row in agent_contexts::load_contexts(world.pool)
        .await
        .expect("read the agent contexts")
    {
        let slug = world.slugs.get(&row.project_id.to_string());
        map.insert(
            format!(
                "{}/{}/{}",
                slug.map_or("", String::as_str),
                row.agent,
                row.session_id
            ),
            json!({
                "repository": row.repository,
                "branch": row.branch,
                "user_id": row.user_id,
            }),
        );
    }
    Value::Object(map)
}

/// annotation_queues (with annotation_queue_definitions and annotation_queue_items):
/// `annotation_queues::list`, `counts` and `list_items`, by queue name.
async fn queues_section(world: &World<'_>) -> Value {
    let (queues, _) = annotation_queues::list(world.pool, world.p1.id, None, 100, None, None)
        .await
        .expect("read the queues");
    let ids: Vec<Uuid> = queues.iter().map(|queue| queue.id).collect();
    let counts = annotation_queues::counts(world.pool, world.p1.id, &ids, None)
        .await
        .expect("read the counts");
    let mut map = Map::new();
    for queue in queues {
        let (items, _) = annotation_queues::list_items(world.pool, queue.id, None, None, 100, None)
            .await
            .expect("read the items");
        let count = counts.get(&queue.id).cloned().unwrap_or_default();
        map.insert(
            queue.name.clone(),
            json!({
                "description": queue.description,
                "instructions": queue.instructions,
                "target": queue.target.as_str(),
                "definitions": queue.definition_ids.len(),
                "version": queue.version,
                "created_by": queue.created_by.and_then(|id| email_of(world, id)),
                "counts": {
                    "pending": count.pending,
                    "completed": count.completed,
                    "skipped": count.skipped,
                },
                "items": items.iter().map(|item| json!({
                    "trace_id": item.trace_id,
                    "span_id": item.span_id,
                    "session_id": item.session_id,
                    "status": item.status.as_str(),
                    "added_by_kind": item.added_by_kind.as_str(),
                    "added_by": email_of(world, item.added_by_id),
                })).collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(map)
}

/// assistant (with assistant_conversations, assistant_messages and org_assistant_settings):
/// `assistant::get_settings`, `list_conversations` and `get_conversation_with_messages`, as the
/// owner, by conversation title.
async fn assistant_section(world: &World<'_>) -> Value {
    let owner = world
        .emails
        .iter()
        .find(|(_, email)| email.as_str() == "golden-owner@example.com")
        .map(|(id, _)| *id)
        .expect("the owner is a golden user");
    let settings = assistant::get_settings(world.pool, world.org.id)
        .await
        .expect("read the settings")
        .map(|row| {
            json!({
                "enabled": row.enabled,
                "has_connection": row.connection_id.is_some(),
                "model": row.model,
                "updated_by": row.updated_by.and_then(|id| email_of(world, id)),
            })
        });
    let mut conversations = Map::new();
    for row in assistant::list_conversations(world.pool, world.p1.id, owner)
        .await
        .expect("read the conversations")
    {
        let (conversation, messages) =
            assistant::get_conversation_with_messages(world.pool, world.p1.id, owner, row.id)
                .await
                .expect("read a conversation")
                .expect("a listed conversation exists");
        conversations.insert(
            conversation.title.clone(),
            json!({
                "message_count": conversation.message_count,
                "messages": messages.iter().map(|message| json!({
                    "seq": message.seq,
                    "role": message.role.as_str(),
                    "parts": json_text(&message.parts),
                    "context": message.context.as_deref().map(json_text),
                    "status": message.status.as_str(),
                    "feedback": message.feedback.map(|feedback| feedback.as_str()),
                    "has_turn_trace": message.turn_trace_id.is_some(),
                })).collect::<Vec<_>>(),
            }),
        );
    }
    json!({ "settings": settings, "conversations": conversations })
}

/// issues (with issue_events, issue_candidates, issue_source_days, issue_refits and
/// issue_source_channels): `issues::list_sources`, `list_issues`, `list_events`,
/// `candidate_counts`, `spent_between`, `list_refits` and `issue_links::source_channel_ids`, by
/// source name, then issue title.
async fn issues_section(world: &World<'_>) -> Value {
    let all = issues::list_issues(
        world.pool,
        world.p1.id,
        &issues::IssueListQuery {
            states: &[],
            source: None,
            q: None,
            ids: None,
            sort: issues::IssueSort::LastSeen,
            cursor: None,
            limit: 100,
        },
    )
    .await
    .expect("read the issues");
    let mut map = Map::new();
    for source in issues::list_sources(world.pool, world.p1.id)
        .await
        .expect("read the sources")
    {
        let mut by_title = Map::new();
        for issue in all.iter().filter(|issue| issue.source_id == source.id) {
            let events = issues::list_events(world.pool, issue.id, 100)
                .await
                .expect("read the events");
            by_title.insert(
                issue.title.clone(),
                json!({
                    "fingerprint": issue.fingerprint,
                    "state": issue.state.as_str(),
                    "regressed": issue.regressed,
                    "centroid": issue.centroid,
                    "member_count": issue.member_count,
                    "events": events.iter().map(|event| json!({
                        "kind": event.kind.as_str(),
                        "actor": event.actor_user_id.and_then(|id| email_of(world, id)),
                        "from": event.from_value,
                        "to": event.to_value,
                    })).collect::<Vec<_>>(),
                }),
            );
        }
        let counts = issues::candidate_counts(world.pool, source.id, 0)
            .await
            .expect("read the candidate counts");
        let spent = issues::spent_between(world.pool, source.id, 0, i64::MAX)
            .await
            .expect("read the spend");
        let refits = issues::list_refits(world.pool, source.id, 100)
            .await
            .expect("read the refits");
        let channels = issue_links::source_channel_ids(world.pool, source.id)
            .await
            .expect("read the channels");
        map.insert(
            source.name.clone(),
            json!({
                "kind": source.kind.as_str(),
                "enabled": source.enabled,
                "sample_rate": source.sample_rate,
                "daily_cap": source.daily_cap,
                "chat_model": source.chat_model,
                "embed_model": source.embed_model,
                "embed_dimension": source.embed_dimension,
                "floor": source.floor,
                "min_issue_size": source.min_issue_size,
                "created_by": source.created_by.and_then(|id| email_of(world, id)),
                "channels": channels.len(),
                "issues": by_title,
                "candidates": {
                    "queued": counts.queued,
                    "pending": counts.pending,
                    "assigned": counts.assigned,
                    "dropped": counts.dropped,
                    "failed": counts.failed,
                    "skipped": counts.skipped,
                },
                "spent_usd": spent,
                "refits": refits.iter().map(|refit| json!({
                    "trigger": refit.trigger,
                    "finished": refit.finished_at.is_some(),
                    "pending": refit.pending,
                    "clusters": refit.clusters,
                    "created": refit.created,
                    "joined": refit.joined,
                    "outliers": refit.outliers,
                    "error_class": refit.error_class,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(map)
}
