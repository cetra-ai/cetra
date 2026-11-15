//! Experiments: the plan window on experiments (D-J-14), and their results and comparison,
//! computed in memory from meta.db rows and two storage reads (D-J-13). Plain functions, no
//! traits.
//!
//! The aggregation works on a [`Loaded`] per experiment: the light projection of its results
//! (no text), and its scores and traces from storage. Text is read from meta.db only for the
//! rows and items a page answers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use llmobs_core::enums::ScoreDataType;
use llmobs_core::ids::{OrgId, TraceId};
use llmobs_storage::{ExperimentScore, ExperimentTrace};
use serde_json::Value;
use uuid::Uuid;

use crate::api::experiments::{
    CompareCell, CompareRow, ExperimentComparison, ExperimentSummary, ResultRow, ResultScore,
    ScoreDelta, ScoreDeltas, ScoreSummary,
};
use crate::entitlements::Visibility;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::MetaError;
use crate::meta::experiments::{ExperimentRow, ResultKey, ResultRow as StoredResult};
use crate::routes::experiments::{experiment_json, invalid_cursor};

/// The org's window bound now (`created_at` before it is hidden), or `None` without a window
/// (D-J-14).
pub(crate) fn visible_from(state: &AppState, org: OrgId) -> Option<i64> {
    Visibility::window(state, org).visible_from()
}

/// Everything the aggregation reads of one experiment (D-J-13).
pub(crate) struct Loaded {
    pub(crate) experiment: ExperimentRow,
    /// The light projection of every result, ordered by item, then repetition.
    pub(crate) keys: Vec<ResultKey>,
    /// The live scores whose `experiment_id` is the experiment.
    pub(crate) scores: Vec<ExperimentScore>,
    /// The traces whose `llmobs.experiment_id` is the experiment.
    pub(crate) traces: Vec<ExperimentTrace>,
}

/// One score name on one trace after rules 2 and 3: the kept data type, the mean of the numeric
/// values, and the label of the first score in label order.
#[derive(Debug, Clone, PartialEq)]
struct Kept {
    data_type: ScoreDataType,
    value_num: Option<f64>,
    value_str: Option<String>,
}

/// The kept scores of one experiment (rules 1 to 3), and the cost of each of its row traces.
struct Index {
    /// The data type kept for each name (rule 2).
    types: BTreeMap<String, ScoreDataType>,
    /// The kept scores of each trace (lowercase hex), by name.
    by_trace: HashMap<String, BTreeMap<String, Kept>>,
    /// The cost of each row trace (lowercase hex), `None` when unpriced.
    costs: BTreeMap<String, Option<f64>>,
}

impl Index {
    /// The kept scores of a row's trace. A row without a trace has none (rule 1).
    fn row_scores(&self, trace: Option<&TraceId>) -> Option<&BTreeMap<String, Kept>> {
        self.by_trace.get(&trace?.to_string())
    }

    /// The cost of a row's trace (rule 5).
    fn row_cost(&self, trace: Option<&TraceId>) -> Option<f64> {
        self.costs.get(&trace?.to_string()).copied().flatten()
    }
}

/// Rules 1 to 3: the kept type of each score name and the kept scores of each row trace.
fn index(loaded: &Loaded) -> Result<Index, ApiError> {
    let rows: HashSet<String> = loaded
        .keys
        .iter()
        .filter_map(|key| key.trace_id.map(|trace| trace.to_string()))
        .collect();

    // Rule 2: the type with the most scores of a name, over the scores of the rows' traces. A
    // tie goes to the type that comes first in text order.
    let mut counts: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    for score in &loaded.scores {
        if rows.contains(&score.trace_id) {
            *counts
                .entry(score.name.as_str())
                .or_default()
                .entry(score.data_type.as_str())
                .or_default() += 1;
        }
    }
    let mut chosen: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, by_type) in counts {
        let mut best: Option<(&str, usize)> = None;
        for (data_type, count) in by_type {
            match best {
                Some((_, top)) if top >= count => {}
                _ => best = Some((data_type, count)),
            }
        }
        if let Some((data_type, _)) = best {
            chosen.insert(name, data_type);
        }
    }
    let mut types = BTreeMap::new();
    for (name, data_type) in &chosen {
        types.insert((*name).to_owned(), parse_type(data_type)?);
    }

    // Rule 3: the scores of one name on one trace, of the kept type.
    let mut groups: BTreeMap<(&str, &str), Vec<&ExperimentScore>> = BTreeMap::new();
    for score in &loaded.scores {
        let kept_type = chosen.get(score.name.as_str()).copied();
        if rows.contains(&score.trace_id) && kept_type == Some(score.data_type.as_str()) {
            groups
                .entry((score.trace_id.as_str(), score.name.as_str()))
                .or_default()
                .push(score);
        }
    }
    let mut by_trace: HashMap<String, BTreeMap<String, Kept>> = HashMap::new();
    for ((trace, name), mut scores) in groups {
        let Some(data_type) = types.get(name) else {
            continue;
        };
        let mut values: Vec<f64> = scores.iter().filter_map(|score| score.value_num).collect();
        values.sort_by(f64::total_cmp);
        scores.sort_by(|a, b| a.value_str.cmp(&b.value_str));
        let value_str = scores.first().and_then(|score| score.value_str.clone());
        by_trace.entry(trace.to_owned()).or_default().insert(
            name.to_owned(),
            Kept {
                data_type: *data_type,
                value_num: mean(&values),
                value_str,
            },
        );
    }

    let costs = loaded
        .traces
        .iter()
        .filter(|trace| rows.contains(&trace.trace_id))
        .map(|trace| (trace.trace_id.clone(), trace.cost_total))
        .collect();
    Ok(Index {
        types,
        by_trace,
        costs,
    })
}

