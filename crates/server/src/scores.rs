//! Score write core: validation, ids, times and append (spec 06 § Scores; D-G-26 to D-G-28).
//!
//! Every write path ends in [`write`] or [`delete`]. The score routes call them (G3.4), and
//! public feedback calls [`write`] (G3.5).

use std::collections::{BTreeSet, HashMap};

use llmobs_core::enums::ScoreSource;
use llmobs_core::ids::ProjectId;
use llmobs_core::score::{Score, ScoreTarget, ScoreValue};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::common::{FieldError, ProblemCode};
use crate::api::convert;
use crate::api::evaluators::EvaluatorKind;
use crate::api::scores::ScoreCreate;
use crate::entitlements::units::check_cap;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::ingest::IngestError;
use crate::meta::projects::ProjectRow;
use crate::meta::score_definitions::DefinitionRow;
use crate::routes::read::{READ_TIMEOUT, open_query};

/// Most scores one request writes.
const MAX_SCORES: usize = 1000;
/// Longest name, session id and categorical label, in bytes.
const TEXT_MAX: usize = 255;
const HOUR_NS: i64 = 3_600_000_000_000;
const DAY_NS: i64 = 86_400_000_000_000;
/// Prefix of the derived-id input (spec 06).
const DERIVE_TAG: &[u8] = b"llmobs.score.v1";

/// Who is writing, which decides `source`, the author and the ownership check (D-G-27, D-I-36).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Writer {
    /// A web session on the project route. `admin`: the caller's effective project role is admin.
    Human { user_id: Uuid, admin: bool },
    /// A personal access token on the project route: the token, its user, and whether that user
    /// is a project admin. Its scores are stored as `api`, like a key's (D-G-27).
    Pat {
        token_id: Uuid,
        user_id: Uuid,
        admin: bool,
    },
    /// An `sk` key (`key_id`) on the project route.
    Api { author: Uuid },
    /// An `sk` key on `POST /api/v1/scores`.
    Sdk { key_id: Uuid },
    /// A `pk` key on `POST /api/public/feedback`.
    EndUser { key_id: Uuid },
    /// An evaluator through the queue: a rule's job, or a manual run (`rule_id: None`).
    Evaluator {
        kind: EvaluatorKind,
        evaluator_id: Uuid,
        rule_id: Option<Uuid>,
    },
}

impl Writer {
    /// The stored `source` for a body `source`, or `None` when the body may not set it here.
    fn source(self, requested: Option<ScoreSource>) -> Option<ScoreSource> {
        match (self, requested) {
            (Self::Human { .. }, None | Some(ScoreSource::Human)) => Some(ScoreSource::Human),
            (Self::Api { .. } | Self::Pat { .. }, None | Some(ScoreSource::Api)) => {
                Some(ScoreSource::Api)
            }
            (Self::Api { .. } | Self::Pat { .. }, Some(ScoreSource::Sdk)) => Some(ScoreSource::Sdk),
            (Self::Sdk { .. }, None | Some(ScoreSource::Sdk)) => Some(ScoreSource::Sdk),
            (Self::EndUser { .. }, None | Some(ScoreSource::EndUser)) => Some(ScoreSource::EndUser),
            (Self::Evaluator { kind, .. }, None) => Some(evaluator_source(kind)),
            (Self::Evaluator { kind, .. }, Some(requested))
                if requested == evaluator_source(kind) =>
            {
                Some(requested)
            }
            _ => None,
        }
    }

    /// The id a derived `score_id` uses as its author (D-G-27).
    fn author(self) -> Uuid {
        match self {
            Self::Human { user_id, .. } => user_id,
            Self::Pat { token_id, .. } => token_id,
            Self::Api { author } => author,
            Self::Sdk { key_id } | Self::EndUser { key_id } => key_id,
            Self::Evaluator {
                evaluator_id,
                rule_id,
                ..
            } => rule_id.unwrap_or(evaluator_id),
        }
    }

    /// `author_user_id` of the stored row. Only a person has one.
    fn author_user(self) -> Option<Uuid> {
        match self {
            Self::Human { user_id, .. } => Some(user_id),
            Self::Pat { .. }
            | Self::Api { .. }
            | Self::Sdk { .. }
            | Self::EndUser { .. }
            | Self::Evaluator { .. } => None,
        }
    }

    /// The person behind a session or PAT write, and whether they are a project admin. `None`
    /// for keys, feedback and evaluators, which the ownership check leaves alone (D-I-26).
    fn person(self) -> Option<(Uuid, bool)> {
        match self {
            Self::Human { user_id, admin } | Self::Pat { user_id, admin, .. } => {
                Some((user_id, admin))
            }
            Self::Api { .. } | Self::Sdk { .. } | Self::EndUser { .. } | Self::Evaluator { .. } => {
                None
            }
        }
    }

    /// `evaluator_id` and `rule_id` of the stored rows. Only an evaluator has them.
    fn evaluator_ids(self) -> (Option<Uuid>, Option<Uuid>) {
        match self {
            Self::Evaluator {
                evaluator_id,
                rule_id,
                ..
            } => (Some(evaluator_id), rule_id),
            Self::Human { .. }
            | Self::Pat { .. }
            | Self::Api { .. }
            | Self::Sdk { .. }
            | Self::EndUser { .. } => (None, None),
        }
    }
}

/// The `source` an evaluator's scores are stored with (spec 06 § Scores).
fn evaluator_source(kind: EvaluatorKind) -> ScoreSource {
    match kind {
        EvaluatorKind::LlmJudge => ScoreSource::LlmJudge,
        EvaluatorKind::Check => ScoreSource::Check,
        EvaluatorKind::Webhook => ScoreSource::Webhook,
    }
}

/// `403 forbidden` when a person who is not a project admin would replace or delete another
/// user's `human` score (D-I-26, D-I-36). Keys, feedback and evaluators always pass.
fn check_owner(writer: Writer, live: &Score) -> Result<(), ApiError> {
    let Some((user_id, admin)) = writer.person() else {
        return Ok(());
    };
    if admin || live.source != ScoreSource::Human || live.author_user_id == Some(user_id) {
        return Ok(());
    }
    Err(ApiError::Forbidden {
        code: ProblemCode::Forbidden,
    })
}

