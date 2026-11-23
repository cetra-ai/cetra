//! Opt-in crash reports (track TL § 3, D-TL-5, D-TL-12). With `telemetry.crash_reports` on and
//! the switchboard agreeing, every panic and every ERROR `tracing` event becomes a report of
//! code-defined facts, spooled to `<data_dir>/telemetry/spool/` at once and sent to
//! `<telemetry.endpoint>/v1/crash` by a background task. Nothing in a report comes from a
//! message or a field value.

pub(crate) mod limiter;
pub(crate) mod report;