fn parse_type(text: &str) -> Result<ScoreDataType, ApiError> {
    ScoreDataType::parse(text).map_err(|_| corrupt("scores", "data_type"))
}

fn corrupt(table: &'static str, column: &'static str) -> ApiError {
    ApiError::from(MetaError::Corrupt { table, column })
}

fn json_text(text: &str, column: &'static str) -> Result<Value, ApiError> {
    serde_json::from_str(text).map_err(|_| corrupt("experiment_items", column))
}

fn json_opt(text: Option<&str>, column: &'static str) -> Result<Option<Value>, ApiError> {
    text.map(|text| json_text(text, column)).transpose()
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Median, min and max of `sorted`, which is in ascending order.
fn stats(sorted: &[f64]) -> (Option<f64>, Option<f64>, Option<f64>) {
    let median = match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[n / 2]),
        n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    };
    (median, sorted.first().copied(), sorted.last().copied())
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The nearest-rank percentile `p` (0 to 1) of an ascending list.
fn nearest_rank(sorted: &[i64], p: f64) -> Option<i64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let rank = (p * n as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1).min(n - 1)).copied()
}

/// The score summary of one kept name over the experiment's rows (rule 4).
fn score_summary(
    loaded: &Loaded,
    index: &Index,
    name: &str,
    data_type: &ScoreDataType,
) -> ScoreSummary {
    let mut rows = 0_i64;
    let mut values = Vec::new();
    let mut labels: BTreeMap<String, i64> = BTreeMap::new();
    for key in &loaded.keys {
        let Some(kept) = index
            .row_scores(key.trace_id.as_ref())
            .and_then(|scores| scores.get(name))
        else {
            continue;
        };
        rows += 1;
        if let Some(value) = kept.value_num {
            values.push(value);
        }
        if let (ScoreDataType::Categorical, Some(label)) = (data_type, &kept.value_str) {
            *labels.entry(label.clone()).or_insert(0) += 1;
        }
    }
    values.sort_by(f64::total_cmp);
    let numeric = !matches!(data_type, ScoreDataType::Text);
    let (median, min, max) = if numeric {
        stats(&values)
    } else {
        (None, None, None)
    };
    let mean_value = if numeric { mean(&values) } else { None };
    let boolean = matches!(data_type, ScoreDataType::Boolean);
    ScoreSummary {
        name: name.to_owned(),
        data_type: *data_type,
        count: rows,
        mean: mean_value,
        median,
        min,
        max,
        pass_rate: if boolean { mean_value } else { None },
        labels,
    }
}

/// The experiment's summary (rule 5 and rule 4).
pub(crate) fn summary(loaded: &Loaded) -> Result<ExperimentSummary, ApiError> {
    let index = index(loaded)?;
    let scores = index
        .types
        .iter()
        .map(|(name, data_type)| score_summary(loaded, &index, name, data_type))
        .collect();
    let mut latencies: Vec<i64> = loaded
        .keys
        .iter()
        .filter_map(|key| key.latency_ns)
        .collect();
    latencies.sort_unstable();
    let errors = loaded.keys.iter().filter(|key| key.has_error).count();
    let costs: Vec<f64> = index.costs.values().flatten().copied().collect();
    Ok(ExperimentSummary {
        experiment: experiment_json(&loaded.experiment)?,
        scores,
        result_count: to_i64(loaded.keys.len()),
        error_count: to_i64(errors),
        latency_mean_ns: latency_mean(&latencies),
        latency_p50_ns: nearest_rank(&latencies, 0.5),
        latency_p95_ns: nearest_rank(&latencies, 0.95),
        cost_total: (!costs.is_empty()).then(|| costs.iter().sum::<f64>()),
    })
}