/// Validates `items`, resolves ids and times, appends the rows, and returns one id per item
/// in input order (spec 06 § Score write rules).
pub(crate) async fn write(
    state: &AppState,
    project: &ProjectRow,
    writer: Writer,
    items: Vec<ScoreCreate>,
) -> Result<Vec<Uuid>, ApiError> {
    check_cap(state, project.org_id)
        .await
        .map_err(ApiError::from)?;
    let now = state.clock.now_ns();
    let definitions = state.definitions.live(&state.meta, project.id, now).await?;
    let mut errors = Vec::new();
    if !(1..=MAX_SCORES).contains(&items.len()) {
        errors.push(FieldError {
            field: "scores".to_owned(),
            message: "between 1 and 1000 scores per request".to_owned(),
            position: None,
        });
    }
    let checked: Vec<Option<ScoreSource>> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            check_item(index, item, writer, project, now, &definitions, &mut errors)
        })
        .collect();
    // Experiment ids must name an experiment of the project. The window does not apply: a late
    // flush of a hidden experiment's scores is accepted (D-J-14).
    // An experiment deleted between this check and the write leaves its scores naming no
    // experiment. Results ignore them, and no later check is made (D-J-12).
    let experiment_ids: BTreeSet<Uuid> =
        items.iter().filter_map(|item| item.experiment_id).collect();
    let mut experiment_created: HashMap<Uuid, i64> = HashMap::new();
    if !experiment_ids.is_empty() {
        let ids: Vec<Uuid> = experiment_ids.into_iter().collect();
        experiment_created =
            crate::meta::experiments::existing_ids(state.meta.reader(), project.id, &ids).await?;
        for (index, item) in items.iter().enumerate() {
            if item
                .experiment_id
                .is_some_and(|id| !experiment_created.contains_key(&id))
            {
                errors.push(field(index, "experiment_id", "unknown experiment"));
            }
        }
    }
    if !errors.is_empty() {
        return Err(invalid(errors));
    }
    // With no errors every item has a source, so nothing is dropped here.
    let sources: Vec<ScoreSource> = checked.into_iter().flatten().collect();

    let author = writer.author();
    let pending: Vec<Pending> = items
        .into_iter()
        .zip(sources)
        .map(|(item, source)| {
            // An experiment score's author is its experiment (spec 06, D-J-12).
            let id = item.score_id.unwrap_or_else(|| {
                let author = item.experiment_id.unwrap_or(author);
                derive_id(project.id, &item.target, &item.name, source, author)
            });
            Pending { id, source, item }
        })
        .collect();

    // Answer: one id per input item, in input order.
    let ids: Vec<Uuid> = pending.iter().map(|pending| pending.id).collect();
    // Duplicates keep the last item (D-G-28).
    let last: HashMap<Uuid, usize> = pending
        .iter()
        .enumerate()
        .map(|(index, pending)| (pending.id, index))
        .collect();
    let kept: Vec<Pending> = pending
        .into_iter()
        .enumerate()
        .filter_map(|(index, pending)| (last.get(&pending.id) == Some(&index)).then_some(pending))
        .collect();

    // A person's write also looks up the UUIDv7 ids it supplies, for the ownership check
    // (D-I-36). Other writers need only the non-v7 ids, for the times.
    let person = writer.person().is_some();
    let lookup: Vec<String> = kept
        .iter()
        .filter(|pending| person || !is_v7(pending.id))
        .map(|pending| pending.id.to_string())
        .collect();
    let live = live_scores(state, project, &lookup).await?;
    for pending in &kept {
        if let Some(found) = live.get(&pending.id) {
            check_owner(writer, found)?;
        }
    }

    let author_user = writer.author_user();
    let (evaluator_id, rule_id) = writer.evaluator_ids();
    let rows: Vec<Score> = kept
        .into_iter()
        .map(|pending| {
            let Pending { id, source, item } = pending;
            let (derived, updated_at) = times(id, now, &live);
            let ScoreCreate {
                target,
                session_id,
                user_id,
                name,
                value,
                reason,
                metadata,
                experiment_id,
                dataset_item_id,
                ..
            } = item;
            // An experiment score is at least as old as its experiment (D-J-13): a client's
            // clock may stamp a v7 id before the experiment was created.
            let created_at = experiment_id
                .and_then(|experiment| experiment_created.get(&experiment).copied())
                .map_or(derived, |started| derived.max(started));
            let session_id = match target {
                ScoreTarget::Session { .. } => None,
                ScoreTarget::Span { .. } | ScoreTarget::Trace { .. } => session_id,
            };
            Score {
                project_id: project.id,
                score_id: id,
                target,
                session_id,
                user_id,
                name,
                value,
                reason,
                source,
                evaluator_id,
                rule_id,
                experiment_id,
                dataset_item_id,
                author_user_id: author_user,
                metadata,
                created_at,
                updated_at,
                deleted: false,
                ingest_time: now,
            }
        })
        .collect();

    append(state, project.id, &rows).await?;
    state.usage.add(
        project.org_id,
        project.id,
        now,
        0,
        0,
        u64::try_from(rows.len()).unwrap_or(u64::MAX),
    );
    Ok(ids)
}

/// Appends a deleted copy of the live score `score_id` (spec 03 §5). `NotFound` when none;
/// `Forbidden` when `writer` may not delete it (D-I-36).
pub(crate) async fn delete(
    state: &AppState,
    project: &ProjectRow,
    writer: Writer,
    score_id: Uuid,
) -> Result<(), ApiError> {
    let now = state.clock.now_ns();
    let mut live = live_scores(state, project, &[score_id.to_string()]).await?;
    let Some(mut tombstone) = live.remove(&score_id) else {
        return Err(ApiError::NotFound);
    };
    check_owner(writer, &tombstone)?;
    tombstone.deleted = true;
    tombstone.updated_at = now.max(tombstone.updated_at.saturating_add(1));
    tombstone.ingest_time = now;
    append(state, project.id, std::slice::from_ref(&tombstone)).await
}

/// An item that passed the checks, with its final id.
struct Pending {
    id: Uuid,
    source: ScoreSource,
    item: ScoreCreate,
}

fn invalid(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: Some("invalid scores".to_owned()),
        errors,
    }
}

fn field(index: usize, name: &str, message: &str) -> FieldError {
    FieldError {
        field: format!("scores[{index}].{name}"),
        message: message.to_owned(),
        position: None,
    }
}

fn text_ok(text: &str) -> bool {
    (1..=TEXT_MAX).contains(&text.len())
}

/// Pushes every field error of one item. Returns its stored source when it has none.
fn check_item(
    index: usize,
    item: &ScoreCreate,
    writer: Writer,
    project: &ProjectRow,
    now: i64,
    definitions: &[DefinitionRow],
    errors: &mut Vec<FieldError>,
) -> Option<ScoreSource> {
    let before = errors.len();
    let source = writer.source(item.source);
    if let Some(requested) = item.source.filter(|_| source.is_none()) {
        errors.push(field(
            index,
            "source",
            &format!("source {} is not allowed here", requested.as_str()),
        ));
    }
    if !text_ok(&item.name) {
        errors.push(field(index, "name", "name must be 1 to 255 bytes"));
    }
    if let ScoreTarget::Session { session_id } = &item.target
        && !text_ok(session_id)
    {
        errors.push(FieldError {
            field: format!("scores[{index}].target.session_id"),
            message: "session_id must be 1 to 255 bytes".to_owned(),
            position: None,
        });
    }
    match (item.experiment_id, item.dataset_item_id) {
        (Some(_), Some(_)) => {
            if !matches!(item.target, ScoreTarget::Trace { .. }) {
                errors.push(FieldError {
                    field: format!("scores[{index}].target"),
                    message: "an experiment score needs a trace target".to_owned(),
                    position: None,
                });
            }
        }
        (None, None) => {}
        _ => errors.push(field(
            index,
            "experiment_id",
            "experiment_id and dataset_item_id go together",
        )),
    }
    if let ScoreValue::Categorical { label, .. } = &item.value
        && !text_ok(label)
    {
        errors.push(field(index, "value", "label must be 1 to 255 bytes"));
    }
    if let Some(id) = item.score_id.filter(|id| is_v7(*id)) {
        match v7_created_at(id) {
            None => errors.push(field(index, "score_id", "score_id time is in the future")),
            Some(created) if created > now.saturating_add(HOUR_NS) => {
                errors.push(field(index, "score_id", "score_id time is in the future"));
            }
            Some(created) => {
                if let Some(days) = project.retention_days {
                    let cutoff = now.saturating_sub(i64::from(days).saturating_mul(DAY_NS));
                    if created < cutoff {
                        errors.push(field(
                            index,
                            "score_id",
                            "score_id time is older than the project's retention",
                        ));
                    }
                }
            }
        }
    }
    check_definition(index, item, definitions, errors);
    source.filter(|_| errors.len() == before)
}

