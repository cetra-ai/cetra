//! The admin socket's `debug_info` (track TL § 4): the running server's state for a bug report. Every part that fails is listed in `errors` by name and static kind, so the answer always comes back.

use cetra_storage::{StorageHealth, StorageStats};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http::AppState;

use super::disk::{LevelFiles, catalog_levels};
use super::heartbeat::StorageKind;
use super::ring::LogRing;
use super::switchboard::Switch;
use super::{facts, heartbeat, instance, switchboard};

/// The `debug_info` schema version. Within admin protocol 1 fields are only added. A new field
/// of `DebugInfo` or of any type nested in it is an `Option` or carries `#[serde(default)]`, so
/// a recorded answer (the golden socket lines) keeps decoding (D-R-36).
pub const DEBUG_INFO_SCHEMA: u32 = 1;

/// What `debug_info` answers: the running server's state for a bug report (track TL § 4).
/// Counts are exact (the file stays local); no value is a payload, a secret or an email.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugInfo {
    /// [`DEBUG_INFO_SCHEMA`].
    pub schema: u32,
    /// The server's version (it may differ from the CLI's after an upgrade).
    pub version: String,
    /// Seconds since the process started (`facts::process_uptime`).
    pub uptime_secs: u64,
    /// The ring buffer, `None` when the process installed none (tests).
    pub logs: Option<LogsView>,
    /// Storage statistics.
    pub storage: StorageView,
    /// The `/readyz` checks.
    pub ready: ReadyView,
    /// `meta.db` counts, `None` when the query failed (see `errors`).
    pub meta: Option<MetaCounts>,
    /// The export to the operator's OTLP endpoint (M1).
    pub self_monitoring: SelfMonitoringState,
    /// What may go to us and what would be sent (M4).
    pub telemetry: TelemetryState,
    /// The parts that could not be read, by name and static kind. Never error text.
    pub errors: Vec<SectionError>,
}

/// The ring buffer's lines, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsView {
    /// JSON lines at the stdout filter.
    pub lines: Vec<String>,
    /// Lines the ring dropped since the process started (its bounds).
    pub dropped: u64,
    /// Lines were removed to fit the response or the bundle cap.
    pub trimmed: bool,
}

/// One value per table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerTable {
    pub spans: u64,
    pub scores: u64,
}

/// `StorageStats`, with named tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsView {
    pub wal_bytes: PerTable,
    pub flushes_ok: PerTable,
    pub flushes_failed: PerTable,
    pub flush_nanos: PerTable,
    pub compactions_ok: u64,
    pub compactions_failed: u64,
    pub compactions_deferred: u64,
    pub compactions_conflict: u64,
    pub compaction_bytes_written: u64,
    pub gc_objects_deleted: u64,
}

/// `StorageHealth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthView {
    pub spans_wal_failed: bool,
    pub scores_wal_failed: bool,
    pub objects_sync_failed: bool,
    pub catalog_failed: bool,
    pub memtable_bytes: u64,
    pub memtable_hard_limit: u64,
    pub disk_low: bool,
    pub disk_free_bytes: u64,
    pub disk_min_free_bytes: u64,
}

/// The storage part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageView {
    /// `file` or `s3`.
    pub object_store: String,
    pub stats: StatsView,
    pub health: HealthView,
    /// Live files per table and level, from `catalog.db` (empty when unreadable; see `errors`).
    pub catalog_files: Vec<LevelFiles>,
}

/// The `/readyz` checks (D-N-30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyView {
    /// No check failed.
    pub ready: bool,
    /// The failed checks' names, in `/readyz` order.
    pub failed: Vec<String>,
}

/// `meta.db` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaCounts {
    pub orgs: u64,
    pub active_orgs: u64,
    pub active_projects: u64,
    pub users: u64,
    pub disabled_users: u64,
    /// API keys not revoked.
    pub api_keys: u64,
    /// Personal access tokens not revoked.
    pub personal_tokens: u64,
    pub alert_rules: u64,
    pub online_rules: u64,
}

