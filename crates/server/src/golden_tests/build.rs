//! Writes the golden scenario: a data directory filled through the store functions, served once,
//! and backed up through the admin socket (SF6, D-R-38). Only the surfaces the server's routes use
//! write rows here; the byte-level secret format is SF3's and the restore is SF7's.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};
use sqlx::SqliteConnection;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

use super::{
    GOLDEN_KEY, INGEST_KEY, INVITE_TOKEN, OWNER_PASSWORD, PERSONAL_TOKEN, PUBLIC_KEY,
    SESSION_TOKEN, row_counts,
};
use crate::admin_socket::{ADMIN_PROTOCOL, AdminEnvelope, AdminRequest, decode_response};
use crate::alerts::channel::{ChannelConfig, seal_config};
use crate::api::account::OrgRole;
use crate::api::alerts::AlertCondition;
use crate::api::dashboards::Definition;
use crate::api::evaluators::EvaluatorKind;
use crate::api::org_sso::JitRole;
use crate::api::saved_views::SavedViewKind;
use crate::api::score_definitions::ScoreDirection;
use crate::auth::password::Hasher;
use crate::auth::roles::{KeyScope, ProjectOverride};
use crate::auth::secret::hash_token;
use crate::clock::Clock;
use crate::crypto::{self, Aad, SecretBox};
use crate::ee::audit::stream_store::{self, StreamConfig};
use crate::ee::org_sso::store as sso;
use crate::ee::scim::store::{self as scim, ScimUserFields};
use crate::meta::{MetaDb, MetaError};
use crate::meta::{
    agent_contexts::{self, AgentContextRow},
    alerts::{self, Attempt, Evaluation, NewChannel, NewRule},
    annotation_queues::{self, AddedByKind, NewItem, NewQueue, QueueCreate, Target},
    api_keys::{self, NewApiKey},
    assistant::{self, AssistantSettingsRow, MessageRole, MessageStatus, NewMessage},
    backfills::{self, BackfillEnd, NewBackfill},
    connections::{self, ConnectionFields},
    dashboards::{self, NewDashboard},
    datasets::{self, DatasetFields, NewRevision, VersionChange},
    deletions::{self, ActorKind, DeletionKind, DeletionState, NewDeletion, RequestDetails},
    email_verifications, entitlements,
    eval_jobs::{self, Finish, JobUsage, NewJob},
    evaluators::{self, EvaluatorFields},
    experiments::{self, NewExperiment, NewResult},
    identities, instance_settings, invites, issue_links,
    issues::{
        self, CandidateRow, CandidateStatus, CandidateTarget, EventKind, EventRow, IssueRow,
        IssueState, RefitRow, SourceKind, SourceRow,
    },
    members::{self},
    model_prices,
    online_rules::{self, NewRule as NewOnlineRule, RuleFields},
    orgs, password_resets,
    pending_traces::{self, PendingRow},
    personal_tokens::{self, NewToken},
    projects,
    prompts::{self, NewPrompt, NewVersion, PromptType},
    rule_spend,
    saved_views::{self, NewSavedView},
    score_definitions::{self, DefinitionCategory, DefinitionFields},
    users::{self, NewUser},
    web_sessions::{self, NewSession},
};
use crate::test_support;
use cetra_core::enums::ScoreDataType;
use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
use cetra_eval::OutputScore;
use cetra_otel::{PriceRow, PriceScope};

/// One call on the admin socket: the request line as sent (with `to` replaced) and the response.
#[derive(Debug, Clone)]
pub(super) struct SocketLine {
    pub(super) request: Value,
    pub(super) response: Value,
}

/// The day, nanoseconds: the rule spend and the window end use days and days' start.
const DAY_NS: i64 = 86_400_000_000_000;
/// Far-future expiry of sessions and tokens, so the golden directory keeps signing in for years
/// (50 years in nanoseconds, from `now`).
const FAR_NS: i64 = 50 * 365 * DAY_NS;
/// The replacement of every backup target in `admin_socket.jsonl` (paths differ per machine).
pub(super) const TARGET: &str = "/golden/out";

/// The golden org's SCIM token (`scim-cetra-` and 32 tail characters).
const SCIM_TOKEN: &str = "scim-cetra-GoldenScimTokenABCDEFGHIJKLMNOPQ";

/// Writes the golden scenario into `out` (must not exist): a data directory filled through the
/// store functions, served once, and backed up through the admin socket into `out`, which also
/// receives `admin_socket.jsonl`. Returns the socket lines.
pub(super) async fn build(out: &Path) -> Vec<SocketLine> {
    let temp = tempfile::tempdir().expect("a temp dir for the golden data directory");
    let data = temp.path().join("data");
    fs::create_dir_all(&data).expect("create the golden data directory");
    crypto::write_key_file(&crypto::key_path(&data), &GOLDEN_KEY).expect("write secret.key");

    let meta = MetaDb::open(&data).await.expect("open meta.db");
    let secrets = SecretBox::new(&GOLDEN_KEY);
    crypto::verify_or_record(&meta, &secrets, &crypto::key_path(&data))
        .await
        .expect("record the encryption check");

    let now = Clock::System.now_ns();
    let hasher = Hasher::new().await.expect("the password hasher");
    let owner_hash = hasher
        .hash(OWNER_PASSWORD)
        .await
        .expect("hash the owner's password");
    let scenario = Scenario::hashed(owner_hash, now);
    meta.write(move |conn| Box::pin(async move { populate(conn, &scenario).await }))
        .await
        .expect("write the scenario");

    // The row counts before bind and in the backup: the startup tasks may empty a table, and
    // build.rs must keep each table non-empty (EMPTY_TABLES).
    let before = row_counts(&data.join("meta.db")).await;
    meta.close().await;

    let running = crate::serve::bind(test_support::config(&data), None)
        .await
        .expect("bind the golden server");

    let target = out.to_path_buf();
    let mut lines = vec![
        send_backup(&data, &target, false).await,
        send_backup(&data, &target, false).await,
    ];
    lines.push(
        send_raw(
            &data,
            json!({"v": ADMIN_PROTOCOL, "command": "backup", "to": "relative"}),
        )
        .await,
    );
    lines.push(send_raw(&data, json!({"v": ADMIN_PROTOCOL, "command": "restore"})).await);
    lines.push(send_raw(&data, json!({"command": "backup", "to": "/nowhere"})).await);
    running
        .shutdown()
        .await
        .expect("shut the golden server down");
    // The answers the check replays: the first backup works, the second finds its target
    // non-empty, and the three refusals name their codes.
    let expected_codes = [
        None,
        Some("target_not_empty"),
        Some("bad_request"),
        Some("unknown_command"),
        Some("unsupported_protocol"),
    ];
    for (line, code) in lines.iter().zip(expected_codes) {
        assert_eq!(
            line.response["ok"],
            json!(code.is_none()),
            "socket answer {}",
            line.request
        );
        assert_eq!(
            line.response.get("code").and_then(Value::as_str),
            code,
            "socket answer {}",
            line.request
        );
    }

    let after = row_counts(&out.join("meta.db")).await;
    for (table, count) in &before {
        let kept = after
            .iter()
            .find(|(name, _)| name == table)
            .map_or(0, |(_, count)| *count);
        if kept != *count {
            eprintln!("golden: {table} {count} rows before bind, {kept} in the backup");
        }
    }

    write_socket_lines(out, &lines, &target);
    lines
}