/// Checks the item against the live definition of its name, if there is one (spec 06,
/// D-I-26): the type, then the numeric range or the categorical label.
fn check_definition(
    index: usize,
    item: &ScoreCreate,
    definitions: &[DefinitionRow],
    errors: &mut Vec<FieldError>,
) {
    let Some(definition) = definitions
        .iter()
        .find(|definition| definition.name == item.name)
    else {
        return;
    };
    if item.value.data_type() != definition.data_type {
        errors.push(field(
            index,
            "value",
            &format!(
                "value must be {} for this score definition",
                definition.data_type.as_str()
            ),
        ));
        return;
    }
    match &item.value {
        ScoreValue::Numeric(value) => {
            let below = definition.min_value.is_some_and(|min| *value < min);
            let above = definition.max_value.is_some_and(|max| *value > max);
            if below || above {
                errors.push(field(
                    index,
                    "value",
                    "value is outside the score definition's range",
                ));
            }
        }
        ScoreValue::Categorical { label, .. } => {
            if !definition
                .categories
                .iter()
                .any(|category| category.label == *label)
            {
                errors.push(field(
                    index,
                    "value",
                    "label is not a category of this score definition",
                ));
            }
        }
        ScoreValue::Boolean(_) | ScoreValue::Text(_) => {}
    }
}

fn is_v7(id: Uuid) -> bool {
    id.get_version() == Some(uuid::Version::SortRand)
}

/// `created_at` of a UUIDv7: its Unix time in nanoseconds. `None` when the time does not fit.
fn v7_created_at(id: Uuid) -> Option<i64> {
    let (secs, nanos) = id.get_timestamp()?.to_unix();
    i64::try_from(secs)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(i64::from(nanos))
}

/// `(created_at, updated_at)` of a row with final id `id` (spec 06 § Times).
///
/// A UUIDv7 gives `created_at` from its time and needs no lookup. Any other id takes the
/// live version's `created_at`, or `now` for a new score.
fn times(id: Uuid, now: i64, live: &HashMap<Uuid, Score>) -> (i64, i64) {
    if let Some(created) = v7_created_at(id).filter(|_| is_v7(id)) {
        return (created, now.max(created));
    }
    match live.get(&id) {
        Some(found) => (
            found.created_at,
            now.max(found.updated_at.saturating_add(1)),
        ),
        None => (now, now),
    }
}

