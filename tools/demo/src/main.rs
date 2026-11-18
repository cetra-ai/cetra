//! `cetra-demo`: the sidecar that keeps the cetra demo org busy (D187).
//!
//! It builds traces from the captured OTLP fixtures and sends them to a cell's
//! `POST /v1/traces`. When the demo project has no trace in the backfill window it
//! first writes that window, at the daily curve's rate, oldest first. Then it sends
//! the live traces every tick. Lines go through `tracing`; the key is never printed.

mod curve;
mod error;
mod generate;
mod otlp;
mod sink;
mod synth;
mod templates;

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

    let now = Utc::now();
    let backfill_from = now - chrono::Duration::days(i64::from(args.backfill_days));
    let last = if sink.has_trace_between(backfill_from, now).await? {
        info!("the project has recent traces, no backfill");
        now
    } else {
        info!(days = args.backfill_days, "no recent traces, backfilling");
        backfill(&sink, &mut generator, backfill_from, now).await?
    };
    live(&sink, &mut generator, last).await
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
