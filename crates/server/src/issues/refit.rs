//! The IC refits (track IC M3, D-IC-8). Every 6 h, or on demand, an LLM source's pending
//! candidates are clustered by average linkage. A cluster whose centroid is near an issue joins
//! that issue; the others are named by a chat call and become new issues.

use std::collections::HashSet;
use std::time::Duration;

use cetra_eval::issue::cluster;
use cetra_eval::{Adapter, CallTrace, CompletionInput, ErrorClass};
use serde_json::Value;
use tokio::sync::watch;
use uuid::Uuid;

use super::errors::{self, Occurrence};
use super::prompts;
use super::worker::{
    BUDGET_DETAIL, BudgetStop, CENTROID_MEMBERS, call_trace_id, pause_connection_sources, reserve,
};
use crate::eval::budget;
use crate::eval::execute::{self, JudgeUsage};
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::connections;
use crate::meta::issues::{
    self, CandidateRow, EventKind, EventRow, IssueListQuery, IssueRow, IssueSort, IssueState,
    RefitRow, SourceRow,
};
use crate::meta::projects::{self, ProjectRow};

/// How often the refit loop looks for due sources.
const TICK: Duration = Duration::from_secs(60);
/// The gap between refits of a source, in nanoseconds (D-IC-8, A8): six hours.
const REFIT_EVERY_NS: i64 = 6 * 3_600_000_000_000;
/// Fewer pending candidates than this are not clustered (D-IC-8).
const MIN_PENDING: usize = 20;
/// Most pending candidates a refit clusters (D-IC-8).
const PENDING_LIMIT: u32 = 5_000;
/// How far back pending candidates count: seven days (D-IC-8).
const WINDOW_NS: i64 = 7 * budget::DAY_NS;
/// Members a naming call sees: the ones nearest the centroid.
const NAME_MEMBERS: usize = 20;
/// Issue titles a naming call sees: the newest of the source.
const NAME_TITLES: u32 = 50;
/// Longest issue title a naming call may give, in characters.
const TITLE_CHARS: usize = 80;
/// Longest issue description, in characters.
const DESCRIPTION_CHARS: usize = 400;
/// Output cap of the naming call, in tokens (M3.5 step 7.3.4).
const NAME_TOKENS: u32 = 300;
/// Whole-call timeout of the naming call.
const NAME_TIMEOUT: Duration = Duration::from_secs(60);
/// The wait between two tries of a chat slot (M3.5 step 7.3.2). Tests use a short real wait
/// instead of a paused clock: tokio's paused clock jumps ahead whenever the runtime waits on
/// SQLite's blocking threads, which fires sqlx's own timeouts under load.
const SLOT_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(10)
} else {
    Duration::from_secs(1)
};
/// How long a naming call waits for a chat slot before the refit stops as `busy`.
const SLOT_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(120)
};

/// Runs the refit loop until `stop` is set. Each pass refits the due sources one after the other,
/// so the loop is the process-wide limit of one refit at a time.
pub(crate) async fn run_refits(state: AppState, mut stop: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
        }
        let now = state.clock.now_ns();
        let due = match issues::due_refits(state.meta.reader(), now).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!(error = %error, "issue refits could not be listed");
                continue;
            }
        };
        for (source, trigger) in due {
            if *stop.borrow() {
                return;
            }
            let now = state.clock.now_ns();
            if let Err(error) = refit_source(&state, &source, trigger, now).await {
                tracing::error!(
                    source_id = %source.id,
                    error = %error,
                    "issue refit could not be recorded"
                );
            }
        }
    }
}

/// Runs one refit of `source` (D-IC-8) and records it. A refit that stops early records its error
/// class and keeps the issues it created. The next refit is due six hours after this one ends
/// (A8). Returns the stored refit row.
///
/// # Errors
///
/// Returns [`MetaError`] when the refit row cannot be written or finished. A `meta.db` failure in
/// the middle of the refit is recorded as the `storage` error class instead.
pub(crate) async fn refit_source(
    state: &AppState,
    source: &SourceRow,
    trigger: &'static str,
    now: i64,
) -> Result<RefitRow, MetaError> {
    let mut row = RefitRow {
        id: Uuid::now_v7(),
        source_id: source.id,
        trigger,
        started_at: now,
        finished_at: None,
        pending: 0,
        clusters: 0,
        created: 0,
        joined: 0,
        outliers: 0,
        cost_usd: 0.0,
        error_class: None,
    };
    let opened = row.clone();
    state
        .meta
        .write(move |conn| Box::pin(async move { issues::insert_refit(conn, &opened).await }))
        .await?;
    if let Err(stop) = steps(state, source, &mut row, now).await {
        let class = match stop {
            Stop::Class(class) => class.to_owned(),
            Stop::Meta(error) => {
                tracing::error!(
                    source_id = %source.id,
                    refit_id = %row.id,
                    error = %error,
                    "issue refit failed in storage"
                );
                "storage".to_owned()
            }
        };
        row.error_class = Some(class);
    }
    let finished = state.clock.now_ns();
    row.finished_at = Some(finished);
    let next = finished.saturating_add(REFIT_EVERY_NS);
    let stored = row.clone();
    let source_id = source.id;
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                issues::finish_refit(conn, &stored).await?;
                issues::set_next_refit(conn, source_id, next, "schedule").await
            })
        })
        .await?;
    tracing::info!(
        source_id = %source.id,
        refit_id = %row.id,
        trigger,
        pending = row.pending,
        clusters = row.clusters,
        created = row.created,
        joined = row.joined,
        outliers = row.outliers,
        cost_usd = row.cost_usd,
        error_class = row.error_class.as_deref().unwrap_or("none"),
        "issue refit finished"
    );
    Ok(row)
}

/// Why a refit stops before its last step: an error class, or a `meta.db` failure.
enum Stop {
    Class(&'static str),
    Meta(MetaError),
}

impl From<MetaError> for Stop {
    fn from(error: MetaError) -> Self {
        Self::Meta(error)
    }
}

/// What every step of a refit reads: the state, the source and its project, and the refit's time.
#[derive(Clone, Copy)]
struct Scope<'a> {
    state: &'a AppState,
    source: &'a SourceRow,
    project: &'a ProjectRow,
    now: i64,
}

