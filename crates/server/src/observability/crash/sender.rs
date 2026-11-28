//! Posts spooled crash reports to `<telemetry.endpoint>/v1/crash` (D-TL-12): oldest first, with the instance id added; deleted on 2xx or on a 4xx other than 408 and 429, kept on anything else, dropped unsent after 7 days. Logs nothing itself; the loop in `start` logs a summary at `debug`.

use std::time::Duration;

use uuid::Uuid;

use super::report::{CRASH_SCHEMA, CrashReport, MAX_SPOOLED_BYTES};
use super::spool::Spool;

/// One pass a minute.
pub(crate) const SEND_PERIOD: Duration = Duration::from_secs(60);
/// A report not sent within this long is dropped.
pub(crate) const MAX_AGE_NS: i64 = 7 * 24 * 3_600 * 1_000_000_000;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SendSummary {
    /// Accepted (2xx) and deleted.
    pub(crate) sent: u32,
    /// Deleted unsent: refused (a 4xx other than 408 and 429), too old, too large, unreadable as a report.
    pub(crate) dropped: u32,
    /// Kept for the next pass: 408, 429, 1xx, 3xx, 5xx, network failure (the pass stops there).
    pub(crate) kept: u32,
}

/// `<endpoint>/v1/crash`.
pub(crate) fn crash_url(endpoint: &str) -> String {
    format!("{}/v1/crash", endpoint.trim_end_matches('/'))
}

