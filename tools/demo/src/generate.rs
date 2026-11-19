//! Decides how many traces start in a time window, and for each one the plan
//! (user, session, environment, release, error, factors), then builds it (DO5 steps 3 and 4).

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use crate::curve;
use crate::synth::{Plan, synthesize};
use crate::templates::Trace;

const NS_PER_MINUTE: f64 = 60_000_000_000.0;
const NS_PER_WEEK: i64 = 7 * 24 * 3600 * 1_000_000_000;
/// Fake users `user-0001` to `user-0200`.
const USERS: u32 = 200;
/// A session groups this many consecutive traces at most (1 to 8 of one user).
const MAX_SESSION_TRACES: u32 = 8;
/// Share of traces that are errors.
const ERROR_SHARE: f64 = 0.03;
/// Share of traces in the production environment; the rest is staging.
const PRODUCTION_SHARE: f64 = 0.9;
/// Band of the random per-minute noise on the rate, around 1.0.
const NOISE: (f64, f64) = (0.8, 1.2);

/// The generator's random state. Seeded when a seed is given, so a run repeats.
#[derive(Debug)]
pub(crate) struct Generator {
    rng: StdRng,
    templates: Vec<Trace>,
    peak_per_minute: f64,
    /// Traces expected in earlier windows that were not whole yet.
    carry: f64,
    session: Option<Session>,
}

#[derive(Debug)]
struct Session {
    user: u32,
    id: String,
    /// Traces this session still has to give.
    left: u32,
}

impl Generator {
    /// `templates` must not be empty (see `templates::load`). `peak_per_minute` is the rate at the daily peak.
    pub(crate) fn new(templates: Vec<Trace>, peak_per_minute: u32, seed: Option<u64>) -> Self {
        let rng = match seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => rand::make_rng(),
        };
        Self {
            rng,
            templates,
            peak_per_minute: f64::from(peak_per_minute),
            carry: 0.0,
            session: None,
        }
    }

    /// The traces that start in `[from_ns, to_ns)`, at the daily curve's rate with noise.
    pub(crate) fn window(&mut self, from_ns: i64, to_ns: i64) -> Vec<Trace> {
        if to_ns <= from_ns {
            return Vec::new();
        }
        let span_ns = to_ns - from_ns;
        let minutes = span_ns as f64 / NS_PER_MINUTE;
        let noise = self.rng.random_range(NOISE.0..NOISE.1);
        let expected = self.peak_per_minute * curve::level(from_ns + span_ns / 2) * noise * minutes
            + self.carry;
        let whole = expected.floor();
        self.carry = expected - whole;
        let mut traces = Vec::with_capacity(whole as usize);
        for _ in 0..whole as u64 {
            let start_ns = from_ns + self.rng.random_range(0..span_ns);
            let plan = self.plan(start_ns);
            let template = &self.templates[self.rng.random_range(0..self.templates.len())];
            traces.push(synthesize(template, &plan, &mut self.rng));
        }
        traces
    }

    fn plan(&mut self, start_ns: i64) -> Plan {
        let session = match self.session.take() {
            Some(session) if session.left > 0 => session,
            _ => self.new_session(),
        };
        let user_id = format!("user-{:04}", session.user);
        let session_id = session.id.clone();
        self.session = Some(Session {
            left: session.left.saturating_sub(1),
            ..session
        });
        let environment = if self.rng.random_bool(PRODUCTION_SHARE) {
            "production"
        } else {
            "staging"
        };
        Plan {
            start_ns,
            user_id,
            session_id,
            environment,
            release: release_for(start_ns),
            error: self.rng.random_bool(ERROR_SHARE),
            duration_factor: self.rng.random_range(0.5..2.0),
            token_factor: self.rng.random_range(0.7..1.3),
        }
    }

    fn new_session(&mut self) -> Session {
        Session {
            user: self.rng.random_range(1..=USERS),
            id: format!("sess-{:016x}", self.rng.random::<u64>()),
            left: self.rng.random_range(1..=MAX_SESSION_TRACES),
        }
    }
}

/// The release of the week `start_ns` falls in: it changes every 7 days.
fn release_for(start_ns: i64) -> String {
    format!("1.{}.0", start_ns.div_euclid(NS_PER_WEEK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::fixture_templates;

    /// Two days from 2025-11-18T00:00:00Z.
    const FROM_NS: i64 = 1_763_424_000 * 1_000_000_000;
    const DAY_NS: i64 = 86_400 * 1_000_000_000;

    fn json_of(traces: &[Trace]) -> serde_json::Value {
        serde_json::to_value(traces).expect("serializes")
    }

    #[test]
    fn a_seed_repeats_the_same_traces() {
        let mut first = Generator::new(fixture_templates(), 60, Some(42));
        let mut second = Generator::new(fixture_templates(), 60, Some(42));
        let a = first.window(FROM_NS, FROM_NS + DAY_NS / 24);
        let b = second.window(FROM_NS, FROM_NS + DAY_NS / 24);
        assert!(!a.is_empty());
        assert_eq!(json_of(&a), json_of(&b));
    }

    #[test]
    fn an_empty_window_gives_no_traces() {
        let mut generator = Generator::new(fixture_templates(), 60, Some(1));
        assert!(generator.window(FROM_NS, FROM_NS).is_empty());
        assert!(generator.window(FROM_NS, FROM_NS - 1).is_empty());
    }

    #[test]
    fn a_day_produces_about_the_curves_total_and_the_peak_hour_is_busier() {
        // Two traces a minute at the peak. The curve averages 0.625 of the peak over a
        // day, so a day is about 2 * 1440 * 0.625 = 1800 traces.
        let mut generator = Generator::new(fixture_templates(), 2, Some(7));
        let mut per_hour = [0_usize; 24];
        let mut total = 0_usize;
        for hour in 0..24 {
            let from = FROM_NS + i64::from(hour) * DAY_NS / 24;
            let traces = generator.window(from, from + DAY_NS / 24);
            per_hour[hour as usize] = traces.len();
            total += traces.len();
        }
        assert!((1_500..=2_100).contains(&total), "total was {total}");
        assert!(
            per_hour[14] > per_hour[3] * 2,
            "peak {} trough {}",
            per_hour[14],
            per_hour[3]
        );
    }

    #[test]
    fn sessions_give_one_to_eight_traces_each_and_the_release_changes_weekly() {
        let mut generator = Generator::new(fixture_templates(), 60, Some(9));
        let mut plans = Vec::new();
        for _ in 0..200 {
            plans.push(generator.plan(FROM_NS));
        }
        let mut runs = Vec::new();
        let mut run = 1_u32;
        for pair in plans.windows(2) {
            if pair[0].session_id == pair[1].session_id {
                run += 1;
            } else {
                runs.push(run);
                run = 1;
            }
        }
        runs.push(run);
        assert!(runs.iter().all(|run| *run <= MAX_SESSION_TRACES));
        assert!(plans.iter().all(|plan| plan.user_id.starts_with("user-")));
        assert_eq!(release_for(0), "1.0.0");
        assert_eq!(release_for(NS_PER_WEEK - 1), "1.0.0");
        assert_eq!(release_for(NS_PER_WEEK), "1.1.0");
    }
}