/// The counters of a refit, steps 2 and 3 to 8 of M3.5.
async fn steps(
    state: &AppState,
    source: &SourceRow,
    row: &mut RefitRow,
    now: i64,
) -> Result<(), Stop> {
    let reader = state.meta.reader();
    let source_id = source.id;
    let since = now.saturating_sub(WINDOW_NS);
    // Step 2: candidates older than seven days that are still pending become outliers.
    let outliers = state
        .meta
        .write(move |conn| {
            Box::pin(async move { issues::mark_outliers(conn, source_id, since, now).await })
        })
        .await?;
    row.outliers = count32(outliers);
    // Step 3: with fewer than 20 pending candidates, nothing is clustered.
    let pending = issues::pending_candidates(reader, source_id, since, PENDING_LIMIT).await?;
    row.pending = count32(pending.len());
    if pending.len() < MIN_PENDING {
        return Ok(());
    }
    let Some(project) = projects::project_by_id(reader, source.project_id).await? else {
        return Err(Stop::Class("permanent"));
    };
    let Some(floor) = source.floor else {
        return Err(Stop::Class("permanent"));
    };
    let Some(min_issue_size) = source.min_issue_size else {
        return Err(Stop::Class("permanent"));
    };
    let min_size = usize::try_from(min_issue_size).unwrap_or(usize::MAX);
    // Step 4: the candidates in the order of D-IC-17, with usable vectors only.
    let dimension = source
        .embed_dimension
        .and_then(|dimension| usize::try_from(dimension).ok());
    let mut usable: Vec<CandidateRow> = pending
        .into_iter()
        .filter(|candidate| {
            candidate
                .vector
                .as_ref()
                .is_some_and(|vector| Some(vector.len()) == dimension)
        })
        .collect();
    usable.sort_by_key(|candidate| (candidate.created_at, candidate.id));
    let vectors: Vec<Vec<f32>> = usable
        .iter()
        .filter_map(|candidate| candidate.vector.clone())
        .collect();
    // Step 5: average linkage, off the async threads.
    let max_distance = (1.0 - floor) as f32;
    let clustered =
        tokio::task::spawn_blocking(move || cluster::average_linkage(&vectors, max_distance))
            .await
            .map_err(|_| Stop::Class("cluster"))?
            .map_err(|_| Stop::Class("cluster"))?;
    // Step 6: the clusters of at least `min_issue_size` members, largest first, then by first index.
    let mut kept: Vec<Vec<usize>> = clustered
        .into_iter()
        .filter(|members| members.len() >= min_size)
        .collect();
    kept.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| a.first().cmp(&b.first()))
    });
    row.clusters = count32(kept.len());
    let existing = issues::list_issues(
        reader,
        source.project_id,
        &IssueListQuery {
            states: &[],
            source: Some(source_id),
            q: None,
            ids: None,
            sort: IssueSort::FirstSeen,
            cursor: None,
            limit: NAME_TITLES,
        },
    )
    .await?;
    let mut titles: Vec<String> = existing
        .into_iter()
        .take(NAME_TITLES as usize)
        .map(|issue| issue.title)
        .collect();
    let scope = Scope {
        state,
        source,
        project: &project,
        now,
    };
    // The occurrences older than the project's retention are not written (D-IC-18).
    let kept_from = errors::retention_cutoff(&project, now);
    // Step 7: each kept cluster joins an issue or is named into a new one.
    for (index, members) in kept.iter().enumerate() {
        let group: Vec<&CandidateRow> = members.iter().map(|&position| &usable[position]).collect();
        let vectors: Vec<&[f32]> = group
            .iter()
            .filter_map(|candidate| candidate.vector.as_deref())
            .collect();
        // Step 7.1: the normalized mean of the members.
        let centroid = centroid_of(&vectors);
        // Step 7.2: a join when the nearest issue centroid reaches the floor.
        if let Some((issue_id, assigned)) = join(scope, &centroid, &group).await? {
            row.joined = row.joined.saturating_add(count32(assigned.len()));
            let assigned: HashSet<Uuid> = assigned.into_iter().collect();
            let occurrences =
                occurrences_of(&group, &assigned, issue_id, source_id, now, kept_from);
            write_members(state, &project, source_id, occurrences).await;
            continue;
        }
        // Step 7.3: name the cluster, then create its issue.
        let (title, description) = name(scope, row, &group, &centroid, index, &titles).await?;
        let (issue_id, assigned) =
            create_issue(scope, &group, &title, description.as_deref()).await?;
        if assigned.is_empty() {
            continue;
        }
        row.created = row.created.saturating_add(1);
        titles.push(title);
        let assigned: HashSet<Uuid> = assigned.into_iter().collect();
        let occurrences = occurrences_of(&group, &assigned, issue_id, source_id, now, kept_from);
        write_members(state, &project, source_id, occurrences).await;
    }
    Ok(())
}

/// Step 7.2: when the source's nearest issue centroid has a cosine with `centroid` of at least the
/// floor, the group's pending candidates join that issue in one meta write. The write also moves
/// the issue's centroid and member count (D-IC-7). Returns the issue and the candidates it took,
/// or `None` when no issue is near enough.
async fn join(
    scope: Scope<'_>,
    centroid: &[f32],
    group: &[&CandidateRow],
) -> Result<Option<(Uuid, Vec<Uuid>)>, MetaError> {
    let Scope {
        state,
        source,
        project,
        now,
    } = scope;
    let Some(floor) = source.floor else {
        return Ok(None);
    };
    let source_id = source.id;
    let project_id = project.id;
    let ids: Vec<Uuid> = group.iter().map(|candidate| candidate.id).collect();
    let vectors = vectors_of(group);
    let (first, last) = trace_span(group, now);
    let centroid = centroid.to_vec();
    let clock = state.clock.clone();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The events' `at` is read in the write, never before it (D-IC-21).
                let at = clock.now_ns();
                let centroids = issues::source_centroids(conn, source_id).await?;
                // A centroid of another length cannot be compared; the group is then named.
                let nearest = cluster::nearest(
                    &centroid,
                    centroids.iter().map(|(_, _, vector, _)| vector.as_slice()),
                )
                .ok()
                .flatten();
                let Some((index, similarity)) = nearest else {
                    return Ok(None);
                };
                if f64::from(similarity) < floor {
                    return Ok(None);
                }
                let issue_id = centroids[index].0;
                let Some(mut issue) = issues::get_issue_tx(conn, project_id, issue_id).await?
                else {
                    return Ok(None);
                };
                let assigned = issues::set_candidates_assigned(conn, &ids, issue_id, now).await?;
                if assigned.is_empty() {
                    return Ok(Some((issue_id, assigned)));
                }
                let added = u64::try_from(assigned.len()).unwrap_or(u64::MAX);
                // The issue's centroid moves by the members this write joined, not by the whole
                // group: a member that left `pending` meanwhile is not in it (D-IC-7).
                let joined = centroid_of_assigned(&vectors, &assigned);
                errors::join_issue(conn, &mut issue, (first, last), at).await?;
                let members = issue.member_count;
                let merged = match issue.centroid.as_deref() {
                    Some(old) if members < CENTROID_MEMBERS => {
                        cluster::merge_centroids(old, members, &joined, added).ok()
                    }
                    _ => None,
                };
                if let Some(merged) = merged {
                    issue.centroid = Some(merged);
                }
                issue.member_count = members.saturating_add(added);
                issues::update_issue(conn, &issue).await?;
                Ok(Some((issue_id, assigned)))
            })
        })
        .await
}