/// One pass over the spool: posts each report, oldest first, and deletes, keeps or drops it
/// by the response (module doc). Never logs and never panics: a failure is a kept report.
pub(crate) async fn send_spool(
    client: &reqwest::Client,
    url: &str,
    spool: &Spool,
    instance_id: Option<Uuid>,
    now_ns: i64,
) -> SendSummary {
    let mut summary = SendSummary::default();
    let Ok(files) = spool.list() else {
        return SendSummary::default();
    };
    for file in files {
        if now_ns.saturating_sub(file.written_at_ns) > MAX_AGE_NS {
            let _ = tokio::fs::remove_file(&file.path).await;
            summary.dropped += 1;
            continue;
        }
        let bytes = match tokio::fs::read(&file.path).await {
            Ok(bytes) => bytes,
            // Pruned by the writer or by another pass meanwhile.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                summary.kept += 1;
                continue;
            }
        };
        if bytes.len() > MAX_SPOOLED_BYTES {
            let _ = tokio::fs::remove_file(&file.path).await;
            summary.dropped += 1;
            continue;
        }
        let mut report = match serde_json::from_slice::<CrashReport>(&bytes) {
            Ok(report) if report.schema == CRASH_SCHEMA => report,
            _ => {
                let _ = tokio::fs::remove_file(&file.path).await;
                summary.dropped += 1;
                continue;
            }
        };
        report.instance_id = instance_id;
        match client.post(url).json(&report).send().await {
            Err(_) => {
                summary.kept += 1;
                break;
            }
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let _ = tokio::fs::remove_file(&file.path).await;
                    summary.sent += 1;
                } else if status.as_u16() == 408 || status.as_u16() == 429 {
                    summary.kept += 1;
                    break;
                } else if status.is_client_error() {
                    let _ = tokio::fs::remove_file(&file.path).await;
                    summary.dropped += 1;
                } else {
                    summary.kept += 1;
                    break;
                }
            }
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use axum::http::header;

    use super::*;
    use crate::observability::crash::report::{EventSite, event_report, location_of_event};
    use crate::observability::crash::spool::file_name;
    use crate::observability::facts::{self, Deployment};
    use crate::observability::heartbeat;
    use crate::observability::test_receiver::Receiver;

    /// A fixed "now": 2026-01-01T00:00Z, as `MANUAL_NOW_NS` in the test support.
    const NOW: i64 = 1_767_225_600_000_000_000;

    fn client() -> reqwest::Client {
        heartbeat::client(heartbeat::REQUEST_TIMEOUT).unwrap()
    }

    fn site(line: u32) -> EventSite<'static> {
        EventSite {
            file: Some("crates/x.rs"),
            line: Some(line),
            target: "cetra_x",
            module_path: None,
            fields: Vec::new(),
        }
    }

    /// Writes one report at `line` into `spool`, at `written_at_ns`.
    fn write_report(spool: &Spool, line: u32, written_at_ns: i64) -> PathBuf {
        let report = event_report(&site(line), Vec::new(), Deployment::Binary, Duration::ZERO);
        spool.write(&report, written_at_ns).unwrap()
    }

    /// A spool in a fresh temporary directory, and that directory (kept alive by the caller).
    fn spool_in(dir: &tempfile::TempDir) -> Spool {
        Spool::new(dir.path().join("spool"))
    }

    #[test]
    fn crash_url_appends_the_path() {
        assert_eq!(
            crash_url("https://telemetry.cetra.sh"),
            "https://telemetry.cetra.sh/v1/crash"
        );
        assert_eq!(
            crash_url("https://r.example.com/cetra/"),
            "https://r.example.com/cetra/v1/crash"
        );
    }

    #[tokio::test]
    async fn a_report_is_posted_with_the_instance_id_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        let path = write_report(&spool, 1, NOW);
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;
        let id = Uuid::from_u128(42);
        let url = crash_url(&receiver.base);

        let summary = send_spool(&client(), &url, &spool, Some(id), NOW).await;

        assert_eq!(
            summary,
            SendSummary {
                sent: 1,
                dropped: 0,
                kept: 0
            }
        );
        let requests = receiver.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        let report: CrashReport = serde_json::from_slice(body).unwrap();
        assert_eq!(report.instance_id, Some(id));
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        assert_eq!(
            headers[header::USER_AGENT].to_str().unwrap(),
            format!("cetra/{}", facts::VERSION)
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn without_an_instance_id_the_key_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        write_report(&spool, 1, NOW);
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert_eq!(summary.sent, 1);
        let body: serde_json::Value = serde_json::from_slice(&receiver.bodies()[0]).unwrap();
        assert!(!body.as_object().unwrap().contains_key("instance_id"));
    }

    #[tokio::test]
    async fn a_4xx_deletes_the_report() {
        for status in [400, 413] {
            let dir = tempfile::tempdir().unwrap();
            let spool = spool_in(&dir);
            let path = write_report(&spool, 1, NOW);
            let receiver = Receiver::start("/v1/crash", status, Duration::ZERO).await;

            let summary =
                send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

            assert_eq!(
                summary,
                SendSummary {
                    sent: 0,
                    dropped: 1,
                    kept: 0
                },
                "{status}"
            );
            assert!(!path.exists(), "{status}");
        }
    }

    #[tokio::test]
    async fn a_408_or_429_keeps_the_report_and_stops_the_pass() {
        for status in [408, 429] {
            let dir = tempfile::tempdir().unwrap();
            let spool = spool_in(&dir);
            write_report(&spool, 1, NOW - 2);
            write_report(&spool, 2, NOW - 1);
            let receiver = Receiver::start("/v1/crash", status, Duration::ZERO).await;

            let summary =
                send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

            assert_eq!(receiver.requests().len(), 1, "{status}");
            assert_eq!(spool.list().unwrap().len(), 2, "{status}");
            assert_eq!(
                summary,
                SendSummary {
                    sent: 0,
                    dropped: 0,
                    kept: 1
                },
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_5xx_keeps_the_report_and_stops_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        write_report(&spool, 1, NOW - 2);
        write_report(&spool, 2, NOW - 1);
        let receiver = Receiver::start("/v1/crash", 503, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert_eq!(receiver.requests().len(), 1);
        assert_eq!(spool.list().unwrap().len(), 2);
        assert_eq!(
            summary,
            SendSummary {
                sent: 0,
                dropped: 0,
                kept: 1
            }
        );
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_and_keeps_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        let path = write_report(&spool, 1, NOW);
        let receiver = Receiver::start("/v1/crash", 307, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert_eq!(receiver.elsewhere_hits(), 0);
        assert!(path.exists());
        assert_eq!(
            summary,
            SendSummary {
                sent: 0,
                dropped: 0,
                kept: 1
            }
        );
    }

    #[tokio::test]
    async fn a_network_failure_keeps_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        let path = write_report(&spool, 1, NOW);
        // A port nothing listens on: bind, note the address, close the listener.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/crash", listener.local_addr().unwrap());
        drop(listener);

        let summary = send_spool(&client(), &url, &spool, None, NOW).await;

        assert!(path.exists());
        assert_eq!(
            summary,
            SendSummary {
                sent: 0,
                dropped: 0,
                kept: 1
            }
        );
    }

    #[tokio::test]
    async fn reports_older_than_seven_days_are_dropped_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        let old = write_report(&spool, 1, NOW - MAX_AGE_NS - 1);
        let edge = write_report(&spool, 2, NOW - MAX_AGE_NS);
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert!(!old.exists());
        assert!(!edge.exists());
        assert_eq!(
            summary,
            SendSummary {
                sent: 1,
                dropped: 1,
                kept: 0
            }
        );
        // Only the report at the edge was posted.
        let reports: Vec<CrashReport> = receiver
            .bodies()
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].location,
            location_of_event(Some("crates/x.rs"), Some(2))
        );
    }

    #[tokio::test]
    async fn a_foreign_or_corrupt_file_is_dropped_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        std::fs::create_dir_all(dir.path().join("spool")).unwrap();
        let foreign = dir.path().join("spool").join(file_name(NOW - 3, 1));
        std::fs::write(&foreign, r#"{"schema":1,"message":"x"}"#).unwrap();
        // A full report with the panic schema number 2.
        let mut value = serde_json::to_value(event_report(
            &site(1),
            Vec::new(),
            Deployment::Binary,
            Duration::ZERO,
        ))
        .unwrap();
        value["schema"] = serde_json::json!(2);
        let wrong_schema = dir.path().join("spool").join(file_name(NOW - 2, 2));
        std::fs::write(&wrong_schema, serde_json::to_vec(&value).unwrap()).unwrap();
        let corrupt = dir.path().join("spool").join(file_name(NOW - 1, 3));
        std::fs::write(&corrupt, "not json").unwrap();
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert!(!foreign.exists());
        assert!(!wrong_schema.exists());
        assert!(!corrupt.exists());
        assert!(receiver.requests().is_empty());
        assert_eq!(
            summary,
            SendSummary {
                sent: 0,
                dropped: 3,
                kept: 0
            }
        );
    }

    #[tokio::test]
    async fn an_oversized_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        std::fs::create_dir_all(dir.path().join("spool")).unwrap();
        let big = dir.path().join("spool").join(file_name(NOW - 1, 1));
        std::fs::write(&big, vec![b' '; MAX_SPOOLED_BYTES + 1]).unwrap();
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert!(!big.exists());
        assert!(receiver.requests().is_empty());
        assert_eq!(
            summary,
            SendSummary {
                sent: 0,
                dropped: 1,
                kept: 0
            }
        );
    }

    #[tokio::test]
    async fn the_oldest_report_goes_first() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        // Written in the reverse order of their times.
        write_report(&spool, 2, NOW - 1);
        write_report(&spool, 1, NOW - 2);
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        let locations: Vec<String> = receiver
            .bodies()
            .iter()
            .map(|body| {
                serde_json::from_slice::<CrashReport>(body)
                    .unwrap()
                    .location
            })
            .collect();
        assert_eq!(
            locations,
            vec![
                location_of_event(Some("crates/x.rs"), Some(1)),
                location_of_event(Some("crates/x.rs"), Some(2)),
            ]
        );
    }

    #[tokio::test]
    async fn an_empty_or_missing_spool_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let spool = spool_in(&dir);
        let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;

        let summary = send_spool(&client(), &crash_url(&receiver.base), &spool, None, NOW).await;

        assert_eq!(summary, SendSummary::default());
        assert!(receiver.requests().is_empty());
    }
}
