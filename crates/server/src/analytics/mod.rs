//! Product analytics (track TL M6, D-TL-14, D-TL-15): one typed catalog of product events,
//! recorded once wherever an action happens (web UI, API, CLI, SDK, MCP), feeding two sinks:
//! per-event feature counters for the heartbeat's `usage` buckets, and per-user events to the
//! operator's PostHog when `[analytics]` is set.

pub(crate) mod actor;
pub(crate) mod catalog;
pub(crate) mod counters;
pub(crate) mod posthog;
#[cfg(test)]
pub(crate) mod test_receiver;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event;
use crate::analytics::counters::Counters;
use crate::analytics::posthog::{FLUSH_INTERVAL, PosthogSink, SinkWorker, capture, group_identify};
use crate::auth::limits::{Bucket, KeyedLimiter};
use crate::config::ServerConfig;
use crate::http::AppState;
use crate::observability::switchboard::{Switch, for_config};

/// Requests to `POST /api/v1/analytics/events` per user and minute; more are dropped.
pub(crate) const EVENTS_PER_MINUTE: u32 = 60;
/// Users the limiter tracks at most.
const LIMITER_KEYS: usize = 100_000;

/// The analytics state on `AppInner`.
pub(crate) struct Analytics {
    /// Feature counters for the heartbeat's `usage`.
    pub(crate) counters: Counters,
    /// The heartbeat (and so the feature counts) runs: the switchboard's heartbeat decision.
    counting: AtomicBool,
    /// `None` unless `[analytics]` is set and restore mode is off.
    sink: Option<PosthogSink>,
    /// The per-user limit of the events endpoint.
    limiter: KeyedLimiter<Uuid>,
}

impl Analytics {
    /// The state for `config`, and the sink's worker for `serve` to spawn. Counting follows
    /// the switchboard's heartbeat decision for `config` (the same pure function `serve`
    /// calls).
    pub(crate) fn new(config: &ServerConfig) -> (Self, Option<SinkWorker>) {
        let counting = matches!(for_config(config).heartbeat, Switch::On);
        Self::with_flush_interval(config, FLUSH_INTERVAL, counting)
    }

    /// [`Analytics::new`] with another flush interval and an explicit counting flag (tests).
    pub(crate) fn with_flush_interval(
        config: &ServerConfig,
        flush_interval: Duration,
        counting: bool,
    ) -> (Self, Option<SinkWorker>) {
        let (sink, worker) = match config.analytics.posthog() {
            Some((host, key)) if !config.restore_mode => {
                match PosthogSink::new(host, key.clone(), flush_interval) {
                    Ok((sink, worker)) => (Some(sink), Some(worker)),
                    Err(_) => {
                        tracing::warn!(
                            "analytics: the PostHog client could not be built; per-user product analytics are off"
                        );
                        (None, None)
                    }
                }
            }
            _ => (None, None),
        };
        let limiter = KeyedLimiter::new(
            Bucket {
                capacity: EVENTS_PER_MINUTE,
                per: Duration::from_secs(60),
            },
            LIMITER_KEYS,
        );
        let analytics = Self {
            counters: Counters::default(),
            counting: AtomicBool::new(counting),
            sink,
            limiter,
        };
        (analytics, worker)
    }

    /// Whether per-user events go to PostHog.
    pub(crate) fn posthog_enabled(&self) -> bool {
        self.sink.is_some()
    }

    /// `analytics` in `GET /api/auth/config`: the UI sends its events when either sink runs,
    /// the PostHog sink or the heartbeat's feature counters.
    pub(crate) fn ui_events_wanted(&self) -> bool {
        self.counting.load(Ordering::Relaxed) || self.sink.is_some()
    }

    /// Takes one request of `user` from the events endpoint's limit.
    pub(crate) fn admit(&self, user: Uuid, now_ns: i64) -> bool {
        self.limiter.check(&user, now_ns).is_ok()
    }

    /// Turns counting on or off. `Analytics::new` follows the switchboard's heartbeat
    /// decision (off in a debug build), while `TestApp` counts by default so handler tests
    /// can assert the counters; tests that need it off set it by hand.
    #[cfg(test)]
    pub(crate) fn set_counting(&self, on: bool) {
        self.counting.store(on, Ordering::Relaxed);
    }
}