/// Step 7.3.1 to 7.3.6: names the cluster with one chat call and returns its title and
/// description. The title is trimmed, cut to 80 characters and made unique among `titles`; the
/// description is trimmed and cut to 400 characters (an empty one is `None`).
async fn name(
    scope: Scope<'_>,
    row: &mut RefitRow,
    group: &[&CandidateRow],
    centroid: &[f32],
    index: usize,
    titles: &[String],
) -> Result<(String, Option<String>), Stop> {
    let Scope {
        state,
        source,
        project,
        now,
    } = scope;
    // Step 7.3.1: the members nearest the centroid, then the prompt.
    let mut ranked: Vec<(f32, &CandidateRow)> = group
        .iter()
        .filter_map(|candidate| {
            let vector = candidate.vector.as_deref()?;
            let similarity = cluster::cosine(vector, centroid).unwrap_or(f32::NEG_INFINITY);
            Some((similarity, *candidate))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    let summaries: Vec<&str> = ranked
        .iter()
        .take(NAME_MEMBERS)
        .filter_map(|(_, candidate)| candidate.summary.as_deref())
        .collect();
    let title_refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    let prompt = prompts::name_prompt(source.kind, &summaries, &title_refs);
    // Step 7.3.2: a chat connection slot, retried every second for up to two minutes.
    let chat = source.chat_connection_id.ok_or(Stop::Class("permanent"))?;
    let model = source
        .chat_model
        .as_deref()
        .ok_or(Stop::Class("permanent"))?;
    let connection_row = connections::get_connection(state.meta.reader(), project.org_id, chat)
        .await?
        .ok_or(Stop::Class("permanent"))?;
    let adapter =
        Adapter::from_name(&connection_row.adapter).map_err(|_| Stop::Class("permanent"))?;
    let connection = state
        .connections
        .get(&state.secrets, &connection_row)
        .map_err(|_| Stop::Class("permanent"))?;
    let prices = state
        .ingest
        .prices()
        .for_project(project.org_id, project.id);
    let limits = (
        connection_row.max_concurrency,
        connection_row.requests_per_minute,
    );
    let deadline = tokio::time::Instant::now() + SLOT_WAIT;
    let _slot = loop {
        if let Some(slot) = state
            .eval
            .try_connection_permit(chat, limits, state.clock.now_ns())
        {
            break slot;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Stop::Class("busy"));
        }
        tokio::time::sleep(SLOT_RETRY).await;
    };
    // Step 7.3.3: the budget, as for a summary.
    let estimate = budget::model_price(&prices, model, adapter, now)
        .map(|price| budget::estimate_usd(prompt.chars().count(), NAME_TOKENS, price));
    let reservation = match estimate {
        Some(estimate) => match reserve(state, source, estimate, now).await {
            Ok(reservation) => reservation,
            Err(stop) => return Err(budget_stop(state, source, stop, now).await),
        },
        None => None,
    };
    // Step 7.3.4: the call and its trace, which holds only the summaries (not guarded, as M3.5
    // states).
    let schema = prompts::name_schema();
    let source_text = source.id.to_string();
    let metadata = [("cetra.issue.source_id", source_text.as_str())];
    let ingest = state.ingest.context(project, None, now);
    let call = count32(index).saturating_add(3);
    let trace_id = call_trace_id(row.id, 0, call);
    let run = state
        .eval_runner
        .complete(CompletionInput {
            connection: &connection,
            model,
            prompt: &prompt,
            schema: &schema,
            name: prompts::NAME_NAME,
            description: "Name the issue.",
            temperature: Some(0.0),
            max_output_tokens: NAME_TOKENS,
            timeout: NAME_TIMEOUT,
            trace: CallTrace {
                trace_id,
                root_name: "cetra.issue.name",
                metadata: &metadata,
                ingest: &ingest,
                prices: &prices,
            },
        })
        .await;
    // Step 7.3.5: the usage counts, the reservation settles and the spend is added.
    let usage = JudgeUsage {
        input_tokens: run.usage.as_ref().map(|usage| usage.input),
        output_tokens: run.usage.as_ref().map(|usage| usage.output),
        cost_usd: run.cost_usd,
    };
    let actual = execute::spent(Some(usage), estimate);
    execute::count_judge_usage(state, project, state.clock.now_ns(), usage);
    if let Some(reservation) = reservation {
        reservation.settle(actual);
    }
    row.cost_usd += actual;
    if actual > 0.0 {
        let source_id = source.id;
        let day = budget::day_start(now);
        state
            .meta
            .write(move |conn| {
                Box::pin(async move { issues::add_spend(conn, source_id, day, actual).await })
            })
            .await?;
    }
    if let Err(error) =
        execute::commit_judge_trace(state, project, state.clock.now_ns(), run.trace).await
    {
        tracing::warn!(
            refit_id = %row.id,
            error = %error,
            "issue naming trace was not committed"
        );
    }
    let answer = match run.outcome {
        Ok(answer) => answer,
        Err(error) => {
            let class = error.class();
            if class == ErrorClass::Pause {
                // A connection error pauses the sources that use the chat connection (M3.6a).
                let detail = error.to_string();
                let (org, updated_at) = (project.org_id, connection_row.updated_at);
                let paused_at = state.clock.now_ns();
                state
                    .meta
                    .write(move |conn| {
                        Box::pin(async move {
                            pause_connection_sources(
                                conn, org, chat, updated_at, &detail, paused_at,
                            )
                            .await
                        })
                    })
                    .await
                    .map_err(Stop::Meta)?;
            }
            return Err(Stop::Class(class_name(class)));
        }
    };
    // Step 7.3.6: the title and the description.
    let title = answer
        .get("title")
        .and_then(Value::as_str)
        .ok_or(Stop::Class("permanent"))?;
    let description = answer
        .get("description")
        .and_then(Value::as_str)
        .ok_or(Stop::Class("permanent"))?;
    let title: String = title.trim().chars().take(TITLE_CHARS).collect();
    if title.is_empty() {
        return Err(Stop::Class("permanent"));
    }
    let description: String = description.trim().chars().take(DESCRIPTION_CHARS).collect();
    Ok((
        unique_title(&title, titles),
        (!description.is_empty()).then_some(description),
    ))
}

/// Step 7.3.7: creates the issue of a named cluster in one meta write. Its candidates are assigned
/// to it in the same write, and its centroid and member count are those of the members the write
/// assigned (a member that left `pending` since the read is not counted). Returns the issue and
/// the candidates assigned.
///
/// When no member is still `pending` in this write (a deletion removed them since the read), no
/// issue is written and the returned list is empty. The `any_pending` check and the assignment
/// share the write, so when it passes, the assignment takes at least one member.
async fn create_issue(
    scope: Scope<'_>,
    group: &[&CandidateRow],
    title: &str,
    description: Option<&str>,
) -> Result<(Uuid, Vec<Uuid>), MetaError> {
    let Scope {
        state, source, now, ..
    } = scope;
    let (first, last) = trace_span(group, now);
    let issue_id = Uuid::now_v7();
    let mut issue = IssueRow {
        id: issue_id,
        project_id: source.project_id,
        source_id: source.id,
        fingerprint: None,
        title: title.to_owned(),
        description: description.map(str::to_owned),
        state: IssueState::Open,
        regressed: false,
        merged_into: None,
        first_seen: first,
        last_seen: last,
        resolved_at: None,
        resolved_by: None,
        created_at: now,
        updated_at: now,
        centroid: None,
        member_count: 0,
    };
    let ids: Vec<Uuid> = group.iter().map(|candidate| candidate.id).collect();
    let vectors = vectors_of(group);
    let clock = state.clock.clone();
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The event's `at` is read in the write, never before it (D-IC-21).
                let at = clock.now_ns();
                if !issues::any_pending(conn, &ids).await? {
                    return Ok((issue_id, Vec::new()));
                }
                issues::insert_issue(conn, &issue).await?;
                let assigned = issues::set_candidates_assigned(conn, &ids, issue_id, now).await?;
                issue.centroid = Some(centroid_of_assigned(&vectors, &assigned));
                issue.member_count = u64::try_from(assigned.len()).unwrap_or(u64::MAX);
                issues::update_issue(conn, &issue).await?;
                issues::insert_event(
                    conn,
                    &EventRow {
                        id: Uuid::now_v7(),
                        issue_id,
                        kind: EventKind::Created,
                        actor_user_id: None,
                        from_value: None,
                        to_value: Some(IssueState::Open.as_str().to_owned()),
                        at,
                    },
                )
                .await?;
                Ok((issue_id, assigned))
            })
        })
        .await
}

