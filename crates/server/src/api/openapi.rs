//! The OpenAPI document.
//!
//! Spec 09. `just openapi` writes `crates/server/openapi.json`.

use std::collections::{BTreeMap, BTreeSet};

use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::score::Score;
use utoipa::openapi::path::{
    HttpMethod, OperationBuilder, Parameter, ParameterBuilder, ParameterIn, PathItem, Paths,
    PathsBuilder,
};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::response::{Response, ResponseBuilder, Responses, ResponsesBuilder};
use utoipa::openapi::schema::{
    AdditionalProperties, AllOfBuilder, ArrayItems, Components, ComponentsBuilder, KnownFormat,
    ObjectBuilder, Schema, SchemaFormat, SchemaType, Type,
};
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme,
};
use utoipa::openapi::tag::{Tag, TagBuilder};
use utoipa::openapi::{Content, InfoBuilder, OpenApi, OpenApiBuilder, Ref, RefOr, Required};
use utoipa::{IntoParams, PartialSchema, ToSchema};

use super::account::{
    AuthConfig, ChangePasswordRequest, CreateOrgRequest, CreateProjectRequest,
    ForgotPasswordRequest, LoginRequest, Me, OrgMembership, Project, ResendVerificationRequest,
    ResetPasswordRequest, SetupRequest, SetupResponse, SignupRequest, SignupResponse,
    UpdateMeRequest, UpdateOrgRequest, UpdateProjectRequest, User, VerifyEmailRequest,
};
use super::admin::{
    AdminLicense, AdminResetLink, AdminSettings, AdminUser, CreateAdminUserRequest,
    CreatedAdminUser, UpdateAdminUserRequest,
};
use super::alerts::{
    AlertCondition, AlertDelivery, AlertEvent, AlertEventKind, AlertMetric, AlertRule, AlertState,
    AlertWindow, ChannelDelivery, ChannelKind, ChannelTestResult, Comparator,
    CreateAlertRuleRequest, CreateChannelRequest, DeliveryStatus, LatencyTarget,
    NotificationChannel, ScoreAggregation, UpdateAlertRuleRequest, UpdateChannelRequest,
};
use super::backfills::{Backfill, BackfillEstimate, BackfillStatus, CreateBackfillRequest};
use super::common::{ExclusionParams, Limit, Page, PageParams, Problem, SortOrder, TimeParam};
use super::connections::{
    ConnectionStatus, CreateLlmConnectionRequest, LlmAdapter, LlmConnection,
    LlmConnectionTestError, LlmConnectionTestResult, StructuredOutputMode, TestErrorClass,
    TestLlmConnectionRequest, UpdateLlmConnectionRequest,
};
use super::datasets::{
    AddItemsRequest, AddItemsResponse, CreateDatasetRequest, Dataset, DatasetItem,
    DatasetItemListParams, DatasetItemParams, DatasetListParams, DatasetVersion,
    DatasetVersionListParams, ExportFormat, ExportParams, FromTraceRequest, ImportColumn,
    ImportFormat, ImportMapping, ImportParams, ImportPreview, ImportResult, ImportRowError,
    NewDatasetItem, PreviewParams, UpdateDatasetRequest, UpdateItemRequest,
};
use super::deletions::{
    CreateDeletion, Deletion, DeletionPreview, DeletionPreviewRequest, DropParams,
};
use super::entitlements::{
    Feature, OrgEntitlements, OrgLimits, OrgMonthUsage, OrgPlan, PlanSource, PlanStatus,
    PutOrgTokenRequest, UsageNotice,
};
use super::eval_jobs::{
    EvalJob, EvalJobKind, EvalJobListParams, EvalJobStatus, RunEvaluatorRequest,
};
use super::evaluators::{
    CreateEvaluatorRequest, Evaluator, EvaluatorKind, EvaluatorStatsParams, EvaluatorStatsResponse,
    EvaluatorTemplate, EvaluatorTemplateList, EvaluatorTestError, EvaluatorTestRequest,
    EvaluatorTestResult, EvaluatorTestUsage, EvaluatorVersion, OutputScoreBody, TemplateMapping,
    TestEvaluatorBody, UpdateEvaluatorRequest,
};
use super::experiments::{
    CompareCell, CompareOnly, CompareParams, CompareRow, CreateExperimentRequest,
    CreateExperimentResponse, Experiment, ExperimentComparison, ExperimentListParams,
    ExperimentResult, ExperimentResults, ExperimentResultsRequest, ExperimentResultsResponse,
    ExperimentStatus, ExperimentSummaries, ExperimentSummary, FinishExperimentRequest, ResultRow,
    ResultScore, ResultsParams, ScoreDelta, ScoreDeltas, ScoreSummary, SummariesParams,
};
use super::internal::{
    CreateInternalOrgRequest, InternalDrop, InternalDropKind, InternalDropsQuery, InternalMember,
    InternalMembers, InternalMembersQuery, InternalOrg, InternalOwner, InternalUsageQuery,
    InternalUsageRow, InternalUserDeleted, InternalUserRef, MembershipLookup,
    MembershipLookupRequest, MembershipOrg,
};
use super::invites::{CreateInviteRequest, CreatedInvite, Invite, InvitePreview};
use super::keys::{
    ApiKey as ProjectApiKey, CreateApiKeyRequest, CreatePersonalTokenRequest, CreatedApiKey,
    CreatedPersonalToken, PersonalToken,
};
use super::members::{OrgMember, ProjectMember, SetProjectOverrideRequest, UpdateOrgMemberRequest};
use super::metrics::{MetricsBucket, MetricsGroupBy, MetricsParams, MetricsResponse};
use super::model_prices::{
    ModelPrice, ModelPriceInput, ModelPriceListParams, ModelPriceResolution,
    ModelPriceResolveParams, ModelPriceScope, ModelPriceTier,
};
use super::online_rules::{
    BudgetPeriod, CreateOnlineRuleRequest, JobCounts, OnlineRule, OnlineRulePage,
    OnlineRulePreview, OnlineRulePreviewRequest, OnlineRuleStatsResponse, PauseReason, RuleSpend,
    RuleTarget, UpdateOnlineRuleRequest,
};
use super::org_sso::{
    AddOrgSsoDomain, DomainStatus, JitRole, OrgSso, OrgSsoDomain, OrgSsoProvider, PatchOrgSso,
    PutOrgSsoProvider, SsoDiscoverRequest, SsoDiscovery,
};
use super::saved_views::{CreateSavedView, SavedView, SavedViewListParams, UpdateSavedView};
use super::schema::SchemaResponse;
use super::score_definitions::{
    CreateScoreDefinitionRequest, ScoreDefinition, ScoreDefinitionCategory,
    ScoreDefinitionListParams, ScoreDirection, UpdateScoreDefinitionRequest,
};
use super::scores::{
    FeedbackRequest, FeedbackResult, FeedbackValue, ScoreBatch, ScoreCreate, ScoreListParams,
    ScoreWriteResult,
};
use super::sessions::{SessionDetail, SessionListParams, SessionSort, SessionSummary};
use super::sql::{SqlColumnInfo, SqlQueryRequest, SqlQueryResult};
use super::traces::{
    SpanDetail, SpanListParams, SpanSort, SpanSummary, TraceDetail, TraceDetailParams,
    TraceListParams, TraceSort, TraceStreamParams, TraceSummary,
};
use super::usage::{OrgUsage, UsageDay, UsageParams};
use super::users::{
    EndUserDetail, EndUserSummary, UserDetailParams, UserListParams, UserScore, UserSort,
};

const PROJECT: &str = "/api/v1/projects/{project}";
const COOKIE_NAME: &str = concat!(cetra_core::cetra_name!(), "_session");

const AUTH_ERRORS: &[u16] = &[401, 403, 404, 429];
const QUERY_ERRORS: &[u16] = &[400, 401, 403, 404, 429];
/// Read operations with a handler (D-G-23): they also return `422` and `503`.
const READ_ERRORS: &[u16] = &[400, 401, 403, 404, 422, 429, 503];

/// Schemas collected for the document.
///
/// `clash` is set when two types produce the same component name with different schemas.
/// OpenAPI would keep only one of them.
pub(crate) struct SchemaSet {
    /// Component name to schema. Names are unique.
    pub schemas: BTreeMap<String, RefOr<Schema>>,
    /// The component name that two types disagreed on.
    pub clash: Option<String>,
}

/// Collects every component schema the document registers.
///
/// The same name may be reached from several types. A second insert is kept when the schema
/// matches, and a concrete schema replaces a `$ref` placeholder. A differing schema sets [`SchemaSet::clash`].
pub(crate) fn collect_schemas() -> SchemaSet {
    let mut set = SchemaSet {
        schemas: BTreeMap::new(),
        clash: None,
    };
    set.insert::<Problem>();
    set.insert::<Limit>();
    set.insert::<TimeParam>();
    set.insert::<Deletion>();
    set.insert::<CreateDeletion>();
    set.insert::<DeletionPreviewRequest>();
    set.insert::<DeletionPreview>();
    set.insert_page::<Deletion>("DeletionPage");
    set.insert::<SortOrder>();
    set.insert::<TraceSort>();
    set.insert::<SpanSort>();
    set.insert::<SessionSort>();
    set.insert::<UserSort>();
    set.insert::<ScoreTargetKind>();
    set.insert::<ScoreSource>();
    set.insert::<MetricsBucket>();
    set.insert::<MetricsGroupBy>();
    set.insert::<AuthConfig>();
    set.insert::<SetupRequest>();
    set.insert::<SetupResponse>();
    set.insert::<LoginRequest>();
    set.insert::<SignupRequest>();
    set.insert::<SignupResponse>();
    set.insert::<Invite>();
    set.insert::<CreateInviteRequest>();
    set.insert::<CreatedInvite>();
    set.insert::<InvitePreview>();
    set.insert_page::<Invite>("InvitePage");
    set.insert::<ResetPasswordRequest>();
    set.insert::<ForgotPasswordRequest>();
    set.insert::<VerifyEmailRequest>();
    set.insert::<ResendVerificationRequest>();
    set.insert::<UpdateMeRequest>();
    set.insert::<ChangePasswordRequest>();
    set.insert::<Me>();
    set.insert::<User>();
    set.insert::<AdminUser>();
    set.insert_page::<AdminUser>("AdminUserPage");
    set.insert::<CreateAdminUserRequest>();
    set.insert::<CreatedAdminUser>();
    set.insert::<UpdateAdminUserRequest>();
    set.insert::<AdminResetLink>();
    set.insert::<AdminSettings>();
    set.insert::<AdminLicense>();
    set.insert::<OrgEntitlements>();
    set.insert::<OrgMonthUsage>();
    set.insert::<UsageNotice>();
    set.insert::<OrgUsage>();
    set.insert::<UsageDay>();
    set.insert::<JitRole>();
    set.insert::<OrgSsoProvider>();
    set.insert::<DomainStatus>();
    set.insert::<OrgSsoDomain>();
    set.insert::<OrgSso>();
    set.insert::<PutOrgSsoProvider>();
    set.insert::<AddOrgSsoDomain>();
    set.insert::<PatchOrgSso>();
    set.insert::<SsoDiscoverRequest>();
    set.insert::<SsoDiscovery>();
    set.insert::<OrgPlan>();
    set.insert::<PlanSource>();
    set.insert::<PlanStatus>();
    set.insert::<OrgLimits>();
    set.insert::<PutOrgTokenRequest>();
    set.insert::<CreateInternalOrgRequest>();
    set.insert::<InternalOwner>();
    set.insert::<InternalOrg>();
    set.insert::<InternalUserRef>();
    set.insert::<InternalUserDeleted>();
    set.insert::<MembershipLookupRequest>();
    set.insert::<MembershipOrg>();
    set.insert::<MembershipLookup>();
    set.insert::<InternalMember>();
    set.insert::<InternalMembers>();
    set.insert_page::<InternalUsageRow>("InternalUsagePage");
    set.insert::<InternalDropKind>();
    set.insert_page::<InternalDrop>("InternalDropPage");
    set.insert::<Feature>();
    set.insert::<OrgMembership>();
    set.insert::<CreateOrgRequest>();
    set.insert::<UpdateOrgRequest>();
    set.insert::<CreateProjectRequest>();
    set.insert::<UpdateProjectRequest>();
    set.insert::<OrgMember>();
    set.insert::<UpdateOrgMemberRequest>();
    set.insert::<ProjectMember>();
    set.insert::<SetProjectOverrideRequest>();
    set.insert::<Project>();
    set.insert::<ProjectApiKey>();
    set.insert::<CreatedApiKey>();
    set.insert::<CreateApiKeyRequest>();
    set.insert::<PersonalToken>();
    set.insert::<CreatedPersonalToken>();
    set.insert::<CreatePersonalTokenRequest>();
    set.insert::<LlmAdapter>();
    set.insert::<StructuredOutputMode>();
    set.insert::<ConnectionStatus>();
    set.insert::<TestErrorClass>();
    set.insert::<LlmConnection>();
    set.insert::<CreateLlmConnectionRequest>();
    set.insert::<UpdateLlmConnectionRequest>();
    set.insert::<TestLlmConnectionRequest>();
    set.insert::<LlmConnectionTestError>();
    set.insert::<LlmConnectionTestResult>();
    set.insert_page::<LlmConnection>("LlmConnectionPage");
    set.insert::<TraceSummary>();
    set.insert::<SpanSummary>();
    set.insert::<TraceDetail>();
    set.insert::<SpanDetail>();
    set.insert::<SessionSummary>();
    set.insert::<SessionDetail>();
    set.insert::<EndUserSummary>();
    set.insert::<EndUserDetail>();
    set.insert::<UserScore>();
    set.insert::<Score>();
    set.insert::<ScoreCreate>();
    set.insert::<ScoreBatch>();
    set.insert::<ScoreWriteResult>();
    set.insert::<FeedbackRequest>();
    set.insert::<FeedbackValue>();
    set.insert::<FeedbackResult>();
    set.insert::<SchemaResponse>();
    set.insert::<SavedView>();
    set.insert::<CreateSavedView>();
    set.insert::<UpdateSavedView>();
    set.insert::<MetricsResponse>();
    set.insert::<SqlQueryRequest>();
    set.insert::<SqlQueryResult>();
    set.insert::<SqlColumnInfo>();
    set.insert_page::<TraceSummary>("TracePage");
    set.insert_page::<SpanSummary>("SpanPage");
    set.insert_page::<SessionSummary>("SessionPage");
    set.insert_page::<EndUserSummary>("EndUserPage");
    set.insert_page::<Score>("ScorePage");
    set.insert_page::<OrgMembership>("OrgPage");
    set.insert_page::<OrgMember>("OrgMemberPage");
    set.insert_page::<ProjectMember>("ProjectMemberPage");
    set.insert_page::<Project>("ProjectPage");
    set.insert_page::<ProjectApiKey>("ApiKeyPage");
    set.insert_page::<PersonalToken>("PersonalTokenPage");
    set.insert_page::<SavedView>("SavedViewPage");
    set.insert::<EvaluatorKind>();
    set.insert::<OutputScoreBody>();
    set.insert::<Evaluator>();
    set.insert::<CreateEvaluatorRequest>();
    set.insert::<UpdateEvaluatorRequest>();
    set.insert::<TemplateMapping>();
    set.insert::<EvaluatorTemplate>();
    set.insert::<EvaluatorTemplateList>();
    set.insert::<TestEvaluatorBody>();
    set.insert::<EvaluatorTestRequest>();
    set.insert::<EvaluatorTestError>();
    set.insert::<EvaluatorTestUsage>();
    set.insert::<EvaluatorTestResult>();
    set.insert::<EvaluatorStatsResponse>();
    set.insert_page::<Evaluator>("EvaluatorPage");
    set.insert_page::<EvaluatorVersion>("EvaluatorVersionPage");
    set.insert::<ScoreDefinitionCategory>();
    set.insert::<ScoreDirection>();
    set.insert::<ScoreDefinition>();
    set.insert::<CreateScoreDefinitionRequest>();
    set.insert::<UpdateScoreDefinitionRequest>();
    set.insert_page::<ScoreDefinition>("ScoreDefinitionPage");
    set.insert::<Dataset>();
    set.insert::<CreateDatasetRequest>();
    set.insert::<UpdateDatasetRequest>();
    set.insert_page::<Dataset>("DatasetPage");
    set.insert::<DatasetVersion>();
    set.insert::<DatasetItem>();
    set.insert::<NewDatasetItem>();
    set.insert::<AddItemsRequest>();
    set.insert::<AddItemsResponse>();
    set.insert::<UpdateItemRequest>();
    set.insert::<FromTraceRequest>();
    set.insert_page::<DatasetVersion>("DatasetVersionPage");
    set.insert_page::<DatasetItem>("DatasetItemPage");
    set.insert::<ImportFormat>();
    set.insert::<ExportFormat>();
    set.insert::<ImportColumn>();
    set.insert::<ImportMapping>();
    set.insert::<ImportRowError>();
    set.insert::<ImportPreview>();
    set.insert::<ImportResult>();
    set.insert::<Experiment>();
    set.insert::<ExperimentStatus>();
    set.insert::<CreateExperimentRequest>();
    set.insert::<CreateExperimentResponse>();
    set.insert::<ExperimentResult>();
    set.insert::<ExperimentResultsRequest>();
    set.insert::<ExperimentResultsResponse>();
    set.insert::<FinishExperimentRequest>();
    set.insert_page::<Experiment>("ExperimentPage");
    set.insert::<ExperimentSummary>();
    set.insert::<ScoreSummary>();
    set.insert::<ResultScore>();
    set.insert::<ResultRow>();
    set.insert::<ExperimentResults>();
    set.insert::<CompareCell>();
    set.insert::<CompareRow>();
    set.insert::<ScoreDelta>();
    set.insert::<ScoreDeltas>();
    set.insert::<ExperimentComparison>();
    set.insert::<CompareOnly>();
    set.insert::<ExperimentSummaries>();
    set.insert::<ModelPriceScope>();
    set.insert::<ModelPriceTier>();
    set.insert::<ModelPrice>();
    set.insert::<ModelPriceInput>();
    set.insert::<ModelPriceResolution>();
    set.insert_page::<ModelPrice>("ModelPricePage");
    set.insert::<ChannelKind>();
    set.insert::<DeliveryStatus>();
    set.insert::<ChannelDelivery>();
    set.insert::<NotificationChannel>();
    set.insert::<ChannelTestResult>();
    set.insert::<CreateChannelRequest>();
    set.insert::<UpdateChannelRequest>();
    set.insert_page::<NotificationChannel>("ChannelPage");
    set.insert::<AlertMetric>();
    set.insert::<AlertWindow>();
    set.insert::<Comparator>();
    set.insert::<LatencyTarget>();
    set.insert::<ScoreAggregation>();
    set.insert::<AlertState>();
    set.insert::<AlertCondition>();
    set.insert::<AlertRule>();
    set.insert::<CreateAlertRuleRequest>();
    set.insert::<UpdateAlertRuleRequest>();
    set.insert::<AlertEventKind>();
    set.insert::<AlertDelivery>();
    set.insert::<AlertEvent>();
    set.insert_page::<AlertRule>("AlertRulePage");
    set.insert_page::<AlertEvent>("AlertEventPage");
    set.insert::<BudgetPeriod>();
    set.insert::<RuleTarget>();
    set.insert::<PauseReason>();
    set.insert::<RuleSpend>();
    set.insert::<JobCounts>();
    set.insert::<OnlineRule>();
    set.insert::<OnlineRulePage>();
    set.insert::<CreateOnlineRuleRequest>();
    set.insert::<UpdateOnlineRuleRequest>();
    set.insert::<OnlineRulePreviewRequest>();
    set.insert::<OnlineRulePreview>();
    set.insert::<OnlineRuleStatsResponse>();
    set.insert::<EvalJobKind>();
    set.insert::<EvalJobStatus>();
    set.insert::<EvalJob>();
    set.insert_page::<EvalJob>("EvalJobPage");
    set.insert::<RunEvaluatorRequest>();
    set.insert::<BackfillStatus>();
    set.insert::<Backfill>();
    set.insert_page::<Backfill>("BackfillPage");
    set.insert::<CreateBackfillRequest>();
    set.insert::<BackfillEstimate>();
    set
}