/// Sends one backup request as the CLI does: an `AdminEnvelope` line, answered by a line that
/// `decode_response` reads. The request is recorded as sent.
async fn send_backup(data: &Path, to: &Path, meta_only: bool) -> SocketLine {
    let line = serde_json::to_string(&AdminEnvelope {
        v: ADMIN_PROTOCOL,
        request: AdminRequest::Backup {
            to: to.to_path_buf(),
            meta_only,
        },
    })
    .expect("the request serializes");
    let response = exchange(data, &line).await;
    decode_response(&response).expect("the server speaks this protocol");
    SocketLine {
        request: serde_json::from_str(&line).expect("the request is JSON"),
        response: serde_json::from_str(&response).expect("the response is JSON"),
    }
}

/// Sends a hand-written request line (for the refusals) and records it.
async fn send_raw(data: &Path, request: Value) -> SocketLine {
    let response = exchange(data, &request.to_string()).await;
    SocketLine {
        request,
        response: serde_json::from_str(&response).expect("the response is JSON"),
    }
}

/// One connection: the request line, then the answer line. The server closes the connection.
pub(super) async fn exchange(data: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(data.join(crate::admin_socket::ADMIN_SOCKET))
        .await
        .expect("connect to admin.sock");
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write the request");
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
        .expect("read the response");
    response.trim_end().to_owned()
}

/// Writes `admin_socket.jsonl`: one `{request, response}` object per line, with the backup target
/// replaced by `/golden/out` and the times of the report redacted.
fn write_socket_lines(out: &Path, lines: &[SocketLine], target: &Path) {
    let mut text = String::new();
    for line in lines {
        let mut request = line.request.clone();
        redact_target(&mut request, target);
        let mut response = line.response.clone();
        if let Some(report) = response.get_mut("report").and_then(Value::as_object_mut) {
            report.insert("created_at".to_owned(), json!("<redacted>"));
            report.insert("duration_ms".to_owned(), json!(0));
            report.insert("dir".to_owned(), json!(TARGET));
        }
        text.push_str(&json!({"request": request, "response": response}).to_string());
        text.push('\n');
    }
    fs::write(out.join("admin_socket.jsonl"), text).expect("write admin_socket.jsonl");
}

/// Replaces the `to` of a backup request whose value is `target` by [`TARGET`].
fn redact_target(request: &mut Value, target: &Path) {
    if let Some(to) = request.get_mut("to")
        && to.as_str() == target.to_str()
    {
        *to = json!(TARGET);
    }
}

/// The values the scenario writes, fixed except the ids and the times.
struct Scenario {
    owner_hash: String,
    now: i64,
}

impl Scenario {
    fn hashed(owner_hash: String, now: i64) -> Self {
        Self { owner_hash, now }
    }
}

/// The seal of a secret column of the row `id` (table and column names as in
/// `crypto::ENCRYPTED_COLUMNS`).
fn seal(
    secrets: &SecretBox,
    table: &'static str,
    column: &'static str,
    id: &Uuid,
    plain: &[u8],
) -> Vec<u8> {
    secrets
        .seal(
            plain,
            Aad {
                table,
                column,
                row_id: id,
            },
        )
        .expect("seal a golden secret")
}

/// What an evaluator route receives for one evaluator.
struct PreparedInput<'a> {
    id: Uuid,
    project: ProjectId,
    name: &'a str,
    kind: EvaluatorKind,
    config: Value,
    output: OutputScore,
    webhook_secret: Option<&'a str>,
    created_by: Uuid,
}

/// Validates and stores one evaluator as the evaluator routes do: `eval::evaluator::prepare`
/// builds the config, output score and webhook document; the webhook document is sealed to the
/// row.
async fn insert_prepared(
    conn: &mut SqliteConnection,
    secrets: &SecretBox,
    input: PreparedInput<'_>,
    now: i64,
) -> Result<(), MetaError> {
    let prepared = crate::eval::evaluator::prepare(
        input.id,
        input.name,
        input.kind,
        &input.config,
        &input.output,
        input.webhook_secret,
        "",
    )
    .unwrap_or_else(|errors| panic!("the golden evaluator {} is valid: {errors:?}", input.name));
    let secret_enc = prepared.secrets.as_ref().map(|document| {
        seal(
            secrets,
            "evaluators",
            "secret_enc",
            &input.id,
            &serde_json::to_vec(document).expect("the webhook document serializes"),
        )
    });
    evaluators::insert_evaluator(
        conn,
        input.id,
        input.project,
        input.kind.as_str(),
        EvaluatorFields {
            name: prepared.evaluator.name.clone(),
            config: prepared.config_json,
            output_score: prepared.output_json,
            connection_id: prepared.connection_id,
            secret_enc,
            version: 1,
        },
        Some(input.created_by),
        now,
    )
    .await
    .map(|_| ())
}