/// The self-monitoring export (M1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfMonitoringState {
    /// `self_monitoring.exports()`.
    pub exporting: bool,
    /// Log records are exported (`logs_endpoint().is_some()`).
    pub logs: bool,
    /// The endpoint's host, never its path or headers.
    pub endpoint_host: Option<String>,
    /// `self_monitoring.level`.
    pub level: String,
    /// Why nothing is exported: `no_endpoint` or `logs_off`; `None` when exporting.
    pub reason: Option<String>,
}

/// On, or off with the switchboard's reason (`OffReason::as_str`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchView {
    pub on: bool,
    pub reason: Option<String>,
}

/// What may go to us (D-TL-11) and the heartbeat as it would be sent now (D-TL-8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryState {
    /// The switchboard's heartbeat decision.
    pub heartbeat: SwitchView,
    /// `instance_settings['telemetry.instance_id']`, `None` when none was created yet.
    pub instance_id: Option<Uuid>,
    /// `telemetry.endpoint`.
    pub endpoint: String,
    /// `heartbeat::collect`, serialized; shown even
    /// when the heartbeat is off; the nil UUID stands for an id not created yet. `None` when
    /// it could not be built (see `errors`).
    pub heartbeat_preview: Option<serde_json::Value>,
    /// The switchboard's crash-report decision.
    pub crash_reports: SwitchView,
}

/// A part of `debug_info` that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionError {
    /// The part, for example `meta` or `storage.catalog_files`.
    pub section: String,
    /// A static kind, for example `query_failed`.
    pub kind: String,
}

impl From<[u64; 2]> for PerTable {
    fn from([spans, scores]: [u64; 2]) -> Self {
        Self { spans, scores }
    }
}

impl From<StorageStats> for StatsView {
    fn from(stats: StorageStats) -> Self {
        let StorageStats {
            wal_bytes,
            flushes_ok,
            flushes_failed,
            flush_nanos,
            compactions_ok,
            compactions_failed,
            compactions_deferred,
            compactions_conflict,
            compaction_bytes_written,
            gc_objects_deleted,
        } = stats;
        Self {
            wal_bytes: wal_bytes.into(),
            flushes_ok: flushes_ok.into(),
            flushes_failed: flushes_failed.into(),
            flush_nanos: flush_nanos.into(),
            compactions_ok,
            compactions_failed,
            compactions_deferred,
            compactions_conflict,
            compaction_bytes_written,
            gc_objects_deleted,
        }
    }
}

impl From<StorageHealth> for HealthView {
    fn from(health: StorageHealth) -> Self {
        let StorageHealth {
            spans_wal_failed,
            scores_wal_failed,
            objects_sync_failed,
            catalog_failed,
            memtable_bytes,
            memtable_hard_limit,
            disk_low,
            disk_free_bytes,
            disk_min_free_bytes,
        } = health;
        Self {
            spans_wal_failed,
            scores_wal_failed,
            objects_sync_failed,
            catalog_failed,
            memtable_bytes,
            memtable_hard_limit,
            disk_low,
            disk_free_bytes,
            disk_min_free_bytes,
        }
    }
}

impl SwitchView {
    fn of(switch: Switch) -> Self {
        match switch {
            Switch::On => Self {
                on: true,
                reason: None,
            },
            Switch::Off(reason) => Self {
                on: false,
                reason: Some(reason.as_str().to_owned()),
            },
        }
    }
}

const META_COUNTS_SQL: &str = "SELECT \
    (SELECT COUNT(*) FROM orgs), \
    (SELECT COUNT(*) FROM orgs WHERE state = 'active'), \
    (SELECT COUNT(*) FROM projects WHERE state = 'active'), \
    (SELECT COUNT(*) FROM users), \
    (SELECT COUNT(*) FROM users WHERE disabled_at IS NOT NULL), \
    (SELECT COUNT(*) FROM api_keys WHERE revoked_at IS NULL), \
    (SELECT COUNT(*) FROM personal_tokens WHERE revoked_at IS NULL), \
    (SELECT COUNT(*) FROM alert_rules), \
    (SELECT COUNT(*) FROM online_rules)";

