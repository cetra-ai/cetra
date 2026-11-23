//! The PostHog sink (D-TL-14): per-user catalog events, batched to `<host>/batch/` every 10 s or
//! 100 events, from a queue of 10 000 that drops new events when full. No retry; failures are
//! logged at `debug` at most once a minute. A capture carries the user's UUID (as `distinct_id`
//! at the top level and in `properties`), the org id as the `org` group, the catalog properties,
//! `project_id`, `via`, `$ip: null`, `$geoip_disable: true`, and the org role as the only person
//! property; the org's plan is the only group property, sent by `$groupidentify` with the
//! group-scoped `distinct_id` `org_<org id>`. Q verifies the shape with one real batch (TL-Q4).
//! Field names follow PostHog's capture API (https://posthog.com/docs/api/capture).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use cetra_core::ids::OrgId;
use chrono::{DateTime, SecondsFormat};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::{Event, Source};
use crate::config::Secret;

/// A batch goes out at least this often while events wait.
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// A batch goes out as soon as it holds this many events.
pub(crate) const MAX_BATCH: usize = 100;
/// Events waiting at most; later ones are dropped until the worker catches up.
pub(crate) const QUEUE_CAPACITY: usize = 10_000;
/// Each request gives up after this long.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// At most one failure line per this interval.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// The PostHog group type of an org.
pub(crate) const GROUP_TYPE: &str = "org";
/// A group's `plan` is sent again after this long (ns), or when it changes.
const GROUP_REFRESH_NS: i64 = 3_600 * 1_000_000_000;

/// Why a batch was not sent. Never carries a URL, the key or a body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PosthogError {
    /// The HTTP client could not be built.
    #[error("analytics client could not be built")]
    Client,
    /// The request failed before a response.
    #[error("analytics request failed (timeout: {timeout})")]
    Request {
        /// The request hit the client's timeout.
        timeout: bool,
    },
    /// PostHog answered something other than 2xx (redirects are not followed).
    #[error("analytics endpoint answered {0}")]
    Status(u16),
}

/// One PostHog event of a batch. Field order is the JSON key order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Capture {
    /// The catalog name or `$groupidentify`.
    pub(crate) event: String,
    /// The user's UUID (or `org_<id>` for `$groupidentify`).
    pub(crate) distinct_id: String,
    /// RFC 3339 UTC with milliseconds.
    pub(crate) timestamp: String,
    /// As built by [`capture`] or [`group_identify`].
    pub(crate) properties: Map<String, Value>,
}

/// The body of `POST <host>/batch/`.
#[derive(Serialize)]
struct BatchBody<'a> {
    api_key: &'a str,
    batch: &'a [Capture],
}

/// The handle `record` uses: the queue and the group bookkeeping.
#[derive(Debug)]
pub(crate) struct PosthogSink {
    queue: mpsc::Sender<Capture>,
    /// Per org: the plan last sent and when (ns).
    groups: Mutex<HashMap<OrgId, (String, i64)>>,
}

/// The background half: drains the queue and posts batches. Spawned by `serve`.
#[derive(Debug)]
pub(crate) struct SinkWorker {
    queue: mpsc::Receiver<Capture>,
    client: reqwest::Client,
    url: String,
    key: Secret<String>,
    flush_interval: Duration,
}

/// `<host>/batch/`.
pub(crate) fn batch_url(host: &str) -> String {
    format!("{}/batch/", host.trim_end_matches('/'))
}

