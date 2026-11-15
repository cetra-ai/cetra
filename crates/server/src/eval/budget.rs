//! Rule budgets (D-I-15): UTC periods, the judge call's cost estimate, and in-memory
//! reservations that keep the jobs of one rule inside its budget.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Datelike, Utc};
use llmobs_eval::Adapter;
use llmobs_otel::{PriceRow, PriceTable};
use uuid::Uuid;

use crate::api::online_rules::BudgetPeriod;

pub(crate) const DAY_NS: i64 = 86_400_000_000_000;

/// The UTC midnight at or before `now`.
pub(crate) fn day_start(now: i64) -> i64 {
    now - now.rem_euclid(DAY_NS)
}

/// 00:00 UTC on the first day of `now`'s month, or the day start if that cannot be built.
pub(crate) fn month_start(now: i64) -> i64 {
    DateTime::<Utc>::from_timestamp_nanos(now)
        .date_naive()
        .with_day(1)
        .and_then(|first| first.and_hms_opt(0, 0, 0))
        .and_then(|midnight| midnight.and_utc().timestamp_nanos_opt())
        .unwrap_or_else(|| day_start(now))
}

/// The start of the current period.
pub(crate) fn period_start(period: BudgetPeriod, now: i64) -> i64 {
    match period {
        BudgetPeriod::Day => day_start(now),
        BudgetPeriod::Month => month_start(now),
    }
}

/// The judge model's price row when it prices both input and output, else `None` (D-I-15).
pub(crate) fn model_price<'a>(
    prices: &'a PriceTable,
    model: &str,
    adapter: Adapter,
    now: i64,
) -> Option<&'a PriceRow> {
    prices
        .price_for(model, Some(adapter.provider_name()), now)
        .filter(|row| row.price_input.is_some() && row.price_output.is_some())
}

/// The cost estimate of one judge call: `ceil(prompt_chars / 4)` input tokens and
/// `max_output_tokens` output tokens at the row's base prices (USD per million tokens).
/// Tiers are ignored. Rows come from [`model_price`], which guarantees both prices.
pub(crate) fn estimate_usd(prompt_chars: usize, max_output_tokens: u32, price: &PriceRow) -> f64 {
    let input_tokens = prompt_chars.div_ceil(4) as f64;
    let input_price = price.price_input.unwrap_or(0.0);
    let output_price = price.price_output.unwrap_or(0.0);
    input_tokens * input_price / 1e6 + f64::from(max_output_tokens) * output_price / 1e6
}

/// Spend and reservations of every rule, for the current period of each (D-I-15).
#[derive(Debug, Default)]
pub(crate) struct Budgets {
    rules: Mutex<HashMap<Uuid, RuleBudget>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RuleBudget {
    period_start: i64,
    spent: f64,
    reserved: f64,
}

/// The answer of [`Budgets::try_reserve`].
#[derive(Debug)]
pub(crate) enum Reserve<'a> {
    /// Reserved: settle the guard after the call, or drop it to give the amount back.
    Reserved(Reservation<'a>),
    /// `spent + reserved + estimate > budget`: pause the rule.
    Over,
    /// The rule's spend for this period is not loaded: call `load`, then try again.
    NotLoaded,
}

/// One reservation. Dropping it releases the amount; [`Reservation::settle`] turns it into the
/// actual cost.
#[derive(Debug)]
#[must_use = "dropping a reservation releases it"]
pub(crate) struct Reservation<'a> {
    budgets: &'a Budgets,
    rule: Uuid,
    period_start: i64,
    amount: f64,
    done: bool,
}

impl Reservation<'_> {
    /// After a call: the reservation becomes `actual` spend.
    pub(crate) fn settle(mut self, actual: f64) {
        self.done = true;
        let mut rules = self.budgets.lock();
        if let Some(entry) = rules.get_mut(&self.rule)
            && entry.period_start == self.period_start
        {
            entry.reserved = (entry.reserved - self.amount).max(0.0);
            // A NaN, infinite or negative cost must not corrupt the spend: it adds nothing.
            let actual = if actual.is_finite() && actual > 0.0 {
                actual
            } else {
                0.0
            };
            entry.spent += actual;
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut rules = self.budgets.lock();
        if let Some(entry) = rules.get_mut(&self.rule)
            && entry.period_start == self.period_start
        {
            entry.reserved = (entry.reserved - self.amount).max(0.0);
        }
    }
}

