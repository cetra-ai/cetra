//! `POST /api/v1/analytics/events` (track TL M6, D-TL-14): the UI's catalog events. Session only; always 204 past authentication, whatever the body holds, so the UI never retries and a probe learns nothing.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use cetra_core::ids::{OrgId, ProjectId};
use serde::Deserialize;
use serde_json::Value;

use crate::analytics::actor::{Actor, request_opted_out};
use crate::analytics::catalog::parse_web;
use crate::analytics::record;
use crate::api::analytics::AnalyticsEventInput;
use crate::auth::principal::{AuthUser, Principal};
use crate::auth::roles::{OrgRole, ProjectPerm};
use crate::demo::org_role_for;
use crate::http::AppState;
use crate::http::access::{Access, RouteDef, user_project};
use crate::meta::orgs::{org_by_id, org_by_slug};

/// Events read per request; later ones are dropped.
pub(crate) const MAX_EVENTS: usize = 50;
/// Largest body read; a larger one is dropped whole.
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;
/// Longest `org` or `project` value looked up.
const MAX_KEY_BYTES: usize = 256;
/// Distinct `(org, project)` pairs resolved per request; events of further pairs are not
/// sent to PostHog.
const MAX_CONTEXTS: usize = 5;

/// The analytics route.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::POST,
        path: "/api/v1/analytics/events",
        access: Access::User { session_only: true },
        documented: true,
        handler: post(post_events),
    }]
}

/// The body, read loosely so one bad event never spoils the others.
#[derive(Deserialize)]
struct RawBatch {
    #[serde(default)]
    events: Vec<Value>,
}

/// What an event's `org` and `project` resolved to.
#[derive(Debug, Clone, Copy, Default)]
struct Context {
    org_id: Option<OrgId>,
    org_role: Option<OrgRole>,
    project_id: Option<ProjectId>,
}

/// Records each valid web event for the session's user; answers 204 whatever happens.
async fn post_events(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Body,
) -> StatusCode {
    let Principal::Session { user, .. } = &principal else {
        return StatusCode::NO_CONTENT;
    };
    if !state.analytics.admit(user.id, state.clock.now_ns()) {
        return StatusCode::NO_CONTENT;
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return StatusCode::NO_CONTENT;
    };
    let Ok(raw) = serde_json::from_slice::<RawBatch>(&bytes) else {
        return StatusCode::NO_CONTENT;
    };
    let opted_out = request_opted_out();
    let resolving = state.analytics.posthog_enabled();
    let mut contexts: HashMap<(Option<String>, Option<String>), Option<Context>> = HashMap::new();
    for value in raw.events.into_iter().take(MAX_EVENTS) {
        let Ok(input) = serde_json::from_value::<AnalyticsEventInput>(value) else {
            continue;
        };
        let Some(event) = parse_web(&input.name, &input.properties) else {
            continue;
        };
        let context = if resolving {
            let key = (input.org.clone(), input.project.clone());
            match contexts.get(&key) {
                Some(cached) => *cached,
                None if contexts.len() >= MAX_CONTEXTS => None,
                None => {
                    let resolved =
                        resolve(&state, user, input.org.as_deref(), input.project.as_deref()).await;
                    contexts.insert(key, resolved);
                    resolved
                }
            }
        } else {
            Some(Context::default())
        };
        // A context that resolved feeds the sink; without one the actor has no user, so
        // `record` drops the event.
        let actor = match context {
            Some(context) => Actor {
                user_id: Some(user.id),
                via: None,
                org_id: context.org_id,
                org_role: context.org_role,
                project_id: context.project_id,
                opted_out,
            },
            None => Actor {
                user_id: None,
                via: None,
                org_id: None,
                org_role: None,
                project_id: None,
                opted_out,
            },
        };
        record(&state, &event, &actor);
    }
    StatusCode::NO_CONTENT
}