/// The occurrences of the assigned members of a group on `issue` (A3, A10): one per candidate
/// that has a trace and is not older than `kept_from`, the project's retention cutoff.
fn occurrences_of(
    group: &[&CandidateRow],
    assigned: &HashSet<Uuid>,
    issue: Uuid,
    source: Uuid,
    now: i64,
    kept_from: Option<i64>,
) -> Vec<Occurrence> {
    group
        .iter()
        .filter(|candidate| assigned.contains(&candidate.id))
        .filter_map(|candidate| occurrence(candidate, source, issue, now, kept_from))
        .collect()
}

/// The occurrence of one candidate on `issue`. `None` when the candidate has no trace, or when its
/// trace is older than the retention cutoff `kept_from` (as `keep_hits` decides it).
fn occurrence(
    candidate: &CandidateRow,
    source: Uuid,
    issue: Uuid,
    now: i64,
    kept_from: Option<i64>,
) -> Option<Occurrence> {
    let at = candidate
        .trace_start
        .unwrap_or(candidate.created_at)
        .min(now);
    if errors::too_old(kept_from, at) {
        return None;
    }
    Some(Occurrence {
        source_id: source,
        issue_id: issue,
        trace_id: candidate.trace_id?,
        span_id: candidate.span_id,
        at,
        user_id: candidate.user_id.clone(),
        session_id: candidate.session_id.clone(),
        reason: candidate.summary.clone(),
    })
}

/// Writes occurrence scores through the errors writer. A failure is logged and keeps the
/// assignments (as the candidate worker does).
async fn write_members(
    state: &AppState,
    project: &ProjectRow,
    source: Uuid,
    occurrences: Vec<Occurrence>,
) {
    if occurrences.is_empty() {
        return;
    }
    if errors::write_occurrences(state, project, source, occurrences)
        .await
        .is_err()
    {
        tracing::warn!(source_id = %source, "issue occurrences of a refit were not written");
    }
}

/// The first and last trace start of a group, nanoseconds (the trace start stands in for a
/// candidate without one).
fn trace_span(group: &[&CandidateRow], now: i64) -> (i64, i64) {
    let starts: Vec<i64> = group
        .iter()
        .map(|candidate| candidate.trace_start.unwrap_or(candidate.created_at))
        .collect();
    (
        starts.iter().copied().min().unwrap_or(now),
        starts.iter().copied().max().unwrap_or(now),
    )
}

/// The vectors of a group's candidates, by candidate id. A candidate without a vector is left out.
fn vectors_of(group: &[&CandidateRow]) -> Vec<(Uuid, Vec<f32>)> {
    group
        .iter()
        .filter_map(|candidate| Some((candidate.id, candidate.vector.clone()?)))
        .collect()
}

/// The normalized mean of the vectors of the `assigned` candidates among `vectors` (step 7.1, as
/// the centroid of the members a write actually takes, D-IC-7).
fn centroid_of_assigned(vectors: &[(Uuid, Vec<f32>)], assigned: &[Uuid]) -> Vec<f32> {
    let assigned: HashSet<Uuid> = assigned.iter().copied().collect();
    let taken: Vec<&[f32]> = vectors
        .iter()
        .filter(|(id, _)| assigned.contains(id))
        .map(|(_, vector)| vector.as_slice())
        .collect();
    centroid_of(&taken)
}