impl SchemaSet {
    fn insert<T: ToSchema>(&mut self) {
        let mut nested = Vec::new();
        T::schemas(&mut nested);
        let name = T::name().into_owned();
        if !nested.iter().any(|(existing, _)| existing == &name) {
            nested.insert(0, (name, T::schema()));
        }
        for (name, schema) in nested {
            self.add(name, schema);
        }
    }

    fn insert_page<T: PartialSchema + ToSchema + 'static>(&mut self, name: &str) {
        self.add(name.to_owned(), <Page<T> as PartialSchema>::schema());
        self.insert::<T>();
    }

    fn add(&mut self, name: String, schema: RefOr<Schema>) {
        match self.schemas.get(&name) {
            Some(existing) => match (schema_json(existing), schema_json(&schema)) {
                (Some(previous), Some(next)) if previous == next => {}
                (Some(_), Some(_)) if is_ref(existing) && !is_ref(&schema) => {
                    self.schemas.insert(name, schema);
                }
                (Some(_), Some(_)) if !is_ref(existing) && is_ref(&schema) => {}
                _ => {
                    self.clash.get_or_insert(name);
                }
            },
            None => {
                self.schemas.insert(name, schema);
            }
        }
    }
}

fn schema_json(schema: &RefOr<Schema>) -> Option<serde_json::Value> {
    serde_json::to_value(schema).ok()
}

fn is_ref(schema: &RefOr<Schema>) -> bool {
    matches!(schema, RefOr::Ref(_))
}

/// The OpenAPI document. Spec 09.
pub fn openapi() -> OpenApi {
    let schemas = collect_schemas();
    let mut document = OpenApiBuilder::new()
        .info(
            InfoBuilder::new()
                .title(concat!(cetra_core::cetra_name!(), " API"))
                .version(env!("CARGO_PKG_VERSION"))
                .description(Some(
                    "Observability read API. The rules are in docs/spec/09-api.md.",
                ))
                .build(),
        )
        .tags(Some([
            tag(
                "auth",
                "Login, logout, sign-up, password reset, the current user, and first-run setup.",
            ),
            tag(
                "admin",
                "Instance administrators: accounts and the effective settings.",
            ),
            tag("orgs", "Organizations the caller belongs to."),
            tag(
                "llm_connections",
                "LLM provider connections that evaluators use. Keys are write-only.",
            ),
            tag("projects", "Projects the caller can see."),
            tag("traces", "Trace list, detail, and live streams."),
            tag("spans", "Span list and detail."),
            tag("sessions", "Session list and detail."),
            tag("users", "End users."),
            tag("scores", "Scores attached to spans, traces, and sessions."),
            tag(
                "evaluation",
                "Evaluators, built-in templates and test runs.",
            ),
            tag(
                "score_definitions",
                "Named score shapes that score writes and evaluators must fit.",
            ),
            tag("datasets", "Versioned datasets and their items."),
            tag(
                "experiments",
                "Experiments run by the SDK and their results.",
            ),
            tag("model_prices", "Custom and built-in model prices"),
            tag("schema", "Filter and SQL catalog."),
            tag("saved_views", "Saved list views and SQL queries."),
            tag("sql", "SQL console over the project's views."),
            tag("metrics", "Dashboard metrics."),
            tag("media", "Extracted media blobs."),
            tag("deletions", "GDPR deletions and their status."),
            tag(
                "alerts",
                "Alert rules, alert events and notification channels.",
            ),
            tag(
                "internal",
                "Endpoints the cloud control plane calls on a cell.",
            ),
        ]))
        .components(Some(components(&schemas)))
        .security(Some([
            SecurityRequirement::new("session", [] as [&str; 0]),
            SecurityRequirement::new("bearer", [] as [&str; 0]),
        ]))
        .paths(paths())
        .build();
    drop_null_from_omitted_optionals(&mut document);
    document
}

/// Spec 09: an absent optional field is omitted, never `null`.
///
/// utoipa marks every `Option<T>` nullable. This removes `null` from every property
/// that is not `required` and from every optional query parameter. The D65 message and
/// part components are skipped: their optional fields can really be `null`. So are the
/// `NULL_IS_A_VALUE` properties.
fn drop_null_from_omitted_optionals(openapi: &mut OpenApi) {
    let skip = d65_component_names();
    if let Some(components) = openapi.components.as_mut() {
        for (name, schema) in &mut components.schemas {
            if skip.contains(name.as_str()) {
                continue;
            }
            let kept = take_null_is_a_value(name, schema);
            strip_nested(schema);
            if let RefOr::T(Schema::Object(object)) = schema {
                object.properties.extend(kept);
            }
        }
    }
    for path_item in openapi.paths.paths.values_mut() {
        strip_query_parameters(path_item.parameters.as_mut());
        strip_operation(path_item.get.as_mut());
        strip_operation(path_item.put.as_mut());
        strip_operation(path_item.post.as_mut());
        strip_operation(path_item.delete.as_mut());
        strip_operation(path_item.options.as_mut());
        strip_operation(path_item.head.as_mut());
        strip_operation(path_item.patch.as_mut());
        strip_operation(path_item.trace.as_mut());
        strip_operation(path_item.query.as_mut());
    }
}

/// Properties where `null` is a value of their own, so the contract keeps it: `null`
/// on a request means "keep data forever", while an absent field leaves the default
/// (spec 09). The server takes both (`api/account.rs`).
const NULL_IS_A_VALUE: &[(&str, &str)] = &[
    ("CreateProjectRequest", "retention_days"),
    ("UpdateProjectRequest", "retention_days"),
    ("UpdateDatasetRequest", "description"),
    ("UpdateDatasetRequest", "input_schema"),
    ("UpdateDatasetRequest", "expected_schema"),
];

/// Removes the `NULL_IS_A_VALUE` properties of `name` from `schema` so that
/// [`strip_nested`] leaves them alone. The caller puts them back.
fn take_null_is_a_value(name: &str, schema: &mut RefOr<Schema>) -> BTreeMap<String, RefOr<Schema>> {
    let RefOr::T(Schema::Object(object)) = schema else {
        return BTreeMap::new();
    };
    NULL_IS_A_VALUE
        .iter()
        .filter(|(component, _)| *component == name)
        .filter_map(|(_, field)| object.properties.remove_entry(*field))
        .collect()
}

fn d65_component_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    names.insert(<cetra_core::message::Message as ToSchema>::name().into_owned());
    for name in cetra_core::message::part_component_names() {
        names.insert((*name).to_owned());
    }
    names
}

fn strip_operation(operation: Option<&mut utoipa::openapi::path::Operation>) {
    if let Some(operation) = operation {
        strip_query_parameters(operation.parameters.as_mut());
    }
}

fn strip_query_parameters(parameters: Option<&mut Vec<RefOr<Parameter>>>) {
    let Some(parameters) = parameters else {
        return;
    };
    for parameter in parameters {
        let RefOr::T(parameter) = parameter else {
            continue;
        };
        if parameter.parameter_in != ParameterIn::Query {
            continue;
        }
        if let Some(schema) = parameter.schema.as_mut() {
            strip_value(schema);
        }
    }
}

/// Removes null from an omitted optional schema, then walks nested properties.
fn strip_value(schema: &mut RefOr<Schema>) {
    if let Some(replacement) = without_null_wrapper(schema) {
        *schema = replacement;
    }
    if let RefOr::T(inner) = schema {
        remove_null_type(inner);
    }
    strip_nested(schema);
}

/// Walks properties and items. A required property keeps its own null.
fn strip_nested(schema: &mut RefOr<Schema>) {
    let RefOr::T(schema) = schema else {
        return;
    };
    match schema {
        Schema::Object(object) => {
            let required: BTreeSet<&str> = object.required.iter().map(String::as_str).collect();
            for (name, property) in &mut object.properties {
                if required.contains(name.as_str()) {
                    strip_nested(property);
                } else {
                    strip_value(property);
                }
            }
            if let Some(AdditionalProperties::RefOr(schema)) =
                object.additional_properties.as_deref_mut()
            {
                strip_nested(schema);
            }
        }
        Schema::Array(array) => {
            if let ArrayItems::RefOrSchema(items) = &mut array.items {
                strip_nested(items);
            }
        }
        Schema::OneOf(one_of) => {
            for item in &mut one_of.items {
                strip_nested(item);
            }
        }
        Schema::AllOf(all_of) => {
            for item in &mut all_of.items {
                strip_nested(item);
            }
        }
        Schema::AnyOf(any_of) => {
            for item in &mut any_of.items {
                strip_nested(item);
            }
        }
        _ => {}
    }
}

fn remove_null_type(schema: &mut Schema) {
    let schema_type = match schema {
        Schema::Object(object) => &mut object.schema_type,
        Schema::Array(array) => &mut array.schema_type,
        Schema::OneOf(one_of) => &mut one_of.schema_type,
        Schema::AllOf(all_of) => &mut all_of.schema_type,
        Schema::AnyOf(any_of) => &mut any_of.schema_type,
        _ => return,
    };
    let SchemaType::Array(types) = schema_type else {
        return;
    };
    types.retain(|ty| *ty != Type::Null);
    if types.len() == 1 {
        let ty = types.remove(0);
        *schema_type = SchemaType::Type(ty);
    }
}

/// `oneOf: [X, {"type": "null"}]` becomes `X`. A described `$ref` is wrapped in `allOf`.
fn without_null_wrapper(schema: &RefOr<Schema>) -> Option<RefOr<Schema>> {
    let RefOr::T(Schema::OneOf(one_of)) = schema else {
        return None;
    };
    if one_of.items.len() != 2 {
        return None;
    }
    let null_at = one_of.items.iter().position(is_null_schema)?;
    let kept = one_of.items[1 - null_at].clone();
    let has_meta =
        one_of.description.is_some() || one_of.default.is_some() || one_of.example.is_some();
    if matches!(kept, RefOr::Ref(_)) && has_meta {
        let mut builder = AllOfBuilder::new().item(kept);
        if let Some(description) = one_of.description.clone() {
            builder = builder.description(Some(description));
        }
        if let Some(default) = one_of.default.clone() {
            builder = builder.default(Some(default));
        }
        if let Some(example) = one_of.example.clone() {
            let mut all_of = builder.build();
            all_of.example = Some(example);
            return Some(RefOr::T(Schema::AllOf(all_of)));
        }
        return Some(RefOr::T(Schema::AllOf(builder.build())));
    }
    if has_meta {
        return Some(apply_wrapper_meta(
            kept,
            one_of.description.clone(),
            one_of.default.clone(),
            one_of.example.clone(),
        ));
    }
    Some(kept)
}

fn apply_wrapper_meta(
    schema: RefOr<Schema>,
    description: Option<String>,
    default: Option<serde_json::Value>,
    example: Option<serde_json::Value>,
) -> RefOr<Schema> {
    let RefOr::T(mut inner) = schema else {
        return schema;
    };
    match &mut inner {
        Schema::Object(object) => {
            if object.description.is_none() {
                object.description = description;
            }
            if object.default.is_none() {
                object.default = default;
            }
            if object.example.is_none() {
                object.example = example;
            }
        }
        Schema::Array(array) => {
            if array.description.is_none() {
                array.description = description;
            }
            if array.default.is_none() {
                array.default = default;
            }
            if array.example.is_none() {
                array.example = example;
            }
        }
        _ => {}
    }
    RefOr::T(inner)
}