/// `None`: the event is not sent to PostHog (the org or project is unknown, inactive, or
/// not readable by the user, or a lookup failed).
async fn resolve(
    state: &AppState,
    user: &AuthUser,
    org: Option<&str>,
    project: Option<&str>,
) -> Option<Context> {
    if org.is_some_and(|value| value.len() > MAX_KEY_BYTES)
        || project.is_some_and(|value| value.len() > MAX_KEY_BYTES)
    {
        return None;
    }
    match (org, project) {
        (None, None) => Some(Context::default()),
        (org, Some(project)) => {
            let key = match org {
                Some(org) => format!("{org}/{project}"),
                None => project.to_owned(),
            };
            let access = user_project(state, user, &key, ProjectPerm::View)
                .await
                .ok()?;
            let found = org_role_for(state, access.project.org_id, user)
                .await
                .ok()??;
            Some(Context {
                org_id: Some(access.project.org_id),
                org_role: Some(found.role),
                project_id: Some(access.project.id),
            })
        }
        (Some(org), None) => {
            let pool = state.meta.reader();
            let row = match org.parse::<OrgId>() {
                Ok(id) => org_by_id(pool, id).await.ok()?,
                Err(_) => org_by_slug(pool, org).await.ok()?,
            }?;
            if row.state != "active" {
                return None;
            }
            let found = org_role_for(state, row.id, user).await.ok()??;
            Some(Context {
                org_id: Some(row.id),
                org_role: Some(found.role),
                project_id: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use super::*;
    use crate::analytics::EVENTS_PER_MINUTE;
    use crate::analytics::test_receiver::Receiver;
    use crate::config::Secret;
    use crate::test_support::{TestApp, second_org};

    const PATH: &str = "/api/v1/analytics/events";
    const TRACES: &str = "/$org/$project/traces/";

    /// A test app whose PostHog sink sends to `receiver` (as `analytics::tests::posthog_app`).
    async fn posthog_app(receiver: &Receiver) -> TestApp {
        TestApp::with_config(|c| {
            c.analytics.posthog_host = Some(receiver.base.clone());
            c.analytics.posthog_key = Some(Secret::new("phc_test".to_owned()));
        })
        .await
    }

    /// One `page.viewed` event; `project` is omitted when `None`.
    fn page(route: &str, org: &str, project: Option<&str>) -> Value {
        let mut event = json!({
            "name": "page.viewed",
            "org": org,
            "properties": { "route": route },
        });
        if let Some(project) = project {
            event["project"] = json!(project);
        }
        event
    }

    async fn post(app: &TestApp, cookie: &str, body: &Value) -> StatusCode {
        app.request(Method::POST, PATH)
            .header("cookie", cookie)
            .json(body)
            .send()
            .await
            .status
    }

    /// The `batch` events of every request the receiver got, in order.
    fn batch_events(bodies: &[Value]) -> Vec<Value> {
        bodies
            .iter()
            .flat_map(|body| body["batch"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    #[tokio::test]
    async fn a_handler_event_reaches_posthog() {
        // Server events are recorded by the handlers, not by the events endpoint: creating a
        // project through the API must reach the PostHog sink too.
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let created = app
            .request(Method::POST, "/api/v1/orgs/acme/projects")
            .header("cookie", &seeded.cookie)
            .json(&json!({ "name": "Handler", "slug": "handler" }))
            .send()
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "project.created"),
            "project.created did not reach PostHog: {events:?}"
        );
    }

    #[tokio::test]
    async fn without_posthog_events_are_dropped_without_lookups() {
        // No sink: the events are validated and answered 204, and no org or project is
        // looked up.
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = json!({ "events": [page(TRACES, "nope", Some("missing"))] });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn a_page_view_reaches_posthog_by_template() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [page(TRACES, "acme", Some("support-bot"))] });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );

        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        let event = events
            .iter()
            .find(|event| event["event"] == "page.viewed")
            .expect("a page.viewed event reaches PostHog");
        assert_eq!(event["properties"]["route"], TRACES);
        assert_eq!(
            event["properties"]["project_id"],
            seeded.project.id.to_string()
        );
        assert_eq!(
            event["properties"]["$groups"]["org"],
            seeded.org_id.to_string()
        );
        assert_eq!(event["properties"]["$set"]["org_role"], "owner");
        assert!(event["properties"].get("via").is_none());
    }

    #[tokio::test]
    async fn planted_text_never_reaches_posthog() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [
            {
                "name": "page.viewed",
                "org": "acme",
                "project": "support-bot",
                "properties": { "route": "/acme/support-bot/traces" },
            },
            {
                "name": "page.viewed",
                "org": "acme",
                "project": "support-bot",
                "properties": {
                    "route": TRACES,
                    "email": "ada@example.com",
                    "note": "Planted free text",
                },
            },
            { "name": "custom.ada@example.com" },
            {
                "name": "theme.changed",
                "properties": { "theme": "Planted" },
            },
        ]});
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );

        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        let page_views: Vec<&Value> = events
            .iter()
            .filter(|event| event["event"] == "page.viewed")
            .collect();
        assert_eq!(page_views.len(), 1);
        assert!(page_views[0]["properties"].get("email").is_none());
        assert!(page_views[0]["properties"].get("note").is_none());

        let text: String = bodies.iter().map(|body| body.to_string()).collect();
        for needle in [
            "ada@",
            "example.com",
            "Planted",
            "acme",
            "Acme",
            "support-bot",
            "Support",
            "/acme/",
        ] {
            assert!(!text.contains(needle), "PostHog body carries {needle:?}");
        }
    }

    #[tokio::test]
    async fn server_event_names_are_refused() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [
            { "name": "dataset.created" },
            { "name": "org.created" },
        ]});
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(sent.is_empty(), "a server event name reached PostHog");
    }

    #[tokio::test]
    async fn dnt_and_gpc_skip_posthog() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [page(TRACES, "acme", Some("support-bot"))] });
        for (name, value) in [("DNT", "1"), ("Sec-GPC", "1")] {
            let status = app
                .request(Method::POST, PATH)
                .header("cookie", &seeded.cookie)
                .header(name, value)
                .json(&body)
                .send()
                .await
                .status;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(sent.is_empty(), "an opted-out request reached PostHog");
    }

    #[tokio::test]
    async fn at_most_50_events_are_read() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let events: Vec<Value> = (0..60)
            .map(|_| json!({ "name": "command_palette.opened" }))
            .collect();
        let body = json!({ "events": events });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "command_palette.opened")
                .count(),
            50
        );
    }

    #[tokio::test]
    async fn a_foreign_org_is_not_sent() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        second_org(&app, "other", "bob@example.com").await;

        // The org does not resolve for this user: the event is dropped.
        let foreign = json!({ "events": [page(TRACES, "other", None)] });
        assert_eq!(
            post(&app, &seeded.cookie, &foreign).await,
            StatusCode::NO_CONTENT
        );
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(sent.is_empty(), "a foreign org's event reached PostHog");

        let own = json!({ "events": [page(TRACES, "acme", Some("support-bot"))] });
        assert_eq!(
            post(&app, &seeded.cookie, &own).await,
            StatusCode::NO_CONTENT
        );
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "page.viewed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn an_unknown_project_is_not_sent() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [page(TRACES, "acme", Some("nope"))] });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(
            sent.is_empty(),
            "an unknown project's event reached PostHog"
        );
    }

    #[tokio::test]
    async fn at_most_five_contexts_per_request() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        // `acme` resolves; the next four unknown orgs take the remaining context slots and
        // do not resolve; the last two never get a lookup. Only the resolved one is sent.
        let mut events = vec![page(TRACES, "acme", None)];
        for index in 1..=6 {
            events.push(page(TRACES, &format!("unknown-{index}"), None));
        }
        let body = json!({ "events": events });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "page.viewed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_per_user_limit_drops_requests() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [{ "name": "command_palette.opened" }] });
        for _ in 0..EVENTS_PER_MINUTE {
            assert_eq!(
                post(&app, &seeded.cookie, &body).await,
                StatusCode::NO_CONTENT
            );
        }
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        let sent = events
            .iter()
            .filter(|event| event["event"] == "command_palette.opened")
            .count();
        assert_eq!(sent, usize::from(EVENTS_PER_MINUTE));
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let events = batch_events(&receiver.wait_for(1, Duration::from_millis(300)).await);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "command_palette.opened")
                .count(),
            sent
        );
    }

    #[tokio::test]
    async fn an_org_only_event_resolves_the_org() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [page(TRACES, "acme", None)] });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        let event = events
            .iter()
            .find(|event| event["event"] == "page.viewed")
            .expect("a page.viewed event reaches PostHog");
        assert_eq!(
            event["properties"]["$groups"]["org"],
            seeded.org_id.to_string()
        );
        assert!(event["properties"].get("project_id").is_none());
    }

    #[tokio::test]
    async fn a_user_level_event_has_no_group() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let body = json!({ "events": [{ "name": "command_palette.opened" }] });
        assert_eq!(
            post(&app, &seeded.cookie, &body).await,
            StatusCode::NO_CONTENT
        );
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let events = batch_events(&bodies);
        let event = events
            .iter()
            .find(|event| event["event"] == "command_palette.opened")
            .expect("a command_palette.opened event reaches PostHog");
        assert!(event["properties"].get("$groups").is_none());
        assert!(event["properties"].get("project_id").is_none());
    }

    #[tokio::test]
    async fn bad_bodies_answer_204() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let oversized = format!(
            "{{\"events\":[{{\"name\":\"command_palette.opened\",\"properties\":{{\"pad\":\"{}\"}}}}]}}",
            "x".repeat(70 * 1024)
        );
        let bodies: Vec<Vec<u8>> = vec![
            b"not json".to_vec(),
            b"{}".to_vec(),
            br#"{"events":5}"#.to_vec(),
            br#"{"events":[1,"x",null]}"#.to_vec(),
            oversized.into_bytes(),
        ];
        for body in bodies {
            let status = app
                .request(Method::POST, PATH)
                .header("cookie", &seeded.cookie)
                .body(body)
                .send()
                .await
                .status;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
    }

    #[tokio::test]
    async fn sessions_only() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = json!({ "events": [{ "name": "command_palette.opened" }] });

        let anonymous = app
            .request(Method::POST, PATH)
            .json(&body)
            .send()
            .await
            .status;
        assert_eq!(anonymous, StatusCode::UNAUTHORIZED);

        let token = app.personal_token(seeded.user_id, true).await;
        let with_token = app
            .request(Method::POST, PATH)
            .header("authorization", &format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .status;
        assert_eq!(with_token, StatusCode::FORBIDDEN);
    }
}