/// The normalized mean of the vectors (step 7.1). A zero mean takes the first vector.
fn centroid_of(vectors: &[&[f32]]) -> Vec<f32> {
    let dimension = vectors.first().map_or(0, |vector| vector.len());
    let mut sum = vec![0.0_f64; dimension];
    for vector in vectors {
        for (total, value) in sum.iter_mut().zip(vector.iter()) {
            *total += f64::from(*value);
        }
    }
    let mut centroid: Vec<f32> = sum.into_iter().map(|total| total as f32).collect();
    if cluster::l2_normalize(&mut centroid) {
        return centroid;
    }
    vectors
        .first()
        .map(|vector| vector.to_vec())
        .unwrap_or_default()
}

/// `candidate`, or `candidate (2)`, `candidate (3)`, … when a title equal to it (case-insensitive)
/// is taken (step 7.3.6).
fn unique_title(title: &str, taken: &[String]) -> String {
    let is_taken = |candidate: &str| {
        let candidate = candidate.to_lowercase();
        taken.iter().any(|known| known.to_lowercase() == candidate)
    };
    if !is_taken(title) {
        return title.to_owned();
    }
    (2_u32..)
        .map(|number| format!("{title} ({number})"))
        .find(|candidate| !is_taken(candidate.as_str()))
        .unwrap_or_else(|| title.to_owned())
}

/// The error class of a failed naming call, as the candidate worker names it.
fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Retryable => "retryable",
        ErrorClass::Permanent => "permanent",
        ErrorClass::Pause => "pause",
    }
}

/// The refit's stop for a budget that does not reserve (D-I-15). Over budget, the source pauses
/// with `budget` (M3.6a) and the refit stops with that class. A spend that is not loaded yet stops
/// the refit with the same class, without a pause.
async fn budget_stop(state: &AppState, source: &SourceRow, stop: BudgetStop, now: i64) -> Stop {
    match stop {
        BudgetStop::Over => {
            let source_id = source.id;
            let paused = state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        issues::pause_source(conn, source_id, "budget", Some(BUDGET_DETAIL), now)
                            .await
                    })
                })
                .await;
            match paused {
                Ok(()) => {
                    tracing::info!(
                        target: "audit",
                        source_id = %source_id,
                        reason = "budget",
                        "issue source paused"
                    );
                    Stop::Class("budget")
                }
                Err(error) => Stop::Meta(error),
            }
        }
        BudgetStop::NotLoaded => Stop::Class("budget"),
        BudgetStop::Meta(error) => Stop::Meta(error),
    }
}