/// Records one catalog event (design § 6): counts it while the heartbeat runs (the `counting`
/// flag; when the heartbeat is off nothing is counted, saved or sent), and when the PostHog
/// sink runs, the actor has a user and the request did not opt out, queues it (after the
/// org's `$groupidentify` when due). Never fails and never blocks.
pub(crate) fn record(state: &AppState, event: &Event, actor: &Actor) {
    if state.analytics.counting.load(Ordering::Relaxed) {
        state.analytics.counters.increment(event.name());
    }
    let Some(sink) = state.analytics.sink.as_ref() else {
        return;
    };
    let Some(user_id) = actor.user_id else {
        return;
    };
    if actor.opted_out {
        return;
    }
    let now = state.clock.now_ns();
    if let Some(org) = actor.org_id {
        let plan = state.entitlements.plan_at(org, now).name;
        if sink.group_due(org, &plan, now) {
            sink.enqueue(group_identify(org, &plan, now));
        }
    }
    sink.enqueue(capture(event, actor, user_id, now));
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::Method;
    use axum::routing::get;

    use super::*;
    use crate::analytics::test_receiver::Receiver;
    use crate::auth::principal::{AuthUser, Principal};
    use crate::auth::roles::Role;
    use crate::config::Secret;
    use crate::http::access::ProjectAccess;
    use crate::test_support::{TestApp, config};

    /// A test app whose PostHog sink sends to `receiver`.
    async fn posthog_app(receiver: &Receiver) -> TestApp {
        TestApp::with_config(|c| {
            c.analytics.posthog_host = Some(receiver.base.clone());
            c.analytics.posthog_key = Some(Secret::new("phc_test".to_owned()));
        })
        .await
    }

    fn session_user(id: Uuid) -> AuthUser {
        AuthUser {
            id,
            is_instance_admin: false,
            email: "ada@example.com".to_owned(),
        }
    }

    fn no_actor() -> Actor {
        Actor {
            user_id: None,
            via: None,
            org_id: None,
            org_role: None,
            project_id: None,
            opted_out: false,
        }
    }

    #[tokio::test]
    async fn record_counts_without_a_sink() {
        let app = TestApp::new().await;
        assert!(!app.state.analytics.posthog_enabled());
        record(&app.state, &Event::DatasetCreated, &no_actor());
        record(&app.state, &Event::DatasetCreated, &no_actor());
        assert_eq!(app.state.analytics.counters.get("dataset.created"), 2);
    }

    #[tokio::test]
    async fn counting_off_leaves_the_counters_at_zero() {
        // `TestApp` counts by default; a real debug build does not, so the hook turns it off.
        // The PostHog sink does not depend on counting: the event still reaches it.
        let app = TestApp::new().await;
        app.state.analytics.set_counting(false);
        assert!(!app.state.analytics.ui_events_wanted());
        record(&app.state, &Event::DatasetCreated, &no_actor());
        assert_eq!(app.state.analytics.counters.get("dataset.created"), 0);

        let receiver = Receiver::start(200).await;
        let with_sink = posthog_app(&receiver).await;
        with_sink.state.analytics.set_counting(false);
        assert!(with_sink.state.analytics.posthog_enabled());
        let actor = Actor {
            user_id: Some(Uuid::now_v7()),
            ..no_actor()
        };
        record(&with_sink.state, &Event::DatasetCreated, &actor);
        assert_eq!(with_sink.state.analytics.counters.get("dataset.created"), 0);
        assert_eq!(receiver.wait_for(1, Duration::from_secs(2)).await.len(), 1);
    }

    #[tokio::test]
    async fn ui_events_follow_either_sink() {
        let app = TestApp::new().await;
        app.state.analytics.set_counting(false);
        assert!(!app.state.analytics.ui_events_wanted());
        app.state.analytics.set_counting(true);
        assert!(app.state.analytics.ui_events_wanted());

        let receiver = Receiver::start(200).await;
        // With counting off, the PostHog sink alone makes the flag true.
        let with_sink = posthog_app(&receiver).await;
        with_sink.state.analytics.set_counting(false);
        assert!(with_sink.state.analytics.ui_events_wanted());
    }

    #[test]
    fn new_counts_only_when_the_heartbeat_runs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (analytics, worker) = Analytics::new(&config(dir.path()));
        assert!(worker.is_none());
        // The switchboard's heartbeat is off in a test build, so counting is off.
        assert!(!analytics.ui_events_wanted());
    }

    #[test]
    fn admit_limits_each_user() {
        let analytics = {
            let dir = tempfile::tempdir().expect("temp dir");
            Analytics::new(&config(dir.path())).0
        };
        let user = Uuid::now_v7();
        let other = Uuid::now_v7();
        let now: i64 = 1_000_000_000_000;
        for _ in 0..EVENTS_PER_MINUTE {
            assert!(analytics.admit(user, now));
        }
        assert!(!analytics.admit(user, now));
        assert!(analytics.admit(other, now));
        let sixty_one_seconds = now + 61 * 1_000_000_000;
        assert!(analytics.admit(user, sixty_one_seconds));
    }

    #[tokio::test]
    async fn record_sends_a_session_event_with_its_group() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let principal = Principal::Session {
            user: session_user(seeded.user_id),
            session_hash: "h".to_owned(),
            sso_org: None,
        };
        let access = ProjectAccess {
            project: seeded.project,
            role: Some(Role::Admin),
            implicit: false,
        };
        let mut actor = Actor::project(&principal, &access);
        actor.opted_out = false;
        record(&app.state, &Event::DatasetCreated, &actor);

        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let batch: Vec<serde_json::Value> = bodies
            .iter()
            .flat_map(|body| body["batch"].as_array().cloned().unwrap_or_default())
            .collect();
        assert_eq!(batch.len(), 2, "one $groupidentify and one event");
        let org_key = format!("org_{}", seeded.org_id);
        assert_eq!(batch[0]["event"], "$groupidentify");
        assert_eq!(batch[0]["distinct_id"], org_key.as_str());
        assert_eq!(batch[1]["event"], "dataset.created");
        let user_id = seeded.user_id.to_string();
        assert_eq!(batch[1]["distinct_id"], user_id.as_str());
        assert_eq!(batch[1]["properties"]["distinct_id"], user_id.as_str());
        assert_eq!(
            batch[1]["properties"]["$groups"]["org"],
            seeded.org_id.to_string().as_str()
        );
        assert_eq!(batch[1]["properties"]["via"], "session");
        assert!(batch[1]["properties"]["$ip"].is_null());
        assert_eq!(batch[1]["properties"]["$geoip_disable"], true);

        let text = serde_json::to_string(&bodies).expect("bodies serialize");
        for forbidden in [
            "ada@",
            "example.com",
            "Acme",
            "acme",
            "Support",
            "support-bot",
        ] {
            assert!(!text.contains(forbidden), "the body carries {forbidden}");
        }
    }

    #[tokio::test]
    async fn the_group_is_identified_once() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let seeded = app.seed().await;
        let principal = Principal::Session {
            user: session_user(seeded.user_id),
            session_hash: "h".to_owned(),
            sso_org: None,
        };
        let access = ProjectAccess {
            project: seeded.project,
            role: Some(Role::Admin),
            implicit: false,
        };
        let mut actor = Actor::project(&principal, &access);
        actor.opted_out = false;
        record(&app.state, &Event::DatasetCreated, &actor);
        record(&app.state, &Event::DatasetCreated, &actor);

        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        let batch: Vec<serde_json::Value> = bodies
            .iter()
            .flat_map(|body| body["batch"].as_array().cloned().unwrap_or_default())
            .collect();
        let identifies = batch
            .iter()
            .filter(|event| event["event"] == "$groupidentify")
            .count();
        let events = batch
            .iter()
            .filter(|event| event["event"] == "dataset.created")
            .count();
        assert_eq!(identifies, 1);
        assert_eq!(events, 2);
    }

    #[tokio::test]
    async fn an_opted_out_actor_is_counted_but_not_sent() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let actor = Actor {
            user_id: Some(Uuid::now_v7()),
            opted_out: true,
            ..no_actor()
        };
        record(&app.state, &Event::DatasetCreated, &actor);
        assert_eq!(app.state.analytics.counters.get("dataset.created"), 1);
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(sent.is_empty(), "an opted-out event is not sent");
    }

    #[tokio::test]
    async fn an_actor_without_a_user_is_counted_but_not_sent() {
        let receiver = Receiver::start(200).await;
        let app = posthog_app(&receiver).await;
        let principal = Principal::ApiKey {
            key_id: Uuid::now_v7(),
            project_id: cetra_core::ids::ProjectId::new_v7(),
            kind: crate::auth::secret::KeyKind::Secret,
            scopes: crate::auth::roles::KeyScopes::from_scopes(&[]),
        };
        let actor = Actor::principal(&principal);
        record(&app.state, &Event::DatasetCreated, &actor);
        assert_eq!(app.state.analytics.counters.get("dataset.created"), 1);
        let sent = receiver.wait_for(1, Duration::from_millis(300)).await;
        assert!(sent.is_empty(), "an event without a user is not sent");
    }

    #[tokio::test]
    async fn restore_mode_has_no_sink() {
        let receiver = Receiver::start(200).await;
        let app = TestApp::with_config(|c| {
            c.analytics.posthog_host = Some(receiver.base.clone());
            c.analytics.posthog_key = Some(Secret::new("phc_test".to_owned()));
            c.restore_mode = true;
        })
        .await;
        assert!(!app.state.analytics.posthog_enabled());
    }

    #[tokio::test]
    async fn the_router_sets_the_privacy_signal() {
        let app = TestApp::with_routes(Router::new().route(
            "/probe",
            get(|| async { actor::request_opted_out().to_string() }),
        ))
        .await;
        let with_dnt = app
            .request(Method::GET, "/probe")
            .header("DNT", "1")
            .send()
            .await;
        assert_eq!(String::from_utf8_lossy(&with_dnt.body), "true");
        let without = app.request(Method::GET, "/probe").send().await;
        assert_eq!(String::from_utf8_lossy(&without.body), "false");
    }
}