/// The integer mean of an ascending list, truncated (rule 5).
fn latency_mean(sorted: &[i64]) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let sum: i128 = sorted.iter().map(|ns| i128::from(*ns)).sum();
    i64::try_from(sum / sorted.len() as i128).ok()
}

/// Parses a results cursor `<item uuid>:<repetition>` (rule 7).
pub(crate) fn parse_results_cursor(text: &str) -> Result<(Uuid, i64), ApiError> {
    let (item, repetition) = text.split_once(':').ok_or_else(invalid_cursor)?;
    let item = item.parse::<Uuid>().map_err(|_| invalid_cursor())?;
    let repetition = repetition
        .parse::<i64>()
        .ok()
        .filter(|repetition| *repetition >= 0)
        .ok_or_else(invalid_cursor)?;
    Ok((item, repetition))
}

fn parse_item_cursor(text: &str) -> Result<Uuid, ApiError> {
    text.parse::<Uuid>().map_err(|_| invalid_cursor())
}

/// One page of full rows (rule 6): JSON parsed from the text, scores and cost from the trace.
pub(crate) fn result_rows(
    loaded: &Loaded,
    page: &[StoredResult],
) -> Result<Vec<ResultRow>, ApiError> {
    let index = index(loaded)?;
    let mut rows = Vec::with_capacity(page.len());
    for row in page {
        let scores: BTreeMap<String, ResultScore> = index
            .row_scores(row.trace_id.as_ref())
            .map(|kept| {
                kept.iter()
                    .map(|(name, kept)| {
                        (
                            name.clone(),
                            ResultScore {
                                data_type: kept.data_type,
                                value_num: kept.value_num,
                                value_str: kept.value_str.clone(),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        rows.push(ResultRow {
            dataset_item_id: row.dataset_item_id,
            repetition: row.repetition,
            input: json_text(&row.input, "input")?,
            expected_output: json_opt(row.expected_output.as_deref(), "expected_output")?,
            output: json_opt(row.output.as_deref(), "output")?,
            error: row.error.clone(),
            trace_id: row.trace_id,
            latency_ns: row.latency_ns,
            cost_total: index.row_cost(row.trace_id.as_ref()),
            scores,
        });
    }
    Ok(rows)
}

/// One experiment's answer for one item: the mean of each score over the repetitions, and
/// whether any repetition failed (rule 8).
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    scores: BTreeMap<String, f64>,
    has_error: bool,
}

/// The cells of one experiment, by item. An item without a row has no cell.
fn cells(loaded: &Loaded) -> Result<BTreeMap<Uuid, Cell>, ApiError> {
    let index = index(loaded)?;
    let mut per_item: BTreeMap<Uuid, (BTreeMap<String, Vec<f64>>, bool)> = BTreeMap::new();
    for key in &loaded.keys {
        let (values, has_error) = per_item.entry(key.dataset_item_id).or_default();
        *has_error |= key.has_error;
        for (name, kept) in index
            .row_scores(key.trace_id.as_ref())
            .into_iter()
            .flatten()
        {
            if let Some(value) = kept.value_num {
                values.entry(name.clone()).or_default().push(value);
            }
        }
    }
    Ok(per_item
        .into_iter()
        .map(|(item, (values, has_error))| {
            let scores = values
                .into_iter()
                .filter_map(|(name, values)| mean(&values).map(|value| (name, value)))
                .collect();
            (item, Cell { scores, has_error })
        })
        .collect())
}

/// Rule 9: a candidate cell regressed against the baseline's when a score it shares with the
/// baseline is lower by more than the threshold, or when the baseline ran and the candidate
/// failed.
fn regressed(candidate: &Cell, baseline: &Cell, threshold: f64) -> bool {
    let lower = candidate.scores.iter().any(|(name, value)| {
        baseline
            .scores
            .get(name)
            .is_some_and(|base| *value < *base - threshold)
    });
    lower || (!baseline.has_error && candidate.has_error)
}

fn item_regressed(
    baseline: &BTreeMap<Uuid, Cell>,
    candidates: &[BTreeMap<Uuid, Cell>],
    item: &Uuid,
    threshold: f64,
) -> bool {
    let Some(base) = baseline.get(item) else {
        return false;
    };
    candidates.iter().any(|cells| {
        cells
            .get(item)
            .is_some_and(|cell| regressed(cell, base, threshold))
    })
}

/// Rules 8, 9 and 12: the page's item ids and the next cursor, from the keys and scores only.
/// The first experiment is the baseline.
pub(crate) fn compare_items(
    loaded: &[Loaded],
    threshold: f64,
    regressions_only: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<(Vec<Uuid>, Option<String>), ApiError> {
    let per_experiment = loaded.iter().map(cells).collect::<Result<Vec<_>, _>>()?;
    let after = cursor.map(parse_item_cursor).transpose()?;
    let Some((baseline, candidates)) = per_experiment.split_first() else {
        return Ok((Vec::new(), None));
    };
    let items: BTreeSet<Uuid> = per_experiment
        .iter()
        .flat_map(|cells| cells.keys().copied())
        .collect();
    let mut selected: Vec<Uuid> = items
        .into_iter()
        .filter(|item| after.is_none_or(|after| *item > after))
        .filter(|item| !regressions_only || item_regressed(baseline, candidates, item, threshold))
        .collect();
    let more = selected.len() > limit;
    selected.truncate(limit);
    let next = if more {
        selected.last().map(Uuid::to_string)
    } else {
        None
    };
    Ok((selected, next))
}

/// Rule 10: the candidate's mean minus the baseline's, over the items both ran without error and
/// scored with `name`. `None` when no item is compared.
fn score_delta(
    baseline: &BTreeMap<Uuid, Cell>,
    candidate: &BTreeMap<Uuid, Cell>,
    name: &str,
) -> Option<ScoreDelta> {
    let mut base_values = Vec::new();
    let mut candidate_values = Vec::new();
    for (item, cell) in candidate {
        let Some(base) = baseline.get(item) else {
            continue;
        };
        if base.has_error || cell.has_error {
            continue;
        }
        if let (Some(base_value), Some(value)) = (base.scores.get(name), cell.scores.get(name)) {
            base_values.push(*base_value);
            candidate_values.push(*value);
        }
    }
    let delta = mean(&candidate_values)? - mean(&base_values)?;
    Some(ScoreDelta {
        delta,
        n_compared: to_i64(base_values.len()),
    })
}

/// The whole comparison (rules 5 to 11): the summaries, the deltas, the warnings and the page's
/// rows. `texts[i]` holds `item_rows` of experiment `i` for `items`; the first experiment is the
/// baseline.
pub(crate) fn compare(
    loaded: &[Loaded],
    texts: &[Vec<StoredResult>],
    items: &[Uuid],
    threshold: f64,
    next_cursor: Option<String>,
) -> Result<ExperimentComparison, ApiError> {
    let experiments = loaded.iter().map(summary).collect::<Result<Vec<_>, _>>()?;
    let per_experiment = loaded.iter().map(cells).collect::<Result<Vec<_>, _>>()?;
    let Some((baseline, candidates)) = per_experiment.split_first() else {
        return Ok(ExperimentComparison {
            experiments,
            deltas: Vec::new(),
            warnings: Vec::new(),
            rows: Vec::new(),
            next_cursor,
        });
    };

    let mut deltas = Vec::new();
    for (experiment, candidate) in loaded.iter().skip(1).zip(candidates) {
        let names: BTreeSet<&str> = baseline
            .values()
            .chain(candidate.values())
            .flat_map(|cell| cell.scores.keys().map(String::as_str))
            .collect();
        let scores = names
            .into_iter()
            .filter_map(|name| {
                score_delta(baseline, candidate, name).map(|delta| (name.to_owned(), delta))
            })
            .collect();
        deltas.push(ScoreDeltas {
            experiment_id: experiment.experiment.id,
            scores,
        });
    }

    let mut warnings = Vec::new();
    let versions: BTreeSet<i64> = loaded
        .iter()
        .map(|experiment| experiment.experiment.dataset_version)
        .collect();
    if versions.len() > 1 {
        warnings.push("experiments use different dataset versions".to_owned());
    }
    let missing: BTreeSet<Uuid> = candidates
        .iter()
        .flat_map(|cells| cells.keys().copied())
        .filter(|item| !baseline.contains_key(item))
        .collect();
    if !missing.is_empty() {
        warnings.push(format!(
            "{} items have no result in the baseline",
            missing.len()
        ));
    }
    for (experiment, candidate) in loaded.iter().skip(1).zip(candidates) {
        let union: BTreeSet<&Uuid> = baseline.keys().chain(candidate.keys()).collect();
        let both_ran = candidate
            .iter()
            .filter(|(item, cell)| {
                !cell.has_error && baseline.get(*item).is_some_and(|base| !base.has_error)
            })
            .count();
        let left_out = union.len().saturating_sub(both_ran);
        if left_out > 0 {
            warnings.push(format!(
                "{}: {left_out} items are left out of the deltas (missing or failed in one of the two)",
                experiment.experiment.id
            ));
        }
    }

    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let mut source: Option<&StoredResult> = None;
        let mut cells = Vec::new();
        for (index, (experiment, experiment_cells)) in
            loaded.iter().zip(&per_experiment).enumerate()
        {
            let Some(cell) = experiment_cells.get(item) else {
                continue;
            };
            let stored: Vec<&StoredResult> = texts
                .get(index)
                .map(|rows| {
                    rows.iter()
                        .filter(|row| row.dataset_item_id == *item)
                        .collect()
                })
                .unwrap_or_default();
            let Some(lowest) = stored.first().copied() else {
                return Err(corrupt("experiment_items", "dataset_item_id"));
            };
            if source.is_none() {
                source = Some(lowest);
            }
            let error = lowest
                .error
                .clone()
                .or_else(|| stored.iter().find_map(|row| row.error.clone()));
            cells.push(CompareCell {
                experiment_id: experiment.experiment.id,
                output: json_opt(lowest.output.as_deref(), "output")?,
                error,
                trace_id: lowest.trace_id,
                latency_ns: lowest.latency_ns,
                scores: cell.scores.clone(),
                regressed: index > 0
                    && baseline
                        .get(item)
                        .is_some_and(|base| regressed(cell, base, threshold)),
            });
        }
        let source = source.ok_or_else(|| corrupt("experiment_items", "dataset_item_id"))?;
        rows.push(CompareRow {
            dataset_item_id: *item,
            input: json_text(&source.input, "input")?,
            expected_output: json_opt(source.expected_output.as_deref(), "expected_output")?,
            cells,
        });
    }

    Ok(ExperimentComparison {
        experiments,
        deltas,
        warnings,
        rows,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::ProjectId;

    use super::*;

    fn trace(n: u128) -> TraceId {
        TraceId::from_slice(&n.to_be_bytes()).expect("16 bytes")
    }

    fn uuid_of(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn experiment_row(id: u128, version: i64) -> ExperimentRow {
        ExperimentRow {
            id: uuid_of(id),
            project_id: ProjectId::from_uuid(uuid_of(1)),
            dataset_id: Some(uuid_of(0xc01)),
            dataset_name: "faq".to_owned(),
            dataset_version: version,
            name: "run".to_owned(),
            description: None,
            metadata: "{}".to_owned(),
            repetitions: 1,
            item_count: 1,
            result_count: 0,
            status: "completed".to_owned(),
            error: None,
            created_by: None,
            created_at: 0,
            finished_at: None,
        }
    }

    /// Rows: (item, repetition, trace, has_error, latency). Scores: (trace, name, type, value,
    /// label). Traces: (trace, cost).
    type RowSpec = (u128, i64, Option<u128>, bool, Option<i64>);
    type ScoreSpec = (
        u128,
        &'static str,
        &'static str,
        Option<f64>,
        Option<&'static str>,
    );

    fn loaded(rows: &[RowSpec], scores: &[ScoreSpec], traces: &[(u128, Option<f64>)]) -> Loaded {
        Loaded {
            experiment: experiment_row(0xe01, 1),
            keys: rows
                .iter()
                .map(
                    |&(item, repetition, trace_id, has_error, latency_ns)| ResultKey {
                        dataset_item_id: uuid_of(item),
                        repetition,
                        trace_id: trace_id.map(trace),
                        has_error,
                        latency_ns,
                    },
                )
                .collect(),
            scores: scores
                .iter()
                .map(
                    |&(trace_n, name, data_type, value_num, value_str)| ExperimentScore {
                        trace_id: trace(trace_n).to_string(),
                        dataset_item_id: None,
                        name: name.to_owned(),
                        data_type: data_type.to_owned(),
                        value_num,
                        value_str: value_str.map(str::to_owned),
                    },
                )
                .collect(),
            traces: traces
                .iter()
                .map(|&(trace_n, cost_total)| ExperimentTrace {
                    trace_id: trace(trace_n).to_string(),
                    cost_total,
                    duration_ns: 0,
                    error_count: 0,
                })
                .collect(),
        }
    }

    /// The same loaded experiment with another id and dataset version.
    fn named(mut experiment: Loaded, id: u128, version: i64) -> Loaded {
        experiment.experiment = experiment_row(id, version);
        experiment
    }

    fn summary_of(loaded: &Loaded) -> ExperimentSummary {
        match summary(loaded) {
            Ok(summary) => summary,
            Err(_) => panic!("summary failed"),
        }
    }

    fn stored(item: u128, repetition: i64, error: Option<&str>) -> StoredResult {
        StoredResult {
            dataset_item_id: uuid_of(item),
            repetition,
            revision_id: Uuid::nil(),
            input: "\"question\"".to_owned(),
            expected_output: Some("\"answer\"".to_owned()),
            trace_id: None,
            output: Some("\"output\"".to_owned()),
            error: error.map(str::to_owned),
            latency_ns: None,
            created_at: 0,
        }
    }

    fn cell_of(loaded: &Loaded, item: u128) -> Cell {
        match cells(loaded) {
            Ok(cells) => match cells.get(&uuid_of(item)) {
                Some(cell) => cell.clone(),
                None => panic!("no cell"),
            },
            Err(_) => panic!("cells failed"),
        }
    }

    #[test]
    fn median_and_percentiles() {
        let rows: Vec<RowSpec> = (1..=4).map(|n| (n, 0, Some(n), false, None)).collect();
        let scores: Vec<ScoreSpec> = (1..=4)
            .map(|n| (n, "n", "numeric", Some(n as f64), None))
            .collect();
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        let score = &summary.scores[0];
        assert_eq!(
            (score.median, score.mean, score.min, score.max),
            (Some(2.5), Some(2.5), Some(1.0), Some(4.0))
        );

        let latencies: Vec<RowSpec> = (1..=10)
            .map(|n| (n, 0, None, false, Some(n as i64 * 10)))
            .collect();
        let summary = summary_of(&loaded(&latencies, &[], &[]));
        assert_eq!(summary.latency_p50_ns, Some(50));
        assert_eq!(summary.latency_p95_ns, Some(100));
        assert_eq!(summary.latency_mean_ns, Some(55));
    }

    #[test]
    fn boolean_pass_rate() {
        let rows: Vec<RowSpec> = (1..=3).map(|n| (n, 0, Some(n), false, None)).collect();
        let scores: Vec<ScoreSpec> = vec![
            (1, "ok", "boolean", Some(1.0), None),
            (2, "ok", "boolean", Some(1.0), None),
            (3, "ok", "boolean", Some(0.0), None),
        ];
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        let score = &summary.scores[0];
        let expected = 2.0 / 3.0;
        assert!(
            score
                .pass_rate
                .is_some_and(|rate| (rate - expected).abs() < 1e-9)
        );
        assert!(
            score
                .mean
                .is_some_and(|mean| (mean - expected).abs() < 1e-9)
        );
    }

    #[test]
    fn categorical_labels() {
        let rows: Vec<RowSpec> = (1..=3).map(|n| (n, 0, Some(n), false, None)).collect();
        let scores: Vec<ScoreSpec> = vec![
            (1, "tone", "categorical", Some(1.0), Some("good")),
            (2, "tone", "categorical", Some(1.0), Some("good")),
            (3, "tone", "categorical", Some(0.0), Some("bad")),
        ];
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        let score = &summary.scores[0];
        assert_eq!(score.labels.get("bad"), Some(&1));
        assert_eq!(score.labels.get("good"), Some(&2));
        assert!(
            score
                .mean
                .is_some_and(|mean| (mean - 2.0 / 3.0).abs() < 1e-9)
        );

        let unvalued: Vec<ScoreSpec> = vec![(1, "tone", "categorical", None, Some("good"))];
        let summary = summary_of(&loaded(&rows[..1], &unvalued, &[]));
        assert_eq!(summary.scores[0].mean, None);
    }

    #[test]
    fn text_scores_count_only() {
        let rows: Vec<RowSpec> = (1..=2).map(|n| (n, 0, Some(n), false, None)).collect();
        let scores: Vec<ScoreSpec> = vec![
            (1, "note", "text", None, Some("a")),
            (2, "note", "text", None, Some("b")),
        ];
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        let score = &summary.scores[0];
        assert_eq!(
            (score.count, score.mean, score.median, score.pass_rate),
            (2, None, None, None)
        );
    }

    #[test]
    fn majority_type_wins() {
        let rows: Vec<RowSpec> = (1..=4).map(|n| (n, 0, Some(n), false, None)).collect();
        let scores: Vec<ScoreSpec> = vec![
            (1, "q", "numeric", Some(1.0), None),
            (2, "q", "numeric", Some(1.0), None),
            (3, "q", "numeric", Some(1.0), None),
            (4, "q", "text", None, Some("x")),
        ];
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        assert_eq!(summary.scores.len(), 1);
        assert_eq!(summary.scores[0].data_type, ScoreDataType::Numeric);
        assert_eq!(summary.scores[0].count, 3);
    }

    #[test]
    fn several_scores_on_one_trace_average() {
        let rows: Vec<RowSpec> = vec![(1, 0, Some(1), false, None)];
        let scores: Vec<ScoreSpec> = vec![
            (1, "x", "numeric", Some(1.0), None),
            (1, "x", "numeric", Some(0.0), None),
        ];
        let summary = summary_of(&loaded(&rows, &scores, &[]));
        assert_eq!(summary.scores[0].mean, Some(0.5));
        assert_eq!(summary.scores[0].count, 1);
    }

    #[test]
    fn cost_sums_row_traces_only() {
        let rows: Vec<RowSpec> = vec![(1, 0, Some(1), false, None)];
        let traces = [(1, Some(0.25)), (9, Some(1.0))];
        assert_eq!(
            summary_of(&loaded(&rows, &[], &traces)).cost_total,
            Some(0.25)
        );
        let unpriced = [(1, None), (9, Some(1.0))];
        assert_eq!(summary_of(&loaded(&rows, &[], &unpriced)).cost_total, None);
    }

    #[test]
    fn results_cursor_parsing() {
        let item = uuid_of(7);
        assert_eq!(
            parse_results_cursor(&format!("{item}:3")).ok(),
            Some((item, 3))
        );
        let bare = item.to_string();
        let negative = format!("{item}:-1");
        let letter = format!("{item}:a");
        for bad in ["x", bare.as_str(), negative.as_str(), letter.as_str()] {
            assert!(parse_results_cursor(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn comparison_regressions() {
        let base = || {
            loaded(
                &[(1, 0, Some(1), false, None)],
                &[(1, "exact", "numeric", Some(1.0), None)],
                &[],
            )
        };
        let lower = loaded(
            &[(1, 0, Some(2), false, None)],
            &[(2, "exact", "numeric", Some(0.0), None)],
            &[],
        );
        let close = loaded(
            &[(1, 0, Some(2), false, None)],
            &[(2, "exact", "numeric", Some(0.95), None)],
            &[],
        );
        let failed = loaded(&[(1, 0, None, true, None)], &[], &[]);
        let page = |candidate: Loaded| {
            let (items, _) = match compare_items(&[base(), candidate], 0.1, true, 10, None) {
                Ok(page) => page,
                Err(_) => panic!("compare_items failed"),
            };
            items
        };
        assert_eq!(page(lower), vec![uuid_of(1)]);
        assert!(page(close).is_empty());
        assert_eq!(page(failed), vec![uuid_of(1)]);

        // regressions_only keeps only the regressed items: item 1 drops, item 2 does not.
        let pair = || {
            [
                loaded(
                    &[(1, 0, Some(1), false, None), (2, 0, Some(4), false, None)],
                    &[
                        (1, "exact", "numeric", Some(1.0), None),
                        (4, "exact", "numeric", Some(1.0), None),
                    ],
                    &[],
                ),
                loaded(
                    &[(1, 0, Some(2), false, None), (2, 0, Some(3), false, None)],
                    &[
                        (2, "exact", "numeric", Some(0.0), None),
                        (3, "exact", "numeric", Some(1.0), None),
                    ],
                    &[],
                ),
            ]
        };
        let (all, _) = match compare_items(&pair(), 0.1, false, 10, None) {
            Ok(page) => page,
            Err(_) => panic!("compare_items failed"),
        };
        assert_eq!(all, vec![uuid_of(1), uuid_of(2)]);
        let (only, _) = match compare_items(&pair(), 0.1, true, 10, None) {
            Ok(page) => page,
            Err(_) => panic!("compare_items failed"),
        };
        assert_eq!(only, vec![uuid_of(1)]);
    }

    #[test]
    fn comparison_repetitions_average() {
        let candidate = loaded(
            &[(1, 0, Some(1), false, None), (1, 1, Some(2), false, None)],
            &[
                (1, "exact", "numeric", Some(1.0), None),
                (2, "exact", "numeric", Some(0.0), None),
            ],
            &[],
        );
        assert_eq!(cell_of(&candidate, 1).scores.get("exact"), Some(&0.5));
    }

    #[test]
    fn comparison_warnings_and_deltas() {
        let base = named(
            loaded(
                &[(1, 0, Some(1), false, None)],
                &[(1, "exact", "numeric", Some(1.0), None)],
                &[],
            ),
            0xe01,
            1,
        );
        let candidate = named(
            loaded(
                &[(1, 0, Some(2), false, None), (2, 0, Some(3), false, None)],
                &[
                    (2, "exact", "numeric", Some(0.5), None),
                    (3, "exact", "numeric", Some(1.0), None),
                ],
                &[],
            ),
            0xe02,
            2,
        );
        let texts = vec![
            vec![stored(1, 0, None)],
            vec![stored(1, 0, None), stored(2, 0, None)],
        ];
        let items = [uuid_of(1), uuid_of(2)];
        let comparison = match compare(&[base, candidate], &texts, &items, 0.1, None) {
            Ok(comparison) => comparison,
            Err(_) => panic!("compare failed"),
        };
        assert!(
            comparison
                .warnings
                .contains(&"experiments use different dataset versions".to_owned())
        );
        assert!(
            comparison
                .warnings
                .contains(&"1 items have no result in the baseline".to_owned())
        );
        assert_eq!(
            comparison.deltas[0].scores.get("exact"),
            Some(&ScoreDelta {
                delta: -0.5,
                n_compared: 1
            })
        );
    }

    #[test]
    fn deltas_ignore_failed_items() {
        let base = named(
            loaded(
                &[(1, 0, Some(1), false, None), (2, 0, Some(2), false, None)],
                &[
                    (1, "exact", "numeric", Some(1.0), None),
                    (2, "exact", "numeric", Some(0.0), None),
                ],
                &[],
            ),
            0xe01,
            1,
        );
        let candidate = named(
            loaded(
                &[(1, 0, Some(3), false, None), (2, 0, None, true, None)],
                &[(3, "exact", "numeric", Some(1.0), None)],
                &[],
            ),
            0xe02,
            1,
        );
        let texts = vec![
            vec![stored(1, 0, None), stored(2, 0, None)],
            vec![stored(1, 0, None), stored(2, 0, Some("boom"))],
        ];
        let items = [uuid_of(1), uuid_of(2)];
        let comparison = match compare(&[base, candidate], &texts, &items, 0.1, None) {
            Ok(comparison) => comparison,
            Err(_) => panic!("compare failed"),
        };
        assert_eq!(
            comparison.deltas[0].scores.get("exact"),
            Some(&ScoreDelta {
                delta: 0.0,
                n_compared: 1
            })
        );
        let warning = format!(
            "{}: 1 items are left out of the deltas (missing or failed in one of the two)",
            uuid_of(0xe02)
        );
        assert!(comparison.warnings.contains(&warning));
        assert!(comparison.rows[1].cells[1].regressed);
    }

    #[test]
    fn cells_only_for_experiments_with_a_result() {
        let full = |id| {
            named(
                loaded(
                    &[
                        (1, 0, None, false, None),
                        (2, 0, None, false, None),
                        (3, 0, None, false, None),
                    ],
                    &[],
                    &[],
                ),
                id,
                1,
            )
        };
        let missing_c = named(
            loaded(
                &[(1, 0, None, false, None), (2, 0, None, false, None)],
                &[],
                &[],
            ),
            0xe02,
            1,
        );
        let experiments = [full(0xe01), missing_c, full(0xe03)];
        let texts = vec![
            vec![stored(1, 0, None), stored(2, 0, None), stored(3, 0, None)],
            vec![stored(1, 0, None), stored(2, 0, None)],
            vec![stored(1, 0, None), stored(2, 0, None), stored(3, 0, None)],
        ];
        let items = [uuid_of(1), uuid_of(2), uuid_of(3)];
        let comparison = match compare(&experiments, &texts, &items, 0.1, None) {
            Ok(comparison) => comparison,
            Err(_) => panic!("compare failed"),
        };
        let ids: Vec<Uuid> = comparison.rows[2]
            .cells
            .iter()
            .map(|cell| cell.experiment_id)
            .collect();
        assert_eq!(ids, vec![uuid_of(0xe01), uuid_of(0xe03)]);
    }
}