impl Budgets {
    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, RuleBudget>> {
        self.rules.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserves `estimate` for `rule` when `spent + reserved + estimate` is within `budget`.
    pub(crate) fn try_reserve(
        &self,
        rule: Uuid,
        period_start: i64,
        budget: f64,
        estimate: f64,
    ) -> Reserve<'_> {
        let mut rules = self.lock();
        let Some(entry) = rules.get_mut(&rule) else {
            return Reserve::NotLoaded;
        };
        if entry.period_start != period_start {
            return Reserve::NotLoaded;
        }
        if entry.spent + entry.reserved + estimate > budget {
            return Reserve::Over;
        }
        entry.reserved += estimate;
        Reserve::Reserved(Reservation {
            budgets: self,
            rule,
            period_start,
            amount: estimate,
            done: false,
        })
    }

    /// Sets the spend read from `rule_spend` for `period_start`, only when the rule has no entry
    /// for that period. An entry for the same period is kept as it is: another job loaded it first,
    /// and settles may have added to it since that read.
    pub(crate) fn load(&self, rule: Uuid, period_start: i64, spent: f64) {
        let mut rules = self.lock();
        if rules
            .get(&rule)
            .is_some_and(|entry| entry.period_start == period_start)
        {
            return;
        }
        rules.insert(
            rule,
            RuleBudget {
                period_start,
                spent,
                reserved: 0.0,
            },
        );
    }

    /// The view of the rule: `(spent, reserved)` for `period_start`, or `None`.
    pub(crate) fn view(&self, rule: Uuid, period_start: i64) -> Option<(f64, f64)> {
        self.lock()
            .get(&rule)
            .filter(|entry| entry.period_start == period_start)
            .map(|entry| (entry.spent, entry.reserved))
    }

    /// Drops the rule (deleted, or its budget changed).
    pub(crate) fn forget(&self, rule: Uuid) {
        self.lock().remove(&rule);
    }
}

#[cfg(test)]
mod tests {
    use llmobs_otel::PriceScope;

    use super::*;

    fn ns(text: &str) -> i64 {
        DateTime::parse_from_rfc3339(text)
            .expect("rfc3339")
            .timestamp_nanos_opt()
            .expect("in range")
    }

    fn price_row(pattern: &str, input: Option<f64>, output: Option<f64>) -> PriceRow {
        PriceRow {
            scope: PriceScope::Builtin,
            model_name: pattern.to_owned(),
            match_pattern: pattern.to_owned(),
            provider: None,
            valid_from: 0,
            price_input: input,
            price_output: output,
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        }
    }

    const RULE: Uuid = Uuid::from_u128(1);

    #[test]
    fn day_and_month_starts() {
        let march_afternoon = ns("2026-03-15T13:00:00Z");
        assert_eq!(day_start(march_afternoon), ns("2026-03-15T00:00:00Z"));
        assert_eq!(
            day_start(ns("2026-03-15T00:00:00Z")),
            ns("2026-03-15T00:00:00Z")
        );
        assert_eq!(month_start(march_afternoon), ns("2026-03-01T00:00:00Z"));
        assert_eq!(
            month_start(ns("2026-03-01T00:00:00Z")),
            ns("2026-03-01T00:00:00Z")
        );
        let leap_day = ns("2024-02-29T23:59:59.999999999Z");
        assert_eq!(day_start(leap_day), ns("2024-02-29T00:00:00Z"));
        assert_eq!(month_start(leap_day), ns("2024-02-01T00:00:00Z"));
        assert_eq!(
            period_start(BudgetPeriod::Month, march_afternoon),
            ns("2026-03-01T00:00:00Z")
        );
        assert_eq!(
            period_start(BudgetPeriod::Day, march_afternoon),
            ns("2026-03-15T00:00:00Z")
        );
    }

