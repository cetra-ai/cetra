//! Feature counters (design § 6): every catalog event, from any source, adds one to its name's counter. The heartbeat sends them as buckets in `usage` and subtracts what was accepted. While the heartbeat runs they are saved hourly to `instance_settings`; otherwise they stay in memory and nothing reads them. No users, orgs, sequences or timings. This module reads nothing of the licence code (D172).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::task::JoinHandle;

use crate::analytics::catalog::{CATALOG, static_name};
use crate::http::AppState;
use crate::meta::instance_settings::{TELEMETRY_USAGE_COUNTS, get_setting, put_setting};
use crate::observability::switchboard::{Decision, Switch};

/// How often the counts are saved while the heartbeat runs.
pub(crate) const SAVE_PERIOD: Duration = Duration::from_secs(60 * 60);

/// Counts by catalog event name.
pub(crate) type Counts = BTreeMap<&'static str, u64>;

/// A count since the last heartbeat, as sent (D-TL-8: never an exact count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum UsageBucket {
    #[serde(rename = "0")]
    Zero,
    #[serde(rename = "1-9")]
    UnderTen,
    #[serde(rename = "10-99")]
    UnderHundred,
    #[serde(rename = "100-999")]
    UnderThousand,
    #[serde(rename = "1000+")]
    ThousandOrMore,
}

/// `n` as a [`UsageBucket`].
pub(crate) fn usage_bucket(n: u64) -> UsageBucket {
    match n {
        0 => UsageBucket::Zero,
        1..=9 => UsageBucket::UnderTen,
        10..=99 => UsageBucket::UnderHundred,
        100..=999 => UsageBucket::UnderThousand,
        _ => UsageBucket::ThousandOrMore,
    }
}

/// The heartbeat's `usage`: every catalog name, with its bucket (`0` when not counted).
pub(crate) fn usage_buckets(counts: &Counts) -> BTreeMap<&'static str, UsageBucket> {
    CATALOG
        .iter()
        .map(|spec| {
            let count = counts.get(spec.name).copied().unwrap_or(0);
            (spec.name, usage_bucket(count))
        })
        .collect()
}

/// The in-memory counters.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    counts: Mutex<Counts>,
}

impl Counters {
    fn lock(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds one to `name` (saturating).
    pub(crate) fn increment(&self, name: &'static str) {
        let mut counts = self.lock();
        let count = counts.entry(name).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// The count of `name`. Read by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn get(&self, name: &str) -> u64 {
        self.lock().get(name).copied().unwrap_or(0)
    }

    /// A copy of every count.
    pub(crate) fn snapshot(&self) -> Counts {
        self.lock().clone()
    }

    /// Removes what a heartbeat sent; events counted since the snapshot stay.
    pub(crate) fn subtract(&self, sent: &Counts) {
        let mut counts = self.lock();
        for (name, sent) in sent {
            if let Some(count) = counts.get_mut(name) {
                *count = count.saturating_sub(*sent);
            }
        }
        counts.retain(|_, count| *count > 0);
    }

    /// Adds saved counts (at startup).
    pub(crate) fn add(&self, saved: &Counts) {
        let mut counts = self.lock();
        for (name, saved) in saved {
            let count = counts.entry(*name).or_insert(0);
            *count = count.saturating_add(*saved);
        }
    }
}

/// The saved form: a JSON object of name to count.
pub(crate) fn encode(counts: &Counts) -> String {
    // A map of strings to integers always serializes; "{}" is never reached.
    serde_json::to_string(counts).unwrap_or_else(|_| "{}".to_owned())
}

/// Reads the saved form. Names no longer in the catalog and unreadable text are dropped:
/// a lost count is the safe side.
pub(crate) fn decode(text: &str) -> Counts {
    let Ok(parsed) = serde_json::from_str::<BTreeMap<String, u64>>(text) else {
        return Counts::new();
    };
    parsed
        .into_iter()
        .filter_map(|(name, count)| static_name(&name).map(|name| (name, count)))
        .collect()
}

/// Adds the saved counts to the in-memory counters. A read that fails is logged at `debug`
/// and leaves the counters as they are.
pub(crate) async fn load(state: &AppState) {
    match get_setting(state.meta.reader(), TELEMETRY_USAGE_COUNTS).await {
        Ok(Some(text)) => state.analytics.counters.add(&decode(&text)),
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(error = %error, "analytics: saved feature counts not loaded");
        }
    }
}