/// A count as the refit row stores it, saturating at `u32::MAX`.
fn count32(value: impl TryInto<u32>) -> u32 {
    value.try_into().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use cetra_core::ids::{ProjectId, TraceId};
    use serde_json::json;

    use super::*;
    use crate::issues::triggers::{Place, queued};
    use crate::meta::issues::{
        CandidateStatus, IssueRow, IssueState, SourceKind, insert_candidate, insert_issue,
        insert_source,
    };
    use crate::test_support::mock_llm::{MockLlm, Reply, completion, error as error_reply};
    use crate::test_support::{Seeded, TestApp};

    const MINUTE: i64 = 60_000_000_000;
    const SIX_HOURS: i64 = 6 * 3_600_000_000_000;

    /// A trace id, unique for each `n` (never all zero).
    fn trace(n: u8) -> TraceId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 1;
        bytes[15] = n;
        TraceId::from_slice(&bytes).expect("not all zero")
    }

    /// A unit vector on `axis`, tilted towards the next axis by `tilt`.
    fn near(axis: usize, tilt: f32) -> Vec<f32> {
        let mut vector = vec![0.0_f32; 3];
        vector[axis] = 1.0;
        vector[(axis + 1) % 3] = tilt;
        assert!(cluster::l2_normalize(&mut vector));
        vector
    }

    /// A naming reply with `title`.
    fn name(title: &str) -> Reply {
        completion(&json!({"title": title, "description": "Refit names the group."}).to_string())
    }

    /// An app that may reach a mock on `127.0.0.1`, with its clock at the real time and its seed.
    async fn llm_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let real_now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64");
        app.state.clock.set(real_now);
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// A chat and embedding connection of `acme` that reaches `mock`, created through the API.
    async fn connection(app: &TestApp, seeded: &Seeded, mock: &MockLlm) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test"
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// A `score` source with `connection` as both its chat and embedding connection, dimension 3,
    /// floor 0.9 and minimum issue size 2, with no budget. Not stored.
    fn llm_source(project: ProjectId, connection: Uuid, now: i64) -> SourceRow {
        SourceRow {
            id: Uuid::now_v7(),
            project_id: project,
            name: format!("Refit {}", Uuid::now_v7()),
            kind: SourceKind::Score,
            filter: None,
            enabled: true,
            paused_reason: None,
            paused_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
            sample_rate: Some(1.0),
            daily_cap: Some(100),
            score_names: vec!["quality".to_owned()],
            feedback_sources: Vec::new(),
            chat_connection_id: Some(connection),
            chat_model: Some("gpt-test".to_owned()),
            embed_connection_id: Some(connection),
            embed_model: Some("text-embedding-test".to_owned()),
            embed_dimension: Some(3),
            floor: Some(0.9),
            min_issue_size: Some(2),
            budget_usd: None,
            budget_period: None,
            paused_detail: None,
            next_refit_at: None,
        }
    }

    /// `row`, stored.
    async fn store_source(app: &TestApp, row: SourceRow) -> SourceRow {
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
            .await
            .expect("source stored");
        stored
    }

    /// A pending candidate of `source` on `trace(n)` with `vector`, created at `created_at`.
    async fn pending(
        app: &TestApp,
        source: &SourceRow,
        n: u8,
        vector: Vec<f32>,
        created_at: i64,
    ) -> CandidateRow {
        let mut row = queued(
            created_at,
            source.project_id,
            source.id,
            Place::Trace {
                trace: trace(n),
                session: None,
            },
            None,
            Some(created_at),
            None,
        );
        row.status = CandidateStatus::Pending;
        row.vector = Some(vector);
        row.summary = Some("Login fails on refresh.".to_owned());
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_candidate(conn, &row).await }))
            .await
            .expect("candidate stored");
        stored
    }

    /// An open issue of `source` with `centroid` and `member_count`, stored. Returns its id.
    async fn existing_issue(
        app: &TestApp,
        project: ProjectId,
        source: Uuid,
        centroid: Option<Vec<f32>>,
        title: &str,
        member_count: u64,
    ) -> Uuid {
        let now = app.state.clock.now_ns();
        let id = Uuid::now_v7();
        let row = IssueRow {
            id,
            project_id: project,
            source_id: source,
            fingerprint: None,
            title: title.to_owned(),
            description: None,
            state: IssueState::Open,
            regressed: false,
            merged_into: None,
            first_seen: now,
            last_seen: now,
            resolved_at: None,
            resolved_by: None,
            created_at: now,
            updated_at: now,
            centroid,
            member_count,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_issue(conn, &row).await }))
            .await
            .expect("issue stored");
        id
    }

    /// The status of candidate `id`.
    async fn status_of(app: &TestApp, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM issue_candidates WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("status read")
    }

    /// One count from `meta.db`.
    async fn count(app: &TestApp, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count read")
    }

    /// The titles of the source's issues, sorted.
    async fn titles(app: &TestApp, source: Uuid) -> Vec<String> {
        sqlx::query_scalar("SELECT title FROM issues WHERE source_id = ? ORDER BY title")
            .bind(source.to_string())
            .fetch_all(app.state.meta.reader())
            .await
            .expect("titles read")
    }

    #[tokio::test]
    async fn two_tight_groups_become_two_named_issues() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Refund refused"), name("Login fails")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for i in 0..12_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        for i in 0..13_u8 {
            pending(
                &app,
                &source,
                i + 20,
                near(1, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class, None);
        assert_eq!(
            (
                row.pending,
                row.clusters,
                row.created,
                row.joined,
                row.outliers
            ),
            (25, 2, 2, 0, 0)
        );
        assert_eq!(
            titles(&app, source.id).await,
            vec!["Login fails".to_owned(), "Refund refused".to_owned()]
        );
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM issue_candidates WHERE status = 'assigned' AND issue_id IS NOT NULL"
            )
            .await,
            25
        );
        assert_eq!(mock.requests().len(), 2);
        // The next refit is due six hours after this one finished (A8).
        let finished = row.finished_at.expect("the refit finished");
        let stored = issues::get_source(app.state.meta.reader(), seeded.project.id, source.id)
            .await
            .expect("source read")
            .expect("source exists");
        assert_eq!(stored.next_refit_at, Some(finished + SIX_HOURS));
    }

    #[tokio::test]
    async fn a_group_near_an_issue_joins_it() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Login fails")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let existing = existing_issue(
            &app,
            seeded.project.id,
            source.id,
            Some(near(0, 0.0)),
            "Refund questions",
            3,
        )
        .await;
        for i in 0..12_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        for i in 0..13_u8 {
            pending(
                &app,
                &source,
                i + 20,
                near(1, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class, None);
        assert_eq!((row.clusters, row.created, row.joined), (2, 1, 12));
        // Only the group that did not join is named.
        assert_eq!(mock.requests().len(), 1);
        let joined: i64 = count(
            &app,
            &format!("SELECT COUNT(*) FROM issue_candidates WHERE issue_id = '{existing}'"),
        )
        .await;
        assert_eq!(joined, 12);
        let members: i64 = sqlx::query_scalar("SELECT member_count FROM issues WHERE id = ?")
            .bind(existing.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("member count read");
        assert_eq!(members, 15);
    }

    #[tokio::test]
    async fn fewer_than_twenty_pending_are_not_clustered() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Unused")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for i in 0..19_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class, None);
        assert_eq!((row.pending, row.clusters, row.created), (19, 0, 0));
        assert!(mock.requests().is_empty());
        assert_eq!(count(&app, "SELECT COUNT(*) FROM issues").await, 0);
        assert_eq!(
            count(
                &app,
                "SELECT COUNT(*) FROM issue_candidates WHERE status = 'pending'"
            )
            .await,
            19
        );
    }

    #[tokio::test]
    async fn a_small_cluster_stays_pending_and_an_old_candidate_is_an_outlier() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Refund refused")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for i in 0..19_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        // A lone candidate on another axis: a cluster of one, below the minimum of two.
        let lone = pending(&app, &source, 40, near(2, 0.0), now - MINUTE).await;
        let old = pending(&app, &source, 41, near(0, 0.0), now - 8 * budget::DAY_NS).await;

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class, None);
        assert_eq!(
            (row.pending, row.outliers, row.clusters, row.created),
            (20, 1, 1, 1)
        );
        assert_eq!(status_of(&app, lone.id).await, "pending");
        assert_eq!(status_of(&app, old.id).await, "outlier");
    }

    #[tokio::test]
    async fn a_duplicate_title_gets_a_number() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("refund refused")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        existing_issue(
            &app,
            seeded.project.id,
            source.id,
            None,
            "Refund refused",
            0,
        )
        .await;
        for i in 0..20_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class, None);
        assert_eq!(row.created, 1);
        assert!(
            titles(&app, source.id)
                .await
                .contains(&"refund refused (2)".to_owned())
        );
    }

    #[tokio::test]
    async fn a_naming_error_ends_the_refit_and_keeps_the_issues_before() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![
            name("Refund refused"),
            error_reply(400, "bad request"),
        ])
        .await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for i in 0..12_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        for i in 0..13_u8 {
            pending(
                &app,
                &source,
                i + 20,
                near(1, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        // The larger group is named first and its issue stays; the second naming call fails.
        assert!(row.error_class.is_some());
        assert_eq!((row.clusters, row.created), (2, 1));
        assert_eq!(
            titles(&app, source.id).await,
            vec!["Refund refused".to_owned()]
        );
    }

    #[tokio::test]
    async fn no_issue_is_created_when_no_member_is_still_pending() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(Vec::new()).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let project = projects::project_by_id(app.state.meta.reader(), seeded.project.id)
            .await
            .expect("project read")
            .expect("project");
        // A member the refit read but a deletion removed before the create write: it is never
        // stored, so the write finds it not pending.
        let member = pending(&app, &source, 90, near(0, 0.0), now - MINUTE).await;
        let member_id = member.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM issue_candidates WHERE id = ?")
                        .bind(member_id.to_string())
                        .execute(&mut *conn)
                        .await
                        .map_err(MetaError::from)?;
                    Ok(())
                })
            })
            .await
            .expect("candidate deleted");
        let scope = Scope {
            state: &app.state,
            source: &source,
            project: &project,
            now,
        };

        let (_, assigned) = create_issue(scope, &[&member], "Ghost", None)
            .await
            .expect("create runs");

        assert!(assigned.is_empty());
        assert_eq!(count(&app, "SELECT COUNT(*) FROM issues").await, 0);
    }

    /// The centroid of `issue`, with its member count.
    async fn centroid_and_members(
        app: &TestApp,
        project: ProjectId,
        issue: Uuid,
    ) -> (Vec<f32>, u64) {
        let stored = issues::get_issue(app.state.meta.reader(), project, issue)
            .await
            .expect("issue read")
            .expect("issue exists");
        (stored.centroid.expect("a centroid"), stored.member_count)
    }

    /// Asserts that two vectors are equal up to float rounding.
    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-5, "{actual:?} != {expected:?}");
        }
    }

    /// Marks candidate `id` as assigned elsewhere, as a join or a refit of another group does.
    async fn assigned_elsewhere(app: &TestApp, id: Uuid) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE issue_candidates SET status = 'assigned' WHERE id = ?")
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await
                        .map_err(MetaError::from)?;
                    Ok(())
                })
            })
            .await
            .expect("candidate updated");
    }

    /// The members of each issue of `source`: the title and the trace ids of its candidates.
    async fn members_by_title(app: &TestApp, source: Uuid) -> Vec<(String, Vec<u8>)> {
        sqlx::query_as(
            "SELECT i.title, c.trace_id FROM issue_candidates c \
             JOIN issues i ON i.id = c.issue_id WHERE c.source_id = ? ORDER BY i.title, c.trace_id",
        )
        .bind(source.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .expect("members read")
    }

    #[tokio::test]
    async fn a_refit_now_records_a_manual_refit_and_the_next_one_is_scheduled() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(Vec::new()).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let id = source.id;
        // "Refit now" makes the source due at once and records the request as manual.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { issues::set_next_refit(conn, id, now, "manual").await })
            })
            .await
            .expect("request stored");

        let due = issues::due_refits(app.state.meta.reader(), now)
            .await
            .expect("due read");
        let (due_source, trigger) = due
            .into_iter()
            .find(|(row, _)| row.id == id)
            .expect("the source is due");
        assert_eq!(trigger, "manual");
        let row = refit_source(&app.state, &due_source, trigger, now)
            .await
            .expect("refit runs");

        assert_eq!(row.trigger, "manual");
        let listed = issues::list_refits(app.state.meta.reader(), id, 10)
            .await
            .expect("refits read");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].trigger, "manual");
        // The refit schedules the next one, on the schedule, six hours after it ended.
        let finished = row.finished_at.expect("the refit finished");
        assert!(
            issues::due_refits(app.state.meta.reader(), finished)
                .await
                .expect("due read")
                .is_empty()
        );
        let scheduled = issues::due_refits(app.state.meta.reader(), finished + SIX_HOURS)
            .await
            .expect("due read");
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].1, "schedule");
    }

    #[tokio::test]
    async fn the_same_pending_set_gives_the_same_clusters_twice() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![
            name("Refund refused"),
            name("Login fails"),
            name("Refund refused"),
            name("Login fails"),
        ])
        .await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let first = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let second = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for source in [&first, &second] {
            for i in 0..12_u8 {
                pending(
                    &app,
                    source,
                    i + 1,
                    near(0, 0.01 * f32::from(i)),
                    now - MINUTE,
                )
                .await;
            }
            for i in 0..13_u8 {
                pending(
                    &app,
                    source,
                    i + 20,
                    near(1, 0.01 * f32::from(i)),
                    now - MINUTE,
                )
                .await;
            }
        }

        let a = refit_source(&app.state, &first, "manual", now)
            .await
            .expect("refit runs");
        let b = refit_source(&app.state, &second, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(
            (a.pending, a.clusters, a.created, a.joined),
            (b.pending, b.clusters, b.created, b.joined)
        );
        assert_eq!((a.created, a.joined), (2, 0));
        let members = members_by_title(&app, first.id).await;
        assert_eq!(members.len(), 25);
        assert_eq!(members, members_by_title(&app, second.id).await);
    }

    #[tokio::test]
    async fn a_naming_call_over_the_budget_stops_the_refit_and_pauses_the_source() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Unused")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(
            &app,
            SourceRow {
                // A priced chat model with a budget no call fits in.
                chat_model: Some("gpt-4o-mini".to_owned()),
                budget_usd: Some(0.0000001),
                budget_period: Some("day".to_owned()),
                ..llm_source(seeded.project.id, chat, now)
            },
        )
        .await;
        for i in 0..12_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        for i in 0..13_u8 {
            pending(
                &app,
                &source,
                i + 20,
                near(1, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class.as_deref(), Some("budget"));
        assert_eq!((row.clusters, row.created), (2, 0));
        assert!(mock.requests().is_empty());
        let stored = issues::get_source(app.state.meta.reader(), seeded.project.id, source.id)
            .await
            .expect("source read")
            .expect("source exists");
        assert_eq!(stored.paused_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn naming_waits_for_a_chat_slot_and_the_refit_stops_busy_when_none_comes() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(vec![name("Unused")]).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        for i in 0..12_u8 {
            pending(
                &app,
                &source,
                i + 1,
                near(0, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        for i in 0..13_u8 {
            pending(
                &app,
                &source,
                i + 20,
                near(1, 0.01 * f32::from(i)),
                now - MINUTE,
            )
            .await;
        }
        // Every slot the connection has is taken for the whole wait.
        let connection_row =
            connections::get_connection(app.state.meta.reader(), seeded.org_id, chat)
                .await
                .expect("connection read")
                .expect("connection exists");
        let limits = (
            connection_row.max_concurrency,
            connection_row.requests_per_minute,
        );
        let mut held = Vec::new();
        while let Some(slot) = app.state.eval.try_connection_permit(chat, limits, now) {
            held.push(slot);
        }
        assert!(!held.is_empty());

        let row = refit_source(&app.state, &source, "manual", now)
            .await
            .expect("refit runs");

        assert_eq!(row.error_class.as_deref(), Some("busy"));
        assert_eq!((row.clusters, row.created), (2, 0));
        assert!(mock.requests().is_empty());
        assert!(titles(&app, source.id).await.is_empty());
    }

    #[tokio::test]
    async fn a_join_moves_the_centroid_by_the_members_it_takes() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(Vec::new()).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let existing = existing_issue(
            &app,
            seeded.project.id,
            source.id,
            Some(near(0, 0.0)),
            "Refund questions",
            3,
        )
        .await;
        let first = pending(&app, &source, 1, near(0, 0.0), now - MINUTE).await;
        let second = pending(&app, &source, 2, near(0, 0.1), now - MINUTE).await;
        let left = pending(&app, &source, 3, near(0, 0.2), now - MINUTE).await;
        // `left` was assigned meanwhile, so the join takes the other two.
        assigned_elsewhere(&app, left.id).await;
        let project = projects::project_by_id(app.state.meta.reader(), seeded.project.id)
            .await
            .expect("project read")
            .expect("project");
        let scope = Scope {
            state: &app.state,
            source: &source,
            project: &project,
            now,
        };

        let joined = join(scope, &near(0, 0.1), &[&first, &second, &left])
            .await
            .expect("join runs")
            .expect("the group joins the issue");

        assert_eq!(joined.0, existing);
        assert_eq!(joined.1.len(), 2);
        let (centroid, members) = centroid_and_members(&app, seeded.project.id, existing).await;
        assert_eq!(members, 5);
        let taken = centroid_of(&[near(0, 0.0).as_slice(), near(0, 0.1).as_slice()]);
        let expected = cluster::merge_centroids(&near(0, 0.0), 3, &taken, 2).expect("merge");
        assert_close(&centroid, &expected);
    }

    #[tokio::test]
    async fn a_new_issue_takes_the_centroid_of_the_members_it_assigns() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(Vec::new()).await;
        let chat = connection(&app, &seeded, &mock).await;
        let now = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, now)).await;
        let first = pending(&app, &source, 1, near(0, 0.0), now - MINUTE).await;
        let second = pending(&app, &source, 2, near(0, 0.1), now - MINUTE).await;
        let left = pending(&app, &source, 3, near(1, 0.0), now - MINUTE).await;
        assigned_elsewhere(&app, left.id).await;
        let project = projects::project_by_id(app.state.meta.reader(), seeded.project.id)
            .await
            .expect("project read")
            .expect("project");
        let scope = Scope {
            state: &app.state,
            source: &source,
            project: &project,
            now,
        };

        let (issue, assigned) = create_issue(scope, &[&first, &second, &left], "Fresh", None)
            .await
            .expect("create runs");

        assert_eq!(assigned.len(), 2);
        let (centroid, members) = centroid_and_members(&app, seeded.project.id, issue).await;
        assert_eq!(members, 2);
        let expected = centroid_of(&[near(0, 0.0).as_slice(), near(0, 0.1).as_slice()]);
        assert_close(&centroid, &expected);
    }

    #[tokio::test]
    async fn created_and_regressed_events_carry_the_clock_of_their_write() {
        let (app, seeded) = llm_app().await;
        let mock = MockLlm::start(Vec::new()).await;
        let chat = connection(&app, &seeded, &mock).await;
        let started = app.state.clock.now_ns();
        let source = store_source(&app, llm_source(seeded.project.id, chat, started)).await;
        let first = pending(&app, &source, 1, near(0, 0.0), started - MINUTE).await;
        let second = pending(&app, &source, 2, near(0, 0.1), started - MINUTE).await;
        let project = projects::project_by_id(app.state.meta.reader(), seeded.project.id)
            .await
            .expect("project read")
            .expect("project");
        // The refit read its time at `started`; naming took an hour.
        let scope = Scope {
            state: &app.state,
            source: &source,
            project: &project,
            now: started,
        };
        let created_at = started + 60 * MINUTE;
        app.state.clock.set(created_at);

        let (issue, _) = create_issue(scope, &[&first, &second], "Fresh", None)
            .await
            .expect("create runs");

        let event_at = |kind: &'static str| {
            let pool = app.state.meta.reader().clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT at FROM issue_events WHERE kind = ?")
                    .bind(kind)
                    .fetch_all(&pool)
                    .await
                    .expect("events read")
            }
        };
        assert_eq!(event_at("created").await, [created_at]);

        // A resolved issue regresses on a join, stamped with the clock of that write.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE issues SET state = 'resolved', resolved_at = 1 WHERE id = ?",
                    )
                    .bind(issue.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                    Ok(())
                })
            })
            .await
            .expect("resolved");
        let third = pending(&app, &source, 3, near(0, 0.05), started - MINUTE).await;
        let fourth = pending(&app, &source, 4, near(0, 0.06), started - MINUTE).await;
        let regressed_at = started + 120 * MINUTE;
        app.state.clock.set(regressed_at);
        join(scope, &near(0, 0.05), &[&third, &fourth])
            .await
            .expect("join runs")
            .expect("the group joins the issue");
        assert_eq!(event_at("regressed").await, [regressed_at]);
    }

    #[test]
    fn an_occurrence_older_than_the_retention_cutoff_is_not_written() {
        // Millisecond times: the cutoff is an id time, which keeps milliseconds only (D-G-26).
        const SECOND: i64 = 1_000_000_000;
        let fresh = candidate_at(1, 2 * SECOND);
        let stale = candidate_at(2, SECOND / 2);
        let assigned: HashSet<Uuid> = [fresh.id, stale.id].into_iter().collect();
        let group = [&fresh, &stale];
        let issue = Uuid::from_u128(7);
        let source = Uuid::from_u128(8);

        let kept = occurrences_of(&group, &assigned, issue, source, 3 * SECOND, Some(SECOND));
        assert_eq!(
            kept.iter()
                .map(|occurrence| occurrence.at)
                .collect::<Vec<_>>(),
            vec![2 * SECOND]
        );
        // Without a retention, every trace of the group is written.
        let all = occurrences_of(&group, &assigned, issue, source, 3 * SECOND, None);
        assert_eq!(all.len(), 2);
    }

    /// A candidate of trace `n` that started at `start`, with no vector.
    fn candidate_at(n: u8, start: i64) -> CandidateRow {
        let mut row = queued(
            start,
            ProjectId::from_uuid(Uuid::from_u128(5)),
            Uuid::from_u128(6),
            Place::Trace {
                trace: trace(n),
                session: None,
            },
            None,
            Some(start),
            None,
        );
        row.summary = Some("Login fails on refresh.".to_owned());
        row
    }
}