fn is_null_schema(schema: &RefOr<Schema>) -> bool {
    matches!(
        schema,
        RefOr::T(Schema::Object(object))
            if matches!(object.schema_type, SchemaType::Type(Type::Null))
    )
}

fn components(schemas: &SchemaSet) -> Components {
    ComponentsBuilder::new()
        .schemas_from_iter(schemas.schemas.clone())
        .security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                COOKIE_NAME,
                "Browser session cookie.",
            ))),
        )
        .security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "A personal access token (`pat-…`), a secret key (`sk-…`), or a public key (`pk-…`, feedback only).",
                    ))
                    .build(),
            ),
        )
        .build()
}

fn tag(name: &str, description: &str) -> Tag {
    TagBuilder::new()
        .name(name)
        .description(Some(description))
        .build()
}

fn paths() -> Paths {
    let paths = PathsBuilder::new();
    let paths = auth_paths(paths);
    let paths = admin_paths(paths);
    let paths = invite_paths(paths);
    let paths = org_sso_paths(paths);
    let paths = project_paths(paths);
    let paths = member_paths(paths);
    let paths = connection_paths(paths);
    let paths = api_key_paths(paths);
    let paths = token_paths(paths);
    let paths = trace_paths(paths);
    let paths = session_paths(paths);
    let paths = score_paths(paths);
    let paths = evaluator_paths(paths);
    let paths = online_rule_paths(paths);
    let paths = eval_job_paths(paths);
    let paths = backfill_paths(paths);
    let paths = score_definition_paths(paths);
    let paths = dataset_paths(paths);
    let paths = experiment_paths(paths);
    let paths = model_price_paths(paths);
    let paths = org_model_price_paths(paths);
    let paths = saved_view_paths(paths);
    let paths = sql_paths(paths);
    let paths = deletion_paths(paths);
    let paths = channel_paths(paths);
    let paths = alert_rule_paths(paths);
    media_paths(paths).build()
}

fn admin_paths(paths: PathsBuilder) -> PathsBuilder {
    let session = Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]);
    let list = operation("list_admin_users", "admin", "List accounts")
        .securities(session.clone())
        .parameters(Some(with_query::<PageParams>(Vec::new())))
        .responses(responses(
            "200",
            json_named("AdminUserPage", "One page of accounts, ordered by email."),
            &[400, 401, 403, 429],
        ))
        .build();
    let create = operation("post_admin_user", "admin", "Create an account")
        .securities(session.clone())
        .request_body(Some(json_body("CreateAdminUserRequest")))
        .responses(responses(
            "201",
            json_of::<CreatedAdminUser>(
                "The account, already verified, and a one-hour link that sets its password.",
            ),
            &[400, 401, 403, 409, 429],
        ))
        .build();
    let update = operation("patch_admin_user", "admin", "Disable or demote an account")
        .securities(session.clone())
        .parameters(Some(vec![path_string("id", "User id.")]))
        .request_body(Some(json_body("UpdateAdminUserRequest")))
        .responses(responses(
            "200",
            json_of::<AdminUser>("The account."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let reset = operation(
        "post_admin_user_reset_link",
        "admin",
        "Mint a password reset link",
    )
    .securities(session.clone())
    .parameters(Some(vec![path_string("id", "User id.")]))
    .responses(responses(
        "200",
        json_of::<AdminResetLink>("A one-hour link that sets the password."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let settings = operation("get_admin_settings", "admin", "Read the instance settings")
        .securities(session.clone())
        .responses(responses(
            "200",
            json_of::<AdminSettings>("The effective settings. No secret is included."),
            &[401, 403, 429],
        ))
        .build();
    let license = operation("get_admin_license", "admin", "Read the instance licence")
        .securities(session.clone())
        .responses(responses(
            "200",
            json_of::<AdminLicense>("The licence status. The key itself is never returned."),
            &[401, 403, 429],
        ))
        .build();
    let test_email = operation(
        "post_admin_settings_test_email",
        "admin",
        "Send a test email to yourself",
    )
    .securities(session)
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The test message was accepted by the SMTP server.")
            .build(),
        &[400, 401, 403, 429, 503],
    ))
    .build();
    let mut users = PathItem::new(HttpMethod::Get, list);
    users.post = Some(create);
    paths
        .path("/api/v1/admin/users", users)
        .path(
            "/api/v1/admin/users/{id}",
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            "/api/v1/admin/users/{id}/reset-link",
            PathItem::new(HttpMethod::Post, reset),
        )
        .path(
            "/api/v1/admin/settings",
            PathItem::new(HttpMethod::Get, settings),
        )
        .path(
            "/api/v1/admin/license",
            PathItem::new(HttpMethod::Get, license),
        )
        .path(
            "/api/v1/admin/settings/test-email",
            PathItem::new(HttpMethod::Post, test_email),
        )
}

fn auth_paths(paths: PathsBuilder) -> PathsBuilder {
    let authorize = operation(
        "get_oauth_authorize",
        "auth",
        "Start an OAuth or OIDC login",
    )
    .description(Some(
        "Redirects to the provider and sets the `cetra_oauth` cookie. `redirect` is kept when it \
         starts with `/`, does not start with `//` or `/\\`, is at most 1024 bytes, and contains \
         no ASCII control character. Otherwise the browser returns to `/`. An unknown provider is \
         404. When metadata cannot be loaded, the response redirects to \
         `/login?error=provider_unavailable` and sets no cookie.",
    ))
    .securities(Some(Vec::new()))
    .parameters(Some(vec![
        path_string(
            "provider",
            "Provider id: `github`, `google`, or `oidc:<name>`.",
        ),
        query_string(
            "redirect",
            "Relative path to open after a successful login.",
            false,
        ),
        query_string(
            "setup_token",
            "First-run setup token. Stored in the login cookie and accepted only while no user exists.",
            false,
        ),
    ]))
    .responses(responses(
        "302",
        redirect_to("Redirect to the provider, or to `/login?error=provider_unavailable`."),
        &[404],
    ))
    .build();
    let callback = operation(
        "get_oauth_callback",
        "auth",
        "Finish an OAuth or OIDC login",
    )
    .description(Some(
        "Clears the login cookie. Success sets the session cookie and redirects to the saved \
         relative path. Failure redirects to `/login?error=<code>`. Codes: `invalid_state`, \
         `provider_error`, `provider_unavailable`, `email_not_verified`, `domain_not_allowed`, \
         `signup_closed`, `account_disabled`, `link_refused`, `setup_required`, `seat_limit`, `sso_unavailable`, `sso_required`. The provider is `github`, `google`, `oidc:<name>`, or `sso` (an org's provider, started through `/api/auth/sso/authorize`).",
    ))
    .securities(Some(Vec::new()))
    .parameters(Some(vec![
        path_string(
            "provider",
            "Provider id: `github`, `google`, or `oidc:<name>`.",
        ),
        query_string("code", "Authorization code from the provider.", false),
        query_string(
            "state",
            "State value from the authorization redirect.",
            false,
        ),
        query_string(
            "error",
            "Error code from the provider, when it refused the login.",
            false,
        ),
    ]))
    .responses(responses(
        "302",
        redirect_to(
            "Redirect to the saved path, or to `/login?error=<code>` (`invalid_state`, \
             `provider_error`, `provider_unavailable`, `email_not_verified`, \
             `domain_not_allowed`, `signup_closed`, `account_disabled`, `link_refused`, \
             `setup_required`).",
        ),
        &[],
    ))
    .build();
    let config = operation("get_auth_config", "auth", "Get the auth config")
        .securities(Some(Vec::new()))
        .responses(responses(
            "200",
            json_of::<AuthConfig>("Login methods the UI should offer."),
            &[],
        ))
        .build();
    let setup = operation("post_setup", "auth", "Create the first admin")
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("SetupRequest")))
        .responses(responses(
            "201",
            json_of::<SetupResponse>("The admin, org, project, and secret key shown once."),
            &[400, 403, 409, 429],
        ))
        .build();
    let login = operation("post_login", "auth", "Log in with a password")
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("LoginRequest")))
        .responses(responses(
            "200",
            json_of::<Me>("The signed-in user and org memberships."),
            &[401, 403, 429],
        ))
        .build();
    let logout = operation("post_logout", "auth", "Log out")
        .securities(Some(Vec::new()))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The session is deleted and the cookie is cleared.")
                .build(),
            &[],
        ))
        .build();
    let signup = operation("post_signup", "auth", "Create an account")
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("SignupRequest")))
        .responses(responses(
            "201",
            json_of::<SignupResponse>("The new account, or a verification requirement."),
            &[400, 403, 409, 429],
        ))
        .build();
    let me = operation("get_me", "auth", "Get the current user")
        .responses(responses(
            "200",
            json_of::<Me>("The signed-in user and org memberships."),
            &[401, 429],
        ))
        .build();
    let forgot = operation("post_password_forgot", "auth", "Request a password reset")
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("ForgotPasswordRequest")))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description(
                    "Accepted. A reset message is sent only when that address can receive one.",
                )
                .build(),
            &[400, 403, 429],
        ))
        .build();
    let reset = operation(
        "post_password_reset",
        "auth",
        "Set a password from a reset link",
    )
    .securities(Some(Vec::new()))
    .request_body(Some(json_body("ResetPasswordRequest")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The password is updated and every web session is deleted.")
            .build(),
        &[400, 403, 429],
    ))
    .build();
    let verify = operation("post_email_verify", "auth", "Verify an email address")
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("VerifyEmailRequest")))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The email address is verified.")
                .build(),
            &[400],
        ))
        .build();
    let resend = operation(
        "post_email_verify_resend",
        "auth",
        "Resend an email verification",
    )
    .securities(Some(Vec::new()))
    .request_body(Some(json_body("ResendVerificationRequest")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("Accepted. A verification message is sent only for an unverified account.")
            .build(),
        &[400, 429],
    ))
    .build();
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let patch_me = operation("patch_me", "auth", "Change the display name")
        .securities(Some(session_only.clone()))
        .request_body(Some(json_body("UpdateMeRequest")))
        .responses(responses(
            "200",
            json_of::<User>("The updated user."),
            &[400, 401, 403],
        ))
        .build();
    let password = operation("post_me_password", "auth", "Change the password")
        .securities(Some(session_only))
        .request_body(Some(json_body("ChangePasswordRequest")))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description(
                    "The password is updated. Other web sessions and personal access tokens are revoked.",
                )
                .build(),
            &[400, 401, 403, 429],
        ))
        .build();
    let session = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let preview = operation("get_invite_preview", "auth", "Preview an invitation")
        .securities(Some(Vec::new()))
        .parameters(Some(vec![path_string(
            "token",
            "Invitation token from the accept link.",
        )]))
        .responses(responses(
            "200",
            json_of::<InvitePreview>("The invitation, when it is pending and unexpired."),
            &[404, 429],
        ))
        .build();
    let accept = operation("post_invite_accept", "auth", "Accept an invitation")
        .securities(Some(session))
        .parameters(Some(vec![path_string(
            "token",
            "Invitation token from the accept link.",
        )]))
        .responses(responses(
            "200",
            json_of::<OrgMembership>("The caller's membership in the org."),
            &[401, 403, 404, 429],
        ))
        .build();
    let sso_authorize = operation(
        "get_sso_authorize",
        "auth",
        "Start an org's single sign-on login",
    )
    .description(Some(
        "Starts a login through the identity provider of the org that owns the email domain (D-P-60). A domain that is not verified, a disabled provider (except for an owner's Test sign-in, D-P-57) and an unknown domain all redirect to `/login?error=sso_unavailable` with no cookie. Discovery and authorize share 300 requests a minute per client IP; a burst over that is 429. `redirect` is kept as for the instance providers.",
    ))
    .securities(Some(Vec::new()))
    .parameters(Some(vec![
        query_string("domain", "The email domain, e.g. `acme.com`.", false),
        query_string(
            "redirect",
            "Relative path to open after a successful login.",
            false,
        ),
    ]))
    .responses(responses(
        "302",
        redirect_to("Redirect to the identity provider, or to `/login?error=sso_unavailable`."),
        &[429],
    ))
    .build();
    let sso_discover = operation(
        "post_sso_discover",
        "auth",
        "Ask whether an email's domain signs in through an org",
    )
    .description(Some(
        "Answers whether the email's domain has an org provider that is enabled (D-P-60). The answer depends only on the domain, so no account is needed and nothing is logged. Shares the per-IP bucket of 300 a minute with `/api/auth/sso/authorize`; over it is 429.",
    ))
    .securities(Some(Vec::new()))
    .request_body(Some(json_body("SsoDiscoverRequest")))
    .responses(responses(
        "200",
        json_of::<SsoDiscovery>("Whether the domain signs in through an org."),
        &[400, 429],
    ))
    .build();
    paths
        .path(
            "/api/auth/sso/discover",
            PathItem::new(HttpMethod::Post, sso_discover),
        )
        .path(
            "/api/auth/sso/authorize",
            PathItem::new(HttpMethod::Get, sso_authorize),
        )
        .path(
            "/api/auth/authorize/{provider}",
            PathItem::new(HttpMethod::Get, authorize),
        )
        .path(
            "/api/auth/callback/{provider}",
            PathItem::new(HttpMethod::Get, callback),
        )
        .path("/api/auth/config", PathItem::new(HttpMethod::Get, config))
        .path("/api/auth/setup", PathItem::new(HttpMethod::Post, setup))
        .path("/api/auth/login", PathItem::new(HttpMethod::Post, login))
        .path("/api/auth/logout", PathItem::new(HttpMethod::Post, logout))
        .path("/api/auth/signup", PathItem::new(HttpMethod::Post, signup))
        .path(
            "/api/auth/invites/{token}",
            PathItem::new(HttpMethod::Get, preview),
        )
        .path(
            "/api/auth/invites/{token}/accept",
            PathItem::new(HttpMethod::Post, accept),
        )
        .path(
            "/api/auth/password/forgot",
            PathItem::new(HttpMethod::Post, forgot),
        )
        .path(
            "/api/auth/password/reset",
            PathItem::new(HttpMethod::Post, reset),
        )
        .path(
            "/api/auth/email/verify",
            PathItem::new(HttpMethod::Post, verify),
        )
        .path(
            "/api/auth/email/verify/resend",
            PathItem::new(HttpMethod::Post, resend),
        )
        .path("/api/auth/me", PathItem::new(HttpMethod::Get, me))
        .path("/api/v1/me", PathItem::new(HttpMethod::Patch, patch_me))
        .path(
            "/api/v1/me/password",
            PathItem::new(HttpMethod::Post, password),
        )
}