/// The derived `score_id` of an item without one (spec 06, D-G-28): UUIDv8 over SHA-256.
///
/// The seven fields are length-prefixed, so no two different field lists share an input.
fn derive_id(
    project: ProjectId,
    target: &ScoreTarget,
    name: &str,
    source: ScoreSource,
    author: Uuid,
) -> Uuid {
    let (trace, locator) = match target {
        ScoreTarget::Span { trace_id, span_id } => (trace_id.to_string(), span_id.to_string()),
        ScoreTarget::Trace { trace_id } => (trace_id.to_string(), String::new()),
        ScoreTarget::Session { session_id } => (String::new(), session_id.clone()),
    };
    let fields = [
        project.to_string(),
        target.kind().as_str().to_owned(),
        trace,
        locator,
        name.to_owned(),
        source.as_str().to_owned(),
        author.to_string(),
    ];
    let mut hasher = Sha256::new();
    hasher.update(DERIVE_TAG);
    for field in &fields {
        // Names and ids are at most 255 bytes here, so the length always fits in `u32`.
        let length = u32::try_from(field.len()).unwrap_or(u32::MAX);
        hasher.update(length.to_be_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    // SHA-256 has 32 bytes, so the first 16 always exist.
    let bytes: [u8; 16] = std::array::from_fn(|index| digest[index]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// The `score_id` that [`write`] derives for a rule's evaluator score on `target` without a
/// given id (D-I-18): the rule is the author and the evaluator kind gives the source. Backfill
/// looks these ids up to skip targets the rule already scored (D-I-40).
pub(crate) fn rule_score_id(
    project: ProjectId,
    target: &ScoreTarget,
    name: &str,
    kind: EvaluatorKind,
    rule: Uuid,
) -> Uuid {
    derive_id(project, target, name, evaluator_source(kind), rule)
}

/// The live versions of `ids` (UUID strings), keyed by score id. No ids means no lookup.
async fn live_scores(
    state: &AppState,
    project: &ProjectRow,
    ids: &[String],
) -> Result<HashMap<Uuid, Score>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ctx = open_query(state, project.org_id, project.id, None).await?;
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let batches = llmobs_storage::scores_by_id(&ctx, &refs, READ_TIMEOUT)
        .await
        .map_err(|error| {
            tracing::warn!(kind = error.kind(), "score lookup failed");
            ApiError::Unavailable {
                detail: "score lookup is unavailable, retry later",
            }
        })?;
    let scores = convert::scores(&batches, project.id)?;
    Ok(scores
        .into_iter()
        .map(|score| (score.score_id, score))
        .collect())
}

/// Appends `rows` as one batch (spec 06 § Score write rules, step 7).
async fn append(state: &AppState, project: ProjectId, rows: &[Score]) -> Result<(), ApiError> {
    let batch = llmobs_core::batch::scores_to_record_batch(rows)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    state
        .storage
        .append_scores(project, batch)
        .await
        .map_err(|error| ApiError::from(IngestError::Append(error)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use llmobs_core::ids::OrgId;

    use super::*;
    use crate::meta::datasets::{DatasetFields, insert_dataset};
    use crate::meta::experiments::{NewExperiment, insert_tx};
    use crate::meta::score_definitions::{DefinitionCategory, DefinitionFields, insert_definition};
    use crate::test_support::{Seeded, TestApp, set_limits};
    use llmobs_core::enums::ScoreDataType;
    use llmobs_storage::ScoreListRequest;

    /// The fixed project of the derived-id tests.
    const PROJECT: &str = "018f6b4e-7c1a-7c1a-8c1a-000000000001";
    const TRACE: &str = "a1000000000000000000000000000001";
    const SPAN: &str = "b100000000000012";
    const AUTHOR: &str = "01900000-0000-7000-8000-000000000402";
    /// Computed from the spec's byte layout with SHA-256 outside this code, then pinned.
    const DERIVED: &str = "119b8f11-8f63-8b55-a76d-e39b7f513b86";
    /// UUIDv7 with Unix time 1_717_986_918_400 ms (2024-06-10).
    const V7_FIXED: &str = "01900000-0000-7000-8000-000000000401";
    const V7_FIXED_MS: i64 = 1_717_986_918_400;

    fn span_target() -> ScoreTarget {
        ScoreTarget::Span {
            trace_id: TRACE.parse().unwrap(),
            span_id: SPAN.parse().unwrap(),
        }
    }

    fn author() -> Uuid {
        AUTHOR.parse().unwrap()
    }

    fn api_writer() -> Writer {
        Writer::Api { author: author() }
    }

    fn item(name: &str, value: f64) -> ScoreCreate {
        ScoreCreate {
            score_id: None,
            target: span_target(),
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value: ScoreValue::Numeric(value),
            reason: None,
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: None,
            dataset_item_id: None,
        }
    }

    /// A UUIDv4 with the given first byte.
    fn v4(byte: u8) -> Uuid {
        uuid::Builder::from_random_bytes([byte; 16]).into_uuid()
    }

    /// A UUIDv7 whose time is `ms` milliseconds after the Unix epoch.
    fn v7_at(ms: u64) -> Uuid {
        uuid::Builder::from_unix_timestamp_millis(ms, &[0; 10]).into_uuid()
    }

    /// Every stored live score of `project`, read through storage.
    async fn stored(app: &TestApp, org: OrgId, project: ProjectId) -> Vec<Score> {
        let ctx = open_query(&app.state, org, project, None).await.unwrap();
        let page = llmobs_storage::score_list(
            &ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: None,
                source: None,
                evaluator_id: None,
                rule_id: None,
                window: None,
                limit: 100,
                cursor: None,
                timeout: READ_TIMEOUT,
            },
        )
        .await
        .unwrap();
        convert::scores(&page.batches, project).unwrap()
    }

    /// The `(field, message)` pairs of a refused write.
    fn refused(result: Result<Vec<Uuid>, ApiError>) -> Vec<(String, String)> {
        match result {
            Err(ApiError::BadRequest { errors, .. }) => errors
                .into_iter()
                .map(|error| (error.field, error.message))
                .collect(),
            other => panic!("expected a bad request, got {other:?}"),
        }
    }

    fn project_id() -> ProjectId {
        PROJECT.parse().unwrap()
    }

    #[test]
    fn derive_id_matches_the_pinned_value() {
        let id = derive_id(
            project_id(),
            &span_target(),
            "accuracy",
            ScoreSource::Sdk,
            author(),
        );
        assert_eq!(id.to_string(), DERIVED);
        assert_eq!(id.get_version_num(), 8);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn derive_id_changes_with_each_of_the_seven_fields() {
        let base = derive_id(
            project_id(),
            &span_target(),
            "accuracy",
            ScoreSource::Sdk,
            author(),
        );
        let other_project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap();
        let trace = ScoreTarget::Trace {
            trace_id: TRACE.parse().unwrap(),
        };
        let other_trace = ScoreTarget::Span {
            trace_id: "a1000000000000000000000000000002".parse().unwrap(),
            span_id: SPAN.parse().unwrap(),
        };
        let other_span = ScoreTarget::Span {
            trace_id: TRACE.parse().unwrap(),
            span_id: "b100000000000013".parse().unwrap(),
        };
        let session = ScoreTarget::Session {
            session_id: "chat-42".to_owned(),
        };
        let other_author: Uuid = "01900000-0000-7000-8000-000000000403".parse().unwrap();
        let changed = [
            derive_id(
                other_project,
                &span_target(),
                "accuracy",
                ScoreSource::Sdk,
                author(),
            ),
            derive_id(project_id(), &trace, "accuracy", ScoreSource::Sdk, author()),
            derive_id(
                project_id(),
                &other_trace,
                "accuracy",
                ScoreSource::Sdk,
                author(),
            ),
            derive_id(
                project_id(),
                &other_span,
                "accuracy",
                ScoreSource::Sdk,
                author(),
            ),
            derive_id(
                project_id(),
                &session,
                "accuracy",
                ScoreSource::Sdk,
                author(),
            ),
            derive_id(
                project_id(),
                &span_target(),
                "relevance",
                ScoreSource::Sdk,
                author(),
            ),
            derive_id(
                project_id(),
                &span_target(),
                "accuracy",
                ScoreSource::Api,
                author(),
            ),
            derive_id(
                project_id(),
                &span_target(),
                "accuracy",
                ScoreSource::Sdk,
                other_author,
            ),
        ];
        for other in changed {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn v7_time_is_the_created_at_of_the_id() {
        let id: Uuid = V7_FIXED.parse().unwrap();
        assert_eq!(v7_created_at(id), Some(V7_FIXED_MS * 1_000_000));
        assert!(is_v7(id));
        assert!(!is_v7(v4(1)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v7_id_far_in_the_future_is_refused_and_one_hour_is_accepted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let now_ms = u64::try_from(now / 1_000_000).unwrap();
        let hour_ms = 3_600_000;

        let accepted = v7_at(now_ms + hour_ms);
        let written = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                score_id: Some(accepted),
                ..item("accuracy", 0.5)
            }],
        )
        .await
        .unwrap();
        assert_eq!(written, vec![accepted]);
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows[0].created_at, now + 3_600_000_000_000);

        let refused_id = v7_at(now_ms + hour_ms + 1);
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                api_writer(),
                vec![ScoreCreate {
                    score_id: Some(refused_id),
                    ..item("accuracy", 0.5)
                }],
            )
            .await,
        );
        assert_eq!(
            errors,
            vec![(
                "scores[0].score_id".to_owned(),
                "score_id time is in the future".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v7_time_becomes_created_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let id: Uuid = V7_FIXED.parse().unwrap();
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.5)
            }],
        )
        .await
        .unwrap();
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].score_id, id);
        assert_eq!(rows[0].created_at, V7_FIXED_MS * 1_000_000);
        assert_eq!(rows[0].updated_at, now);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_score_takes_now_for_both_times() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let id = v4(1);
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.8)
            }],
        )
        .await
        .unwrap();
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.score_id, id);
        assert_eq!(row.created_at, now);
        assert_eq!(row.updated_at, now);
        assert_eq!(row.source, ScoreSource::Api);
        assert_eq!(row.author_user_id, None);
        assert_eq!(row.value, ScoreValue::Numeric(0.8));
        assert_eq!(row.target, span_target());
        assert!(!row.deleted);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_keeps_created_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let id = v4(2);
        let write_value = |value: f64| {
            write(
                &app.state,
                &seeded.project,
                api_writer(),
                vec![ScoreCreate {
                    score_id: Some(id),
                    ..item("accuracy", value)
                }],
            )
        };
        write_value(0.8).await.unwrap();
        app.state.clock.advance(std::time::Duration::from_secs(60));
        write_value(0.2).await.unwrap();

        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, ScoreValue::Numeric(0.2));
        assert_eq!(rows[0].created_at, now);
        assert_eq!(rows[0].updated_at, now + 60_000_000_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_hides_the_score_and_a_second_delete_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = v4(3);
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.8)
            }],
        )
        .await
        .unwrap();

        delete(&app.state, &seeded.project, api_writer(), id)
            .await
            .unwrap();
        assert!(
            stored(&app, seeded.org_id, seeded.project.id)
                .await
                .is_empty()
        );
        match delete(&app.state, &seeded.project, api_writer(), id).await {
            Err(ApiError::NotFound) => {}
            other => panic!("expected not found, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sources_follow_the_caller() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let user = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0500);
        let human = Writer::Human {
            user_id: user,
            admin: false,
        };
        let sdk = Writer::Sdk { key_id: author() };

        // A person may not send `source: api`.
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                human,
                vec![ScoreCreate {
                    source: Some(ScoreSource::Api),
                    ..item("accuracy", 0.5)
                }],
            )
            .await,
        );
        assert_eq!(
            errors,
            vec![(
                "scores[0].source".to_owned(),
                "source api is not allowed here".to_owned()
            )]
        );
        // An `sk` key may not send `source: human`.
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                sdk,
                vec![ScoreCreate {
                    source: Some(ScoreSource::Human),
                    ..item("accuracy", 0.5)
                }],
            )
            .await,
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "scores[0].source");
        // A PAT or key on the project route may send `sdk`.
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                source: Some(ScoreSource::Sdk),
                ..item("relevance", 0.5)
            }],
        )
        .await
        .unwrap();
        // A person's score stores the person as its author.
        write(&app.state, &seeded.project, human, vec![item("tone", 1.0)])
            .await
            .unwrap();
        // A PAT's score is stored as `api`, with no author, like a key's (D-G-27).
        let pat = Writer::Pat {
            token_id: author(),
            user_id: user,
            admin: false,
        };
        write(&app.state, &seeded.project, pat, vec![item("clarity", 0.5)])
            .await
            .unwrap();

        // Feedback is stored as `end_user`; any other source is refused.
        let end_user = Writer::EndUser { key_id: author() };
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                end_user,
                vec![ScoreCreate {
                    source: Some(ScoreSource::Sdk),
                    ..item("helpful", 1.0)
                }],
            )
            .await,
        );
        assert_eq!(errors.len(), 1);
        write(
            &app.state,
            &seeded.project,
            end_user,
            vec![item("helpful", 1.0)],
        )
        .await
        .unwrap();

        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        let relevance = rows.iter().find(|row| row.name == "relevance").unwrap();
        assert_eq!(relevance.source, ScoreSource::Sdk);
        assert_eq!(relevance.author_user_id, None);
        let tone = rows.iter().find(|row| row.name == "tone").unwrap();
        assert_eq!(tone.source, ScoreSource::Human);
        assert_eq!(tone.author_user_id, Some(user));
        let clarity = rows.iter().find(|row| row.name == "clarity").unwrap();
        assert_eq!(clarity.source, ScoreSource::Api);
        assert_eq!(clarity.author_user_id, None);
        let helpful = rows.iter().find(|row| row.name == "helpful").unwrap();
        assert_eq!(helpful.source, ScoreSource::EndUser);
        assert_eq!(helpful.author_user_id, None);
    }

    /// A UUIDv7 score id with an explicit `score_id`.
    fn with_id(id: Uuid, name: &str, value: f64) -> ScoreCreate {
        ScoreCreate {
            score_id: Some(id),
            ..item(name, value)
        }
    }

    fn user_a() -> Uuid {
        Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0600)
    }

    fn user_b() -> Uuid {
        Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0601)
    }

    /// Asserts that a write or delete answered `403 forbidden`.
    fn assert_forbidden<T: std::fmt::Debug>(result: &Result<T, ApiError>) {
        assert!(
            matches!(result, Err(ApiError::Forbidden { .. })),
            "expected forbidden, got {result:?}"
        );
    }

    /// The live row with `id`, if there is one.
    async fn live_row(app: &TestApp, seeded: &Seeded, id: Uuid) -> Option<Score> {
        stored(app, seeded.org_id, seeded.project.id)
            .await
            .into_iter()
            .find(|row| row.score_id == id)
    }

    /// Writes `A`'s human score `id` (value 1.0, `tone`) through a session.
    async fn human_score_of_a(app: &TestApp, seeded: &Seeded, id: Uuid) {
        let writer = Writer::Human {
            user_id: user_a(),
            admin: false,
        };
        write(
            &app.state,
            &seeded.project,
            writer,
            vec![with_id(id, "tone", 1.0)],
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_refuses_another_users_human_score_to_a_person() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id: Uuid = V7_FIXED.parse().unwrap();
        let (a, b) = (user_a(), user_b());
        human_score_of_a(&app, &seeded, id).await;

        let by_b = Writer::Human {
            user_id: b,
            admin: false,
        };
        assert_forbidden(
            &write(
                &app.state,
                &seeded.project,
                by_b,
                vec![with_id(id, "tone", 0.0)],
            )
            .await,
        );
        let row = live_row(&app, &seeded, id).await.unwrap();
        assert_eq!(row.value, ScoreValue::Numeric(1.0));
        assert_eq!(row.author_user_id, Some(a));

        // A PAT of B is checked as B.
        let pat_of_b = Writer::Pat {
            token_id: author(),
            user_id: b,
            admin: false,
        };
        assert_forbidden(
            &write(
                &app.state,
                &seeded.project,
                pat_of_b,
                vec![with_id(id, "tone", 0.0)],
            )
            .await,
        );

        app.state.clock.advance(std::time::Duration::from_secs(60));
        let by_a = Writer::Human {
            user_id: a,
            admin: false,
        };
        write(
            &app.state,
            &seeded.project,
            by_a,
            vec![with_id(id, "tone", 0.5)],
        )
        .await
        .unwrap();
        assert_eq!(
            live_row(&app, &seeded, id).await.unwrap().value,
            ScoreValue::Numeric(0.5)
        );

        // A project admin replaces it and becomes its author.
        app.state.clock.advance(std::time::Duration::from_secs(60));
        let admin_b = Writer::Human {
            user_id: b,
            admin: true,
        };
        write(
            &app.state,
            &seeded.project,
            admin_b,
            vec![with_id(id, "tone", 0.0)],
        )
        .await
        .unwrap();
        let row = live_row(&app, &seeded, id).await.unwrap();
        assert_eq!(row.value, ScoreValue::Numeric(0.0));
        assert_eq!(row.author_user_id, Some(b));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_batch_stores_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id: Uuid = V7_FIXED.parse().unwrap();
        let new_id = v7_at(1_717_986_918_401);
        human_score_of_a(&app, &seeded, id).await;

        let by_b = Writer::Human {
            user_id: user_b(),
            admin: false,
        };
        assert_forbidden(
            &write(
                &app.state,
                &seeded.project,
                by_b,
                vec![with_id(new_id, "tone", 0.3), with_id(id, "tone", 0.0)],
            )
            .await,
        );
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].score_id, id);
        assert_eq!(rows[0].value, ScoreValue::Numeric(1.0));
        assert_eq!(rows[0].author_user_id, Some(user_a()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_checks_ownership_after_validation() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id: Uuid = V7_FIXED.parse().unwrap();
        human_score_of_a(&app, &seeded, id).await;

        let by_b = Writer::Human {
            user_id: user_b(),
            admin: false,
        };
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                by_b,
                vec![with_id(id, "tone", 0.0), item("", 1.0)],
            )
            .await,
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "scores[1].name");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_and_evaluators_skip_the_ownership_check() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id: Uuid = V7_FIXED.parse().unwrap();
        human_score_of_a(&app, &seeded, id).await;

        app.state.clock.advance(std::time::Duration::from_secs(60));
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![with_id(id, "tone", 0.25)],
        )
        .await
        .unwrap();
        let row = live_row(&app, &seeded, id).await.unwrap();
        assert_eq!(row.source, ScoreSource::Api);

        app.state.clock.advance(std::time::Duration::from_secs(60));
        let by_a = Writer::Human {
            user_id: user_a(),
            admin: false,
        };
        write(
            &app.state,
            &seeded.project,
            by_a,
            vec![with_id(id, "tone", 1.0)],
        )
        .await
        .unwrap();
        assert_eq!(
            live_row(&app, &seeded, id).await.unwrap().source,
            ScoreSource::Human
        );

        app.state.clock.advance(std::time::Duration::from_secs(60));
        let evaluator = Writer::Evaluator {
            kind: EvaluatorKind::Check,
            evaluator_id: author(),
            rule_id: None,
        };
        write(
            &app.state,
            &seeded.project,
            evaluator,
            vec![with_id(id, "tone", 0.5)],
        )
        .await
        .unwrap();
        assert_eq!(
            live_row(&app, &seeded, id).await.unwrap().source,
            ScoreSource::Check
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_person_may_replace_another_users_non_human_score() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = v4(7);
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![with_id(id, "tone", 1.0)],
        )
        .await
        .unwrap();

        app.state.clock.advance(std::time::Duration::from_secs(60));
        let by_b = Writer::Human {
            user_id: user_b(),
            admin: false,
        };
        write(
            &app.state,
            &seeded.project,
            by_b,
            vec![with_id(id, "tone", 0.0)],
        )
        .await
        .unwrap();
        let row = live_row(&app, &seeded, id).await.unwrap();
        assert_eq!(row.source, ScoreSource::Human);
        assert_eq!(row.author_user_id, Some(user_b()));
        assert_eq!(row.value, ScoreValue::Numeric(0.0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_person_may_delete_a_non_human_score() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let by_b = Writer::Human {
            user_id: user_b(),
            admin: false,
        };

        let api_id = v4(8);
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![with_id(api_id, "tone", 1.0)],
        )
        .await
        .unwrap();
        app.state.clock.advance(std::time::Duration::from_secs(60));
        delete(&app.state, &seeded.project, by_b, api_id)
            .await
            .unwrap();
        assert!(live_row(&app, &seeded, api_id).await.is_none());

        let evaluator_id = v4(9);
        let evaluator = Writer::Evaluator {
            kind: EvaluatorKind::Check,
            evaluator_id: author(),
            rule_id: None,
        };
        write(
            &app.state,
            &seeded.project,
            evaluator,
            vec![with_id(evaluator_id, "tone", 1.0)],
        )
        .await
        .unwrap();
        app.state.clock.advance(std::time::Duration::from_secs(60));
        delete(&app.state, &seeded.project, by_b, evaluator_id)
            .await
            .unwrap();
        assert!(live_row(&app, &seeded, evaluator_id).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_refuses_another_users_human_score_to_a_person() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id: Uuid = V7_FIXED.parse().unwrap();
        let (a, b) = (user_a(), user_b());
        human_score_of_a(&app, &seeded, id).await;

        let by_b = Writer::Human {
            user_id: b,
            admin: false,
        };
        assert_forbidden(&delete(&app.state, &seeded.project, by_b, id).await);
        assert!(live_row(&app, &seeded, id).await.is_some());

        let pat_of_b = Writer::Pat {
            token_id: v4(9),
            user_id: b,
            admin: false,
        };
        assert_forbidden(&delete(&app.state, &seeded.project, pat_of_b, id).await);
        assert!(live_row(&app, &seeded, id).await.is_some());

        app.state.clock.advance(std::time::Duration::from_secs(60));
        delete(&app.state, &seeded.project, api_writer(), id)
            .await
            .unwrap();
        assert!(live_row(&app, &seeded, id).await.is_none());

        app.state.clock.advance(std::time::Duration::from_secs(60));
        human_score_of_a(&app, &seeded, id).await;
        assert!(live_row(&app, &seeded, id).await.is_some());
        app.state.clock.advance(std::time::Duration::from_secs(60));
        let admin_b = Writer::Human {
            user_id: b,
            admin: true,
        };
        delete(&app.state, &seeded.project, admin_b, id)
            .await
            .unwrap();
        assert!(live_row(&app, &seeded, id).await.is_none());

        app.state.clock.advance(std::time::Duration::from_secs(60));
        human_score_of_a(&app, &seeded, id).await;
        app.state.clock.advance(std::time::Duration::from_secs(60));
        let by_a = Writer::Human {
            user_id: a,
            admin: false,
        };
        delete(&app.state, &seeded.project, by_a, id).await.unwrap();
        assert!(live_row(&app, &seeded, id).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluator_writer_sets_source_ids_and_author() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator: Uuid = "01900000-0000-7000-8000-000000000500".parse().unwrap();
        let rule: Uuid = "01900000-0000-7000-8000-000000000700".parse().unwrap();
        let project = seeded.project.id;

        let through_rule = Writer::Evaluator {
            kind: EvaluatorKind::Check,
            evaluator_id: evaluator,
            rule_id: Some(rule),
        };
        write(
            &app.state,
            &seeded.project,
            through_rule,
            vec![item("same", 1.0)],
        )
        .await
        .unwrap();
        // A manual run has no rule: its author is the evaluator.
        let manual = Writer::Evaluator {
            kind: EvaluatorKind::Check,
            evaluator_id: evaluator,
            rule_id: None,
        };
        write(&app.state, &seeded.project, manual, vec![item("same", 0.0)])
            .await
            .unwrap();

        let rows = stored(&app, seeded.org_id, project).await;
        assert_eq!(rows.len(), 2);
        let by_rule = rows.iter().find(|row| row.rule_id == Some(rule)).unwrap();
        assert_eq!(by_rule.source, ScoreSource::Check);
        assert_eq!(by_rule.evaluator_id, Some(evaluator));
        assert_eq!(by_rule.author_user_id, None);
        assert_eq!(
            by_rule.score_id,
            derive_id(project, &span_target(), "same", ScoreSource::Check, rule)
        );
        let by_evaluator = rows.iter().find(|row| row.rule_id.is_none()).unwrap();
        assert_eq!(by_evaluator.evaluator_id, Some(evaluator));
        assert_eq!(
            by_evaluator.score_id,
            derive_id(
                project,
                &span_target(),
                "same",
                ScoreSource::Check,
                evaluator
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluator_writer_refuses_another_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let writer = Writer::Evaluator {
            kind: EvaluatorKind::LlmJudge,
            evaluator_id: author(),
            rule_id: None,
        };
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                writer,
                vec![ScoreCreate {
                    source: Some(ScoreSource::Human),
                    ..item("same", 1.0)
                }],
            )
            .await,
        );
        assert_eq!(
            errors,
            vec![(
                "scores[0].source".to_owned(),
                "source human is not allowed here".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rule_score_id_matches_the_written_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator: Uuid = "01900000-0000-7000-8000-000000000500".parse().unwrap();
        let rule: Uuid = "01900000-0000-7000-8000-000000000700".parse().unwrap();
        let project = seeded.project.id;
        let writer = Writer::Evaluator {
            kind: EvaluatorKind::LlmJudge,
            evaluator_id: evaluator,
            rule_id: Some(rule),
        };
        let trace = ScoreTarget::Trace {
            trace_id: TRACE.parse().unwrap(),
        };
        let items = vec![
            ScoreCreate {
                target: trace.clone(),
                ..item("same", 1.0)
            },
            item("same", 0.0),
        ];
        let ids = write(&app.state, &seeded.project, writer, items)
            .await
            .unwrap();
        assert_eq!(
            ids[0],
            rule_score_id(project, &trace, "same", EvaluatorKind::LlmJudge, rule)
        );
        assert_eq!(
            ids[1],
            rule_score_id(
                project,
                &span_target(),
                "same",
                EvaluatorKind::LlmJudge,
                rule
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluator_scores_count_in_usage() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let writer = Writer::Evaluator {
            kind: EvaluatorKind::Webhook,
            evaluator_id: author(),
            rule_id: None,
        };
        write(&app.state, &seeded.project, writer, vec![item("same", 1.0)])
            .await
            .unwrap();
        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        let scores: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(scores), 0) FROM usage_daily WHERE project_id = ?",
        )
        .bind(seeded.project.id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("usage reads");
        assert_eq!(scores, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_items_report_every_field_and_write_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let long_name = "n".repeat(256);
        let items = vec![
            item("", 0.5),
            item(&long_name, 0.5),
            ScoreCreate {
                target: ScoreTarget::Session {
                    session_id: String::new(),
                },
                ..item("accuracy", 0.5)
            },
            ScoreCreate {
                value: ScoreValue::Categorical {
                    label: String::new(),
                    value: None,
                },
                ..item("accuracy", 0.5)
            },
        ];
        let errors = refused(write(&app.state, &seeded.project, api_writer(), items).await);
        assert_eq!(
            errors,
            vec![
                (
                    "scores[0].name".to_owned(),
                    "name must be 1 to 255 bytes".to_owned()
                ),
                (
                    "scores[1].name".to_owned(),
                    "name must be 1 to 255 bytes".to_owned()
                ),
                (
                    "scores[2].target.session_id".to_owned(),
                    "session_id must be 1 to 255 bytes".to_owned()
                ),
                (
                    "scores[3].value".to_owned(),
                    "label must be 1 to 255 bytes".to_owned()
                ),
            ]
        );
        assert!(
            stored(&app, seeded.org_id, seeded.project.id)
                .await
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_size_is_between_one_and_a_thousand() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let expected = vec![(
            "scores".to_owned(),
            "between 1 and 1000 scores per request".to_owned(),
        )];
        let empty = refused(write(&app.state, &seeded.project, api_writer(), Vec::new()).await);
        assert_eq!(empty, expected);
        let too_many: Vec<ScoreCreate> = (0..=MAX_SCORES).map(|_| item("accuracy", 0.5)).collect();
        let too_many = refused(write(&app.state, &seeded.project, api_writer(), too_many).await);
        assert_eq!(too_many, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_ids_keep_the_last_item_and_answer_each_position() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = v4(4);
        let items = vec![
            ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.1)
            },
            ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.9)
            },
        ];
        let ids = write(&app.state, &seeded.project, api_writer(), items)
            .await
            .unwrap();
        assert_eq!(ids, vec![id, id]);
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, ScoreValue::Numeric(0.9));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn derived_id_is_overwritten_by_the_same_writer_and_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let first = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("accuracy", 0.1)],
        )
        .await
        .unwrap();
        let second = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("accuracy", 0.9)],
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0].get_version_num(), 8);

        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, ScoreValue::Numeric(0.9));
        assert_eq!(rows[0].created_at, now);
        assert_eq!(rows[0].updated_at, now + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v7_time_must_be_inside_the_project_retention() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (short, _) = app.add_project(&seeded, "short", Some(1)).await;
        let now = app.state.clock.now_ns();
        let cutoff = now - DAY_NS;
        // The last whole millisecond before the cutoff, and the first one at or after it.
        let outside = v7_at(u64::try_from(cutoff / 1_000_000 - 1).unwrap());
        let inside = v7_at(u64::try_from((cutoff + 999_999) / 1_000_000).unwrap());
        let with_id = |id: Uuid| ScoreCreate {
            score_id: Some(id),
            ..item("accuracy", 0.5)
        };

        let errors = refused(write(&app.state, &short, api_writer(), vec![with_id(outside)]).await);
        assert_eq!(
            errors,
            vec![(
                "scores[0].score_id".to_owned(),
                "score_id time is older than the project's retention".to_owned()
            )]
        );
        let written = write(&app.state, &short, api_writer(), vec![with_id(inside)])
            .await
            .unwrap();
        assert_eq!(written, vec![inside]);
        // A project without retention keeps the old id.
        let written = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![with_id(outside)],
        )
        .await
        .unwrap();
        assert_eq!(written, vec![outside]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn v7_time_beyond_the_timestamp_range_is_in_the_future() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let largest = v7_at(u64::MAX >> 16);
        let errors = refused(
            write(
                &app.state,
                &seeded.project,
                api_writer(),
                vec![ScoreCreate {
                    score_id: Some(largest),
                    ..item("accuracy", 0.5)
                }],
            )
            .await,
        );
        assert_eq!(
            errors,
            vec![(
                "scores[0].score_id".to_owned(),
                "score_id time is in the future".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_thousand_scores_are_accepted_in_one_request() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let items: Vec<ScoreCreate> = (0..MAX_SCORES)
            .map(|index| item(&format!("score-{index}"), 0.5))
            .collect();
        let ids = write(&app.state, &seeded.project, api_writer(), items)
            .await
            .unwrap();
        assert_eq!(ids.len(), MAX_SCORES);
        let distinct: BTreeSet<Uuid> = ids.into_iter().collect();
        assert_eq!(distinct.len(), MAX_SCORES);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sdk_writer_without_a_body_source_stores_sdk() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = v4(5);
        write(
            &app.state,
            &seeded.project,
            Writer::Sdk { key_id: author() },
            vec![ScoreCreate {
                score_id: Some(id),
                ..item("accuracy", 0.5)
            }],
        )
        .await
        .unwrap();
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        let row = rows.iter().find(|row| row.score_id == id).unwrap();
        assert_eq!(row.source, ScoreSource::Sdk);
        assert_eq!(row.author_user_id, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_target_drops_the_body_session_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let on_session = v4(6);
        let on_span = v4(7);
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![
                ScoreCreate {
                    score_id: Some(on_session),
                    target: ScoreTarget::Session {
                        session_id: "chat-42".to_owned(),
                    },
                    session_id: Some("chat-42".to_owned()),
                    ..item("session-score", 0.5)
                },
                ScoreCreate {
                    score_id: Some(on_span),
                    session_id: Some("chat-42".to_owned()),
                    ..item("span-score", 0.5)
                },
            ],
        )
        .await
        .unwrap();
        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        let session = rows.iter().find(|row| row.score_id == on_session).unwrap();
        assert_eq!(session.session_id, None);
        let span = rows.iter().find(|row| row.score_id == on_span).unwrap();
        assert_eq!(span.session_id.as_deref(), Some("chat-42"));
    }

    /// Inserts a definition straight into `meta.db`, without the route's cache drop.
    async fn define(
        app: &TestApp,
        project: ProjectId,
        fields: DefinitionFields,
        data_type: ScoreDataType,
    ) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_definition(conn, Uuid::now_v7(), project, data_type, fields, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
    }

    fn numeric_definition(name: &str, archived_at: Option<i64>) -> DefinitionFields {
        DefinitionFields {
            name: name.to_owned(),
            categories: Vec::new(),
            min_value: Some(0.0),
            max_value: Some(1.0),
            description: None,
            archived_at,
        }
    }

    fn tone_definition() -> DefinitionFields {
        DefinitionFields {
            name: "tone".to_owned(),
            categories: ["good", "bad"]
                .iter()
                .map(|label| DefinitionCategory {
                    label: (*label).to_owned(),
                    value: None,
                })
                .collect(),
            min_value: None,
            max_value: None,
            description: None,
            archived_at: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scores_must_fit_their_live_definition() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        define(
            &app,
            seeded.project.id,
            numeric_definition("relevance", None),
            ScoreDataType::Numeric,
        )
        .await;
        define(
            &app,
            seeded.project.id,
            tone_definition(),
            ScoreDataType::Categorical,
        )
        .await;
        let refused_items = vec![
            item("relevance", 1.5),
            ScoreCreate {
                value: ScoreValue::Boolean(true),
                ..item("relevance", 0.5)
            },
            ScoreCreate {
                value: ScoreValue::Categorical {
                    label: "meh".to_owned(),
                    value: None,
                },
                ..item("tone", 0.5)
            },
        ];
        let errors = refused(write(&app.state, &seeded.project, api_writer(), refused_items).await);
        assert_eq!(
            errors,
            vec![
                (
                    "scores[0].value".to_owned(),
                    "value is outside the score definition's range".to_owned()
                ),
                (
                    "scores[1].value".to_owned(),
                    "value must be numeric for this score definition".to_owned()
                ),
                (
                    "scores[2].value".to_owned(),
                    "label is not a category of this score definition".to_owned()
                ),
            ]
        );
        assert!(
            stored(&app, seeded.org_id, seeded.project.id)
                .await
                .is_empty()
        );

        let fitting = vec![
            item("relevance", 0.0),
            ScoreCreate {
                value: ScoreValue::Categorical {
                    label: "good".to_owned(),
                    value: None,
                },
                ..item("tone", 0.5)
            },
        ];
        let ids = write(&app.state, &seeded.project, api_writer(), fitting)
            .await
            .unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archived_definitions_are_not_checked() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        define(
            &app,
            seeded.project.id,
            numeric_definition("relevance", Some(now)),
            ScoreDataType::Numeric,
        )
        .await;
        let ids = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("relevance", 5.0)],
        )
        .await
        .unwrap();
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn definitions_are_cached_until_a_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definitions = format!("/api/v1/projects/{}/score-definitions", seeded.project.id);
        // The first write loads the (empty) definitions into the cache.
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("relevance", 5.0)],
        )
        .await
        .unwrap();

        let created = app
            .request(axum::http::Method::POST, &definitions)
            .header(axum::http::header::COOKIE.as_str(), &seeded.cookie)
            .json(&serde_json::json!({
                "name": "relevance",
                "data_type": "numeric",
                "min_value": 0.0,
                "max_value": 1.0
            }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let id = created.json::<serde_json::Value>()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // The create dropped the cache, so the next write sees the new definition.
        let refused_write = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("relevance", 5.0)],
        )
        .await;
        assert_eq!(refused(refused_write).len(), 1);

        let archived = app
            .request(axum::http::Method::DELETE, &format!("{definitions}/{id}"))
            .header(axum::http::header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(archived.status, 204);
        // The archive dropped the cache again: the same write succeeds at once.
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![item("relevance", 5.0)],
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capped_org_cannot_write_scores() {
        use crate::api::scores::ScoreBatch;
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let scores_path = format!("/api/v1/projects/{}/scores", seeded.project.id);
        let score = |id: &str| ScoreCreate {
            score_id: Some(id.parse().unwrap()),
            target: span_target(),
            session_id: None,
            user_id: None,
            name: "tone".to_owned(),
            value: ScoreValue::Numeric(1.0),
            reason: None,
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: None,
            dataset_item_id: None,
        };
        let id = "01900000-0000-7000-8000-000000000402";
        let before_cap = app
            .request(axum::http::Method::POST, &scores_path)
            .header("cookie", &seeded.cookie)
            .json(&ScoreBatch {
                scores: vec![score(id)],
            })
            .send()
            .await;
        assert_eq!(before_cap.status, axum::http::StatusCode::OK);

        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_cap": 10}),
        );
        app.state.usage.add(
            seeded.org_id,
            seeded.project.id,
            app.state.clock.now_ns(),
            10,
            0,
            0,
        );

        let refused = app
            .request(axum::http::Method::POST, &scores_path)
            .header("cookie", &seeded.cookie)
            .json(&ScoreBatch {
                scores: vec![score("01900000-0000-7000-8000-000000000403")],
            })
            .send()
            .await;
        assert_eq!(refused.status, axum::http::StatusCode::PAYMENT_REQUIRED);
        assert!(refused.header("retry-after").is_some());
        assert_eq!(refused.problem().code, ProblemCode::QuotaExceeded);

        let feedback = app
            .request(axum::http::Method::POST, "/api/public/feedback")
            .header("authorization", &format!("Bearer {}", seeded.public_key))
            .header("content-type", "application/json")
            .body(include_bytes!("../../../fixtures/api/requests/feedback.json").to_vec())
            .send()
            .await;
        assert_eq!(feedback.status, axum::http::StatusCode::PAYMENT_REQUIRED);
        assert_eq!(feedback.problem().code, ProblemCode::QuotaExceeded);

        let deleted = app
            .request(axum::http::Method::DELETE, &format!("{scores_path}/{id}"))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, axum::http::StatusCode::NO_CONTENT);
    }

    /// An experiment of `project`, in a dataset of its own, created at `created_at`.
    async fn experiment(app: &TestApp, project: ProjectId, created_at: i64) -> Uuid {
        let dataset = Uuid::now_v7();
        let experiment = Uuid::now_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_dataset(
                        conn,
                        dataset,
                        project,
                        DatasetFields {
                            name: format!("faq-{dataset}"),
                            description: None,
                            input_schema: None,
                            expected_schema: None,
                        },
                        created_at,
                    )
                    .await?;
                    insert_tx(
                        conn,
                        &NewExperiment {
                            id: experiment,
                            project_id: project,
                            dataset_id: dataset,
                            dataset_name: format!("faq-{dataset}"),
                            dataset_version: 1,
                            name: "baseline".to_owned(),
                            description: None,
                            metadata: "{}".to_owned(),
                            repetitions: 1,
                            item_count: 1,
                            created_by: None,
                            created_at,
                        },
                    )
                    .await
                })
            })
            .await
            .unwrap();
        experiment
    }

    fn trace_target() -> ScoreTarget {
        ScoreTarget::Trace {
            trace_id: TRACE.parse().unwrap(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn experiment_fields_are_stored() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let experiment = experiment(&app, seeded.project.id, app.state.clock.now_ns()).await;
        let dataset_item = Uuid::now_v7();
        write(
            &app.state,
            &seeded.project,
            Writer::Sdk { key_id: author() },
            vec![ScoreCreate {
                target: trace_target(),
                experiment_id: Some(experiment),
                dataset_item_id: Some(dataset_item),
                ..item("accuracy", 0.5)
            }],
        )
        .await
        .unwrap();

        let rows = stored(&app, seeded.org_id, seeded.project.id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].experiment_id, Some(experiment));
        assert_eq!(rows[0].dataset_item_id, Some(dataset_item));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn experiment_fields_go_together() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let experiment = experiment(&app, seeded.project.id, app.state.clock.now_ns()).await;
        let result = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                target: trace_target(),
                experiment_id: Some(experiment),
                ..item("accuracy", 0.5)
            }],
        )
        .await;
        assert_eq!(
            refused(result),
            vec![(
                "scores[0].experiment_id".to_owned(),
                "experiment_id and dataset_item_id go together".to_owned(),
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn experiment_scores_need_a_trace_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let experiment = experiment(&app, seeded.project.id, app.state.clock.now_ns()).await;
        let result = write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                experiment_id: Some(experiment),
                dataset_item_id: Some(Uuid::now_v7()),
                ..item("accuracy", 0.5)
            }],
        )
        .await;
        assert_eq!(
            refused(result),
            vec![(
                "scores[0].target".to_owned(),
                "an experiment score needs a trace target".to_owned(),
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_experiment_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let foreign = experiment(&app, other.id, app.state.clock.now_ns()).await;
        for experiment_id in [Uuid::now_v7(), foreign] {
            let result = write(
                &app.state,
                &seeded.project,
                api_writer(),
                vec![ScoreCreate {
                    target: trace_target(),
                    experiment_id: Some(experiment_id),
                    dataset_item_id: Some(Uuid::now_v7()),
                    ..item("accuracy", 0.5)
                }],
            )
            .await;
            assert_eq!(
                refused(result),
                vec![(
                    "scores[0].experiment_id".to_owned(),
                    "unknown experiment".to_owned(),
                )]
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn derived_id_uses_the_experiment() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let first = experiment(&app, seeded.project.id, now).await;
        let second = experiment(&app, seeded.project.id, now).await;
        let dataset_item = Uuid::now_v7();
        let scored = |experiment: Uuid| ScoreCreate {
            target: trace_target(),
            experiment_id: Some(experiment),
            dataset_item_id: Some(dataset_item),
            ..item("accuracy", 0.5)
        };
        let write_one = |experiment: Uuid| {
            write(
                &app.state,
                &seeded.project,
                api_writer(),
                vec![scored(experiment)],
            )
        };

        let a = write_one(first).await.unwrap();
        let b = write_one(second).await.unwrap();
        let again = write_one(first).await.unwrap();
        assert_eq!(
            a[0],
            derive_id(
                seeded.project.id,
                &trace_target(),
                "accuracy",
                ScoreSource::Api,
                first,
            )
        );
        assert_ne!(a[0], b[0]);
        assert_eq!(a, again);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_experiment_scores_are_accepted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        let experiment = experiment(&app, seeded.project.id, app.state.clock.now_ns()).await;
        app.state
            .clock
            .advance(std::time::Duration::from_secs(31 * 24 * 3600));
        write(
            &app.state,
            &seeded.project,
            api_writer(),
            vec![ScoreCreate {
                target: trace_target(),
                experiment_id: Some(experiment),
                dataset_item_id: Some(Uuid::now_v7()),
                ..item("accuracy", 0.5)
            }],
        )
        .await
        .unwrap();
    }
}