/// RFC 3339, UTC, milliseconds: `2026-01-01T00:00:00.000Z`.
fn timestamp(now_ns: i64) -> String {
    DateTime::from_timestamp_nanos(now_ns).to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The PostHog event of a catalog event.
pub(crate) fn capture(event: &Event, actor: &Actor, user_id: Uuid, now_ns: i64) -> Capture {
    let mut properties = event.properties();
    if let Some(project) = actor.project_id {
        properties.insert("project_id".to_owned(), Value::String(project.to_string()));
    }
    if let (Some(via), Source::Server) = (actor.via, event.source()) {
        // A unit enum serializes to a string, so `Null` is never produced.
        properties.insert(
            "via".to_owned(),
            serde_json::to_value(via).unwrap_or(Value::Null),
        );
    }
    if let Some(org) = actor.org_id {
        let mut groups = Map::new();
        groups.insert(GROUP_TYPE.to_owned(), Value::String(org.to_string()));
        properties.insert("$groups".to_owned(), Value::Object(groups));
    }
    if let Some(role) = actor.org_role {
        let mut set = Map::new();
        // A unit enum serializes to a string, so `Null` is never produced.
        set.insert(
            "org_role".to_owned(),
            serde_json::to_value(role).unwrap_or(Value::Null),
        );
        properties.insert("$set".to_owned(), Value::Object(set));
    }
    properties.insert("distinct_id".to_owned(), Value::String(user_id.to_string()));
    properties.insert("$ip".to_owned(), Value::Null);
    properties.insert("$geoip_disable".to_owned(), Value::Bool(true));
    properties.insert("$process_person_profile".to_owned(), Value::Bool(true));
    Capture {
        event: event.name().to_owned(),
        distinct_id: user_id.to_string(),
        timestamp: timestamp(now_ns),
        properties,
    }
}

/// `$groupidentify` for `org` with its plan, the only group property. Its `distinct_id` is
/// group-scoped (`org_<id>`) and builds no person profile, as PostHog's own SDKs do.
pub(crate) fn group_identify(org: OrgId, plan: &str, now_ns: i64) -> Capture {
    let distinct_id = format!("{GROUP_TYPE}_{org}");
    let mut set = Map::new();
    set.insert("plan".to_owned(), Value::String(plan.to_owned()));
    let mut properties = Map::new();
    properties.insert("distinct_id".to_owned(), Value::String(distinct_id.clone()));
    properties.insert(
        "$group_type".to_owned(),
        Value::String(GROUP_TYPE.to_owned()),
    );
    properties.insert("$group_key".to_owned(), Value::String(org.to_string()));
    properties.insert("$group_set".to_owned(), Value::Object(set));
    properties.insert("$ip".to_owned(), Value::Null);
    properties.insert("$geoip_disable".to_owned(), Value::Bool(true));
    properties.insert("$process_person_profile".to_owned(), Value::Bool(false));
    Capture {
        event: "$groupidentify".to_owned(),
        distinct_id,
        timestamp: timestamp(now_ns),
        properties,
    }
}

/// The sink's own HTTP client: no redirects, 10 s timeout, `User-Agent: cetra/<version>`.
fn client() -> Result<reqwest::Client, PosthogError> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("cetra/{}", env!("CARGO_PKG_VERSION")));
    // A developer's HTTP(S)_PROXY must not catch the tests' 127.0.0.1 receivers (as the heartbeat's client).
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|_| PosthogError::Client)
}

/// Posts one batch. The response body is never read.
async fn post(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    batch: &[Capture],
) -> Result<(), PosthogError> {
    let response = client
        .post(url)
        .json(&BatchBody { api_key, batch })
        .send()
        .await
        .map_err(|error| PosthogError::Request {
            timeout: error.is_timeout(),
        })?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(PosthogError::Status(status.as_u16()))
    }
}

impl PosthogSink {
    /// The sink and its worker. `host` is the operator's `posthog_host`; nothing is sent yet.
    pub(crate) fn new(
        host: &str,
        key: Secret<String>,
        flush_interval: Duration,
    ) -> Result<(Self, SinkWorker), PosthogError> {
        let client = client()?;
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let sink = Self {
            queue: sender,
            groups: Mutex::new(HashMap::new()),
        };
        let worker = SinkWorker {
            queue: receiver,
            client,
            url: batch_url(host),
            key,
            flush_interval,
        };
        Ok((sink, worker))
    }