fn org_sso_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let org_param = path_string("org", "Org UUID or slug.");
    let get = operation(
        "get_org_sso",
        "orgs",
        "Read the org's identity provider and domains",
    )
    .description(Some(
        "The provider without its client secret, the email domains with their TXT records, and the callback URL. Owners only, session only (D-P-51). Needs no feature, so the settings stay readable after a lapse (D-P-61).",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![org_param.clone()]))
    .responses(responses(
        "200",
        json_of::<OrgSso>("The org's SSO settings."),
        AUTH_ERRORS,
    ))
    .build();
    let put = operation(
        "put_org_sso_provider",
        "orgs",
        "Save the org's identity provider",
    )
    .description(Some(
        "Creates or updates the org's one OIDC provider (D-P-52). The issuer's discovery document is loaded before anything is stored. The client secret is write-only: it is needed when creating, and when the issuer or client id changes. A new issuer deletes the org's identities. Needs the Teams add-on or an enterprise licence (D-P-61). Owners only, session only (D-P-51).",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![org_param.clone()]))
    .request_body(Some(json_body("PutOrgSsoProvider")))
    .responses(responses(
        "200",
        json_of::<OrgSso>("The saved SSO settings."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let delete = operation(
        "delete_org_sso_provider",
        "orgs",
        "Remove the org's identity provider",
    )
    .description(Some(
        "Turn enforcement off first (409 otherwise). The org's identities for the provider go with it. Owners only, session only (D-P-51).",
    ))
    .securities(Some(session_only))
    .parameters(Some(vec![org_param]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The provider is removed.")
            .build(),
        &[401, 403, 404, 409, 429],
    ))
    .build();
    let mut provider = PathItem::new(HttpMethod::Put, put);
    provider.delete = Some(delete);
    let patch_settings = operation(
        "patch_org_sso",
        "orgs",
        "Turn the org's identity provider on, off or enforced",
    )
    .description(Some(
        "Sets `enabled` and `enforced`, each optional (D-P-57, D-P-58, D-P-61). Turning enforcement on needs a verified domain, a sign-in through the provider, and this session's own sign-in through it when the caller's domain is covered. It revokes the password sessions and personal access tokens of the covered users, except instance admins (D-P-58). A provider cannot be disabled while enforced. Owners only, session only (D-P-51).",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .request_body(Some(json_body("PatchOrgSso")))
    .responses(responses(
        "200",
        json_of::<OrgSso>("The SSO settings."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let add_domain = operation(
        "post_org_sso_domain",
        "orgs",
        "Add an email domain to the org's SSO",
    )
    .description(Some(
        "Adds a pending domain (D-P-54). The domain is trimmed, lowercased and checked as a DNS name; a domain the server configuration owns is refused. Prove it with the TXT record of the response, then call the verify route. Up to 20 domains per org. Needs the Teams add-on or an enterprise licence (D-P-61). Owners only, session only.",
    ))
    .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .request_body(Some(json_body("AddOrgSsoDomain")))
    .responses(responses(
        "201",
        json_of::<OrgSso>("The SSO settings, with the new domain pending."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let verify = operation(
        "post_org_sso_domain_verify",
        "orgs",
        "Verify an email domain with its DNS TXT record",
    )
    .description(Some(
        "Looks up `_cetra-verify.<domain>` and verifies the domain when its value `cetra-verify=<token>` is there (D-P-54, D-P-64). An already verified domain answers 200 without a lookup. A domain another org holds moves to this org only when the holder's record is gone from the answer. At most 10 lookups per minute per org. A failed lookup answers 503. Needs the Teams add-on or an enterprise licence (D-P-61). Owners only, session only.",
    ))
    .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
    .parameters(Some(vec![
        path_string("org", "Org UUID or slug."),
        path_uuid("id", "Domain id."),
    ]))
    .responses(responses(
        "200",
        json_of::<OrgSso>("The SSO settings, with the domain verified."),
        &[401, 403, 404, 409, 429, 503],
    ))
    .build();
    let remove_domain = operation(
        "delete_org_sso_domain",
        "orgs",
        "Remove an email domain from the org's SSO",
    )
    .description(Some(
        "Removes the domain, verified or not. The org stops routing its users through the provider. Owners only, session only (D-P-51).",
    ))
    .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
    .parameters(Some(vec![
        path_string("org", "Org UUID or slug."),
        path_uuid("id", "Domain id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The domain is removed.")
            .build(),
        AUTH_ERRORS,
    ))
    .build();
    let mut settings = PathItem::new(HttpMethod::Get, get);
    settings.patch = Some(patch_settings);
    paths
        .path("/api/v1/orgs/{org}/sso", settings)
        .path("/api/v1/orgs/{org}/sso/provider", provider)
        .path(
            "/api/v1/orgs/{org}/sso/domains",
            PathItem::new(HttpMethod::Post, add_domain),
        )
        .path(
            "/api/v1/orgs/{org}/sso/domains/{id}/verify",
            PathItem::new(HttpMethod::Post, verify),
        )
        .path(
            "/api/v1/orgs/{org}/sso/domains/{id}",
            PathItem::new(HttpMethod::Delete, remove_domain),
        )
}

fn invite_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let org_param = path_string("org", "Org UUID or slug.");
    let list = operation("list_org_invites", "orgs", "List pending invitations")
        .parameters(Some(with_query::<PageParams>(vec![org_param.clone()])))
        .responses(responses(
            "200",
            json_named(
                "InvitePage",
                "One page of pending invitations, ordered by email.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_org_invite", "orgs", "Invite a member by email")
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![org_param.clone()]))
        .request_body(Some(json_body("CreateInviteRequest")))
        .responses(responses(
            "201",
            json_of::<CreatedInvite>("The invitation and its accept link."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let delete = operation("delete_org_invite", "orgs", "Delete an invitation")
        .securities(Some(session_only))
        .parameters(Some(vec![org_param, path_uuid("id", "Invitation id.")]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The invitation is deleted.")
                .build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let mut collection = PathItem::new(HttpMethod::Get, list);
    collection.post = Some(create);
    paths.path("/api/v1/orgs/{org}/invites", collection).path(
        "/api/v1/orgs/{org}/invites/{id}",
        PathItem::new(HttpMethod::Delete, delete),
    )
}

fn project_paths(paths: PathsBuilder) -> PathsBuilder {
    let list_orgs = operation("list_orgs", "orgs", "List organizations")
        .parameters(Some(with_query::<PageParams>(Vec::new())))
        .responses(responses(
            "200",
            json_named("OrgPage", "One page of organizations."),
            &[400, 401, 429],
        ))
        .build();
    let post_org = operation("post_org", "orgs", "Create an organization")
        .request_body(Some(json_body("CreateOrgRequest")))
        .responses(responses(
            "201",
            json_of::<OrgMembership>("The new organization. The caller is its owner."),
            &[400, 401, 403, 409, 429],
        ))
        .build();
    let get_org = operation("get_org", "orgs", "Get an organization")
        .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
        .responses(responses(
            "200",
            json_of::<OrgMembership>("The organization and the caller's role."),
            AUTH_ERRORS,
        ))
        .build();
    let patch_org = operation("patch_org", "orgs", "Rename an organization")
        .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
        .request_body(Some(json_body("UpdateOrgRequest")))
        .responses(responses(
            "200",
            json_of::<OrgMembership>("The organization. The slug is unchanged."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let list = operation("list_projects", "projects", "List projects")
        .parameters(Some(with_query::<PageParams>(vec![path_string(
            "org",
            "Org UUID or slug.",
        )])))
        .responses(responses(
            "200",
            json_named("ProjectPage", "One page of projects."),
            QUERY_ERRORS,
        ))
        .build();
    let post_project = operation("post_project", "projects", "Create a project")
        .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
        .request_body(Some(json_body("CreateProjectRequest")))
        .responses(responses(
            "201",
            json_of::<Project>("The new project."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let get = operation("get_project", "projects", "Get a project")
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_of::<Project>("The project."),
            AUTH_ERRORS,
        ))
        .build();
    let patch_project = operation("patch_project", "projects", "Update a project")
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("UpdateProjectRequest")))
        .responses(responses(
            "200",
            json_of::<Project>("The project."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let mut orgs = PathItem::new(HttpMethod::Get, list_orgs);
    orgs.post = Some(post_org);
    let delete_org = operation("delete_org", "orgs", "Delete an organization (owners, session only)")
        .description(Some(
            "Answers 202; the organization and all its projects are erased in the background. Its deletion record is then kept in the database and the audit log only. Needs a browser session: API keys and personal access tokens get 403 (D-P-70).",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(with_query::<DropParams>(vec![path_string(
            "org",
            "Org UUID or slug.",
        )])))
        .responses(responses(
            "202",
            json_of::<Deletion>("The deletion, in status pending."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let get_entitlements = operation(
        "get_org_entitlements",
        "orgs",
        "Read the org's entitlements",
    )
    .description(Some(
        "Features the org may use now and where Upgrade buttons go (D179). Any org member.",
    ))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .responses(responses(
        "200",
        json_of::<OrgEntitlements>("The org's entitlements."),
        AUTH_ERRORS,
    ))
    .build();
    let get_usage = operation("get_org_usage", "orgs", "Read the org's usage by day")
        .description(Some(
            "Units, bytes and judge usage per UTC day, summed over the org's projects (D-P-42). `from` and `to` are inclusive days; the default is this month to today, and `to - from` is at most 365 days. Today's row counts flushed usage only, up to 10 seconds behind. Any org member.",
        ))
        .parameters(Some(with_query::<UsageParams>(vec![path_string(
            "org",
            "Org UUID or slug.",
        )])))
        .responses(responses(
            "200",
            json_of::<OrgUsage>("The org's usage by day."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let put_entitlements = operation(
        "put_org_entitlements_token",
        "internal",
        "Store the org's entitlement token",
    )
    .description(Some(
        "The control plane sends a signed `kind: org` token for the org (D-P-11). The signature is the credential, so the route takes no session or bearer. A verified token is stored and applied; a failed one counts against the IP limit.",
    ))
    .securities(Some(Vec::new()))
    .parameters(Some(vec![path_uuid("org_id", "Org UUID.")]))
    .request_body(Some(json_body("PutOrgTokenRequest")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The token is stored, or it was already stored.")
            .build(),
        &[400, 404, 409, 429],
    ))
    .build();
    let post_internal_org = operation("post_internal_org", "internal", "Create an org")
        .description(Some(
            "Needs a `control_plane` bearer token with the `orgs` scope. The id is the control plane's, and it is a UUIDv7. An existing id answers 200 with the stored org and changes nothing. A new id creates the org and makes the owner its owner: a new user is created verified without a password, and an existing user is reused. A disabled user or an instance admin cannot be the owner (422 `owner_not_allowed`). An id that is being deleted, or was deleted, answers 409 `conflict`; a deleted id is never reused.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "bearer",
            [] as [&str; 0],
        )]))
        .request_body(Some(json_body("CreateInternalOrgRequest")))
        .responses(responses_with(
            [
                ("201", json_of::<InternalOrg>("The org was created.")),
                (
                    "200",
                    json_of::<InternalOrg>("The org with this id already exists."),
                ),
            ],
            &[400, 401, 403, 409, 422, 429],
        ))
        .build();
    let delete_internal_org = operation("delete_internal_org", "internal", "Drop an org")
        .description(Some(
            "Needs a `control_plane` bearer token with the `orgs` scope. Starts the org's drop (D-M-7) with no confirm: the org is gone from every route at once, and its projects and data are erased in the background. Answers 202 with the deletion while it runs and 200 once it is applied. Repeats answer for the org's newest org deletion, so the call is idempotent. An org that was never dropped answers 404.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "bearer",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![path_uuid("org_id", "Org UUID.")]))
        .responses(responses_with(
            [
                (
                    "202",
                    json_of::<Deletion>("The org drop, in status pending or resolving."),
                ),
                ("200", json_of::<Deletion>("The org drop is applied.")),
            ],
            &[401, 403, 404, 429],
        ))
        .build();
    let list_internal_usage = operation("list_internal_usage", "internal", "List org usage by day")
        .description(Some(
            "Needs a `control_plane` bearer token with the `usage` scope. Usage per UTC day and org, summed over the org's projects, ordered by day and then org id, keyset-paged. `from` and `to` are required inclusive days, and `to - from` is at most 92 days. Rows of deleted orgs stay.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "bearer",
            [] as [&str; 0],
        )]))
        .parameters(Some(with_query::<InternalUsageQuery>(Vec::new())))
        .responses(responses(
            "200",
            json_named("InternalUsagePage", "One page of org usage by day."),
            &[400, 401, 403, 429],
        ))
        .build();
    let list_internal_deletions = operation(
        "list_internal_deletions",
        "internal",
        "List applied org and project drops",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `deletions` scope. Applied project and org drops, oldest first, keyset-paged, for the control plane's billing and records. Rows are kept forever. `kind` narrows to `project` or `org`.",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "bearer",
        [] as [&str; 0],
    )]))
    .parameters(Some(with_query::<InternalDropsQuery>(Vec::new())))
    .responses(responses(
        "200",
        json_named("InternalDropPage", "One page of applied drops, oldest first."),
        &[400, 401, 403, 429],
    ))
    .build();
    let internal_user_security = || vec![SecurityRequirement::new("bearer", [] as [&str; 0])];
    let post_internal_user_revoke = operation(
        "post_internal_user_revoke",
        "internal",
        "End a user's sessions and personal tokens",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `users` scope. The user is named by its OIDC subject at the control plane's provider (`entitlements.control_plane_provider`), or, when no identity has that subject, by its email if that user has no identity at that provider (D-S-9). The email is checked only then (400 for an invalid one). Deletes the user's web sessions and revokes its personal tokens; the user can sign in again. An instance admin cannot be managed (403 `forbidden`). An unknown user answers 404.",
    ))
    .securities(Some(internal_user_security()))
    .request_body(Some(json_body("InternalUserRef")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The user's sessions and tokens were revoked.")
            .build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let post_internal_user_disable = operation(
        "post_internal_user_disable",
        "internal",
        "Disable a user",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `users` scope. The user is named by its OIDC subject at the control plane's provider (`entitlements.control_plane_provider`), or, when no identity has that subject, by its email if that user has no identity at that provider (D-S-9). The email is checked only then (400 for an invalid one). Revokes the user's sessions and personal tokens, as `revoke` does, and sets the user disabled. Unlike the instance admin's disable, the last owner of an org may be disabled: the org stays. An instance admin cannot be managed (403 `forbidden`). An unknown user answers 404.",
    ))
    .securities(Some(internal_user_security()))
    .request_body(Some(json_body("InternalUserRef")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The user is disabled and its access is revoked.")
            .build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let post_internal_user_delete = operation(
        "post_internal_user_delete",
        "internal",
        "Delete a user",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `users` scope. The user is named by its OIDC subject at the control plane's provider (`entitlements.control_plane_provider`), or, when no identity has that subject, by its email if that user has no identity at that provider (D-S-9). The email is checked only then (400 for an invalid one). Deletes the user in one transaction. Its invites and API keys stay, without their creator. Orgs where the user was the only owner stay, without an owner, and are listed in `sole_owner_of` for the control plane to resolve. An unknown user answers 200 with an empty list, so a retry is safe. An instance admin cannot be managed (403 `forbidden`).",
    ))
    .securities(Some(internal_user_security()))
    .request_body(Some(json_body("InternalUserRef")))
    .responses(responses(
        "200",
        json_of::<InternalUserDeleted>("The user is deleted, with the orgs it solely owned."),
        &[400, 401, 403, 429],
    ))
    .build();
    let post_membership_lookup = operation(
        "post_internal_membership_lookup",
        "internal",
        "Look up a user's memberships",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `members` scope. The user is the one whose OIDC subject at the control plane's provider (`entitlements.control_plane_provider`) matches `subject`; when no identity has the subject, the user with `email`. Lists the user's active orgs sorted by slug. An unknown user answers 200 with no orgs; an invalid email answers 400 only when the subject finds no user.",
    ))
    .securities(Some(internal_user_security()))
    .request_body(Some(json_body("MembershipLookupRequest")))
    .responses(responses(
        "200",
        json_of::<MembershipLookup>("The user's active memberships."),
        &[400, 401, 403, 429],
    ))
    .build();
    let list_internal_org_members = operation(
        "list_internal_org_members",
        "internal",
        "List an org's members",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `members` scope. Members ordered by email, with the email the control plane matches accounts by. `role` is a comma-separated list of `owner`, `admin`, `member` and `viewer`; without it every member is listed. An unknown or inactive org answers 404.",
    ))
    .securities(Some(internal_user_security()))
    .parameters(Some(with_query::<InternalMembersQuery>(vec![path_uuid(
        "org_id",
        "Org UUID.",
    )])))
    .responses(responses(
        "200",
        json_of::<InternalMembers>("The org's members."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let mut org = PathItem::new(HttpMethod::Get, get_org);
    org.patch = Some(patch_org);
    org.delete = Some(delete_org);
    let mut projects = PathItem::new(HttpMethod::Get, list);
    projects.post = Some(post_project);
    let delete_project = operation(
        "delete_project",
        "projects",
        "Delete a project (org admins; session only)",
    )
    .description(Some(
        "Answers 202; the project is gone from every route at once and its data is erased in the background. Needs a browser session: API keys and personal access tokens get 403 (D-P-70).",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(with_query::<DropParams>(vec![project_param()])))
    .responses(responses(
        "202",
        json_of::<Deletion>("The deletion, in status pending."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let mut project = PathItem::new(HttpMethod::Get, get);
    project.patch = Some(patch_project);
    project.delete = Some(delete_project);
    paths
        .path("/api/v1/orgs", orgs)
        .path("/api/v1/orgs/{org}", org)
        .path(
            "/api/v1/orgs/{org}/entitlements",
            PathItem::new(HttpMethod::Get, get_entitlements),
        )
        .path(
            "/api/v1/orgs/{org}/usage",
            PathItem::new(HttpMethod::Get, get_usage),
        )
        .path(
            "/api/internal/v1/orgs/{org_id}/entitlements",
            PathItem::new(HttpMethod::Put, put_entitlements),
        )
        .path(
            "/api/internal/v1/orgs",
            PathItem::new(HttpMethod::Post, post_internal_org),
        )
        .path(
            "/api/internal/v1/orgs/{org_id}",
            PathItem::new(HttpMethod::Delete, delete_internal_org),
        )
        .path(
            "/api/internal/v1/usage",
            PathItem::new(HttpMethod::Get, list_internal_usage),
        )
        .path(
            "/api/internal/v1/deletions",
            PathItem::new(HttpMethod::Get, list_internal_deletions),
        )
        .path(
            "/api/internal/v1/users/revoke",
            PathItem::new(HttpMethod::Post, post_internal_user_revoke),
        )
        .path(
            "/api/internal/v1/users/disable",
            PathItem::new(HttpMethod::Post, post_internal_user_disable),
        )
        .path(
            "/api/internal/v1/users/delete",
            PathItem::new(HttpMethod::Post, post_internal_user_delete),
        )
        .path(
            "/api/internal/v1/memberships/lookup",
            PathItem::new(HttpMethod::Post, post_membership_lookup),
        )
        .path(
            "/api/internal/v1/orgs/{org_id}/members",
            PathItem::new(HttpMethod::Get, list_internal_org_members),
        )
        .path("/api/v1/orgs/{org}/projects", projects)
        .path("/api/v1/projects/{project}", project)
}

fn member_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let org_param = path_string("org", "Org UUID or slug.");
    let user_param = path_uuid("user", "User id.");
    let list_orgs = operation("list_org_members", "orgs", "List organization members")
        .parameters(Some(with_query::<PageParams>(vec![org_param.clone()])))
        .responses(responses(
            "200",
            json_named("OrgMemberPage", "One page of members, ordered by email."),
            QUERY_ERRORS,
        ))
        .build();
    let patch_org = operation("patch_org_member", "orgs", "Change an organization role")
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![org_param, user_param.clone()]))
        .request_body(Some(json_body("UpdateOrgMemberRequest")))
        .responses(responses(
            "200",
            json_of::<OrgMember>("The member."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let delete_org = operation("delete_org_member", "orgs", "Remove an organization member")
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![
            path_string("org", "Org UUID or slug."),
            user_param.clone(),
        ]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The member and their project overrides in this org are removed.")
                .build(),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let list_projects = operation("list_project_members", "projects", "List project members")
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named(
                "ProjectMemberPage",
                "One page of org members and their effective project role, ordered by email.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let put_project = operation(
        "put_project_member",
        "projects",
        "Set a project role override",
    )
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![project_param(), user_param.clone()]))
    .request_body(Some(json_body("SetProjectOverrideRequest")))
    .responses(responses(
        "200",
        json_of::<ProjectMember>("The member on this project."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let delete_project = operation(
        "delete_project_member",
        "projects",
        "Remove a project role override",
    )
    .securities(Some(session_only))
    .parameters(Some(vec![project_param(), user_param]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The override is removed. Also returned when there was none.")
            .build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let mut org_member = PathItem::new(HttpMethod::Patch, patch_org);
    org_member.delete = Some(delete_org);
    paths
        .path(
            "/api/v1/orgs/{org}/members",
            PathItem::new(HttpMethod::Get, list_orgs),
        )
        .path("/api/v1/orgs/{org}/members/{user}", org_member)
        .path(
            "/api/v1/projects/{project}/members",
            PathItem::new(HttpMethod::Get, list_projects),
        )
        .path(
            "/api/v1/projects/{project}/members/{user}",
            PathItem::new(HttpMethod::Put, put_project),
        )
        .path(
            "/api/v1/projects/{project}/members/{user}",
            PathItem::new(HttpMethod::Delete, delete_project),
        )
}

fn connection_paths(paths: PathsBuilder) -> PathsBuilder {
    let org = path_string("org", "Org UUID or slug.");
    let id = path_uuid("id", "Connection id.");
    let list = operation(
        "list_llm_connections",
        "llm_connections",
        "List LLM connections",
    )
    .description(Some("Any member of the organization."))
    .parameters(Some(with_query::<PageParams>(vec![org.clone()])))
    .responses(responses(
        "200",
        json_named(
            "LlmConnectionPage",
            "One page of connections, ordered by name. Keys and header values are not included.",
        ),
        QUERY_ERRORS,
    ))
    .build();
    let create = operation(
        "post_llm_connection",
        "llm_connections",
        "Create an LLM connection",
    )
    .description(Some("Organization admins and owners."))
    .parameters(Some(vec![org.clone()]))
    .request_body(Some(json_body("CreateLlmConnectionRequest")))
    .responses(responses(
        "201",
        json_of::<LlmConnection>("The connection."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let get = operation(
        "get_llm_connection",
        "llm_connections",
        "Get an LLM connection",
    )
    .description(Some("Any member of the organization."))
    .parameters(Some(vec![org.clone(), id.clone()]))
    .responses(responses(
        "200",
        json_of::<LlmConnection>("The connection."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let patch = operation(
        "patch_llm_connection",
        "llm_connections",
        "Update an LLM connection",
    )
    .description(Some("Organization admins and owners."))
    .parameters(Some(vec![org.clone(), id.clone()]))
    .request_body(Some(json_body("UpdateLlmConnectionRequest")))
    .responses(responses(
        "200",
        json_of::<LlmConnection>("The connection."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let delete = operation(
        "delete_llm_connection",
        "llm_connections",
        "Delete an LLM connection",
    )
    .description(Some("Organization admins and owners."))
    .parameters(Some(vec![org.clone(), id.clone()]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The connection is deleted.")
            .build(),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let test = operation(
        "post_llm_connection_test",
        "llm_connections",
        "Test an LLM connection",
    )
    .description(Some("Organization admins and owners."))
    .parameters(Some(vec![org, id]))
    .request_body(Some(json_body("TestLlmConnectionRequest")))
    .responses(responses(
        "200",
        json_of::<LlmConnectionTestResult>(
            "The outcome. A provider failure is ok: false, not an error status.",
        ),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let mut collection = PathItem::new(HttpMethod::Get, list);
    collection.post = Some(create);
    let mut connection = PathItem::new(HttpMethod::Get, get);
    connection.patch = Some(patch);
    connection.delete = Some(delete);
    paths
        .path("/api/v1/orgs/{org}/llm-connections", collection)
        .path("/api/v1/orgs/{org}/llm-connections/{id}", connection)
        .path(
            "/api/v1/orgs/{org}/llm-connections/{id}/test",
            PathItem::new(HttpMethod::Post, test),
        )
}

fn api_key_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let list = operation("list_api_keys", "projects", "List API keys")
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named(
                "ApiKeyPage",
                "One page of API keys, newest first. The secret is not included.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_api_key", "projects", "Create an API key")
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateApiKeyRequest")))
        .responses(responses(
            "201",
            json_of::<CreatedApiKey>("The key and its secret. The secret is shown only here."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let delete = operation("delete_api_key", "projects", "Revoke an API key")
        .securities(Some(session_only))
        .parameters(Some(vec![project_param(), path_uuid("id", "API key id.")]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The key is revoked.")
                .build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let mut keys = PathItem::new(HttpMethod::Get, list);
    keys.post = Some(create);
    paths
        .path("/api/v1/projects/{project}/api-keys", keys)
        .path(
            "/api/v1/projects/{project}/api-keys/{id}",
            PathItem::new(HttpMethod::Delete, delete),
        )
}

fn token_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let list = operation(
        "list_personal_tokens",
        "auth",
        "List personal access tokens",
    )
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(with_query::<PageParams>(Vec::new())))
    .responses(responses(
        "200",
        json_named(
            "PersonalTokenPage",
            "One page of personal access tokens, newest first. The secret is not included.",
        ),
        QUERY_ERRORS,
    ))
    .build();
    let create = operation(
        "post_personal_token",
        "auth",
        "Create a personal access token",
    )
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .request_body(Some(json_body("CreatePersonalTokenRequest")))
    .responses(responses(
        "201",
        json_of::<CreatedPersonalToken>("The token and its secret. The secret is shown only here."),
        &[400, 401, 403, 429],
    ))
    .build();
    let delete_one = operation(
        "delete_personal_token",
        "auth",
        "Revoke a personal access token",
    )
    .securities(Some(session_only))
    .parameters(Some(vec![path_uuid("id", "Personal access token id.")]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The token is revoked.")
            .build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let delete_current = operation(
        "delete_current_token",
        "auth",
        "Revoke the current personal access token",
    )
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The token that authenticated this request is revoked.")
            .build(),
        AUTH_ERRORS,
    ))
    .build();
    let mut tokens = PathItem::new(HttpMethod::Get, list);
    tokens.post = Some(create);
    paths
        .path("/api/v1/me/tokens", tokens)
        .path(
            "/api/v1/me/tokens/current",
            PathItem::new(HttpMethod::Delete, delete_current),
        )
        .path(
            "/api/v1/me/tokens/{id}",
            PathItem::new(HttpMethod::Delete, delete_one),
        )
}

fn trace_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_traces", "traces", "List traces")
        .description(Some(EXCLUSIONS))
        .parameters(Some(with_query::<TraceListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("TracePage", "One page of traces."),
            READ_ERRORS,
        ))
        .build();
    let stream = operation("stream_traces", "traces", "Stream traces")
        .description(Some(format!(
            "{EXCLUSIONS} Server-sent events: `trace` data is a TraceSummary; `resync` and `ping` data are `{{}}`. `ping` is sent every 15 seconds."
        )))
        .parameters(Some(with_query::<TraceStreamParams>(vec![project_param()])))
        .responses(responses(
            "200",
            sse("Live trace summaries."),
            READ_ERRORS,
        ))
        .build();
    let detail = operation("get_trace", "traces", "Get a trace")
        .description(Some(
            "Spans are a flat pre-order list: roots first, children sorted by start time then span id. `depth` is 0 at a root. `detached` is true when the parent is missing or the span sits in a parent cycle. At most 20000 spans; `truncated` is true when more exist.",
        ))
        .parameters(Some(with_query::<TraceDetailParams>(vec![
            project_param(),
            path_ref("trace_id", "TraceId", "Trace id, 16 bytes as hex."),
        ])))
        .responses(responses(
            "200",
            json_of::<TraceDetail>("The trace and its span tree."),
            READ_ERRORS,
        ))
        .build();
    let span_stream = operation("stream_trace", "traces", "Stream a trace")
        .description(Some(
            "Server-sent events: `span` data is a SpanSummary; `resync` and `ping` data are `{}`. `ping` is sent every 15 seconds. The stream closes after 10 minutes idle.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_ref("trace_id", "TraceId", "Trace id, 16 bytes as hex."),
        ]))
        .responses(responses(
            "200",
            sse("Live spans of one trace."),
            QUERY_ERRORS,
        ))
        .build();
    let span = operation("get_span", "spans", "Get a span")
        .parameters(Some(vec![
            project_param(),
            path_ref("trace_id", "TraceId", "Trace id, 16 bytes as hex."),
            path_ref("span_id", "SpanId", "Span id, 8 bytes as hex."),
        ]))
        .responses(responses(
            "200",
            json_of::<SpanDetail>("The span, including payloads."),
            READ_ERRORS,
        ))
        .build();
    let spans = operation("list_spans", "spans", "List spans")
        .description(Some(EXCLUSIONS))
        .parameters(Some(with_query::<SpanListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("SpanPage", "One page of spans."),
            READ_ERRORS,
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/traces"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/traces/stream"),
            PathItem::new(HttpMethod::Get, stream),
        )
        .path(
            format!("{PROJECT}/traces/{{trace_id}}"),
            PathItem::new(HttpMethod::Get, detail),
        )
        .path(
            format!("{PROJECT}/traces/{{trace_id}}/stream"),
            PathItem::new(HttpMethod::Get, span_stream),
        )
        .path(
            format!("{PROJECT}/traces/{{trace_id}}/spans/{{span_id}}"),
            PathItem::new(HttpMethod::Get, span),
        )
        .path(
            format!("{PROJECT}/spans"),
            PathItem::new(HttpMethod::Get, spans),
        )
}

fn session_paths(paths: PathsBuilder) -> PathsBuilder {
    let sessions = operation("list_sessions", "sessions", "List sessions")
        .description(Some(EXCLUSIONS))
        .parameters(Some(with_query::<SessionListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("SessionPage", "One page of sessions."),
            READ_ERRORS,
        ))
        .build();
    let session = operation("get_session", "sessions", "Get a session")
        .description(Some(format!(
            "{EXCLUSIONS} Turns are the session's traces by start time, at most 1000. `input` and `output` are the root span's payloads, omitted when the trace has no root."
        )))
        .parameters(Some(with_query::<ExclusionParams>(vec![
            project_param(),
            path_string("session_id", "Session id."),
        ])))
        .responses(responses(
            "200",
            json_of::<SessionDetail>("The session and its turns."),
            READ_ERRORS,
        ))
        .build();
    let users = operation("list_users", "users", "List users")
        .description(Some(format!(
            "{EXCLUSIONS} The filter runs on the `traces` view before traces are grouped by user."
        )))
        .parameters(Some(with_query::<UserListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("EndUserPage", "One page of end users."),
            READ_ERRORS,
        ))
        .build();
    let user = operation("get_user", "users", "Get a user")
        .description(Some(format!(
            "{EXCLUSIONS} Sessions are the user's 100 most recent."
        )))
        .parameters(Some(with_query::<UserDetailParams>(vec![
            project_param(),
            path_string("user_id", "End-user id."),
        ])))
        .responses(responses(
            "200",
            json_of::<EndUserDetail>("The user and recent sessions."),
            READ_ERRORS,
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/sessions"),
            PathItem::new(HttpMethod::Get, sessions),
        )
        .path(
            format!("{PROJECT}/sessions/{{session_id}}"),
            PathItem::new(HttpMethod::Get, session),
        )
        .path(
            format!("{PROJECT}/users"),
            PathItem::new(HttpMethod::Get, users),
        )
        .path(
            format!("{PROJECT}/users/{{user_id}}"),
            PathItem::new(HttpMethod::Get, user),
        )
}

fn score_paths(paths: PathsBuilder) -> PathsBuilder {
    let scores = operation("list_scores", "scores", "List scores")
        .parameters(Some(with_query::<ScoreListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("ScorePage", "One page of scores."),
            READ_ERRORS,
        ))
        .build();
    let create = operation("create_scores", "scores", "Write scores")
        .description(Some(
            "Writes 1–1000 scores in one batch. The answer has one id per item, in input order. With a session or a personal access token, a `score_id` that names another user's `human` score is refused with 403 unless the caller is a project admin.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("ScoreBatch")))
        .responses(responses(
            "200",
            json_of::<ScoreWriteResult>("The id of each written score, in input order."),
            &[400, 401, 402, 403, 404, 413, 429, 503],
        ))
        .build();
    let delete = operation("delete_score", "scores", "Delete a score")
        .description(Some(
            "Appends a deleted version of the score. Its `created_at` is kept. With a session or a personal access token, another user's `human` score can be deleted only by a project admin (403).",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("score_id", "Score id."),
        ]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[400, 401, 403, 404, 429, 503],
        ))
        .build();
    let key_create = operation(
        "create_key_scores",
        "scores",
        "Write scores with an ingest key",
    )
    .description(Some(
        "Writes 1–1000 scores in one batch. The answer has one id per item, in input order. The project comes from the `sk` key, which needs the `ingest` scope.",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "bearer",
        [] as [&str; 0],
    )]))
    .request_body(Some(json_body("ScoreBatch")))
    .responses(responses(
        "200",
        json_of::<ScoreWriteResult>("The id of each written score, in input order."),
        &[400, 401, 402, 403, 404, 413, 429, 503],
    ))
    .build();
    let feedback = operation("post_feedback", "scores", "Send end-user feedback")
        .description(Some(
            "Needs a `pk` key with the `feedback` scope. CORS allows any origin. At most 4 KiB, 10 requests per second per key and client IP.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "bearer",
            [] as [&str; 0],
        )]))
        .request_body(Some(json_body("FeedbackRequest")))
        .responses(responses(
            "200",
            json_of::<FeedbackResult>("The stored feedback score."),
            &[400, 401, 402, 403, 404, 413, 429],
        ))
        .build();
    let schema = operation("get_schema", "schema", "Get the schema")
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_of::<SchemaResponse>(
                "Views and functions the filter builder and SQL console show.",
            ),
            AUTH_ERRORS,
        ))
        .build();
    let metrics = operation("get_metrics", "metrics", "Get metrics")
        .description(Some(format!(
            "{EXCLUSIONS} Series have one point per bucket. Latencies and TTFT are nanoseconds. Costs are USD. Results are cached for 10 seconds. The evaluation cost series always counts evaluation traces (spec 05 § Dashboard metrics)."
        )))
        .parameters(Some(with_query::<MetricsParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_of::<MetricsResponse>("Dashboard metrics."),
            READ_ERRORS,
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/scores"),
            PathItem::new(HttpMethod::Get, scores),
        )
        .path(
            format!("{PROJECT}/scores"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/scores/{{score_id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            "/api/v1/scores".to_owned(),
            PathItem::new(HttpMethod::Post, key_create),
        )
        .path(
            "/api/public/feedback".to_owned(),
            PathItem::new(HttpMethod::Post, feedback),
        )
        .path(
            format!("{PROJECT}/schema"),
            PathItem::new(HttpMethod::Get, schema),
        )
        .path(
            format!("{PROJECT}/metrics"),
            PathItem::new(HttpMethod::Get, metrics),
        )
}

fn sql_paths(paths: PathsBuilder) -> PathsBuilder {
    let run = operation("run_sql", "sql", "Run a SQL query")
        .description(Some(
            "Runs one read-only statement over the project's `spans`, `traces`, `sessions` and `scores` views (spec 05 § SQL console). `from` and `to` window `spans` and `traces` on `start_time` and `scores` on `created_at`. Without them the query reads all history. At most 10,000 rows; `truncated` says when more matched. SQL errors are `invalid_filter` on the `query` field, with a `position` when one is known. Exports stream; an error before the first row is a problem document, a later one cuts the transfer. `timeout_ms` bounds the whole export.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("SqlQueryRequest")))
        .responses(responses(
            "200",
            sql_result(),
            &[400, 401, 403, 404, 413, 415, 422, 429, 503],
        ))
        .build();
    paths.path(
        format!("{PROJECT}/sql"),
        PathItem::new(HttpMethod::Post, run),
    )
}

fn evaluator_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Evaluator id.");
    let list = operation("list_evaluators", "evaluation", "List evaluators")
        .description(Some("Any project member can read."))
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named(
                "EvaluatorPage",
                "One page of evaluators. Sealed values are not included.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_evaluator", "evaluation", "Create an evaluator")
        .description(Some("Project members and above. API keys cannot call it."))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateEvaluatorRequest")))
        .responses(responses(
            "201",
            json_of::<Evaluator>("The created evaluator."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let get = operation("get_evaluator", "evaluation", "Get an evaluator")
        .description(Some("Any project member can read."))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "200",
            json_of::<Evaluator>("The evaluator."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let versions = operation("list_evaluator_versions", "evaluation", "List evaluator versions")
        .description(Some("Any project member can read. Newest first. The config is shown as `get_evaluator` shows it: sealed webhook values are never included. A version is kept until the evaluator is deleted."))
        .parameters(Some(with_query::<PageParams>(vec![project_param(), id.clone()])))
        .responses(responses(
            "200",
            json_named("EvaluatorVersionPage", "One page of versions, newest first."),
            QUERY_ERRORS,
        ))
        .build();
    let patch = operation("patch_evaluator", "evaluation", "Update an evaluator")
        .description(Some("Project members and above. API keys cannot call it."))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("UpdateEvaluatorRequest")))
        .responses(responses(
            "200",
            json_of::<Evaluator>("The updated evaluator."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let delete = operation("delete_evaluator", "evaluation", "Delete an evaluator")
        .description(Some(
            "Project members and above. API keys cannot call it. Refused with 409 while online rules use the evaluator; `errors` lists them.",
        ))
        .parameters(Some(vec![project_param(), id]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let templates = operation(
        "list_evaluator_templates",
        "evaluation",
        "List built-in judge templates",
    )
    .description(Some(
        "Any project member can read. The list is fixed; the UI copies a template into the editor.",
    ))
    .parameters(Some(vec![project_param()]))
    .responses(responses(
        "200",
        json_of::<EvaluatorTemplateList>("The built-in templates, in display order."),
        AUTH_ERRORS,
    ))
    .build();
    let test = operation("post_evaluator_test", "evaluation", "Run an evaluator once")
        .description(Some(
            "Project members and above. API keys cannot call it. Runs synchronously, writes the judge trace (environment `cetra-eval`) and the judge usage, and writes no score. Shares the 10 per minute per user limit with connection tests.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("EvaluatorTestRequest")))
        .responses(responses(
            "200",
            json_of::<EvaluatorTestResult>(
                "The outcome. An evaluator failure is ok: false, not an error status.",
            ),
            &[400, 401, 403, 404, 429, 503],
        ))
        .build();
    let stats = operation("get_evaluator_stats", "evaluation", "Get evaluator stats")
        .description(Some("Any project member can read. Jobs, costs, latencies and scores cover the last 7 UTC days (finished jobs are kept 7 days). `experiments_with_score` lists experiments that have a score named like the evaluator's output score in the last 30 days (D-I-48). Not cached."))
        .parameters(Some(with_query::<EvaluatorStatsParams>(vec![project_param()])))
        .responses(responses("200", json_of::<EvaluatorStatsResponse>("One entry per evaluator."), READ_ERRORS))
        .build();
    paths
        .path(
            format!("{PROJECT}/evaluators"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/evaluators/stats"),
            PathItem::new(HttpMethod::Get, stats),
        )
        .path(
            format!("{PROJECT}/evaluators"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/evaluators/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/evaluators/{{id}}/versions"),
            PathItem::new(HttpMethod::Get, versions),
        )
        .path(
            format!("{PROJECT}/evaluators/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/evaluators/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/evaluator-templates"),
            PathItem::new(HttpMethod::Get, templates),
        )
        .path(
            format!("{PROJECT}/evaluators/test"),
            PathItem::new(HttpMethod::Post, test),
        )
}

fn online_rule_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Online rule id.");
    let list = operation("list_online_rules", "evaluation", "List online rules")
        .description(Some(
            "Any project member can read. `org_enabled_rules` counts the enabled rules of the whole org (D-I-32).",
        ))
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("OnlineRulePage", "One page of online rules."),
            QUERY_ERRORS,
        ))
        .build();
    let stats = operation("get_online_rule_stats", "evaluation", "Get online rule stats")
        .description(Some("Any project member can read. Jobs and scores cover the last 24 hours, per hour: the finished jobs (`done`, `failed`, `skipped`) by finished time, and the live scores of each rule read with its evaluator's current output type (D-I-50). One entry per rule, none left out. Not cached."))
        .parameters(Some(vec![project_param()]))
        .responses(responses("200", json_of::<OnlineRuleStatsResponse>("One entry per online rule."), READ_ERRORS))
        .build();
    let create = operation("post_online_rule", "evaluation", "Create an online rule")
        .description(Some(
            "Project members and above. API keys cannot call it. 403 `plan_limit` when the org is at its active-rule limit.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateOnlineRuleRequest")))
        .responses(responses(
            "201",
            json_of::<OnlineRule>("The created rule."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let get = operation("get_online_rule", "evaluation", "Get an online rule")
        .description(Some("Any project member can read."))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "200",
            json_of::<OnlineRule>("The rule."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let patch = operation("patch_online_rule", "evaluation", "Update an online rule")
        .description(Some(
            "Project members and above. API keys cannot call it. `target` and `evaluator_id` cannot change. Enabling a rule checks the active-rule limit.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("UpdateOnlineRuleRequest")))
        .responses(responses(
            "200",
            json_of::<OnlineRule>("The updated rule."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let delete = operation("delete_online_rule", "evaluation", "Delete an online rule")
        .description(Some(
            "Project members and above. API keys cannot call it. Queued jobs of the rule are deleted with it.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let preview = operation(
        "post_online_rule_preview",
        "evaluation",
        "Count what a rule's filter matches",
    )
    .description(Some(
        "Project members and above. API keys cannot call it. Counts the traces or spans the filter matches in the window (default: the last 24 hours, at most 30 days), at most 10 000, leaving out evaluation and experiment traces whatever the filter says (D-I-9).",
    ))
    .parameters(Some(vec![project_param()]))
    .request_body(Some(json_body("OnlineRulePreviewRequest")))
    .responses(responses(
        "200",
        json_of::<OnlineRulePreview>("What the filter matches."),
        READ_ERRORS,
    ))
    .build();
    let resume = operation(
        "post_online_rule_resume",
        "evaluation",
        "Resume an online rule",
    )
    .description(Some(
        "Project members and above. API keys cannot call it. Clears the rule's pause.",
    ))
    .parameters(Some(vec![project_param(), id]))
    .responses(responses(
        "200",
        json_of::<OnlineRule>("The rule, not paused."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/online-rules"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/online-rules/stats"),
            PathItem::new(HttpMethod::Get, stats),
        )
        .path(
            format!("{PROJECT}/online-rules"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/online-rules/preview"),
            PathItem::new(HttpMethod::Post, preview),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}/resume"),
            PathItem::new(HttpMethod::Post, resume),
        )
}

fn eval_job_paths(paths: PathsBuilder) -> PathsBuilder {
    let job_id = path_uuid("id", "Eval job id.");
    let list = operation("list_eval_jobs", "evaluation", "List eval jobs")
        .description(Some(
            "Any project member can read. Newest first. Jobs are kept 7 days after they finish.",
        ))
        .parameters(Some(with_query::<EvalJobListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("EvalJobPage", "One page of eval jobs."),
            QUERY_ERRORS,
        ))
        .build();
    let get = operation("get_eval_job", "evaluation", "Get an eval job")
        .description(Some("Any project member can read."))
        .parameters(Some(vec![project_param(), job_id]))
        .responses(responses(
            "200",
            json_of::<EvalJob>("The job."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let evaluator_id = path_uuid("id", "Evaluator id.");
    let run = operation(
        "post_evaluator_run",
        "evaluation",
        "Run an evaluator on one trace",
    )
    .description(Some(
        "Project members and above. API keys cannot call it. Queues one run at the highest priority and answers the job; poll `GET /eval-jobs/{id}`. Shares the 10 per minute per user limit with test runs.",
    ))
    .parameters(Some(vec![project_param(), evaluator_id]))
    .request_body(Some(json_body("RunEvaluatorRequest")))
    .responses(responses(
        "202",
        json_of::<EvalJob>("The queued job."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/eval-jobs"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/eval-jobs/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/evaluators/{{id}}/run"),
            PathItem::new(HttpMethod::Post, run),
        )
}

fn backfill_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Online rule id.");
    let list = operation(
        "list_backfills",
        "evaluation",
        "List an online rule's backfills",
    )
    .description(Some(
        "Any project member can read. The newest 50 backfills of the rule; the list is not paged.",
    ))
    .parameters(Some(vec![project_param(), id.clone()]))
    .responses(responses(
        "200",
        json_named(
            "BackfillPage",
            "The rule's latest 50 backfills, newest first.",
        ),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let estimate = operation(
        "post_backfill_estimate",
        "evaluation",
        "Estimate an online rule backfill",
    )
    .description(Some(
        "Project members and above. API keys cannot call it. Counts the rule's targets in the window (at most `max_targets`, newest first, evaluation and experiment traces left out), how many the sample rate keeps, and, for an LLM judge, the tokens and cost they would use. Targets the rule already scored are counted; the run skips them (D-I-38, D-I-40).",
    ))
    .parameters(Some(vec![project_param(), id.clone()]))
    .request_body(Some(json_body("CreateBackfillRequest")))
    .responses(responses(
        "200",
        json_of::<BackfillEstimate>("What the backfill would read and cost."),
        READ_ERRORS,
    ))
    .build();
    let create = operation("post_backfill", "evaluation", "Start an online rule backfill")
        .description(Some(
            "Project members and above. API keys cannot call it. Starts a backfill: up to `max_targets` targets of the window, newest first, are sampled and queued at the lowest priority, skipping targets the rule already scored. 409 when the rule is disabled or paused, or already has a running backfill (D-I-25).",
        ))
        .parameters(Some(vec![project_param(), id]))
        .request_body(Some(json_body("CreateBackfillRequest")))
        .responses(responses(
            "202",
            json_of::<Backfill>("The started backfill."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/online-rules/{{id}}/backfills"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}/backfills"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/online-rules/{{id}}/backfills/estimate"),
            PathItem::new(HttpMethod::Post, estimate),
        )
}

fn model_price_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Price row id.");
    let read = "Any project member can read; a `read` API key too.";
    let write = "Project admins. API keys cannot call it. A PATCH replaces the whole row. A write that would leave a budgeted online rule without a price is 409.";
    let list = operation("list_models", "model_prices", "List model prices")
        .description(Some(read))
        .parameters(Some(with_query::<ModelPriceListParams>(vec![
            project_param(),
        ])))
        .responses(responses(
            "200",
            json_named(
                "ModelPricePage",
                "One page of the project's effective price table.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let resolve = operation(
        "resolve_model_price",
        "model_prices",
        "Resolve the price of a model",
    )
    .description(Some(read))
    .parameters(Some(with_query::<ModelPriceResolveParams>(vec![
        project_param(),
    ])))
    .responses(responses(
        "200",
        json_of::<ModelPriceResolution>("The row that prices the model now, or none."),
        QUERY_ERRORS,
    ))
    .build();
    let create = operation(
        "post_model_price",
        "model_prices",
        "Create a project model price",
    )
    .description(Some(write))
    .parameters(Some(vec![project_param()]))
    .request_body(Some(json_body("ModelPriceInput")))
    .responses(responses(
        "201",
        json_of::<ModelPrice>("The created price."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let patch = operation(
        "patch_model_price",
        "model_prices",
        "Replace a project model price",
    )
    .description(Some(write))
    .parameters(Some(vec![project_param(), id.clone()]))
    .request_body(Some(json_body("ModelPriceInput")))
    .responses(responses(
        "200",
        json_of::<ModelPrice>("The replaced price."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let delete = operation(
        "delete_model_price",
        "model_prices",
        "Delete a project model price",
    )
    .description(Some(write))
    .parameters(Some(vec![project_param(), id]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Deleted.").build(),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/models"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/models/resolve"),
            PathItem::new(HttpMethod::Get, resolve),
        )
        .path(
            format!("{PROJECT}/models"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/models/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/models/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
}

fn org_model_price_paths(paths: PathsBuilder) -> PathsBuilder {
    let org = path_string("org", "Org UUID or slug.");
    let id = path_uuid("id", "Price row id.");
    let read = "Any org member can read.";
    let write = "Org admins and owners. A PATCH replaces the whole row. A write that would leave a budgeted online rule of any project of the org without a price is 409.";
    let list = operation(
        "list_org_model_prices",
        "model_prices",
        "List org model prices",
    )
    .description(Some(read))
    .parameters(Some(vec![org.clone()]))
    .responses(responses(
        "200",
        json_named(
            "ModelPricePage",
            "The org's own price rows (at most 500), without paging.",
        ),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let create = operation(
        "post_org_model_price",
        "model_prices",
        "Create an org model price",
    )
    .description(Some(write))
    .parameters(Some(vec![org.clone()]))
    .request_body(Some(json_body("ModelPriceInput")))
    .responses(responses(
        "201",
        json_of::<ModelPrice>("The created price."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let patch = operation(
        "patch_org_model_price",
        "model_prices",
        "Replace an org model price",
    )
    .description(Some(write))
    .parameters(Some(vec![org.clone(), id.clone()]))
    .request_body(Some(json_body("ModelPriceInput")))
    .responses(responses(
        "200",
        json_of::<ModelPrice>("The replaced price."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let delete = operation(
        "delete_org_model_price",
        "model_prices",
        "Delete an org model price",
    )
    .description(Some(write))
    .parameters(Some(vec![org, id]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Deleted.").build(),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    paths
        .path(
            "/api/v1/orgs/{org}/models",
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            "/api/v1/orgs/{org}/models",
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            "/api/v1/orgs/{org}/models/{id}",
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            "/api/v1/orgs/{org}/models/{id}",
            PathItem::new(HttpMethod::Delete, delete),
        )
}

const DATASET_READ: &str = "Any project member, or an API key with `read`.";
const DATASET_WRITE: &str = "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5).";

fn dataset_paths(paths: PathsBuilder) -> PathsBuilder {
    let dataset = path_string("dataset", "Dataset id, or its name.");
    let list = operation("list_datasets", "datasets", "List datasets")
        .description(Some(DATASET_READ))
        .parameters(Some(with_query::<DatasetListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("DatasetPage", "One page of datasets."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_dataset", "datasets", "Create a dataset")
        .description(Some(DATASET_WRITE))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateDatasetRequest")))
        .responses(responses(
            "201",
            json_of::<Dataset>("The created dataset."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let get = operation("get_dataset", "datasets", "Get a dataset")
        .description(Some(DATASET_READ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .responses(responses(
            "200",
            json_of::<Dataset>("The dataset."),
            &[401, 403, 404, 429],
        ))
        .build();
    let patch = operation("patch_dataset", "datasets", "Update a dataset")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Setting a schema validates every live item first; up to 20 failures are listed (D-J-2).",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("UpdateDatasetRequest")))
        .responses(responses(
            "200",
            json_of::<Dataset>("The updated dataset."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let delete = operation("delete_dataset", "datasets", "Delete a dataset")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Experiments keep their results; their dataset link is cleared.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[401, 403, 404, 429],
        ))
        .build();
    let item = path_uuid("item_id", "Item id, stable across versions.");
    let version_list = operation(
        "list_dataset_versions",
        "datasets",
        "List the versions of a dataset",
    )
    .description(Some(DATASET_READ))
    .parameters(Some(with_query::<DatasetVersionListParams>(vec![
        project_param(),
        dataset.clone(),
    ])))
    .responses(responses(
        "200",
        json_named("DatasetVersionPage", "One page of versions, newest first."),
        QUERY_ERRORS,
    ))
    .build();
    let item_list = operation(
        "list_dataset_items",
        "datasets",
        "List the items of a version",
    )
    .description(Some(DATASET_READ))
    .parameters(Some(with_query::<DatasetItemListParams>(vec![
        project_param(),
        dataset.clone(),
    ])))
    .responses(responses(
        "200",
        json_named("DatasetItemPage", "One page of items, by item id."),
        QUERY_ERRORS,
    ))
    .build();
    let item_add = operation("post_dataset_items", "datasets", "Add items")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Creates one new version. A batch with any failing item writes nothing; the error lists the failing items.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("AddItemsRequest")))
        .responses(responses(
            "201",
            json_of::<AddItemsResponse>("The new version and the new item ids."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let item_get = operation("get_dataset_item", "datasets", "Get an item")
        .description(Some(DATASET_READ))
        .parameters(Some(with_query::<DatasetItemParams>(vec![
            project_param(),
            dataset.clone(),
            item.clone(),
        ])))
        .responses(responses(
            "200",
            json_of::<DatasetItem>("The item's revision in the version."),
            QUERY_ERRORS,
        ))
        .build();
    let item_patch = operation("patch_dataset_item", "datasets", "Edit an item")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Creates one new version. An edit that changes nothing creates none and answers the item as it is (D-J-3).",
        ))
        .parameters(Some(vec![project_param(), dataset.clone(), item.clone()]))
        .request_body(Some(json_body("UpdateItemRequest")))
        .responses(responses(
            "200",
            json_of::<DatasetItem>("The item after the edit."),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    let item_delete = operation("delete_dataset_item", "datasets", "Delete an item")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Creates one new version in which the item is gone; older versions still hold it.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone(), item]))
        .responses(responses(
            "200",
            json_of::<DatasetVersion>("The new version."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let item_from_trace = operation(
        "post_dataset_item_from_trace",
        "datasets",
        "Add an item copied from a trace or a span",
    )
    .description(Some(
        "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-8). Creates one new version holding one item copied from the trace root or from `span_id`; the request's fields replace the copied values. A trace that a pending deletion covers answers 404 and writes nothing (D-J-9).",
    ))
    .parameters(Some(vec![project_param(), dataset.clone()]))
    .request_body(Some(json_body("FromTraceRequest")))
    .responses(responses(
        "201",
        json_of::<DatasetItem>("The new item."),
        &[400, 401, 403, 404, 409, 429, 503],
    ))
    .build();
    let import_preview = operation(
        "post_dataset_import_preview",
        "datasets",
        "Preview the head of an import file",
    )
    .description(Some(
        "Any project reader. Shows the columns and the first rows of a CSV or JSONL file and writes nothing (D-J-6).",
    ))
    .parameters(Some(with_query::<PreviewParams>(vec![
        project_param(),
        dataset.clone(),
    ])))
    .request_body(Some(import_preview_body()))
    .responses(responses(
        "200",
        json_of::<ImportPreview>("The columns and the first rows."),
        &[400, 401, 403, 404, 413, 429],
    ))
    .build();
    let export = operation("get_dataset_export", "datasets", "Export a dataset version")
        .description(Some(DATASET_READ))
        .parameters(Some(with_query::<ExportParams>(vec![
            project_param(),
            dataset.clone(),
        ])))
        .responses(responses("200", export_result(), QUERY_ERRORS))
        .build();
    let import_op = operation(
        "post_dataset_import",
        "datasets",
        "Import items from a CSV or JSONL file",
    )
    .description(Some(
        "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-6). Creates one new version from the valid rows. A file with no valid row writes nothing; the error lists the rejected rows.",
    ))
    .parameters(Some(with_query::<ImportParams>(vec![
        project_param(),
        dataset,
    ])))
    .request_body(Some(import_upload_body()))
    .responses(responses(
        "201",
        json_of::<ImportResult>("The version the import made."),
        &[400, 401, 403, 404, 409, 413, 415, 429, 503],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/versions"),
            PathItem::new(HttpMethod::Get, version_list),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items"),
            PathItem::new(HttpMethod::Get, item_list),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items"),
            PathItem::new(HttpMethod::Post, item_add),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/{{item_id}}"),
            PathItem::new(HttpMethod::Get, item_get),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/{{item_id}}"),
            PathItem::new(HttpMethod::Patch, item_patch),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/{{item_id}}"),
            PathItem::new(HttpMethod::Delete, item_delete),
        )
        .path(
            format!("{PROJECT}/datasets"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/datasets"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/from-trace"),
            PathItem::new(HttpMethod::Post, item_from_trace),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/import/preview"),
            PathItem::new(HttpMethod::Post, import_preview),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/import"),
            PathItem::new(HttpMethod::Post, import_op),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/export"),
            PathItem::new(HttpMethod::Get, export),
        )
}

/// The body of an export: the items as JSONL or CSV, as a file download (D-J-7).
fn export_result() -> Response {
    ResponseBuilder::new()
        .description("The version's items, one per line (JSONL) or one per row (CSV), with `Content-Disposition: attachment`.")
        .content("application/x-ndjson", Content::new(Some(string_schema())))
        .content("text/csv", Content::new(Some(string_schema())))
        .build()
}

/// The body of the preview: the first bytes of the file, raw.
fn import_preview_body() -> utoipa::openapi::request_body::RequestBody {
    RequestBodyBuilder::new()
        .required(Some(Required::True))
        .description(Some("The first bytes of the file, at most 1 MiB."))
        .content(
            "application/octet-stream",
            Content::new(Some(binary_string())),
        )
        .build()
}

/// The body of an import: a `file` part and, for CSV, a `mapping` part.
fn import_upload_body() -> utoipa::openapi::request_body::RequestBody {
    let schema = ObjectBuilder::new()
        .property("file", binary_string())
        .property("mapping", Ref::from_schema_name("ImportMapping"))
        .required("file");
    RequestBodyBuilder::new()
        .required(Some(Required::True))
        .description(Some(
            "A file part (at most 100 MiB) and, for CSV, a mapping part.",
        ))
        .content("multipart/form-data", Content::new(Some(schema)))
        .build()
}

const EXPERIMENT_READ: &str = "Any project member, or an API key with `read`.";
const EXPERIMENT_WRITE: &str = "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5).";

fn experiment_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Experiment id.");
    let list = operation("list_experiments", "experiments", "List experiments")
        .description(Some(EXPERIMENT_READ))
        .parameters(Some(with_query::<ExperimentListParams>(vec![
            project_param(),
        ])))
        .responses(responses(
            "200",
            json_named("ExperimentPage", "One page of experiments, newest first."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_experiment", "experiments", "Create an experiment")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Pins the dataset's version and copies its items into the first page of the answer; the SDK pages the rest from the items list.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateExperimentRequest")))
        .responses(responses(
            "201",
            json_of::<CreateExperimentResponse>(
                "The experiment and the first page of its pinned items.",
            ),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    let get = operation("get_experiment", "experiments", "Get an experiment")
        .description(Some(EXPERIMENT_READ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "200",
            json_of::<Experiment>("The experiment."),
            QUERY_ERRORS,
        ))
        .build();
    let delete = operation("delete_experiment", "experiments", "Delete an experiment")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5). Deletes the experiment and its results; the traces and scores in storage stay.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[401, 403, 404, 429],
        ))
        .build();
    let results = operation("post_experiment_results", "experiments", "Post results")
        .description(Some(EXPERIMENT_WRITE))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("ExperimentResultsRequest")))
        .responses(responses(
            "200",
            json_of::<ExperimentResultsResponse>("The rows stored and skipped."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let experiment_results = operation("get_experiment_results", "experiments", "Get results")
        .description(Some(EXPERIMENT_READ))
        .parameters(Some(with_query::<ResultsParams>(vec![
            project_param(),
            id.clone(),
        ])))
        .responses(responses(
            "200",
            json_of::<ExperimentResults>("The summary of the experiment and one page of its rows."),
            READ_ERRORS,
        ))
        .build();
    let compare = operation("compare_experiments", "experiments", "Compare experiments")
        .description(Some(
            "Project members, a `read` personal access token or a `read` key (D-J-5). Two to four experiments of one dataset, the first the baseline: their summaries, the deltas of each candidate to the baseline, and one page of items with a cell per experiment.",
        ))
        .parameters(Some(with_query::<CompareParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_of::<ExperimentComparison>("The comparison and one page of its items."),
            READ_ERRORS,
        ))
        .build();
    let summaries = operation(
        "get_experiment_summaries",
        "experiments",
        "Get experiment summaries",
    )
    .description(Some(EXPERIMENT_READ))
    .parameters(Some(with_query::<SummariesParams>(vec![project_param()])))
    .responses(responses(
        "200",
        json_of::<ExperimentSummaries>(
            "The summaries of the experiments found, in request order, and the ids too large to summarize.",
        ),
        READ_ERRORS,
    ))
    .build();
    let finish = operation(
        "post_experiment_finish",
        "experiments",
        "Finish an experiment",
    )
    .description(Some(EXPERIMENT_WRITE))
    .parameters(Some(vec![project_param(), id]))
    .request_body(Some(json_body("FinishExperimentRequest")))
    .responses(responses(
        "200",
        json_of::<Experiment>("The experiment after the finish."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/experiments"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/experiments"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/experiments/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/experiments/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/experiments/{{id}}/items"),
            PathItem::new(HttpMethod::Post, results),
        )
        .path(
            format!("{PROJECT}/experiments/{{id}}/finish"),
            PathItem::new(HttpMethod::Post, finish),
        )
        .path(
            format!("{PROJECT}/experiments/{{id}}/results"),
            PathItem::new(HttpMethod::Get, experiment_results),
        )
        .path(
            format!("{PROJECT}/experiments/compare"),
            PathItem::new(HttpMethod::Get, compare),
        )
        .path(
            format!("{PROJECT}/experiments/summaries"),
            PathItem::new(HttpMethod::Get, summaries),
        )
}

fn score_definition_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Score definition id.");
    let list = operation(
        "list_score_definitions",
        "score_definitions",
        "List score definitions",
    )
    .description(Some("Any project member can read."))
    .parameters(Some(with_query::<ScoreDefinitionListParams>(vec![
        project_param(),
    ])))
    .responses(responses(
        "200",
        json_named("ScoreDefinitionPage", "One page of score definitions."),
        QUERY_ERRORS,
    ))
    .build();
    let create = operation(
        "post_score_definition",
        "score_definitions",
        "Create a score definition",
    )
    .description(Some("Project members and above. API keys cannot call it."))
    .parameters(Some(vec![project_param()]))
    .request_body(Some(json_body("CreateScoreDefinitionRequest")))
    .responses(responses(
        "201",
        json_of::<ScoreDefinition>("The created score definition."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let patch = operation(
        "patch_score_definition",
        "score_definitions",
        "Update or restore a score definition",
    )
    .description(Some("Project members and above. API keys cannot call it."))
    .parameters(Some(vec![project_param(), id.clone()]))
    .request_body(Some(json_body("UpdateScoreDefinitionRequest")))
    .responses(responses(
        "200",
        json_of::<ScoreDefinition>("The updated score definition."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let delete = operation(
        "delete_score_definition",
        "score_definitions",
        "Archive a score definition",
    )
    .description(Some(
        "Project members and above. API keys cannot call it. Archives the definition; score writes stop being checked against it.",
    ))
    .parameters(Some(vec![project_param(), id]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Archived.").build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/score-definitions"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/score-definitions"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/score-definitions/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/score-definitions/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
}

fn saved_view_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_saved_views", "saved_views", "List saved views")
        .description(Some(
            "Views the caller can see: their own and every shared view. An API key sees shared views. Ordered by id (creation order).",
        ))
        .parameters(Some(with_query::<SavedViewListParams>(vec![
            project_param(),
        ])))
        .responses(responses(
            "200",
            json_named("SavedViewPage", "One page of saved views."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("create_saved_view", "saved_views", "Create a saved view")
        .description(Some(
            "Creates a view owned by the caller. Sharing needs role member. At most 1,000 views per user and project (409). A session or a read-write personal access token; API keys cannot write saved views.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateSavedView")))
        .responses(responses(
            "201",
            json_of::<SavedView>("The created saved view."),
            &[400, 401, 403, 404, 409, 413, 415, 429],
        ))
        .build();
    let get = operation("get_saved_view", "saved_views", "Get a saved view")
        .description(Some(
            "One view by id, with the same visibility as the list: the caller's own and shared views (an API key sees shared views). Any other id is 404. Use it to open a view that is not on the first pages of the list.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Saved view id."),
        ]))
        .responses(responses(
            "200",
            json_of::<SavedView>("The saved view."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let update = operation("update_saved_view", "saved_views", "Update a saved view")
        .description(Some(
            "The owner changes name, state, shared and is_default; sharing, or editing a shared view, needs role member. A project admin may also unshare another user's shared view. Setting is_default clears the other default of the same scope: the caller's private views of that kind, or the project's shared views of that kind.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Saved view id."),
        ]))
        .request_body(Some(json_body("UpdateSavedView")))
        .responses(responses(
            "200",
            json_of::<SavedView>("The updated saved view."),
            &[400, 401, 403, 404, 409, 413, 415, 429],
        ))
        .build();
    let delete = operation("delete_saved_view", "saved_views", "Delete a saved view")
        .description(Some(
            "The owner deletes their view, at any role. A project admin may also delete another user's shared view.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Saved view id."),
        ]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            QUERY_ERRORS,
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/saved-views"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/saved-views"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/saved-views/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/saved-views/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/saved-views/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
}

fn deletion_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_deletions", "deletions", "List deletions")
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("DeletionPage", "One page of deletions, newest first."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("create_deletion", "deletions", "Request a deletion")
        .description(Some(
            "Kinds `trace_ids` (an `sk` key with `write` may use it), `filter` and `user` (project admins). Answers 202; the deletion is resolved and applied in the background. A filter matching more than 100,000 traces is 422 `too_many_traces`.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateDeletion")))
        .responses(responses(
            "202",
            json_of::<Deletion>("The deletion, in status resolving."),
            &[400, 401, 403, 404, 409, 422, 429, 503],
        ))
        .build();
    let preview = operation("preview_deletion", "deletions", "Count a filter deletion")
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("DeletionPreviewRequest")))
        .responses(responses(
            "200",
            json_of::<DeletionPreview>("How many traces would be deleted."),
            &[400, 401, 403, 404, 422, 429, 503],
        ))
        .build();
    let get = operation("get_deletion", "deletions", "Get a deletion")
        .parameters(Some(vec![project_param(), path_uuid("id", "Deletion id.")]))
        .responses(responses(
            "200",
            json_of::<Deletion>("The deletion."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let list_org = operation(
        "list_org_deletions",
        "deletions",
        "List an organization's deletions",
    )
    .parameters(Some(with_query::<PageParams>(vec![path_string(
        "org",
        "Org UUID or slug.",
    )])))
    .responses(responses(
        "200",
        json_named("DeletionPage", "One page of deletions, newest first."),
        QUERY_ERRORS,
    ))
    .build();
    let mut deletions = PathItem::new(HttpMethod::Get, list);
    deletions.post = Some(create);
    paths
        .path(format!("{PROJECT}/deletions"), deletions)
        .path(
            "/api/v1/orgs/{org}/deletions",
            PathItem::new(HttpMethod::Get, list_org),
        )
        .path(
            format!("{PROJECT}/deletions/preview"),
            PathItem::new(HttpMethod::Post, preview),
        )
        .path(
            format!("{PROJECT}/deletions/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
}

fn channel_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_channels", "alerts", "List notification channels")
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_named("ChannelPage", "Every channel of the project, by name."),
            AUTH_ERRORS,
        ))
        .build();
    let create = operation("create_channel", "alerts", "Create a notification channel")
        .description(Some(
            "Needs role member. Email channels need SMTP. At most 20 channels per project (409). \
             Under open signup every email recipient must be a verified member of the org (400 on recipients).",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateChannelRequest")))
        .responses(responses(
            "201",
            json_of::<NotificationChannel>("The created channel."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let update = operation("update_channel", "alerts", "Update a notification channel")
        .description(Some(
            "Under open signup every email recipient must be a verified member of the org (400 on recipients).",
        ))
        .parameters(Some(vec![project_param(), path_uuid("id", "Channel id.")]))
        .request_body(Some(json_body("UpdateChannelRequest")))
        .responses(responses(
            "200",
            json_of::<NotificationChannel>("The updated channel."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let delete = operation("delete_channel", "alerts", "Delete a notification channel")
        .parameters(Some(vec![project_param(), path_uuid("id", "Channel id.")]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("Deleted. Rules that used it no longer do.")
                .build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let test = operation("test_channel", "alerts", "Send a test notification")
        .description(Some(
            "Sends one sample message now and reports the result. Nothing is stored. Needs role member. \
             At most 5 sends per minute per user and 5 per minute per channel (429, Retry-After). \
             Email sends count against the org's daily limit of 1000 messages; over it the result is ok false.",
        ))
        .parameters(Some(vec![project_param(), path_uuid("id", "Channel id.")]))
        .responses(responses(
            "200",
            json_of::<ChannelTestResult>("Whether the channel accepted the message."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/channels"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/channels"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/channels/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/channels/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/channels/{{id}}/test"),
            PathItem::new(HttpMethod::Post, test),
        )
}

fn alert_rule_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_alert_rules", "alerts", "List alert rules")
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_named("AlertRulePage", "Every rule of the project, by name."),
            AUTH_ERRORS,
        ))
        .build();
    let create = operation("create_alert_rule", "alerts", "Create an alert rule")
        .description(Some(
            "Needs role member. The filter is checked by the query layer (400 invalid_filter). At most 100 rules per project (409). An enabled rule beyond the org's plan limit is 403 plan_limit.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateAlertRuleRequest")))
        .responses(responses(
            "201",
            json_of::<AlertRule>("The created rule."),
            &[400, 401, 403, 404, 409, 429, 503],
        ))
        .build();
    let get = operation("get_alert_rule", "alerts", "Get an alert rule")
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Alert rule id."),
        ]))
        .responses(responses(
            "200",
            json_of::<AlertRule>("The rule and its state."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let update = operation("update_alert_rule", "alerts", "Update an alert rule")
        .description(Some(
            "A new condition, or a change of enabled, resets the state to ok without an event. Re-enabling a rule beyond the org's plan limit is 403 plan_limit.",
        ))
        .parameters(Some(vec![project_param(), path_uuid("id", "Alert rule id.")]))
        .request_body(Some(json_body("UpdateAlertRuleRequest")))
        .responses(responses(
            "200",
            json_of::<AlertRule>("The updated rule."),
            &[400, 401, 403, 404, 409, 429, 503],
        ))
        .build();
    let delete = operation("delete_alert_rule", "alerts", "Delete an alert rule")
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Alert rule id."),
        ]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("Deleted, with its events.")
                .build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let events = operation("list_alert_events", "alerts", "List a rule's events")
        .parameters(Some(with_query::<PageParams>(vec![
            project_param(),
            path_uuid("id", "Alert rule id."),
        ])))
        .responses(responses(
            "200",
            json_named(
                "AlertEventPage",
                "Events, newest first, with their deliveries.",
            ),
            QUERY_ERRORS,
        ))
        .build();
    let project_events = operation(
        "list_project_alert_events",
        "alerts",
        "List the project's alert events",
    )
    .description(Some(
        "Events of every rule of the project, newest first, with their deliveries.",
    ))
    .parameters(Some(with_query::<PageParams>(vec![project_param()])))
    .responses(responses(
        "200",
        json_named(
            "AlertEventPage",
            "Events of all the project's rules, newest first, with their deliveries.",
        ),
        QUERY_ERRORS,
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/alert-rules"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/alert-rules"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/alert-rules/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/alert-rules/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/alert-rules/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/alert-rules/{{id}}/events"),
            PathItem::new(HttpMethod::Get, events),
        )
        .path(
            format!("{PROJECT}/alert-events"),
            PathItem::new(HttpMethod::Get, project_events),
        )
}

fn media_paths(paths: PathsBuilder) -> PathsBuilder {
    let media = operation("get_media", "media", "Get a media blob")
        .description(Some(
            "Supports `Range`. A satisfiable range returns 206; an unsatisfiable range returns 416. `Cache-Control` is `private, immutable`. The response `Content-Type` is the blob's MIME type.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_sha256(),
            range_header(),
        ]))
        .responses(responses_with(
            [
                ("200", binary("The whole blob.")),
                ("206", binary("The requested byte range.")),
            ],
            &[401, 403, 404, 416, 429, 503],
        ))
        .build();
    paths.path(
        format!("{PROJECT}/media/{{sha256}}"),
        PathItem::new(HttpMethod::Get, media),
    )
}

const EXCLUSIONS: &str = concat!(
    "Evaluation traces (`environment = ",
    cetra_core::cetra_name!("-eval"),
    "`) and experiment traces are excluded unless `include_eval` or `include_experiments` is set."
);

fn operation(id: &str, tag_name: &str, summary: &str) -> OperationBuilder {
    OperationBuilder::new()
        .operation_id(Some(id))
        .tag(tag_name)
        .summary(Some(summary))
}

fn project_param() -> Parameter {
    path_string(
        "project",
        "Project UUID, URL-encoded org-slug/project-slug, or `current`.",
    )
}

fn path_string(name: &str, description: &str) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some(description))
        .schema(Some(string_schema()))
        .build()
}

fn query_string(name: &str, description: &str, required: bool) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Query)
        .required(if required {
            Required::True
        } else {
            Required::False
        })
        .description(Some(description))
        .schema(Some(string_schema()))
        .build()
}

fn redirect_to(description: &str) -> Response {
    ResponseBuilder::new().description(description).build()
}

fn path_ref(name: &str, schema_name: &str, description: &str) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some(description))
        .schema(Some(Ref::from_schema_name(schema_name)))
        .build()
}

fn path_uuid(name: &str, description: &str) -> Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some(description))
        .schema(Some(
            ObjectBuilder::new()
                .schema_type(Type::String)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Uuid))),
        ))
        .build()
}

fn path_sha256() -> Parameter {
    ParameterBuilder::new()
        .name("sha256")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("SHA-256 of the media blob, lowercase hex."))
        .schema(Some(
            ObjectBuilder::new()
                .schema_type(Type::String)
                .pattern(Some("^[0-9a-f]{64}$")),
        ))
        .build()
}

fn range_header() -> Parameter {
    ParameterBuilder::new()
        .name("Range")
        .parameter_in(ParameterIn::Header)
        .required(Required::False)
        .description(Some(
            "Optional byte range. A satisfiable range returns 206; an unsatisfiable range returns 416.",
        ))
        .schema(Some(string_schema()))
        .build()
}

fn string_schema() -> ObjectBuilder {
    ObjectBuilder::new().schema_type(Type::String)
}

fn with_query<T: IntoParams>(mut path_params: Vec<Parameter>) -> Vec<Parameter> {
    path_params.extend(T::into_params(|| Some(ParameterIn::Query)));
    path_params
}

fn json_of<T: ToSchema>(description: &str) -> Response {
    json_named(&T::name(), description)
}

fn json_named(name: &str, description: &str) -> Response {
    ResponseBuilder::new()
        .description(description)
        .content(
            "application/json",
            Content::new(Some(Ref::from_schema_name(name))),
        )
        .build()
}

fn json_body(name: &str) -> utoipa::openapi::request_body::RequestBody {
    RequestBodyBuilder::new()
        .required(Some(Required::True))
        .content(
            "application/json",
            Content::new(Some(Ref::from_schema_name(name))),
        )
        .build()
}

fn sse(description: &str) -> Response {
    ResponseBuilder::new()
        .description(description)
        .content("text/event-stream", Content::new(Some(string_schema())))
        .build()
}

fn binary(description: &str) -> Response {
    ResponseBuilder::new()
        .description(description)
        .content("application/octet-stream", media_bytes())
        .content("image/png", media_bytes())
        .build()
}

fn media_bytes() -> Content {
    Content::new(Some(binary_string().description(Some(
        "Bytes of the blob. The response Content-Type is the blob's MIME type.",
    ))))
}

/// `type: string, format: binary`.
fn binary_string() -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
}

/// The `200` of `run_sql`: the JSON result first, then the three export types (D-H-8).
fn sql_result() -> Response {
    ResponseBuilder::new()
        .description(
            "Columns and rows as JSON. With `Accept: text/csv`, `application/x-ndjson` or `application/vnd.apache.parquet`, the whole result in that format instead (at most 10,000,000 rows; `limit` is validated but not applied; exports need role member), with `Content-Disposition: attachment`. The first of those types in `Accept`, or `application/json`, decides; anything else is JSON.",
        )
        .content(
            "application/json",
            Content::new(Some(Ref::from_schema_name(SqlQueryResult::name()))),
        )
        .content("text/csv", Content::new(Some(string_schema())))
        .content("application/x-ndjson", Content::new(Some(string_schema())))
        .content(
            "application/vnd.apache.parquet",
            Content::new(Some(binary_string())),
        )
        .build()
}

fn responses(status: &str, success: Response, errors: &[u16]) -> Responses {
    responses_with([(status, success)], errors)
}

fn responses_with<const N: usize>(success: [(&str, Response); N], errors: &[u16]) -> Responses {
    let mut builder = ResponsesBuilder::new();
    for (status, response) in success {
        builder = builder.response(status, response);
    }
    for code in errors {
        builder = builder.response(code.to_string(), problem(*code));
    }
    builder.build()
}

fn problem(code: u16) -> Response {
    let description = match code {
        400 => "The request is malformed or a filter is invalid.",
        401 => "No credentials, or the credentials are invalid.",
        402 => {
            "The organization reached its monthly units cap (code quota_exceeded). Retry-After gives the seconds until it resets."
        }
        403 => {
            "The credentials do not allow this operation, or the organization's plan does not include it (code plan_required)."
        }
        404 => "The resource does not exist.",
        409 => "The request conflicts with the current state.",
        413 => "The request body is larger than the limit.",
        416 => "The Range header does not overlap the blob.",
        415 => "The request body is not JSON.",
        422 => {
            "The query took longer than its time limit, needed more memory or temporary disk space than allowed, a filter deletion matched more than 100,000 traces, or the owner cannot be that user (code owner_not_allowed)."
        }
        429 => "Too many requests.",
        503 => "The server is busy or a dependency is unavailable. Retry later.",
        _ => "Request failed.",
    };
    ResponseBuilder::new()
        .description(description)
        .content(
            "application/problem+json",
            Content::new(Some(Ref::from_schema_name(Problem::name()))),
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::collect_schemas;

    #[test]
    fn component_names_are_unique_and_roles_differ() {
        let set = collect_schemas();
        assert!(set.clash.is_none(), "schema clash: {:?}", set.clash);
        let message = serde_json::to_value(set.schemas.get("MessageRole").unwrap()).unwrap();
        let org = serde_json::to_value(set.schemas.get("OrgRole").unwrap()).unwrap();
        assert_ne!(message, org);
        assert!(message.to_string().contains("MessageRole") || message["type"] == "string");
        assert!(org["enum"].as_array().is_some());
    }
}