/// Saves the current counts to `instance_settings`. A failed write is logged at `debug`.
pub(crate) async fn save(state: &AppState) {
    let text = encode(&state.analytics.counters.snapshot());
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move { put_setting(conn, TELEMETRY_USAGE_COUNTS, &text).await })
        })
        .await;
    if let Err(error) = written {
        tracing::debug!(error = %error, "analytics: feature counts not saved");
    }
}

/// Loads the saved counts and saves them every hour, only while the heartbeat runs (the
/// switchboard). Returns `None` when the heartbeat is off: nothing is read or written then.
pub(crate) async fn start(state: &AppState, decision: Decision) -> Option<JoinHandle<()>> {
    if let Switch::Off(_) = decision.heartbeat {
        return None;
    }
    load(state).await;
    let state = state.clone();
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(SAVE_PERIOD).await;
            save(&state).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::switchboard::{OffReason, for_config};
    use crate::test_support::TestApp;

    /// A switchboard decision with `heartbeat` set, whatever other fields `Decision` has.
    fn decision(heartbeat: Switch) -> Decision {
        let mut decision = for_config(&crate::test_support::config(std::path::Path::new(".")));
        decision.heartbeat = heartbeat;
        decision
    }

    fn counts(pairs: &[(&'static str, u64)]) -> Counts {
        pairs.iter().copied().collect()
    }

    #[test]
    fn usage_bucket_edges() {
        assert_eq!(usage_bucket(0), UsageBucket::Zero);
        assert_eq!(usage_bucket(1), UsageBucket::UnderTen);
        assert_eq!(usage_bucket(9), UsageBucket::UnderTen);
        assert_eq!(usage_bucket(10), UsageBucket::UnderHundred);
        assert_eq!(usage_bucket(99), UsageBucket::UnderHundred);
        assert_eq!(usage_bucket(100), UsageBucket::UnderThousand);
        assert_eq!(usage_bucket(999), UsageBucket::UnderThousand);
        assert_eq!(usage_bucket(1000), UsageBucket::ThousandOrMore);
        assert_eq!(usage_bucket(u64::MAX), UsageBucket::ThousandOrMore);
    }

    #[test]
    fn bucket_strings_are_the_wire_format() {
        let names: Vec<String> = [
            UsageBucket::Zero,
            UsageBucket::UnderTen,
            UsageBucket::UnderHundred,
            UsageBucket::UnderThousand,
            UsageBucket::ThousandOrMore,
        ]
        .into_iter()
        .map(|bucket| serde_json::to_string(&bucket).expect("serializes"))
        .collect();
        assert_eq!(
            names,
            [
                r#""0""#,
                r#""1-9""#,
                r#""10-99""#,
                r#""100-999""#,
                r#""1000+""#
            ]
        );
    }

    #[test]
    fn usage_buckets_list_every_catalog_name() {
        let buckets = usage_buckets(&Counts::new());
        assert_eq!(buckets.len(), CATALOG.len());
        assert_eq!(buckets.len(), 40);
        assert!(buckets.values().all(|b| *b == UsageBucket::Zero));
        let keys: Vec<&str> = buckets.keys().copied().collect();
        let names: Vec<&str> = CATALOG.iter().map(|spec| spec.name).collect();
        assert_eq!(keys, names);
    }

    #[test]
    fn usage_buckets_bucket_the_counts() {
        let buckets = usage_buckets(&counts(&[("trace.viewed", 1500), ("page.viewed", 250)]));
        assert_eq!(buckets["trace.viewed"], UsageBucket::ThousandOrMore);
        assert_eq!(buckets["page.viewed"], UsageBucket::UnderThousand);
        for (name, bucket) in &buckets {
            if *name != "trace.viewed" && *name != "page.viewed" {
                assert_eq!(*bucket, UsageBucket::Zero, "name {name}");
            }
        }
    }

    #[test]
    fn increment_get_and_snapshot() {
        let counters = Counters::default();
        for _ in 0..3 {
            counters.increment("dataset.created");
        }
        assert_eq!(counters.get("dataset.created"), 3);
        assert_eq!(counters.get("org.created"), 0);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get("dataset.created"), Some(&3));
    }

    #[test]
    fn subtract_keeps_what_came_after() {
        let counters = Counters::default();
        for _ in 0..5 {
            counters.increment("dataset.created");
        }
        let sent = counters.snapshot();
        counters.increment("dataset.created");
        counters.increment("dataset.created");
        counters.subtract(&sent);
        assert_eq!(counters.get("dataset.created"), 2);

        counters.subtract(&counts(&[("dataset.created", 100)]));
        assert_eq!(counters.get("dataset.created"), 0);
        assert!(counters.snapshot().is_empty());
    }

    #[test]
    fn add_merges_saved_counts() {
        let counters = Counters::default();
        counters.increment("dataset.created");
        counters.add(&counts(&[("dataset.created", 4), ("org.created", 1)]));
        assert_eq!(counters.get("dataset.created"), 5);
        assert_eq!(counters.get("org.created"), 1);
        counters.add(&counts(&[("dataset.created", u64::MAX)]));
        assert_eq!(counters.get("dataset.created"), u64::MAX);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let original = counts(&[("dataset.created", 2), ("org.created", 7)]);
        let text = encode(&original);
        assert_eq!(text, r#"{"dataset.created":2,"org.created":7}"#);
        assert_eq!(decode(&text), original);
    }

    #[test]
    fn decode_drops_unknown_names_and_bad_text() {
        assert_eq!(
            decode(r#"{"dataset.created": 2, "old.event": 9}"#),
            counts(&[("dataset.created", 2)])
        );
        for text in ["not json", "[]", r#"{"dataset.created": -1}"#] {
            assert!(decode(text).is_empty(), "text {text:?}");
        }
    }

    #[test]
    fn concurrent_increments_are_not_lost() {
        let counters = Counters::default();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        counters.increment("dataset.created");
                    }
                });
            }
        });
        assert_eq!(counters.get("dataset.created"), 8000);
    }

    #[tokio::test]
    async fn start_is_off_without_the_heartbeat() {
        let app = TestApp::new().await;
        let handle = start(&app.state, decision(Switch::Off(OffReason::DebugBuild))).await;
        assert!(handle.is_none());
        let saved = get_setting(app.state.meta.reader(), TELEMETRY_USAGE_COUNTS)
            .await
            .expect("reads");
        assert_eq!(saved, None);
    }

    #[tokio::test]
    async fn start_loads_saved_counts() {
        let app = TestApp::new().await;
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    put_setting(conn, TELEMETRY_USAGE_COUNTS, r#"{"org.created":4}"#).await
                })
            })
            .await
            .expect("writes");
        let handle = start(&app.state, decision(Switch::On)).await;
        assert!(handle.is_some());
        if let Some(handle) = handle {
            handle.abort();
        }
        assert_eq!(app.state.analytics.counters.get("org.created"), 4);
    }

    #[tokio::test]
    async fn save_writes_the_snapshot() {
        let app = TestApp::new().await;
        app.state.analytics.counters.increment("dataset.created");
        app.state.analytics.counters.increment("dataset.created");
        save(&app.state).await;
        let saved = get_setting(app.state.meta.reader(), TELEMETRY_USAGE_COUNTS)
            .await
            .expect("reads");
        assert_eq!(saved.as_deref(), Some(r#"{"dataset.created":2}"#));
    }

    #[test]
    fn the_counter_modules_do_not_touch_licences() {
        let sources = [include_str!("counters.rs"), include_str!("catalog.rs")];
        let forbidden = [
            concat!("crate::", "ee"),
            concat!("licen", "se"),
            concat!("entitle", "ments"),
        ];
        for source in sources {
            let code = source.split("#[cfg(test)]").next().unwrap_or(source);
            for word in forbidden {
                assert!(!code.contains(word), "a counter module mentions {word}");
            }
        }
    }
}