    #[test]
    fn estimate_counts_ceil_quarter_chars_and_prices_per_million() {
        let price = price_row("m", Some(1.0), Some(4.0));
        let estimate = estimate_usd(400, 100, &price);
        let expected = 100.0 * 1e-6 + 100.0 * 4e-6;
        assert!((estimate - expected).abs() < 1e-12, "{estimate}");
        // 401 characters are 101 tokens.
        let estimate = estimate_usd(401, 0, &price);
        assert!((estimate - 101.0 * 1e-6).abs() < 1e-12, "{estimate}");
    }

    #[test]
    fn model_price_needs_both_prices() {
        let prices = PriceTable::new(vec![price_row("m", Some(1.0), None)]).expect("pattern");
        assert!(model_price(&prices, "m", Adapter::OpenAi, 0).is_none());
        let prices = PriceTable::new(vec![price_row("m", Some(1.0), Some(2.0))]).expect("pattern");
        assert!(model_price(&prices, "m", Adapter::OpenAi, 0).is_some());
    }

    #[test]
    fn reserve_needs_a_loaded_period_and_refuses_over_budget() {
        let budgets = Budgets::default();
        assert!(matches!(
            budgets.try_reserve(RULE, 0, 5.0, 1.0),
            Reserve::NotLoaded
        ));
        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(first) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("1 + 2 is within 5");
        };
        // Equal to the budget is not over: 1 + 2 + 2 = 5.
        let Reserve::Reserved(second) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("1 + 2 + 2 is not over 5");
        };
        assert!(matches!(
            budgets.try_reserve(RULE, 0, 5.0, 0.5),
            Reserve::Over
        ));
        drop(first);
        drop(second);
    }

    #[test]
    fn settle_moves_the_reservation_to_spend() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(guard) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("reserve");
        };
        guard.settle(1.5);
        assert_eq!(budgets.view(RULE, 0), Some((2.5, 0.0)));
    }

    #[test]
    fn dropping_a_reservation_gives_it_back() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(guard) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("reserve");
        };
        assert_eq!(budgets.view(RULE, 0), Some((1.0, 2.0)));
        drop(guard);
        assert_eq!(budgets.view(RULE, 0), Some((1.0, 0.0)));
    }

    #[test]
    fn a_guard_after_forget_or_a_turned_period_changes_nothing() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(forgotten) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("reserve");
        };
        budgets.forget(RULE);
        assert_eq!(budgets.view(RULE, 0), None);
        forgotten.settle(9.0);
        assert_eq!(budgets.view(RULE, 0), None);

        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(old_period) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("reserve");
        };
        let next_day = DAY_NS;
        budgets.load(RULE, next_day, 0.5);
        old_period.settle(3.0);
        assert_eq!(budgets.view(RULE, next_day), Some((0.5, 0.0)));
    }

    #[test]
    fn load_keeps_an_entry_for_the_same_period() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        let Reserve::Reserved(guard) = budgets.try_reserve(RULE, 0, 5.0, 0.5) else {
            panic!("reserve");
        };
        guard.settle(0.5);
        budgets.load(RULE, 0, 1.0);
        assert_eq!(budgets.view(RULE, 0), Some((1.5, 0.0)));
    }

    #[test]
    fn a_new_period_is_not_loaded_until_loaded() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        assert!(matches!(
            budgets.try_reserve(RULE, DAY_NS, 5.0, 1.0),
            Reserve::NotLoaded
        ));
        budgets.forget(RULE);
        assert_eq!(budgets.view(RULE, 0), None);
    }

    #[test]
    fn settle_adds_only_a_finite_positive_cost() {
        let budgets = Budgets::default();
        budgets.load(RULE, 0, 1.0);
        for actual in [f64::NAN, f64::INFINITY, -3.0] {
            let Reserve::Reserved(guard) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
                panic!("reserve");
            };
            guard.settle(actual);
            assert_eq!(budgets.view(RULE, 0), Some((1.0, 0.0)), "actual {actual}");
        }
        let Reserve::Reserved(guard) = budgets.try_reserve(RULE, 0, 5.0, 2.0) else {
            panic!("reserve");
        };
        guard.settle(0.25);
        assert_eq!(budgets.view(RULE, 0), Some((1.25, 0.0)));
    }
}