    /// Queues a capture. Full or closed: the event is dropped (design: bounded queue, dropped
    /// when full).
    pub(crate) fn enqueue(&self, capture: Capture) {
        let _ = self.queue.try_send(capture);
    }

    /// Whether `org`'s group must be identified now: it was never sent, its plan changed, or
    /// the last send is an hour old. When due, the send is recorded and `true` is returned.
    pub(crate) fn group_due(&self, org: OrgId, plan: &str, now_ns: i64) -> bool {
        let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        let due = match groups.get(&org) {
            None => true,
            Some((sent_plan, sent_at)) => {
                sent_plan != plan || now_ns.saturating_sub(*sent_at) >= GROUP_REFRESH_NS
            }
        };
        if due {
            groups.insert(org, (plan.to_owned(), now_ns));
        }
        due
    }
}

impl SinkWorker {
    /// Drains the queue until the sink is dropped, posting batches as they fill or come due.
    /// Spawned by `serve`.
    pub(crate) async fn run(mut self) {
        let mut batch: Vec<Capture> = Vec::with_capacity(MAX_BATCH);
        let mut ticker = tokio::time::interval(self.flush_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await; // The first tick completes at once.
        let mut last_failure_log: Option<Instant> = None;
        loop {
            tokio::select! {
                received = self.queue.recv() => match received {
                    Some(capture) => {
                        batch.push(capture);
                        if batch.len() >= MAX_BATCH {
                            self.flush(&mut batch, &mut last_failure_log).await;
                        }
                    }
                    None => {
                        self.flush(&mut batch, &mut last_failure_log).await;
                        return;
                    }
                },
                _ = ticker.tick() => {
                    if !batch.is_empty() {
                        self.flush(&mut batch, &mut last_failure_log).await;
                    }
                }
            }
        }
    }

    /// Posts the batch and empties it, whatever the answer (no retry). A failure is logged at
    /// `debug`, at most once a minute.
    async fn flush(&self, batch: &mut Vec<Capture>, last_failure_log: &mut Option<Instant>) {
        if batch.is_empty() {
            return;
        }
        let result = post(&self.client, &self.url, self.key.expose(), batch).await;
        batch.clear();
        if let Err(error) = result
            && last_failure_log.is_none_or(|at| at.elapsed() >= FAILURE_LOG_INTERVAL)
        {
            *last_failure_log = Some(Instant::now());
            tracing::debug!(error = %error, "analytics: batch not sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::ProjectId;

    use super::*;
    use crate::analytics::actor::Via;
    use crate::analytics::test_receiver::Receiver;
    use crate::api::evaluators::EvaluatorKind;
    use crate::auth::roles::OrgRole;
    use crate::test_support::capture_logs;

    const NOW: i64 = 1_767_225_600_000_000_000;

    fn user() -> Uuid {
        Uuid::parse_str("01900000-0000-7000-8000-000000000101").expect("fixed uuid")
    }

    fn org() -> OrgId {
        "01900000-0000-7000-8000-000000000010"
            .parse()
            .expect("fixed org id")
    }

    fn project() -> ProjectId {
        "01900000-0000-7000-8000-000000000001"
            .parse()
            .expect("fixed project id")
    }

    fn full_actor() -> Actor {
        Actor {
            user_id: Some(user()),
            via: Some(Via::Session),
            org_id: Some(org()),
            org_role: Some(OrgRole::Admin),
            project_id: Some(project()),
            opted_out: false,
        }
    }

    /// The minified JSON of `text`, with its key order kept (`preserve_order`).
    fn minified(text: &str) -> String {
        let value: Value = serde_json::from_str(text).expect("test JSON is valid");
        serde_json::to_string(&value).expect("JSON serializes")
    }

    fn sample(n: usize) -> Capture {
        capture(
            &Event::DatasetCreated,
            &full_actor(),
            user(),
            NOW + i64::try_from(n).expect("small index"),
        )
    }

    #[test]
    fn batch_url_appends_batch() {
        assert_eq!(
            batch_url("https://eu.i.posthog.com"),
            "https://eu.i.posthog.com/batch/"
        );
        assert_eq!(
            batch_url("https://eu.i.posthog.com/"),
            "https://eu.i.posthog.com/batch/"
        );
        assert_eq!(batch_url("http://127.0.0.1:9"), "http://127.0.0.1:9/batch/");
    }

    #[test]
    fn capture_has_the_documented_shape() {
        let event = Event::EvaluatorCreated {
            kind: EvaluatorKind::LlmJudge,
        };
        let capture = capture(&event, &full_actor(), user(), NOW);
        let expected = r#"{
            "event": "evaluator.created",
            "distinct_id": "01900000-0000-7000-8000-000000000101",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "properties": {
                "kind": "llm_judge",
                "project_id": "01900000-0000-7000-8000-000000000001",
                "via": "session",
                "$groups": {"org": "01900000-0000-7000-8000-000000000010"},
                "$set": {"org_role": "admin"},
                "distinct_id": "01900000-0000-7000-8000-000000000101",
                "$ip": null,
                "$geoip_disable": true,
                "$process_person_profile": true
            }
        }"#;
        assert_eq!(
            serde_json::to_string(&capture).expect("capture serializes"),
            minified(expected)
        );
    }

    #[test]
    fn a_web_event_has_no_via() {
        let event = Event::PageViewed {
            route: "/$org/$project/traces/",
        };
        let capture = capture(&event, &full_actor(), user(), NOW);
        assert!(!capture.properties.contains_key("via"));
        assert_eq!(
            capture.properties.get("route"),
            Some(&Value::String("/$org/$project/traces/".to_owned()))
        );
    }

    #[test]
    fn capture_without_org_or_role_has_no_groups_or_set() {
        let actor = Actor {
            org_id: None,
            org_role: None,
            ..full_actor()
        };
        let capture = capture(&Event::DatasetCreated, &actor, user(), NOW);
        assert!(!capture.properties.contains_key("$groups"));
        assert!(!capture.properties.contains_key("$set"));
    }

    #[test]
    fn group_identify_has_the_documented_shape() {
        let capture = group_identify(org(), "Self-hosted", NOW);
        let expected = r#"{
            "event": "$groupidentify",
            "distinct_id": "org_01900000-0000-7000-8000-000000000010",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "properties": {
                "distinct_id": "org_01900000-0000-7000-8000-000000000010",
                "$group_type": "org",
                "$group_key": "01900000-0000-7000-8000-000000000010",
                "$group_set": {"plan": "Self-hosted"},
                "$ip": null,
                "$geoip_disable": true,
                "$process_person_profile": false
            }
        }"#;
        assert_eq!(
            serde_json::to_string(&capture).expect("capture serializes"),
            minified(expected)
        );
    }

    #[test]
    fn group_due_rules() {
        let (sink, _worker) = PosthogSink::new(
            "http://127.0.0.1:9",
            Secret::new("phc_test".to_owned()),
            Duration::from_secs(1),
        )
        .expect("client builds");
        let hour = 3_600 * 1_000_000_000_i64;
        assert!(sink.group_due(org(), "Free", NOW));
        assert!(!sink.group_due(org(), "Free", NOW));
        assert!(sink.group_due(org(), "Pro", NOW));
        assert!(sink.group_due(org(), "Pro", NOW + hour));
        assert!(!sink.group_due(org(), "Pro", NOW + hour + hour - 60_000_000_000));
    }

    #[tokio::test]
    async fn the_worker_posts_a_batch_after_the_interval() {
        let receiver = Receiver::start(200).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_millis(50),
        )
        .expect("client builds");
        let task = tokio::spawn(worker.run());
        let captures: Vec<Capture> = (0..3).map(sample).collect();
        for capture in &captures {
            sink.enqueue(capture.clone());
        }

        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["api_key"], "phc_test");
        let batch = bodies[0]["batch"].as_array().expect("batch is an array");
        assert_eq!(batch.len(), 3);
        for (sent, expected) in batch.iter().zip(&captures) {
            assert_eq!(sent["event"], expected.event.as_str());
        }
        let requests = receiver.requests();
        let (headers, _) = &requests[0];
        assert!(
            headers["content-type"]
                .to_str()
                .expect("ascii")
                .starts_with("application/json")
        );
        assert_eq!(
            headers["user-agent"].to_str().expect("ascii"),
            format!("cetra/{}", env!("CARGO_PKG_VERSION"))
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_worker_flushes_at_100_events() {
        let receiver = Receiver::start(200).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_secs(3_600),
        )
        .expect("client builds");
        let task = tokio::spawn(worker.run());
        for n in 0..MAX_BATCH {
            sink.enqueue(sample(n));
        }
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["batch"].as_array().map(Vec::len), Some(MAX_BATCH));

        sink.enqueue(sample(MAX_BATCH));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(receiver.requests().len(), 1);
        task.abort();
    }

