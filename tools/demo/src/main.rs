//! `cetra-demo`: the sidecar that keeps the cetra demo org busy (D187).
//!
//! It builds traces from the captured OTLP fixtures and sends them to a cell's
//! `POST /v1/traces`. At startup it writes the traces the project is missing: from
//! the newest trace in the backfill window, or the whole window when there is none,
//! up to now, at the daily curve's rate, oldest first. Then it sends the live traces
//! every tick. With `--judge-addr` it also serves the judge endpoint the demo's LLM
//! connection uses (`judge.rs`). Lines go through `tracing`; the keys are never printed.

mod curve;
mod error;
mod generate;
mod judge;
mod otlp;
mod sink;
mod synth;
mod templates;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::error::DemoError;
use crate::generate::Generator;
use crate::sink::{MAX_BATCH_TRACES, Sink};
use crate::templates::Trace;

/// How often the live loop sends the traces of the last tick.
const LIVE_TICK: Duration = Duration::from_secs(10);

/// Flags. `--key` is a secret: it is read from its environment variable in
/// practice, and never printed (this type has no `Debug` for that reason).
#[derive(Parser)]
#[command(
    name = "cetra-demo",
    version,
    about = "Keep the cetra demo org busy with realistic traces built from the captured fixtures (D187)"
)]
struct Args {
    /// Base URL of the cell, for example `https://eu1.example.test`.
    #[arg(long, env = "CETRA_DEMO_URL")]
    url: String,
    /// Secret key of the demo project, with the `ingest` and `read` scopes.
    #[arg(long, env = "CETRA_DEMO_KEY")]
    key: String,
    /// Directory of OTLP fixtures to build the traces from.
    #[arg(long, default_value = "fixtures/otlp")]
    fixtures: PathBuf,
    /// Traces per minute at the daily peak (the curve is 25% of this at 03:00 UTC).
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..))]
    rate: u32,
    /// Days of history to write when the project has no trace in that window.
    #[arg(long, default_value_t = 30)]
    backfill_days: u32,
    /// Seed of the random generator, so a run repeats (tests and debugging).
    #[arg(long)]
    seed: Option<u64>,
    /// Address to serve the judge endpoint on, for example `0.0.0.0:8099`. Unset, no judge runs.
    #[arg(long, env = "CETRA_DEMO_JUDGE_ADDR")]
    judge_addr: Option<SocketAddr>,
    /// Bearer key the judge endpoint requires; needed with `--judge-addr`, never printed.
    #[arg(long, env = "CETRA_DEMO_JUDGE_KEY")]
    judge_key: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(false)
        .without_time()
        .init();
}

async fn run(args: Args) -> Result<(), DemoError> {
    let templates = templates::load(&args.fixtures)?;
    let mut generator = Generator::new(templates, args.rate, args.seed);
    let sink = Sink::new(&args.url, args.key)?;
    if let Some(addr) = args.judge_addr {
        let key = args
            .judge_key
            .filter(|key| !key.is_empty())
            .ok_or(DemoError::JudgeKeyMissing)?;
        let listener = judge::bind(addr).await?;
        info!(%addr, "judge endpoint listening");
        tokio::spawn(async move {
            if let Err(error) = judge::serve(listener, key).await {
                tracing::error!("{error}");
            }
        });
    }

    let now = Utc::now();
    let from = backfill_start(&sink, now, args.backfill_days).await?;
    let last = backfill(&sink, &mut generator, from, now).await?;
    live(&sink, &mut generator, last).await
}

/// Where the backfill starts: the newest trace in the window, so a run that stopped
/// part way resumes there, or the window's start when the window has no trace.
async fn backfill_start(
    sink: &Sink,
    now: DateTime<Utc>,
    days: u32,
) -> Result<DateTime<Utc>, DemoError> {
    let window_start = now - chrono::Duration::days(i64::from(days));
    match sink.newest_trace_start(window_start, now).await? {
        Some(newest) => {
            info!(
                days,
                "the project has traces in the backfill window, resuming from the newest"
            );
            Ok(newest.max(window_start))
        }
        None => {
            info!(
                days,
                "no traces in the backfill window, backfilling all of it"
            );
            Ok(window_start)
        }
    }
}

/// Writes the traces of `[from, to)` in time order, one minute at a time, and
/// returns `to`. A request holds at most [`MAX_BATCH_TRACES`] traces.
async fn backfill(
    sink: &Sink,
    generator: &mut Generator,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<DateTime<Utc>, DemoError> {
    let mut pending: Vec<Trace> = Vec::new();
    let mut at = from;
    while at < to {
        let next = (at + chrono::Duration::minutes(1)).min(to);
        pending.extend(generator.window(nanos(at)?, nanos(next)?));
        if pending.len() >= MAX_BATCH_TRACES {
            sink.send(&pending).await?;
            pending.clear();
        }
        at = next;
    }
    sink.send(&pending).await?;
    info!("backfill done");
    Ok(to)
}

/// Sends the traces that start after `last`, every [`LIVE_TICK`], forever.
async fn live(
    sink: &Sink,
    generator: &mut Generator,
    mut last: DateTime<Utc>,
) -> Result<(), DemoError> {
    info!("live ingest started");
    loop {
        tokio::time::sleep(LIVE_TICK).await;
        let now = Utc::now();
        let traces = generator.window(nanos(last)?, nanos(now)?);
        sink.send(&traces).await?;
        last = now;
    }
}

fn nanos(at: DateTime<Utc>) -> Result<i64, DemoError> {
    at.timestamp_nanos_opt().ok_or(DemoError::Clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::fake_cell::{CellState, start};
    use std::sync::Arc;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .expect("a valid time")
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn a_partial_backfill_resumes_at_the_newest_trace() {
        let cell = Arc::new(CellState::default());
        // The newest trace of the window is ten days old: the backfill must start there.
        *cell.newest_start.lock().expect("lock") = Some("2026-11-08T12:00:00Z".to_owned());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let from = backfill_start(&sink, now(), 30).await.expect("planned");
        let ten_days_ago = DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z")
            .expect("a valid time")
            .with_timezone(&Utc);
        assert_eq!(from, ten_days_ago);
        let query = cell
            .last_query
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_default();
        assert!(query.contains("sort=start_time"), "query was {query}");
        assert!(query.contains("order=desc"), "query was {query}");
        assert!(query.contains("limit=1"), "query was {query}");
    }

    #[tokio::test]
    async fn an_empty_window_backfills_the_whole_window() {
        let cell = Arc::new(CellState::default());
        let base = start(cell.clone()).await;
        let sink = Sink::new(&base, "sk-cetra-test".to_owned()).expect("client");
        let from = backfill_start(&sink, now(), 30).await.expect("planned");
        assert_eq!(from, now() - chrono::Duration::days(30));
    }
}