async fn meta_counts(state: &AppState) -> Result<MetaCounts, sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64, i64)>(META_COUNTS_SQL)
        .fetch_one(state.meta.reader())
        .await?;
    let count = |value: i64| u64::try_from(value).unwrap_or(0);
    Ok(MetaCounts {
        orgs: count(row.0),
        active_orgs: count(row.1),
        active_projects: count(row.2),
        users: count(row.3),
        disabled_users: count(row.4),
        api_keys: count(row.5),
        personal_tokens: count(row.6),
        alert_rules: count(row.7),
        online_rules: count(row.8),
    })
}

fn self_monitoring_state(config: &crate::config::SelfMonitoringConfig) -> SelfMonitoringState {
    let exporting = config.exports();
    let logs = config.logs_endpoint().is_some();
    let reason = if exporting {
        None
    } else if config.otlp_endpoint.is_none() {
        Some("no_endpoint".to_owned())
    } else {
        Some("logs_off".to_owned())
    };
    SelfMonitoringState {
        exporting,
        logs,
        endpoint_host: config.otlp_endpoint.as_ref().map(|e| e.host().to_owned()),
        level: config.level.clone(),
        reason,
    }
}

/// The running server's state now. Never fails: a part that cannot be read is empty and listed in `errors`.
pub(crate) async fn collect(state: &AppState, ring: Option<&LogRing>) -> DebugInfo {
    let mut errors = Vec::new();

    let logs = ring.map(|ring| {
        let snapshot = ring.snapshot();
        LogsView {
            lines: snapshot.lines,
            dropped: snapshot.dropped,
            trimmed: false,
        }
    });

    let stats = StatsView::from(state.storage.stats().await);
    let health = HealthView::from(state.storage.health());

    let catalog_files = match catalog_levels(&state.config.data_dir.join("catalog.db"), false).await
    {
        Ok(levels) => levels,
        Err(_) => {
            errors.push(section_error("storage.catalog_files", "unreadable"));
            Vec::new()
        }
    };

    let object_store = match heartbeat::storage_kind(&state.config) {
        StorageKind::File => "file",
        StorageKind::S3 => "s3",
    }
    .to_owned();

    let failed = crate::http::health::failed_checks(state).await;
    let ready = ReadyView {
        ready: failed.is_empty(),
        failed: failed.iter().map(|name| (*name).to_owned()).collect(),
    };

    let meta = match meta_counts(state).await {
        Ok(counts) => Some(counts),
        Err(_) => {
            errors.push(section_error("meta", "query_failed"));
            None
        }
    };

    let decision = switchboard::for_config(&state.config);
    let instance_id = match instance::existing_instance_id(&state.meta).await {
        Ok(id) => id,
        Err(_) => {
            errors.push(section_error("telemetry.instance_id", "unreadable"));
            None
        }
    };
    let preview = heartbeat::collect(
        &state.meta,
        &state.config,
        instance_id.unwrap_or(Uuid::nil()),
        facts::deployment(facts::probe_host()),
        facts::process_uptime(),
        state.clock.now_ns(),
    )
    .await;
    let heartbeat_preview = match preview {
        Ok(heartbeat) => serde_json::to_value(heartbeat).ok(),
        Err(_) => {
            errors.push(section_error("telemetry.heartbeat_preview", "query_failed"));
            None
        }
    };
    let telemetry = TelemetryState {
        heartbeat: SwitchView::of(decision.heartbeat),
        instance_id,
        endpoint: state.config.telemetry.endpoint.clone(),
        heartbeat_preview,
        crash_reports: SwitchView::of(decision.crash_reports),
    };

    DebugInfo {
        schema: DEBUG_INFO_SCHEMA,
        version: facts::VERSION.to_owned(),
        uptime_secs: facts::process_uptime().as_secs(),
        logs,
        storage: StorageView {
            object_store,
            stats,
            health,
            catalog_files,
        },
        ready,
        meta,
        self_monitoring: self_monitoring_state(&state.config.self_monitoring),
        telemetry,
        errors,
    }
}