/// Every table of the golden scenario, through its store functions (build.rs's table list in the
/// report). Times are the clock's; ids are generated.
async fn populate(conn: &mut SqliteConnection, s: &Scenario) -> Result<(), MetaError> {
    let now = s.now;
    let secrets = SecretBox::new(&GOLDEN_KEY);

    // Identity and tenancy.
    let owner = users::insert_user(
        conn,
        NewUser {
            email: "golden-owner@example.com".to_owned(),
            name: Some("Golden Owner".to_owned()),
            password_hash: Some(s.owner_hash.clone()),
            email_verified_at: Some(now),
            email_verified_via: Some(users::EmailVerifiedVia::MailToken),
            is_instance_admin: true,
        },
        now,
    )
    .await?;
    let member = users::insert_user(
        conn,
        NewUser {
            email: "golden-member@example.com".to_owned(),
            name: Some("Golden Member".to_owned()),
            password_hash: None,
            email_verified_at: None,
            email_verified_via: None,
            is_instance_admin: false,
        },
        now,
    )
    .await?;
    identities::insert_identity(
        conn,
        member.id,
        "google",
        "golden-subject-1",
        "golden-member@example.com",
        now,
    )
    .await?;
    email_verifications::replace_verification(
        conn,
        member.id,
        &hash_token("golden-verify-token"),
        now,
        now + FAR_NS,
    )
    .await?;
    password_resets::insert_reset(
        conn,
        &hash_token("golden-reset-token"),
        member.id,
        now,
        now + FAR_NS,
    )
    .await?;
    web_sessions::insert_session(
        conn,
        NewSession {
            hash: hash_token(SESSION_TOKEN),
            user_id: owner.id,
            user_agent: Some("golden-agent".to_owned()),
            ip: None,
            sso_org_id: None,
            via_provider: false,
        },
        now,
        now + FAR_NS,
    )
    .await?;

    let org = orgs::insert_org(conn, OrgId::new_v7(), "Golden Org", "golden-org", now).await?;
    members::add_org_member(conn, org.id, owner.id, OrgRole::Owner, now).await?;
    members::add_org_member(conn, org.id, member.id, OrgRole::Member, now).await?;
    invites::replace_invite(
        conn,
        org.id,
        "golden-invitee@example.com",
        OrgRole::Member,
        &hash_token(INVITE_TOKEN),
        owner.id,
        now,
        now + FAR_NS,
    )
    .await?;

    let p1 =
        projects::insert_project(conn, org.id, "Golden One", "golden-p1", Some(30), now).await?;
    let p2 = projects::insert_project(conn, org.id, "Golden Two", "golden-p2", None, now).await?;
    members::set_project_override(conn, p1.id, member.id, ProjectOverride::Viewer).await?;

    // Keys and tokens: the plaintexts are fixed, so the hashes and prefixes are computed here.
    for (plain, kind, scopes) in [
        (INGEST_KEY, "secret", vec![KeyScope::Ingest, KeyScope::Read]),
        (PUBLIC_KEY, "public", vec![KeyScope::Feedback]),
    ] {
        api_keys::insert_api_key(
            conn,
            NewApiKey {
                project_id: p1.id,
                kind,
                scopes,
                name: Some("golden key".to_owned()),
                prefix: crate::auth::secret::display_prefix(plain),
                hash: hash_token(plain),
                created_by: owner.id,
                expires_at: None,
            },
            now,
        )
        .await?;
    }
    personal_tokens::insert_token(
        conn,
        NewToken {
            user_id: owner.id,
            name: "golden token".to_owned(),
            prefix: crate::auth::secret::display_prefix(PERSONAL_TOKEN),
            hash: hash_token(PERSONAL_TOKEN),
            scopes: vec![KeyScope::Read, KeyScope::Write],
            expires_at: None,
        },
        now,
    )
    .await?;
    instance_settings::put_setting(conn, "golden_setting", "1").await?;

    // LLM connections, evaluators, online rules and score definitions.
    let conn_id = Uuid::now_v7();
    connections::insert_connection(
        conn,
        conn_id,
        org.id,
        ConnectionFields {
            name: "golden-llm".to_owned(),
            adapter: "openai".to_owned(),
            base_url: Some("https://llm.example.com/v1".to_owned()),
            secret_enc: seal(
                &secrets,
                "llm_connections",
                "secret_enc",
                &conn_id,
                b"golden-secret",
            ),
            // The key has fewer than 16 characters: the route stores no hint (`key_hint`).
            api_key_hint: String::new(),
            extra_headers_enc: Some(seal(
                &secrets,
                "llm_connections",
                "extra_headers_enc",
                &conn_id,
                br#"{"x-golden":"golden-header"}"#,
            )),
            extra_header_names: vec!["x-golden".to_owned()],
            default_model: Some("gpt-4o".to_owned()),
            structured_output: "native".to_owned(),
            max_concurrency: 4,
            requests_per_minute: 300,
        },
        Some(owner.id),
        now,
    )
    .await?;

    // Evaluators of each kind, prepared as the evaluator routes do (`eval::evaluator::prepare`):
    // the stored config, output score and sealed webhook document are the server's own.
    let judge_id = Uuid::now_v7();
    insert_prepared(
        conn,
        &secrets,
        PreparedInput {
            id: judge_id,
            project: p1.id,
            name: "Golden judge",
            kind: EvaluatorKind::LlmJudge,
            config: json!({
                "connection_id": conn_id,
                "model": "gpt-4o",
                "prompt": "Rate how relevant the answer is.\n\nQuestion: {{ input }}\nAnswer: {{ output }}",
                "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                "include_reason": true,
                "temperature": null,
                "max_output_tokens": 1024,
                "timeout_ms": 60000
            }),
            output: OutputScore {
                name: "relevance".to_owned(),
                data_type: ScoreDataType::Numeric,
                categories: None,
            },
            webhook_secret: None,
            created_by: owner.id,
        },
        now,
    )
    .await?;
    insert_prepared(
        conn,
        &secrets,
        PreparedInput {
            id: Uuid::now_v7(),
            project: p1.id,
            name: "Golden check",
            kind: EvaluatorKind::Check,
            config: json!({"check": "contains", "value": "refund"}),
            output: OutputScore {
                name: "mentions_refund".to_owned(),
                data_type: ScoreDataType::Boolean,
                categories: None,
            },
            webhook_secret: None,
            created_by: owner.id,
        },
        now,
    )
    .await?;
    insert_prepared(
        conn,
        &secrets,
        PreparedInput {
            id: Uuid::now_v7(),
            project: p1.id,
            name: "Golden webhook",
            kind: EvaluatorKind::Webhook,
            config: json!({
                "url": "https://hooks.example.com/private/path?token=query-secret",
                "headers": {"X-Team": "golden-header-value"}
            }),
            output: OutputScore {
                name: "hook".to_owned(),
                data_type: ScoreDataType::Numeric,
                categories: None,
            },
            webhook_secret: Some("golden-webhook-secret"),
            created_by: owner.id,
        },
        now,
    )
    .await?;

    let rule = online_rules::insert_rule(
        conn,
        NewOnlineRule {
            id: Uuid::now_v7(),
            project_id: p1.id,
            evaluator_id: judge_id,
            target: "trace".to_owned(),
            created_by: Some(owner.id),
        },
        RuleFields {
            name: "golden rule".to_owned(),
            filter: Some("status = 'ok'".to_owned()),
            variable_mapping: r#"{"input":"trace.input","output":"trace.output"}"#.to_owned(),
            sample_rate: 0.25,
            budget_usd: Some(5.0),
            budget_period: Some("day".to_owned()),
            enabled: true,
        },
        now,
    )
    .await?;
    let mut accuracy_definition = None;
    for (kind, data_type) in [
        ("accuracy", ScoreDataType::Numeric),
        ("passed", ScoreDataType::Boolean),
        ("verdict", ScoreDataType::Categorical),
        ("notes", ScoreDataType::Text),
    ] {
        let definition_id = Uuid::now_v7();
        if kind == "accuracy" {
            accuracy_definition = Some(definition_id);
        }
        score_definitions::insert_definition(
            conn,
            definition_id,
            p1.id,
            data_type,
            DefinitionFields {
                name: kind.to_owned(),
                categories: if data_type == ScoreDataType::Categorical {
                    vec![
                        DefinitionCategory {
                            label: "good".to_owned(),
                            value: Some(1.0),
                        },
                        DefinitionCategory {
                            label: "bad".to_owned(),
                            value: Some(0.0),
                        },
                    ]
                } else {
                    Vec::new()
                },
                min_value: (data_type == ScoreDataType::Numeric).then_some(0.0),
                max_value: (data_type == ScoreDataType::Numeric).then_some(1.0),
                description: Some(format!("golden {kind}")),
                direction: if data_type == ScoreDataType::Numeric {
                    ScoreDirection::Higher
                } else {
                    ScoreDirection::None
                },
                archived_at: None,
            },
            now,
        )
        .await?;
    }

    // Saved views.
    saved_views::insert_view(
        conn,
        NewSavedView {
            project_id: p1.id,
            kind: SavedViewKind::Traces,
            name: "golden view".to_owned(),
            state: Map::from_iter([("filter".to_owned(), json!("name = 'golden'"))]),
            owner_user_id: owner.id,
            shared: true,
            is_default: false,
        },
        now,
    )
    .await?;

    // Dashboards: one custom dashboard and the project's stored Overview (kind 'overview').
    let definition: Definition = serde_json::from_value(json!({"widgets": [
        {
            "id": "heading", "title": "Traffic",
            "layout": {"x": 0, "y": 0, "w": 60, "h": 1},
            "kind": "heading"
        },
        {
            "id": "requests", "title": "Requests",
            "layout": {"x": 0, "y": 1, "w": 6, "h": 4},
            "kind": "chart", "chart": {"type": "line"},
            "query": {
                "source": "traces",
                "measures": [{"agg": "count", "label": "Requests"}],
                "breakdown": {"field": "models", "limit": 5}
            }
        }
    ]}))
    .expect("the golden dashboard definition parses");
    dashboards::insert(
        conn,
        NewDashboard {
            project_id: p1.id,
            name: "golden dashboard".to_owned(),
            description: Some("golden".to_owned()),
            definition: definition.clone(),
            user: owner.id,
        },
        now,
    )
    .await?;
    dashboards::save_overview(conn, p1.id, None, &definition, owner.id, now).await?;

    // Alerts: one channel of each kind, a rule on two of them, one event with a delivered and a
    // failed delivery (the dispatcher leaves finished deliveries alone).
    let webhook_channel = Uuid::now_v7();
    let slack_channel = Uuid::now_v7();
    let email_channel = Uuid::now_v7();
    for (id, name, config) in [
        (
            webhook_channel,
            "golden webhook",
            ChannelConfig::Webhook {
                url: "https://hooks.example.com/alert".to_owned(),
                secret: "golden-alert-secret-123".to_owned(),
                headers: BTreeMap::from([("X-Team".to_owned(), "golden".to_owned())]),
            },
        ),
        (
            slack_channel,
            "golden slack",
            ChannelConfig::Slack {
                url: "https://hooks.slack.com/services/T000/B000/golden0000".to_owned(),
            },
        ),
        (
            email_channel,
            "golden email",
            ChannelConfig::Email {
                recipients: vec!["golden-owner@example.com".to_owned()],
            },
        ),
    ] {
        alerts::insert_channel(
            conn,
            NewChannel {
                id,
                project_id: p1.id,
                kind: config.kind(),
                name: name.to_owned(),
                config_enc: seal_config(&secrets, &id, &config).expect("seal a channel config"),
                target_hint: config.target_hint(),
            },
            now,
        )
        .await?;
    }
    let alert_rule = alerts::insert_rule(
        conn,
        NewRule {
            project_id: p1.id,
            name: "golden alert".to_owned(),
            condition: golden_condition(),
            channel_ids: vec![webhook_channel, slack_channel],
            enabled: true,
            cooldown_s: 300,
            notify_while_firing: false,
            created_by: Some(owner.id),
        },
        now,
    )
    .await?;
    alerts::record_evaluation(
        conn,
        alert_rule.id,
        alert_rule.updated_at,
        Evaluation::Measured {
            step: crate::alerts::evaluate::step(
                &alert_rule,
                crate::alerts::evaluate::Reading::Breaching,
                now,
            ),
            count: 25,
            value: Some(0.5),
            threshold: 0.2,
            window_from: now - DAY_NS,
            window_to: now,
            details: alerts::EventDetails {
                rule_name: "golden alert".to_owned(),
                metric: golden_condition().metric,
                comparator: golden_condition().comparator,
                window: golden_condition().window,
                filter: None,
            },
        },
        now,
    )
    .await?;
    let deliveries: Vec<String> =
        sqlx::query_scalar("SELECT id FROM notification_deliveries ORDER BY channel_id")
            .fetch_all(&mut *conn)
            .await?;
    let [sent, failed] = deliveries.as_slice() else {
        panic!("the golden event has two deliveries");
    };
    alerts::finish_delivery(
        conn,
        sent.parse().expect("a delivery id"),
        Attempt::Sent,
        now,
    )
    .await?;
    alerts::finish_delivery(
        conn,
        failed.parse().expect("a delivery id"),
        Attempt::Failed {
            error: "golden failure".to_owned(),
        },
        now,
    )
    .await?;

    // Deletions: one of each kind, walked through the lifecycle the worker walks
    // (`resolving` -> `pending` -> `applied`, or `failed`): `trace_ids` and `user` applied,
    // `filter` and `org` failed, `project` pending. The pending project deletion is on `golden-p2`,
    // which the deletion worker may apply at startup before the backup is taken (it is a race
    // the golden directory records whichever way it goes). A pending or resolving org deletion
    // would drop the org, and an applied one makes the usage pruner delete the org's usage:
    // `org` is a failed one, which the worker leaves alone.
    let deletion_specs = [
        (DeletionKind::TraceIds, Some(p1.id), None),
        (DeletionKind::Filter, Some(p1.id), None),
        (
            DeletionKind::User,
            Some(p1.id),
            Some("golden-member@example.com"),
        ),
        (DeletionKind::Project, Some(p2.id), None),
        (DeletionKind::Org, None, None),
    ];
    for (index, (kind, project, user)) in deletion_specs.into_iter().enumerate() {
        let id = Uuid::from_u128(0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_d000 + index as u128);
        deletions::insert_deletion(
            conn,
            &NewDeletion {
                id,
                org_id: org.id,
                project_id: project,
                kind,
                user_id: user.map(str::to_owned),
                request: RequestDetails::default(),
                actor_kind: ActorKind::User,
                actor_id: owner.id,
                state: if kind == DeletionKind::Project {
                    DeletionState::Pending
                } else {
                    DeletionState::Resolving
                },
                created_at: now,
            },
        )
        .await?;
        match kind {
            DeletionKind::TraceIds => {
                deletions::insert_traces(conn, id, &[trace_id(0xde)]).await?;
                deletions::store_counts(conn, id, 1, None).await?;
                deletions::mark_handed_over(conn, id, now).await?;
                deletions::mark_applied(conn, id, now).await?;
            }
            DeletionKind::Filter => {
                deletions::mark_failed(conn, id, deletions::FailReason::TooManyTraces).await?;
            }
            DeletionKind::User => {
                deletions::insert_sessions(conn, id, &["golden-session".to_owned()]).await?;
                deletions::store_counts(conn, id, 0, Some(1)).await?;
                deletions::mark_handed_over(conn, id, now).await?;
                deletions::mark_applied(conn, id, now).await?;
            }
            DeletionKind::Project => deletions::insert_projects(conn, id, &[p2.id]).await?,
            DeletionKind::Org => {
                deletions::mark_failed(conn, id, deletions::FailReason::ResolutionFailed).await?;
            }
        }
    }

    // Entitlements and usage.
    entitlements::put_token(
        conn,
        &entitlements::StoredToken {
            org_id: org.id,
            token: test_support::org_token(
                org.id,
                "2026-01-01T00:00:00Z",
                "2076-01-01T00:00:00Z",
                "2076-01-01T00:00:00Z",
                &["retention_management", "project_roles", "org_sso"],
                &json!({"units_included": 1000, "data_access_days": 30}),
            ),
            token_id: Uuid::now_v7(),
            issued_at: now,
            received_at: now,
        },
    )
    .await?;
    entitlements::record_window_end(conn, org.id, now - DAY_NS, Some(30)).await?;
    // The SQL of `usage::Usage::flush` (the flush's upsert), for one fixed day; and of
    // `entitlements::notices::notify_org`, for one notice.
    sqlx::query(
        "INSERT INTO usage_daily (org_id, project_id, day, spans, bytes, scores, \
         judge_input_tokens, judge_output_tokens, judge_cost_usd) \
         VALUES (?, ?, ?, 10, 2048, 3, 100, 50, 0.25)",
    )
    .bind(org.id.to_string())
    .bind(p1.id.to_string())
    .bind("2026-01-01")
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO usage_notices (org_id, month, threshold, sent_at) VALUES (?, ?, 80, ?)",
    )
    .bind(org.id.to_string())
    .bind("2026-11")
    .bind(now)
    .execute(&mut *conn)
    .await?;
    // One audit record, through `audit::record` (track EA).
    crate::audit::record(
        conn,
        &crate::audit::Ctx::for_session_user(
            owner.id,
            Some(std::net::IpAddr::from([127, 0, 0, 1])),
            Some("golden"),
        ),
        crate::audit::Event::new(crate::audit::catalog::MemberAction::RoleChanged {
            old_role: "viewer",
            new_role: "member",
        })
        .org(org.id)
        .target(crate::audit::TargetKind::User, owner.id),
        now,
    )
    .await?;

    // One SIEM stream destination of the golden org (track EA M4), its config sealed to the row.
    let stream_id = Uuid::now_v7();
    let stream_config = StreamConfig {
        url: "https://siem.example.com/ingest".to_owned(),
        secret: "golden-stream-secret".to_owned(),
        header: None,
    };
    stream_store::insert(
        conn,
        stream_id,
        Some(org.id),
        "siem.example.com",
        &stream_store::seal(&secrets, &stream_id, &stream_config).expect("seal a golden secret"),
        now,
    )
    .await?;

    // Queue rows in states the startup tasks leave alone: finished eval jobs, a failed backfill,
    // rule spend, a pending trace seen just now.
    for (index, rule_id) in [None, Some(rule.id)].into_iter().enumerate() {
        let job_id = Uuid::now_v7();
        eval_jobs::insert_job(
            conn,
            NewJob {
                id: job_id,
                project_id: p1.id,
                kind: if index == 0 { "manual" } else { "rule" },
                rule_id,
                evaluator_id: judge_id,
                variable_mapping: None,
                trace_id: trace_id(0x0a + index as u8),
                span_id: Some(span_id(7)),
                trace_start_ns: now,
            },
            now,
        )
        .await?;
        let claims = eval_jobs::claim(conn, job_id, now)
            .await?
            .expect("a queued job is claimable");
        let finish = if index == 0 {
            Finish::Done {
                judge_trace_id: trace_id(0x11),
                score_id: Uuid::now_v7(),
                evaluator_version: 1,
                usage: JobUsage::default(),
            }
        } else {
            Finish::Failed {
                class: "judge_error",
                error: "golden failure".to_owned(),
                judge_trace_id: None,
                evaluator_version: None,
                usage: JobUsage::default(),
            }
        };
        eval_jobs::finish(conn, job_id, claims, finish, now).await?;
    }

    let backfill = backfills::insert_backfill(
        conn,
        NewBackfill {
            id: Uuid::now_v7(),
            rule_id: rule.id,
            from_ns: now - DAY_NS,
            to_ns: now,
            max_targets: 100,
            sample_rate: 1.0,
            created_by: Some(owner.id),
        },
        now,
    )
    .await?;
    backfills::finish_backfill(
        conn,
        backfill.id,
        BackfillEnd::Failed("golden failure"),
        now,
    )
    .await?;

    // The day is UTC midnight in nanoseconds (`add_spend`); the eval pruner deletes days before
    // 62 days ago.
    rule_spend::add_spend(conn, rule.id, now / DAY_NS * DAY_NS, 0.125).await?;
    pending_traces::write_pending(
        conn,
        &[],
        &[PendingRow {
            project_id: p1.id,
            trace_id: trace_id(0x22),
            first_seen: now,
            last_seen: now,
            min_start: now,
            root_ended: false,
        }],
    )
    .await?;

    // SSO: a provider with a sealed client secret and a verified domain.
    let sso_id = Uuid::now_v7();
    sso::put_provider(
        conn,
        org.id,
        sso_id,
        sso::ProviderWrite {
            display_name: "Golden SSO".to_owned(),
            issuer: "https://idp.example.com".to_owned(),
            client_id: "golden-client".to_owned(),
            client_secret_enc: Some(seal(
                &secrets,
                "org_sso_providers",
                "client_secret_enc",
                &sso_id,
                b"golden-sso-secret",
            )),
            jit_role: JitRole::Viewer,
        },
        now,
    )
    .await?;
    sso::insert_domain(
        conn,
        &sso::DomainRow {
            id: Uuid::now_v7(),
            org_id: org.id,
            domain: "example.com".to_owned(),
            token: "golden-domain-token".to_owned(),
            verified_at: Some(now),
            last_checked_at: Some(now),
            created_at: now,
        },
    )
    .await?;

    // SAML: a second org whose provider is saved from manual fields, with a verified domain.
    let saml_org =
        orgs::insert_org(conn, OrgId::new_v7(), "Golden SAML", "golden-saml", now).await?;
    sso::put_saml_provider(
        conn,
        saml_org.id,
        Uuid::now_v7(),
        sso::SamlWrite {
            display_name: "Golden SAML".to_owned(),
            jit_role: JitRole::Viewer,
            metadata_url: Some("https://idp.golden.test/metadata".to_owned()),
            idp_entity_id: "https://idp.golden.test/metadata".to_owned(),
            sso_url: "https://idp.golden.test/sso".to_owned(),
            certificates: vec![
                cetra_saml::test_idp::TestKey::Idp
                    .certificate()
                    .der()
                    .to_vec(),
            ],
            attributes: sso::SamlAttributes {
                email: Some("mail".to_owned()),
                ..Default::default()
            },
            refreshed_at: Some(now),
        },
        now,
    )
    .await?;
    sso::insert_domain(
        conn,
        &sso::DomainRow {
            id: Uuid::now_v7(),
            org_id: saml_org.id,
            domain: "golden-saml.example.com".to_owned(),
            token: "golden-saml-domain-token".to_owned(),
            verified_at: Some(now),
            last_checked_at: Some(now),
            created_at: now,
        },
    )
    .await?;

    // Datasets, one version with one item, an experiment with one result, and a model price.
    let dataset_id = Uuid::now_v7();
    datasets::insert_dataset(
        conn,
        dataset_id,
        p1.id,
        DatasetFields {
            name: "golden-dataset".to_owned(),
            description: Some("golden".to_owned()),
            input_schema: None,
            expected_schema: None,
        },
        now,
    )
    .await?;
    let revision_id = Uuid::now_v7();
    let item_id = Uuid::now_v7();
    let version = datasets::write_version(
        conn,
        dataset_id,
        VersionChange {
            close: Vec::new(),
            insert: vec![NewRevision {
                revision_id,
                item_id,
                input: json!({"question": "golden question"}),
                expected_output: Some(json!("golden answer")),
                metadata: Map::new(),
                source_trace_id: None,
                source_span_id: None,
            }],
            description: "golden v1".to_owned(),
            created_by: Some("golden-owner@example.com".to_owned()),
        },
        now,
    )
    .await?;
    let experiment_id = Uuid::now_v7();
    experiments::insert_tx(
        conn,
        &NewExperiment {
            id: experiment_id,
            project_id: p1.id,
            dataset_id,
            dataset_name: "golden-dataset".to_owned(),
            dataset_version: version,
            name: "golden experiment".to_owned(),
            description: None,
            metadata: "{}".to_owned(),
            repetitions: 1,
            item_count: 1,
            created_by: Some("golden-owner@example.com".to_owned()),
            created_at: now,
        },
    )
    .await?;
    experiments::upsert_results_tx(
        conn,
        experiment_id,
        &[NewResult {
            dataset_item_id: item_id,
            repetition: 1,
            revision_id,
            input: r#"{"question":"golden question"}"#.to_owned(),
            expected_output: Some(r#""golden answer""#.to_owned()),
            trace_id: None,
            output: Some(r#""golden""#.to_owned()),
            error: None,
            latency_ns: Some(1000),
        }],
        now,
    )
    .await?;
    model_prices::insert_tx(
        conn,
        Uuid::now_v7(),
        org.id,
        None,
        &PriceRow {
            scope: PriceScope::Org,
            model_name: "golden-model".to_owned(),
            match_pattern: "golden-model".to_owned(),
            provider: Some("golden".to_owned()),
            valid_from: 0,
            price_input: Some(1.0),
            price_output: Some(2.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        },
        now,
    )
    .await?;

    populate_workspace(
        conn,
        &Workspace {
            now,
            org: org.id,
            project: p1.id,
            owner: owner.id,
            connection: conn_id,
            channel: webhook_channel,
            definition: accuracy_definition.expect("the accuracy definition is inserted"),
        },
    )
    .await?;

    // SCIM provisioning (EA M5): the org's settings, one token, one provisioned member and one
    // account that SCIM disabled. Fixed plaintexts, so the hashes and prefixes are computed here.
    scim::put_org_scim(conn, org.id, true, OrgRole::Viewer, now).await?;
    scim::insert_token(
        conn,
        org.id,
        "golden okta",
        &crate::auth::secret::display_prefix(SCIM_TOKEN),
        &hash_token(SCIM_TOKEN),
        owner.id,
        now,
    )
    .await?;
    let scim_member = users::insert_user(
        conn,
        NewUser {
            email: "golden-scim@example.com".to_owned(),
            name: Some("Golden SCIM".to_owned()),
            password_hash: None,
            email_verified_at: None,
            email_verified_via: None,
            is_instance_admin: false,
        },
        now,
    )
    .await?;
    members::add_org_member(conn, org.id, scim_member.id, OrgRole::Viewer, now).await?;
    let scim_row = scim::insert_user(
        conn,
        org.id,
        scim_member.id,
        &ScimUserFields {
            user_name: "golden-scim@example.com".to_owned(),
            external_id: Some("golden-external-1".to_owned()),
            given_name: Some("Golden".to_owned()),
            family_name: Some("SCIM".to_owned()),
            display_name: Some("Golden SCIM".to_owned()),
            department: None,
            employee_number: None,
        },
        now,
    )
    .await?;
    let disabled = users::insert_user(
        conn,
        NewUser {
            email: "golden-scim-off@example.com".to_owned(),
            name: None,
            password_hash: None,
            email_verified_at: None,
            email_verified_via: None,
            is_instance_admin: false,
        },
        now,
    )
    .await?;
    sqlx::query(
        "INSERT INTO scim_disabled_accounts (user_id, org_id, disabled_at) VALUES (?, ?, ?)",
    )
    .bind(disabled.id.to_string())
    .bind(org.id.to_string())
    .bind(now)
    .execute(&mut *conn)
    .await?;

    // SCIM groups (EA M6): one group with the provisioned member, mapped to an org role and to
    // one project.
    let group = scim::insert_group(conn, org.id, "Golden group", None, now).await?;
    scim::add_group_members_tx(conn, org.id, group.id, &[scim_row.id]).await?;
    scim::put_mapping(
        conn,
        group.id,
        &scim::GroupMapping {
            org_role: Some(OrgRole::Member),
            projects: vec![(p1.id, ProjectOverride::Viewer)],
        },
        now,
    )
    .await?;

    Ok(())
}

/// The ids the later tracks' rows refer to.
struct Workspace {
    now: i64,
    org: OrgId,
    project: ProjectId,
    owner: Uuid,
    connection: Uuid,
    channel: Uuid,
    definition: Uuid,
}

/// Prompts, agent contexts, annotation queues, the assistant and issues, each in a state the
/// startup tasks leave alone: assigned candidates, a finished refit, an assigned-source topic
/// that is not due.
async fn populate_workspace(conn: &mut SqliteConnection, w: &Workspace) -> Result<(), MetaError> {
    let now = w.now;
    let owner_email = "golden-owner@example.com";

    // Prompts: one prompt with two versions, a label moved once (two label events), and the
    // project's protected labels.
    let prompt_id = Uuid::now_v7();
    prompts::create_prompt(
        conn,
        w.project,
        NewPrompt {
            id: prompt_id,
            name: "golden prompt".to_owned(),
            prompt_type: PromptType::Text,
            description: Some("golden".to_owned()),
            tags: vec!["golden".to_owned()],
        },
        NewVersion {
            template: r#""Answer {{question}}.""#.to_owned(),
            config: "{}".to_owned(),
            commit_message: Some("golden v1".to_owned()),
            labels: vec!["production".to_owned()],
            actor: Some(owner_email.to_owned()),
        },
        now,
    )
    .await?;
    prompts::add_version(
        conn,
        prompt_id,
        NewVersion {
            template: r#""Answer {{question}} briefly.""#.to_owned(),
            config: "{}".to_owned(),
            commit_message: Some("golden v2".to_owned()),
            labels: vec!["production".to_owned(), "staging".to_owned()],
            actor: Some(owner_email.to_owned()),
        },
        now,
    )
    .await?;
    prompts::set_protected_labels(
        conn,
        w.project,
        &["production".to_owned()],
        Some(owner_email),
        now,
    )
    .await?;

    // An agent context a coding-agent hook reported.
    agent_contexts::upsert_context(
        conn,
        &AgentContextRow {
            project_id: w.project,
            agent: "golden-agent".to_owned(),
            session_id: "golden-session".to_owned(),
            repository: Some("golden/repo".to_owned()),
            branch: Some("main".to_owned()),
            user_id: Some(owner_email.to_owned()),
            updated_at: now,
        },
    )
    .await?;

    // An annotation queue with one score definition and one pending trace item.
    let queue = annotation_queues::insert_queue_tx(
        conn,
        w.project,
        w.org,
        NewQueue {
            name: "golden queue".to_owned(),
            description: Some("golden".to_owned()),
            instructions: Some("Rate the answer.".to_owned()),
            target: Target::Trace,
            definition_ids: vec![w.definition],
            created_by: Some(w.owner),
        },
        None,
        now,
    )
    .await?;
    let QueueCreate::Created(queue) = queue else {
        panic!("the golden queue is created");
    };
    annotation_queues::insert_items_tx(
        conn,
        queue.id,
        w.project,
        &[NewItem {
            trace_id: Some(trace_id(0x31).to_string()),
            span_id: None,
            session_id: None,
            target_time: now,
        }],
        (AddedByKind::User, w.owner),
        now,
    )
    .await?;

    // The assistant: the org setting and one conversation with a question and an answer.
    assistant::put_settings(
        conn,
        w.org,
        &AssistantSettingsRow {
            enabled: true,
            connection_id: Some(w.connection),
            model: Some("gpt-4o".to_owned()),
            updated_by: Some(w.owner),
            updated_at: now,
        },
    )
    .await?;
    let conversation = Uuid::now_v7();
    assistant::insert_conversation(conn, conversation, w.project, w.owner, now).await?;
    for (role, text) in [
        (MessageRole::User, "golden question"),
        (MessageRole::Assistant, "golden answer"),
    ] {
        assistant::append_message(
            conn,
            conversation,
            NewMessage {
                role,
                parts: json!([{"type": "text", "text": text}]).to_string(),
                context: None,
                status: MessageStatus::Complete,
                turn_trace_id: None,
                stub_after: None,
            },
            now,
        )
        .await?;
    }
    assistant::set_title_if_empty(conn, conversation, "Golden conversation").await?;

    // Issues: an error source with one open issue, its events and a digest, and a topic source
    // with an assigned candidate, a day's spend and a finished refit.
    let error_source = Uuid::now_v7();
    let topic_source = Uuid::now_v7();
    for (id, name, kind) in [
        (error_source, "golden errors", SourceKind::Error),
        (topic_source, "golden topics", SourceKind::Topic),
    ] {
        let llm = kind.is_llm();
        issues::insert_source(
            conn,
            &SourceRow {
                id,
                project_id: w.project,
                name: name.to_owned(),
                kind,
                filter: None,
                enabled: true,
                paused_reason: None,
                paused_at: None,
                created_by: Some(w.owner),
                created_at: now,
                updated_at: now,
                sample_rate: llm.then_some(0.5),
                daily_cap: llm.then_some(10),
                score_names: Vec::new(),
                feedback_sources: Vec::new(),
                chat_connection_id: llm.then_some(w.connection),
                chat_model: llm.then(|| "gpt-4o".to_owned()),
                embed_connection_id: llm.then_some(w.connection),
                embed_model: llm.then(|| "golden-embed".to_owned()),
                embed_dimension: llm.then_some(2),
                floor: llm.then_some(0.8),
                min_issue_size: llm.then_some(3),
                budget_usd: None,
                budget_period: None,
                paused_detail: None,
                next_refit_at: None,
            },
        )
        .await?;
    }
    let issue = |id: Uuid, source: Uuid, fingerprint: Option<&str>, title: &str| IssueRow {
        id,
        project_id: w.project,
        source_id: source,
        fingerprint: fingerprint.map(str::to_owned),
        title: title.to_owned(),
        description: None,
        state: IssueState::Open,
        regressed: false,
        merged_into: None,
        first_seen: now,
        last_seen: now,
        resolved_at: None,
        resolved_by: None,
        created_at: now,
        updated_at: now,
        centroid: None,
        member_count: 0,
    };
    let error_issue = Uuid::now_v7();
    issues::insert_issue(
        conn,
        &issue(
            error_issue,
            error_source,
            Some("00112233445566778899aabbccddeeff"),
            "Golden error",
        ),
    )
    .await?;
    let topic_issue = Uuid::now_v7();
    issues::insert_issue(
        conn,
        &IssueRow {
            centroid: Some(vec![0.5, 0.5]),
            member_count: 1,
            ..issue(topic_issue, topic_source, None, "Golden topic")
        },
    )
    .await?;
    for (issue_id, kind, actor) in [
        (error_issue, EventKind::Created, None),
        (error_issue, EventKind::Renamed, Some(w.owner)),
    ] {
        issues::insert_event(
            conn,
            &EventRow {
                id: Uuid::now_v7(),
                issue_id,
                kind,
                actor_user_id: actor,
                from_value: None,
                to_value: None,
                at: now,
            },
        )
        .await?;
    }
    issues::insert_candidate(
        conn,
        &CandidateRow {
            id: Uuid::now_v7(),
            project_id: w.project,
            source_id: topic_source,
            target: CandidateTarget::Trace,
            trace_id: Some(trace_id(0x32)),
            span_id: None,
            session_id: None,
            user_id: None,
            trace_start: Some(now),
            score_id: None,
            dedup_key: format!("{topic_source}:{}", trace_id(0x32)),
            status: CandidateStatus::Assigned,
            attempts: 1,
            claims: 1,
            interruptions: 0,
            load_retries: 0,
            not_before: now,
            error_class: None,
            issue_id: Some(topic_issue),
            summary: Some("golden summary".to_owned()),
            summary_prompt_version: Some(1),
            vector: Some(vec![0.5, 0.5]),
            embed_model: Some("golden-embed".to_owned()),
            dimension: Some(2),
            call_trace_id: None,
            cost_usd: 0.001,
            created_at: now,
            updated_at: now,
        },
    )
    .await?;
    issues::add_spend(conn, topic_source, now / DAY_NS * DAY_NS, 0.001).await?;
    issues::insert_refit(
        conn,
        &RefitRow {
            id: Uuid::now_v7(),
            source_id: topic_source,
            trigger: "manual",
            started_at: now,
            finished_at: Some(now),
            pending: 1,
            clusters: 1,
            created: 1,
            joined: 0,
            outliers: 0,
            cost_usd: 0.0,
            error_class: None,
        },
    )
    .await?;
    issue_links::set_source_channels(conn, error_source, &[w.channel]).await?;
    // The notification `issue_links::notify_source` writes, without its pending deliveries (the
    // dispatcher would send them): one digest of the created issue.
    let digest = issue_links::Digest {
        items: vec![issue_links::DigestItem {
            issue_id: error_issue,
            kind: issue_links::DigestKind::Created,
            title: "Golden error".to_owned(),
        }],
        more: 0,
    };
    sqlx::query(
        "INSERT INTO issue_notifications (id, source_id, digest, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(error_source.to_string())
    .bind(serde_json::to_string(&digest).expect("a digest serializes"))
    .bind(now)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// A fixed trace id: 16 bytes of `n`.
fn trace_id(n: u8) -> TraceId {
    TraceId::from_bytes([n; 16]).expect("16 bytes is a trace id")
}

/// A fixed span id: 8 bytes of `n`.
fn span_id(n: u8) -> SpanId {
    SpanId::from_bytes([n; 8]).expect("8 bytes is a span id")
}

/// The alert condition of the golden rule.
fn golden_condition() -> AlertCondition {
    AlertCondition {
        metric: crate::api::alerts::AlertMetric::ErrorRate,
        window: crate::api::alerts::AlertWindow::M5,
        filter: None,
        comparator: crate::api::alerts::Comparator::Gt,
        threshold: 0.2,
        min_count: Some(20),
        target: None,
        score_name: None,
        aggregation: None,
        issue_id: None,
        source_id: None,
    }
}
