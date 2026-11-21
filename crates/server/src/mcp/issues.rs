//! The issue tool of the MCP server: the issues of a project with their occurrence counts over
//! the last 7 days (D-IC-24, spec 09 § MCP server).

use std::collections::HashMap;

use cetra_storage::TimeWindow;
use rmcp::model::JsonObject;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::output;
use super::tools::arguments;
use crate::alerts::message::issue_url;
use crate::auth::principal::AuthUser;
use crate::auth::roles::ProjectPerm;
use crate::eval::budget::DAY_NS;
use crate::http::AppState;
use crate::http::access::user_project;
use crate::http::error::ApiError;
use crate::issues::read::{names_of, tallies, total};
use crate::meta::issues::{self as store, IssueSort, SourceKind};
use crate::meta::orgs::org_by_id;

pub(crate) const LIST_ISSUES: &str = "list_issues";

/// `limit` when absent, and the bounds it accepts.
const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;
/// The occurrence period of each issue's counts: the last 7 days.
const COUNT_PERIOD_NS: i64 = 7 * DAY_NS;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListIssuesArguments {
    project: String,
    state: Option<StateArg>,
    source: Option<String>,
    limit: Option<i64>,
    max_chars: Option<i64>,
}

/// The `state` argument. `all` is every state but `merged`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StateArg {
    Open,
    Resolved,
    Ignored,
    All,
}

/// One issue of the answer. The times are RFC 3339 (D64).
#[derive(Debug, Serialize)]
struct McpIssue {
    id: Uuid,
    title: String,
    kind: &'static str,
    state: &'static str,
    regressed: bool,
    source_id: Uuid,
    #[serde(with = "cetra_core::time_serde")]
    first_seen: i64,
    #[serde(with = "cetra_core::time_serde")]
    last_seen: i64,
    count_7d: u64,
    users_7d: u64,
    url: String,
}

/// `{"issues": [McpIssue], "truncated", "notes"}`: the issues of the project in `state` (open by
/// default), of `source` when given, newest `last_seen` first, at most `limit` (20 by default).
/// Each issue's counts are its occurrences over the last 7 days, with the issues merged into it.
/// Fitted to `max_chars` with `fit_list` on `issues`.
pub(crate) async fn list_issues(
    state: &AppState,
    user: &AuthUser,
    raw: JsonObject,
) -> Result<Value, ApiError> {
    let args: ListIssuesArguments = arguments(raw)?;
    let access = user_project(state, user, &args.project, ProjectPerm::View).await?;
    let max_chars = output::max_chars(args.max_chars)?;
    let limit = limit_of(args.limit)?;
    let source = args
        .source
        .as_deref()
        .map(|text| {
            text.parse::<Uuid>()
                .map_err(|_| bad_request("source must be a source id"))
        })
        .transpose()?;
    let states = states_of(args.state.unwrap_or(StateArg::Open));
    let project = access.project.id;
    let now = state.clock.now_ns();

    let mut rows = store::list_issues(
        state.meta.reader(),
        project,
        &store::IssueListQuery {
            states: &states,
            source,
            q: None,
            ids: None,
            sort: IssueSort::LastSeen,
            cursor: None,
            limit,
        },
    )
    .await?;
    // The read returns one row more than a page when more issues exist.
    let page_len = usize::try_from(limit).unwrap_or(usize::MAX);
    let more = rows.len() > page_len;
    rows.truncate(page_len);

    let kinds: HashMap<Uuid, SourceKind> = store::list_sources(state.meta.reader(), project)
        .await?
        .into_iter()
        .map(|row| (row.id, row.kind))
        .collect();
    let mut merged = Vec::with_capacity(rows.len());
    for row in &rows {
        merged.push(store::merged_ids(state.meta.reader(), project, row.id).await?);
    }
    let names: Vec<String> = rows
        .iter()
        .zip(&merged)
        .flat_map(|(row, merged)| names_of(row.id, merged))
        .collect();
    let window = TimeWindow::new(now.saturating_sub(COUNT_PERIOD_NS), now)?;
    let counts = tallies(state, access.project.org_id, project, &names, window).await?;
    let org = org_by_id(state.meta.reader(), access.project.org_id)
        .await?
        .ok_or_else(|| ApiError::Internal("project without an org".into()))?;

    let mut issues = Vec::with_capacity(rows.len());
    for (row, merged) in rows.iter().zip(&merged) {
        let kind = kinds
            .get(&row.source_id)
            .ok_or_else(|| ApiError::Internal("issue without a source".into()))?;
        let tally = total(&names_of(row.id, merged), &counts);
        issues.push(McpIssue {
            id: row.id,
            title: row.title.clone(),
            kind: kind.as_str(),
            state: row.state.as_str(),
            regressed: row.regressed,
            source_id: row.source_id,
            first_seen: row.first_seen,
            last_seen: row.last_seen,
            count_7d: tally.count,
            users_7d: tally.users,
            url: issue_url(
                &state.config.public_url,
                &org.slug,
                &access.project.slug,
                row.id,
            ),
        });
    }

    let notes = if more {
        vec![Value::String(
            "more issues exist than limit; raise limit or narrow state/source".to_owned(),
        )]
    } else {
        Vec::new()
    };
    let mut body = Map::new();
    body.insert(
        "issues".to_owned(),
        serde_json::to_value(&issues).map_err(|error| ApiError::Internal(Box::new(error)))?,
    );
    body.insert("truncated".to_owned(), Value::Bool(more));
    body.insert("notes".to_owned(), Value::Array(notes));
    output::fit_list(
        &mut body,
        "issues",
        max_chars,
        &format!("result cut at {max_chars} characters; later issues left out"),
    );
    Ok(Value::Object(body))
}