    #[tokio::test]
    async fn dropping_the_sink_flushes_and_stops() {
        let receiver = Receiver::start(200).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_secs(3_600),
        )
        .expect("client builds");
        let task = tokio::spawn(worker.run());
        sink.enqueue(sample(0));
        sink.enqueue(sample(1));
        drop(sink);

        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the worker stops within 2 s")
            .expect("the worker does not panic");
        let bodies = receiver.bodies();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["batch"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn a_full_queue_drops_new_events() {
        let receiver = Receiver::start(200).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_millis(50),
        )
        .expect("client builds");
        // The worker is not running yet, so nothing drains the queue.
        for n in 0..QUEUE_CAPACITY + 5 {
            sink.enqueue(sample(n));
        }
        let task = tokio::spawn(worker.run());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let total = |bodies: &[Value]| -> usize {
            bodies
                .iter()
                .map(|body| body["batch"].as_array().map_or(0, Vec::len))
                .sum()
        };
        loop {
            let bodies = receiver.bodies();
            if total(&bodies) >= QUEUE_CAPACITY || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(total(&receiver.bodies()), QUEUE_CAPACITY);
        task.abort();
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let receiver = Receiver::start(307).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_millis(50),
        )
        .expect("client builds");
        let task = tokio::spawn(worker.run());
        sink.enqueue(sample(0));
        let bodies = receiver.wait_for(1, Duration::from_secs(2)).await;
        assert_eq!(bodies.len(), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(receiver.requests().len(), 1);
        assert_eq!(
            receiver.elsewhere.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        task.abort();
    }

    #[tokio::test]
    async fn failures_are_logged_at_debug_once_a_minute() {
        let receiver = Receiver::start(503).await;
        let (sink, worker) = PosthogSink::new(
            &receiver.base,
            Secret::new("phc_test".to_owned()),
            Duration::from_millis(50),
        )
        .expect("client builds");
        let task = tokio::spawn(worker.run());
        let ((), logs) = capture_logs(async {
            sink.enqueue(sample(0));
            tokio::time::sleep(Duration::from_millis(200)).await;
            sink.enqueue(sample(1));
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await;

        assert_eq!(receiver.requests().len(), 2);
        assert!(!logs.contains("WARN"));
        assert!(!logs.contains("ERROR"));
        assert!(logs.matches("analytics: batch not sent").count() <= 1);
        assert!(!logs.contains(&receiver.base));
        assert!(!logs.contains("phc_test"));
        assert!(!logs.contains("/batch/"));
        task.abort();
    }

    #[test]
    fn the_key_never_reaches_debug_output() {
        let (_sink, worker) = PosthogSink::new(
            "http://127.0.0.1:9",
            Secret::new("phc_test".to_owned()),
            Duration::from_secs(1),
        )
        .expect("client builds");
        assert!(!format!("{worker:?}").contains("phc_test"));
    }
}