/// A part of `debug_info` that could not be read, by name and static kind.
fn section_error(section: &str, kind: &str) -> SectionError {
    SectionError {
        section: section.to_owned(),
        kind: kind.to_owned(),
    }
}

/// A fixed `DebugInfo` for the tests of this module and of the socket and the bundle.
#[cfg(test)]
pub(crate) fn sample() -> DebugInfo {
    let zero = PerTable {
        spans: 0,
        scores: 0,
    };
    DebugInfo {
        schema: 1,
        version: "0.0.0-test".to_owned(),
        uptime_secs: 0,
        logs: Some(LogsView {
            lines: vec!["{\"fields\":{\"message\":\"sample\"}}".into()],
            dropped: 0,
            trimmed: false,
        }),
        storage: StorageView {
            object_store: "file".to_owned(),
            stats: StatsView {
                wal_bytes: zero,
                flushes_ok: zero,
                flushes_failed: zero,
                flush_nanos: zero,
                compactions_ok: 0,
                compactions_failed: 0,
                compactions_deferred: 0,
                compactions_conflict: 0,
                compaction_bytes_written: 0,
                gc_objects_deleted: 0,
            },
            health: HealthView {
                spans_wal_failed: false,
                scores_wal_failed: false,
                objects_sync_failed: false,
                catalog_failed: false,
                memtable_bytes: 0,
                memtable_hard_limit: 0,
                disk_low: false,
                disk_free_bytes: 0,
                disk_min_free_bytes: 0,
            },
            catalog_files: Vec::new(),
        },
        ready: ReadyView {
            ready: true,
            failed: Vec::new(),
        },
        meta: None,
        self_monitoring: SelfMonitoringState {
            exporting: false,
            logs: false,
            endpoint_host: None,
            level: "warn".to_owned(),
            reason: Some("no_endpoint".to_owned()),
        },
        telemetry: TelemetryState {
            heartbeat: SwitchView {
                on: false,
                reason: Some("debug_build".to_owned()),
            },
            instance_id: None,
            endpoint: "http://127.0.0.1:9".to_owned(),
            heartbeat_preview: None,
            crash_reports: SwitchView {
                on: false,
                reason: Some("config".to_owned()),
            },
        },
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::actor::{Actor, Via};
    use crate::analytics::catalog::Event;
    use crate::analytics::record;
    use crate::config::parse_otlp_endpoint;
    use crate::meta::instance_settings::{TELEMETRY_INSTANCE_ID, get_setting};
    use crate::test_support::TestApp;

    #[tokio::test(flavor = "current_thread")]
    async fn a_fresh_server_reports_its_state() {
        let app = TestApp::new().await;
        let info = collect(&app.state, None).await;
        assert_eq!(info.schema, 1);
        assert_eq!(info.version, facts::VERSION);
        assert!(info.errors.is_empty(), "{:?}", info.errors);
        assert!(info.ready.ready);
        assert!(info.storage.catalog_files.is_empty());
        assert_eq!(
            info.meta,
            Some(MetaCounts {
                orgs: 0,
                active_orgs: 0,
                active_projects: 0,
                users: 0,
                disabled_users: 0,
                api_keys: 0,
                personal_tokens: 0,
                alert_rules: 0,
                online_rules: 0,
            })
        );
        assert_eq!(info.storage.object_store, "file");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn counts_follow_the_rows() {
        let app = TestApp::new().await;
        app.seed().await;
        let meta = collect(&app.state, None).await.meta.unwrap();
        assert_eq!(meta.orgs, 1);
        assert_eq!(meta.active_orgs, 1);
        assert_eq!(meta.active_projects, 1);
        assert_eq!(meta.users, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_telemetry_state_says_why_and_creates_no_id() {
        let app = TestApp::new().await;
        let info = collect(&app.state, None).await;
        assert_eq!(
            info.telemetry.heartbeat,
            SwitchView {
                on: false,
                reason: Some("debug_build".into()),
            }
        );
        assert_eq!(info.telemetry.instance_id, None);
        let preview = info.telemetry.heartbeat_preview.expect("a preview");
        assert_eq!(preview["schema"], 1);
        assert_eq!(
            preview["instance_id"],
            "00000000-0000-0000-0000-000000000000"
        );
        let stored = get_setting(app.state.meta.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap();
        assert_eq!(stored, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_existing_instance_id_is_shown() {
        let app = TestApp::new().await;
        let id = instance::instance_id(&app.state.meta).await.unwrap();
        let info = collect(&app.state, None).await;
        assert_eq!(info.telemetry.instance_id, Some(id));
        let preview = info.telemetry.heartbeat_preview.expect("a preview");
        assert_eq!(preview["instance_id"], id.to_string());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_ring_lines_are_returned() {
        let app = TestApp::new().await;
        let ring = LogRing::new();
        ring.push("{\"a\":1}");
        ring.push("second");
        let info = collect(&app.state, Some(&ring)).await;
        assert_eq!(
            info.logs,
            Some(LogsView {
                lines: vec!["{\"a\":1}".into(), "second".into()],
                dropped: 0,
                trimmed: false,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn self_monitoring_reasons() {
        let app = TestApp::with_config(|config| {
            config.self_monitoring.otlp_endpoint = None;
        })
        .await;
        let state = self_monitoring_state(&app.state.config.self_monitoring);
        assert!(!state.exporting);
        assert_eq!(state.reason.as_deref(), Some("no_endpoint"));

        // Traces are switched off too: with `traces` on, spans would still be exported.
        let app = TestApp::with_config(|config| {
            config.self_monitoring.otlp_endpoint =
                Some(parse_otlp_endpoint("https://otlp.example.com").unwrap());
            config.self_monitoring.logs = false;
            config.self_monitoring.traces = false;
        })
        .await;
        let state = self_monitoring_state(&app.state.config.self_monitoring);
        assert!(!state.exporting);
        assert_eq!(state.reason.as_deref(), Some("logs_off"));
        assert_eq!(state.endpoint_host.as_deref(), Some("otlp.example.com"));

        let app = TestApp::with_config(|config| {
            config.self_monitoring.otlp_endpoint =
                Some(parse_otlp_endpoint("https://otlp.example.com").unwrap());
            config.self_monitoring.logs = true;
        })
        .await;
        let state = self_monitoring_state(&app.state.config.self_monitoring);
        assert!(state.exporting);
        assert_eq!(state.reason, None);
    }

    #[test]
    fn debug_info_round_trips() {
        let value = serde_json::to_value(sample()).unwrap();
        let back: DebugInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn the_key_set_is_fixed() {
        let value = serde_json::to_value(sample()).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "schema",
                "version",
                "uptime_secs",
                "logs",
                "storage",
                "ready",
                "meta",
                "self_monitoring",
                "telemetry",
                "errors",
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_planted_value_reaches_debug_info() {
        let app = TestApp::with_config(|config| {
            config.email_from = Some("Planted <noreply@planted-domain.example>".into());
            config.auth.signup_domains = vec!["planted-domain.example".into()];
        })
        .await;
        app.seed().await;
        let text = serde_json::to_string(&collect(&app.state, None).await).unwrap();
        for planted in ["planted", "noreply@", "ada@example.com"] {
            assert!(!text.contains(planted), "{planted} reached debug_info");
        }
    }

    #[test]
    fn debug_info_modules_use_no_network() {
        for source in [include_str!("debug_info.rs"), include_str!("disk.rs")] {
            let code = source.split("#[cfg(test)]").next().unwrap_or(source);
            for name in ["reqwest", "TcpStream", "UdpSocket", "lookup_host"] {
                assert!(!code.contains(name), "{name} in a non-test module");
            }
        }
    }
}