/// The `limit` argument, as given or 20, inside `1..=100`.
fn limit_of(given: Option<i64>) -> Result<u32, ApiError> {
    match given {
        None => Ok(DEFAULT_LIMIT),
        Some(limit) => u32::try_from(limit)
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| bad_request("limit must be between 1 and 100")),
    }
}

/// The issue states a `state` argument asks for. `all` leaves out `merged` issues.
fn states_of(state: StateArg) -> Vec<store::IssueState> {
    match state {
        StateArg::Open => vec![store::IssueState::Open],
        StateArg::Resolved => vec![store::IssueState::Resolved],
        StateArg::Ignored => vec![store::IssueState::Ignored],
        StateArg::All => vec![
            store::IssueState::Open,
            store::IssueState::Resolved,
            store::IssueState::Ignored,
        ],
    }
}

/// `400 bad_request` with `detail`.
fn bad_request(detail: &str) -> ApiError {
    ApiError::BadRequest {
        detail: Some(detail.to_owned()),
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::LIST_ISSUES;
    use crate::auth::roles::{OrgRole, ProjectOverride};
    use crate::eval::budget::DAY_NS;
    use crate::mcp::test_helpers::{app_with_token, call};
    use crate::meta::issues::{IssueState, SourceKind};
    use crate::meta::members::set_project_override;
    use crate::test_support::issues::{issue_row, occurrence, put_issue, put_source, source_row};
    use crate::test_support::{Seeded, TestApp};

    const HOUR_NS: i64 = 3_600_000_000_000;

    /// A valid trace id of `n`, distinct for each `n` above 0.
    fn trace(n: u128) -> String {
        format!("{n:032x}")
    }

    /// An enabled error source named `name` in the seeded project, and its id.
    async fn error_source(app: &TestApp, seeded: &Seeded, name: &str) -> Uuid {
        let now = app.state.clock.now_ns();
        let row = source_row(seeded.project.id, name, SourceKind::Error, true, now);
        let id = row.id;
        put_source(app, row).await;
        id
    }

    /// The ids of the issues in `body`, in order.
    fn ids(body: &Value) -> Vec<String> {
        body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| issue["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_defaults_to_open_issues_newest_first() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let source = error_source(&app, &seeded, "Errors").await;
        let now = app.state.clock.now_ns();
        let (old, new, resolved) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        put_issue(
            &app,
            issue_row(
                project,
                source,
                old,
                "Oldest",
                IssueState::Open,
                now - 3 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                new,
                "Newest",
                IssueState::Open,
                now - HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                resolved,
                "Resolved",
                IssueState::Resolved,
                now - 2 * HOUR_NS,
            ),
        )
        .await;

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project.to_string()}),
        )
        .await;

        assert!(!is_error, "{body}");
        assert_eq!(ids(&body), vec![new.to_string(), old.to_string()]);
        let issue = &body["issues"][0];
        let mut keys: Vec<&str> = issue
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "count_7d",
                "first_seen",
                "id",
                "kind",
                "last_seen",
                "regressed",
                "source_id",
                "state",
                "title",
                "url",
                "users_7d",
            ]
        );
        assert_eq!(issue["title"], "Newest");
        assert_eq!(issue["kind"], "error");
        assert_eq!(issue["state"], "open");
        assert_eq!(issue["regressed"], false);
        assert_eq!(issue["source_id"], source.to_string());
        assert_eq!(issue["count_7d"], 0);
        assert_eq!(issue["users_7d"], 0);
        assert!(issue["first_seen"].as_str().unwrap().ends_with('Z'));
        assert!(issue["last_seen"].as_str().unwrap().ends_with('Z'));
        let url = format!("/acme/support-bot/issues/{new}");
        assert!(issue["url"].as_str().unwrap().ends_with(url.as_str()));
        assert_eq!(body["truncated"], false);
        assert_eq!(body["notes"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_counts_the_last_seven_days_with_merged_issues() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let source = error_source(&app, &seeded, "Errors").await;
        let now = app.state.clock.now_ns();
        let (target, merged) = (Uuid::now_v7(), Uuid::now_v7());
        put_issue(
            &app,
            issue_row(project, source, target, "Target", IssueState::Open, now),
        )
        .await;
        let mut folded = issue_row(project, source, merged, "Merged", IssueState::Merged, now);
        folded.merged_into = Some(target);
        put_issue(&app, folded).await;
        occurrence(
            &app,
            &seeded,
            source,
            target,
            &trace(1),
            now - HOUR_NS,
            Some("u1"),
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source,
            merged,
            &trace(2),
            now - 2 * HOUR_NS,
            Some("u2"),
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source,
            merged,
            &trace(3),
            now - 3 * HOUR_NS,
            Some("u3"),
            None,
        )
        .await;
        // Ten days old: outside the 7-day window.
        occurrence(
            &app,
            &seeded,
            source,
            target,
            &trace(4),
            now - 10 * DAY_NS,
            Some("u4"),
            None,
        )
        .await;

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project.to_string()}),
        )
        .await;

        assert!(!is_error, "{body}");
        // The merged issue is not listed; its occurrences count on the target.
        assert_eq!(ids(&body), vec![target.to_string()]);
        assert_eq!(body["issues"][0]["count_7d"], 3);
        assert_eq!(body["issues"][0]["users_7d"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_state_filters() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let source = error_source(&app, &seeded, "Errors").await;
        let now = app.state.clock.now_ns();
        let (open, resolved, ignored, merged) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        put_issue(
            &app,
            issue_row(
                project,
                source,
                open,
                "Open",
                IssueState::Open,
                now - HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                resolved,
                "Resolved",
                IssueState::Resolved,
                now - 2 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                ignored,
                "Ignored",
                IssueState::Ignored,
                now - 3 * HOUR_NS,
            ),
        )
        .await;
        let mut folded = issue_row(
            project,
            source,
            merged,
            "Merged",
            IssueState::Merged,
            now - 4 * HOUR_NS,
        );
        folded.merged_into = Some(open);
        put_issue(&app, folded).await;
        let project = project.to_string();

        let (_, open_only) = call(&app, &tok, LIST_ISSUES, &json!({"project": project})).await;
        assert_eq!(ids(&open_only), vec![open.to_string()]);
        let (_, resolved_only) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "state": "resolved"}),
        )
        .await;
        assert_eq!(ids(&resolved_only), vec![resolved.to_string()]);
        let (_, ignored_only) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "state": "ignored"}),
        )
        .await;
        assert_eq!(ids(&ignored_only), vec![ignored.to_string()]);
        let (is_error, all) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "state": "all"}),
        )
        .await;
        assert!(!is_error, "{all}");
        assert_eq!(
            ids(&all),
            vec![open.to_string(), resolved.to_string(), ignored.to_string()]
        );
        let (is_error, _) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "state": "merged"}),
        )
        .await;
        assert!(is_error, "merged is not a state of the tool");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_filters_by_source() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let first = error_source(&app, &seeded, "Errors").await;
        let second = error_source(&app, &seeded, "Other errors").await;
        let now = app.state.clock.now_ns();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        put_issue(
            &app,
            issue_row(project, first, a, "A", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, second, b, "B", IssueState::Open, now),
        )
        .await;
        let project = project.to_string();

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "source": second.to_string()}),
        )
        .await;
        assert!(!is_error, "{body}");
        assert_eq!(ids(&body), vec![b.to_string()]);
        assert_eq!(body["issues"][0]["source_id"], second.to_string());

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "source": "not-a-uuid"}),
        )
        .await;
        assert!(is_error);
        assert_eq!(body["error"]["message"], "source must be a source id");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_limit_and_truncation() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let source = error_source(&app, &seeded, "Errors").await;
        let now = app.state.clock.now_ns();
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, hours) in [(a, 1), (b, 2), (c, 3)] {
            put_issue(
                &app,
                issue_row(
                    project,
                    source,
                    id,
                    "Issue",
                    IssueState::Open,
                    now - hours * HOUR_NS,
                ),
            )
            .await;
        }
        let project = project.to_string();

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "limit": 2}),
        )
        .await;
        assert!(!is_error, "{body}");
        assert_eq!(ids(&body), vec![a.to_string(), b.to_string()]);
        assert_eq!(body["truncated"], true);
        let notes = body["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1, "{body}");
        assert!(notes[0].as_str().unwrap().starts_with("more issues exist"));

        for limit in [0, 101] {
            let (is_error, body) = call(
                &app,
                &tok,
                LIST_ISSUES,
                &json!({"project": project, "limit": limit}),
            )
            .await;
            assert!(is_error, "limit {limit}");
            assert_eq!(body["error"]["message"], "limit must be between 1 and 100");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_respects_max_chars() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id;
        let source = error_source(&app, &seeded, "Errors").await;
        let now = app.state.clock.now_ns();
        let title = "x".repeat(200);
        for hours in 1..=3 {
            put_issue(
                &app,
                issue_row(
                    project,
                    source,
                    Uuid::now_v7(),
                    &title,
                    IssueState::Open,
                    now - hours * HOUR_NS,
                ),
            )
            .await;
        }

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project.to_string(), "max_chars": 1000}),
        )
        .await;

        assert!(!is_error, "{body}");
        // One issue of about 600 characters fits in 1000; two do not.
        assert_eq!(body["issues"].as_array().unwrap().len(), 1, "{body}");
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["notes"],
            json!(["result cut at 1000 characters; later issues left out"])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_needs_a_project_the_caller_can_view() {
        let (app, seeded, _) = app_with_token().await;
        let (hidden, _) = app.add_project(&seeded, "hidden", None).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let (viewer_id, hidden_id) = (viewer.id, hidden.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_project_override(conn, hidden_id, viewer_id, ProjectOverride::None).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let viewer_tok = app.personal_token(viewer_id, false).await;

        let (is_error, body) = call(
            &app,
            &viewer_tok,
            LIST_ISSUES,
            &json!({"project": "acme/hidden"}),
        )
        .await;
        assert!(is_error);
        assert_eq!(
            body,
            json!({"error": {"code": "not_found", "message": "Not Found"}})
        );

        // A viewer of the org reads the project it can see.
        let (is_error, body) = call(
            &app,
            &viewer_tok,
            LIST_ISSUES,
            &json!({"project": "acme/support-bot"}),
        )
        .await;
        assert!(!is_error, "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_issues_refuses_unknown_arguments() {
        let (app, seeded, tok) = app_with_token().await;
        let project = seeded.project.id.to_string();

        let (is_error, body) = call(
            &app,
            &tok,
            LIST_ISSUES,
            &json!({"project": project, "bogus": true}),
        )
        .await;

        assert!(is_error);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid arguments")
        );
    }
}
