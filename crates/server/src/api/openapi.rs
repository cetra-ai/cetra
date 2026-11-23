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

use super::audit::{
    AuditActor, AuditEvent, AuditExportFormat, AuditExportParams, AuditListParams, AuditOutcome,
    AuditRef, AuditRefStatus,
};
use super::account::{
    AuthConfig, ChangePasswordRequest, CreateOrgRequest, CreateProjectRequest,
    ForgotPasswordRequest, LoginRequest, Me, OrgMembership, Project, ResendVerificationRequest,
    ResetPasswordRequest, SetupRequest, SetupResponse, SignupRequest, SignupResponse,
    UpdateMeRequest, UpdateOrgRequest, UpdateProjectRequest, User, VerifyEmailRequest,
};
use super::admin::{
    AdminLicense, AdminResetLink, AdminSettings, AdminUser, CreateAdminUserRequest,
    CreatedAdminUser, InstallLicenseRequest, LicenseKeySource, UpdateAdminUserRequest,
};
use super::agent_context::AgentContextBody;
use super::alerts::{
    AlertCondition, AlertDelivery, AlertEvent, AlertEventKind, AlertMetric, AlertRule, AlertState,
    AlertWindow, ChannelDelivery, ChannelKind, ChannelTestResult, Comparator,
    CreateAlertRuleRequest, CreateChannelRequest, DeliveryStatus, LatencyTarget,
    NotificationChannel, ScoreAggregation, UpdateAlertRuleRequest, UpdateChannelRequest,
};
use super::annotation_queues::{
    AddQueueItems, AddedQueueItems, AnnotationQueue, AnnotationQueueItem,
    AnnotationQueueItemListParams, AnnotationQueueListParams, CreateAnnotationQueue,
    IssueQueueItemsRequest, NextQueueItem, QueueCounts, QueueItemAddedBy, QueueItemStatus,
    QueueItemTarget, QueueMatchingAdded, QueueMatchingPreview, QueueMatchingRequest, QueueTarget,
    SkipQueueItem, SkippedQueueItem, UpdateAnnotationQueue,
};
use super::assistant::{
    AssistantConversation, AssistantConversationDetail, AssistantDraftKind, AssistantFeedback,
    AssistantFeedbackRequest, AssistantMessage, AssistantMessageStatus, AssistantPart,
    AssistantRole, AssistantSettings, AssistantStatus, AssistantTestRequest, AssistantTurnRequest,
    AssistantUnavailable, PutAssistantSettings, UpdateAssistantConversationRequest,
};
use super::backfills::{Backfill, BackfillEstimate, BackfillStatus, CreateBackfillRequest};
use super::common::{ExclusionParams, Limit, Page, PageParams, Problem, SortOrder, TimeParam};
use super::connections::{
    ConnectionStatus, CreateLlmConnectionRequest, LlmAdapter, LlmConnection,
    LlmConnectionTestError, LlmConnectionTestResult, StructuredOutputMode, TestErrorClass,
    TestLlmConnectionRequest, UpdateLlmConnectionRequest,
};
use super::dashboards::{
    AggBody, BreakdownBody, Chart, ChartType, CreateDashboard, Dashboard, DashboardListParams,
    DashboardSummary, Definition, Layout, MeasureBody, SaveOverview, TotalRow, UpdateDashboard,
    Widget, WidgetKind, WidgetPoint, WidgetQueryBody, WidgetQueryRequest, WidgetQueryResponse,
    WidgetSeries, WidgetSeriesResponse, WidgetShape, WidgetSourceBody, WidgetTotalResponse,
};
use super::datasets::{
    AddItemsRequest, AddItemsResponse, ChangeLimit, CreateDatasetRequest, Dataset, DatasetItem,
    DatasetItemListParams, DatasetItemParams, DatasetListParams, DatasetTrend, DatasetVersion,
    DatasetVersionListParams, DeleteItemsRequest, EditItem, EditItemsRequest, EditItemsResponse,
    ExperimentScoreMean, ExperimentTrend, ExperimentTrendParams, ExperimentTrends, ExportFormat,
    ExportParams, FromTraceRequest, FromTracesRequest, FromTracesResult, ImportColumn,
    ImportFormat, ImportMapping, ImportParams, ImportPreview, ImportResult, ImportRowError,
    IssueDatasetItemsRequest, ItemChange, ItemChangeKind, ItemChangeListParams, NewDatasetItem,
    PreviewParams, RestoreRequest, SkippedTrace, TraceRef, UpdateDatasetRequest, UpdateItemRequest,
};
use super::deletions::{
    CreateDeletion, Deletion, DeletionPreview, DeletionPreviewRequest, DropParams,
};
use super::device::{
    DeviceApproveRequest, DeviceCodeRequest, DeviceCodeResponse, DeviceGrant, DeviceGrantProject,
    DevicePurpose, DeviceRequest, DeviceTokenRequest, InternalDeviceCode,
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
use super::facets::{Facet, FacetFieldRequest, FacetResult, FacetsRequest, FacetsResponse};
use super::internal::{
    CreateInternalOrgRequest, InternalDrop, InternalDropKind, InternalDropsQuery, InternalMember,
    InternalMembers, InternalMembersQuery, InternalOrg, InternalOwner, InternalUsageQuery,
    InternalUsageRow, InternalUserDeleted, InternalUserRef, MembershipLookup,
    MembershipLookupRequest, MembershipOrg,
};
use super::invites::{CreateInviteRequest, CreatedInvite, Invite, InvitePreview};
use super::issues::{
    CreateIssueSourceRequest, Issue, IssueCandidateCounts, IssueDay, IssueEvent, IssueEventPage,
    IssueExample, IssueListParams, IssueOccurrenceParams, IssueOccurrences, IssueRefit,
    IssueRefitPage, IssueSource, IssueSourceKind, IssueSourcePage, IssueState, MergeIssueRequest,
    UpdateIssueRequest, UpdateIssueSourceRequest,
};
use super::keys::{
    ApiKey as ProjectApiKey, CreateApiKeyRequest, CreatePersonalTokenRequest, CreatedApiKey,
    CreatedPersonalToken, PersonalToken,
};
use super::members::{OrgMember, ProjectMember, SetProjectOverrideRequest, UpdateOrgMemberRequest};
use super::metrics::{MetricsBucket, MetricsGroupBy};
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
use super::playground::{
    PlaygroundParams, PlaygroundResponseFormat, PlaygroundRunRequest, PlaygroundSource,
    PlaygroundTool,
};
use super::prompts::{
    CreatePromptRequest, CreateVersionRequest, Prompt, PromptKind, PromptLabel, PromptLabelEvent,
    PromptListParams, PromptMetrics, PromptMetricsParams, PromptScoreMean, PromptSettings,
    PromptVersion, PromptVersionMetrics, ResolveParams, SetLabelRequest, UpdatePromptRequest,
};
use super::saved_views::{CreateSavedView, SavedView, SavedViewListParams, UpdateSavedView};
use super::schema::SchemaResponse;
use super::scim::{
    CreateScimTokenRequest, CreatedScimToken, PutScimGroupMapping, ScimGroup, ScimGroupList,
    ScimGroupMapping, ScimProjectRole, ScimSettings, ScimToken, UpdateScimSettings,
};
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
    set.insert::<AuditOutcome>();
    set.insert::<AuditRefStatus>();
    set.insert::<AuditExportFormat>();
    set.insert::<AuditActor>();
    set.insert::<AuditRef>();
    set.insert::<AuditEvent>();
    set.insert_page::<AuditEvent>("AuditEventPage");
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
    set.insert::<PlaygroundRunRequest>();
    set.insert::<PlaygroundParams>();
    set.insert::<PlaygroundTool>();
    set.insert::<PlaygroundResponseFormat>();
    set.insert::<PlaygroundSource>();
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
    set.insert::<InstallLicenseRequest>();
    set.insert::<LicenseKeySource>();
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
    set.insert::<ScimSettings>();
    set.insert::<ScimToken>();
    set.insert::<UpdateScimSettings>();
    set.insert::<CreateScimTokenRequest>();
    set.insert::<CreatedScimToken>();
    set.insert::<ScimGroup>();
    set.insert::<ScimGroupMapping>();
    set.insert::<ScimProjectRole>();
    set.insert::<ScimGroupList>();
    set.insert::<PutScimGroupMapping>();
    set.insert::<SsoDiscoverRequest>();
    set.insert::<SsoDiscovery>();
    set.insert::<DevicePurpose>();
    set.insert::<DeviceCodeRequest>();
    set.insert::<DeviceCodeResponse>();
    set.insert::<DeviceTokenRequest>();
    set.insert::<DeviceGrantProject>();
    set.insert::<DeviceGrant>();
    set.insert::<DeviceRequest>();
    set.insert::<DeviceApproveRequest>();
    set.insert::<InternalDeviceCode>();
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
    set.insert::<AgentContextBody>();
    set.insert::<SchemaResponse>();
    set.insert::<SavedView>();
    set.insert::<CreateSavedView>();
    set.insert::<UpdateSavedView>();
    set.insert::<Dashboard>();
    set.insert::<DashboardSummary>();
    set.insert::<AnnotationQueue>();
    set.insert::<QueueTarget>();
    set.insert::<QueueCounts>();
    set.insert::<CreateAnnotationQueue>();
    set.insert::<UpdateAnnotationQueue>();
    set.insert::<AnnotationQueueItem>();
    set.insert::<QueueItemStatus>();
    set.insert::<QueueItemAddedBy>();
    set.insert::<QueueItemTarget>();
    set.insert::<AddQueueItems>();
    set.insert::<IssueQueueItemsRequest>();
    set.insert::<SkippedQueueItem>();
    set.insert::<AddedQueueItems>();
    set.insert::<QueueMatchingRequest>();
    set.insert::<QueueMatchingPreview>();
    set.insert::<QueueMatchingAdded>();
    set.insert::<NextQueueItem>();
    set.insert::<SkipQueueItem>();
    set.insert::<CreateDashboard>();
    set.insert::<UpdateDashboard>();
    set.insert::<SaveOverview>();
    set.insert::<Definition>();
    set.insert::<Widget>();
    set.insert::<Layout>();
    set.insert::<Chart>();
    set.insert::<ChartType>();
    set.insert::<WidgetKind>();
    set.insert::<WidgetQueryBody>();
    set.insert::<WidgetSourceBody>();
    set.insert::<MeasureBody>();
    set.insert::<AggBody>();
    set.insert::<BreakdownBody>();
    set.insert::<WidgetQueryRequest>();
    set.insert::<WidgetShape>();
    set.insert::<WidgetQueryResponse>();
    set.insert::<WidgetSeriesResponse>();
    set.insert::<WidgetSeries>();
    set.insert::<WidgetPoint>();
    set.insert::<WidgetTotalResponse>();
    set.insert::<TotalRow>();
    set.insert::<FacetsRequest>();
    set.insert::<FacetFieldRequest>();
    set.insert::<FacetResult>();
    set.insert::<Facet>();
    set.insert::<FacetsResponse>();
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
    set.insert_page::<DashboardSummary>("DashboardPage");
    set.insert::<AssistantConversation>();
    set.insert::<AssistantConversationDetail>();
    set.insert::<AssistantMessage>();
    set.insert::<AssistantPart>();
    set.insert::<AssistantRole>();
    set.insert::<AssistantMessageStatus>();
    set.insert::<AssistantFeedback>();
    set.insert::<AssistantDraftKind>();
    set.insert::<UpdateAssistantConversationRequest>();
    set.insert::<AssistantFeedbackRequest>();
    set.insert::<AssistantTurnRequest>();
    set.insert_page::<AssistantConversation>("AssistantConversationPage");
    set.insert::<AssistantSettings>();
    set.insert::<PutAssistantSettings>();
    set.insert::<AssistantTestRequest>();
    set.insert::<AssistantUnavailable>();
    set.insert::<AssistantStatus>();
    set.insert_page::<AnnotationQueue>("AnnotationQueuePage");
    set.insert_page::<AnnotationQueueItem>("AnnotationQueueItemPage");
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
    set.insert::<PromptKind>();
    set.insert::<PromptLabel>();
    set.insert::<Prompt>();
    set.insert::<PromptVersion>();
    set.insert::<CreatePromptRequest>();
    set.insert::<CreateVersionRequest>();
    set.insert::<UpdatePromptRequest>();
    set.insert::<SetLabelRequest>();
    set.insert::<PromptLabelEvent>();
    set.insert::<PromptSettings>();
    set.insert::<PromptMetrics>();
    set.insert::<PromptVersionMetrics>();
    set.insert::<PromptScoreMean>();
    set.insert_page::<Prompt>("PromptPage");
    set.insert_page::<PromptVersion>("PromptVersionPage");
    set.insert_page::<PromptLabelEvent>("PromptLabelEventPage");
    set.insert::<DatasetVersion>();
    set.insert::<DatasetItem>();
    set.insert::<NewDatasetItem>();
    set.insert::<AddItemsRequest>();
    set.insert::<AddItemsResponse>();
    set.insert::<UpdateItemRequest>();
    set.insert::<EditItem>();
    set.insert::<EditItemsRequest>();
    set.insert::<EditItemsResponse>();
    set.insert::<DeleteItemsRequest>();
    set.insert::<RestoreRequest>();
    set.insert::<TraceRef>();
    set.insert::<FromTracesRequest>();
    set.insert::<IssueDatasetItemsRequest>();
    set.insert::<SkippedTrace>();
    set.insert::<FromTracesResult>();
    set.insert::<ExperimentScoreMean>();
    set.insert::<ExperimentTrend>();
    set.insert::<DatasetTrend>();
    set.insert::<ExperimentTrends>();
    set.insert::<FromTraceRequest>();
    set.insert::<ItemChangeKind>();
    set.insert::<ItemChange>();
    set.insert::<ChangeLimit>();
    set.insert_page::<ItemChange>("ItemChangePage");
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
    set.insert::<IssueSourceKind>();
    set.insert::<IssueSource>();
    set.insert::<IssueSourcePage>();
    set.insert::<IssueCandidateCounts>();
    set.insert::<IssueRefit>();
    set.insert::<IssueRefitPage>();
    set.insert::<CreateIssueSourceRequest>();
    set.insert::<UpdateIssueSourceRequest>();
    set.insert::<IssueState>();
    set.insert::<UpdateIssueRequest>();
    set.insert::<MergeIssueRequest>();
    set.insert_page::<Issue>("IssuePage");
    set.insert::<IssueDay>();
    set.insert::<IssueExample>();
    set.insert::<IssueOccurrences>();
    set.insert::<IssueEvent>();
    set.insert::<IssueEventPage>();
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
                    "The cetra REST API: traces, spans, sessions, users, scores, evaluation, datasets, experiments, alerts and administration. Authenticate with an API key (`sk-cetra-…`) or a personal access token (`pat-cetra-…`) as a bearer token.",
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
            tag("playground", "Playground runs: one model run streamed over SSE, traced in cetra-playground (not billed)."),
            tag(
                "prompts",
                "The prompt registry: versioned prompts with labels (D-V-2).",
            ),
            tag(
                "experiments",
                "Experiments run by the SDK and their results.",
            ),
            tag("model_prices", "Custom and built-in model prices"),
            tag("schema", "Filter and SQL catalog."),
            tag("saved_views", "Saved list views and SQL queries."),
            tag(
                "dashboards",
                "Custom dashboards and their widget definitions.",
            ),
            tag(
                "annotation_queues",
                "Annotation queues: lists of traces, spans or sessions that reviewers label one at a time.",
            ),
            tag("sql", "SQL console over the project's views."),
            tag("media", "Extracted media blobs."),
            tag("deletions", "GDPR deletions and their status."),
            tag(
                "audit",
                "Audit records of an organization and of the instance.",
            ),
            tag(
                "ingest",
                "Calls made by coding agents and SDKs on the ingest key.",
            ),
            tag(
                "alerts",
                "Alert rules, alert events and notification channels.",
            ),
            tag("issues", "Issue sources and the issues they find."),
            tag(
                "assistant",
                "The in-app assistant's conversations, private to their author (D-AS-10).",
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
    ("AssistantFeedbackRequest", "feedback"),
    ("CreateProjectRequest", "retention_days"),
    ("UpdateProjectRequest", "retention_days"),
    ("UpdateDatasetRequest", "description"),
    ("UpdatePromptRequest", "description"),
    ("UpdateDatasetRequest", "input_schema"),
    ("UpdateDatasetRequest", "expected_schema"),
    ("UpdateIssueSourceRequest", "filter"),
    ("UpdateIssueRequest", "description"),
    ("AgentContextBody", "repository"),
    ("AgentContextBody", "branch"),
    ("AgentContextBody", "user"),
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
    let paths = scim_paths(paths);
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
    let paths = issue_source_paths(paths);
    let paths = issue_paths(paths);
    let paths = eval_job_paths(paths);
    let paths = backfill_paths(paths);
    let paths = playground_paths(paths);
    let paths = score_definition_paths(paths);
    let paths = dataset_paths(paths);
    let paths = prompt_paths(paths);
    let paths = experiment_paths(paths);
    let paths = model_price_paths(paths);
    let paths = org_model_price_paths(paths);
    let paths = saved_view_paths(paths);
    let paths = dashboard_paths(paths);
    let paths = annotation_queue_paths(paths);
    let paths = sql_paths(paths);
    let paths = deletion_paths(paths);
    let paths = audit_paths(paths);
    let paths = channel_paths(paths);
    let paths = alert_rule_paths(paths);
    let paths = assistant_paths(paths);
    let paths = assistant_settings_paths(paths);
    media_paths(paths).build()
}

fn audit_paths(paths: PathsBuilder) -> PathsBuilder {
    let session = Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]);
    let list_org = operation(
        "list_org_audit_events",
        "audit",
        "List an organization's audit records",
    )
    .description(Some(
        "Newest first. Admins and owners, with a session or a personal access token. Needs the `audit_log` feature: without it, 403 `plan_required`.",
    ))
    .parameters(Some(with_query::<AuditListParams>(vec![path_string(
        "org",
        "Org UUID or slug.",
    )])))
    .responses(responses(
        "200",
        json_named("AuditEventPage", "One page of records, newest first."),
        QUERY_ERRORS,
    ))
    .build();
    let export_org = operation(
        "get_org_audit_export",
        "audit",
        "Export an organization's audit records",
    )
    .description(Some(
        "The records of the filters as CSV or NDJSON, newest first, at most 1,000,000 of them. The export is recorded first as `audit.export_started`. Admins and owners; needs the `audit_log` feature (403 `plan_required` otherwise).",
    ))
    .parameters(Some(with_query::<AuditExportParams>(vec![path_string(
        "org",
        "Org UUID or slug.",
    )])))
    .responses(responses("200", audit_export_result(), QUERY_ERRORS))
    .build();
    let list_admin = operation(
        "list_admin_audit_events",
        "audit",
        "List the instance's audit records",
    )
    .description(Some(
        "Newest first. Instance admins only, with a session. Needs a licence that lists `audit_log`, or a cloud cell (403 `plan_required` otherwise). Org records are never listed here.",
    ))
    .securities(session.clone())
    .parameters(Some(with_query::<AuditListParams>(Vec::new())))
    .responses(responses(
        "200",
        json_named("AuditEventPage", "One page of records, newest first."),
        &[400, 401, 403, 429],
    ))
    .build();
    let export_admin = operation(
        "get_admin_audit_export",
        "audit",
        "Export the instance's audit records",
    )
    .description(Some(
        "The instance's records as CSV or NDJSON, newest first, at most 1,000,000 of them. Recorded first as `audit.export_started`. Instance admins only, with a session; needs the licence or a cloud cell (403 `plan_required` otherwise).",
    ))
    .securities(session)
    .parameters(Some(with_query::<AuditExportParams>(Vec::new())))
    .responses(responses(
        "200",
        audit_export_result(),
        &[400, 401, 403, 429],
    ))
    .build();
    paths
        .path(
            "/api/v1/orgs/{org}/audit-events",
            PathItem::new(HttpMethod::Get, list_org),
        )
        .path(
            "/api/v1/orgs/{org}/audit-events/export",
            PathItem::new(HttpMethod::Get, export_org),
        )
        .path(
            "/api/v1/admin/audit-events",
            PathItem::new(HttpMethod::Get, list_admin),
        )
        .path(
            "/api/v1/admin/audit-events/export",
            PathItem::new(HttpMethod::Get, export_admin),
        )
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
    let install = operation("put_admin_license", "admin", "Install a licence key")
        .securities(session.clone())
        .request_body(Some(json_body("InstallLicenseRequest")))
        .responses(responses(
            "200",
            json_of::<AdminLicense>("The licence now in force. The key itself is never returned."),
            &[400, 401, 403, 409, 429],
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
    let mut license_item = PathItem::new(HttpMethod::Get, license);
    license_item.put = Some(install);
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
        .path("/api/v1/admin/license", license_item)
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
    let device_code = operation("post_device_code", "auth", "Start a device sign-in")
        .description(Some(
            "Issues a device code for `cetra login` or `cetra-cli init` (RFC 8628, D-AO-3). The CLI shows `user_code`, opens `verification_uri_complete` and polls `POST /api/auth/device/token` every `interval` seconds. The codes expire after `expires_in` seconds. No cookie and no `Origin` are needed. Ten codes a minute per client network (an IPv6 address counts by its /64); over that is 429.",
        ))
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("DeviceCodeRequest")))
        .responses(responses(
            "200",
            json_of::<DeviceCodeResponse>(
                "The codes, the approval page's URLs and the polling interval.",
            ),
            &[400, 429, 503],
        ))
        .build();
    let device_token = operation("post_device_token", "auth", "Poll for a device grant")
        .description(Some(
            "Answers the grant once the user approved the code, and only once; a second poll answers `expired_token`. Until then it answers 400 with an RFC 8628 code in `code`: `authorization_pending` (poll again after `interval`), `slow_down` (poll less often; the interval grew by 5 seconds), `access_denied` (the user denied the code) or `expired_token` (the code expired, was already collected, or is unknown). The answer carries `Cache-Control: no-store`.",
        ))
        .securities(Some(Vec::new()))
        .request_body(Some(json_body("DeviceTokenRequest")))
        .responses(responses(
            "200",
            json_of::<DeviceGrant>(
                "The grant: the server origin, the token and, for `init`, the project and the ingest key.",
            ),
            &[400],
        ))
        .build();
    let device_request = operation("get_device_request", "auth", "Read a pending device code")
        .description(Some(
            "What the approval page shows for a pending code: its purpose and the host the CLI reported. Session only. 404 for an unknown, expired or decided code. Thirty lookups, approvals and denials a minute per user.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![path_string(
            "user_code",
            "The code shown by the CLI, `XXXX-XXXX`.",
        )]))
        .responses(responses(
            "200",
            json_of::<DeviceRequest>("The pending code's purpose and host."),
            AUTH_ERRORS,
        ))
        .build();
    let device_approve = operation("post_device_approve", "auth", "Approve a device code")
        .description(Some(
            "Mints the grant for the signed-in user: a `read, write` personal access token for `login`; for `init`, a `read` token and an `ingest` key for `project_id`, a project on which the user may manage settings (an admin there). A project the user cannot see is 404; one the user can see with a lower role is 403. The CLI receives the secrets on its next poll. Session only: a session approval needs the `Origin` header. A failed approval leaves the code pending.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![path_string(
            "user_code",
            "The code shown by the CLI, `XXXX-XXXX`.",
        )]))
        .request_body(Some(json_body("DeviceApproveRequest")))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The code is approved. The CLI receives the grant on its next poll.")
                .build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let device_deny = operation("post_device_deny", "auth", "Deny a device code")
        .description(Some(
            "Denies a pending code. The CLI's next poll answers `access_denied`. Session only. 404 for a code that is not pending.",
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .parameters(Some(vec![path_string(
            "user_code",
            "The code shown by the CLI, `XXXX-XXXX`.",
        )]))
        .responses(responses(
            "204",
            ResponseBuilder::new()
                .description("The code is denied.")
                .build(),
            AUTH_ERRORS,
        ))
        .build();
    paths
        .path(
            "/api/auth/device/code",
            PathItem::new(HttpMethod::Post, device_code),
        )
        .path(
            "/api/auth/device/token",
            PathItem::new(HttpMethod::Post, device_token),
        )
        .path(
            "/api/auth/device/{user_code}",
            PathItem::new(HttpMethod::Get, device_request),
        )
        .path(
            "/api/auth/device/{user_code}/approve",
            PathItem::new(HttpMethod::Post, device_approve),
        )
        .path(
            "/api/auth/device/{user_code}/deny",
            PathItem::new(HttpMethod::Post, device_deny),
        )
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

fn scim_paths(paths: PathsBuilder) -> PathsBuilder {
    let session_only = vec![SecurityRequirement::new("session", [] as [&str; 0])];
    let get = operation(
        "get_org_scim",
        "orgs",
        "Read the org's SCIM provisioning settings",
    )
    .description(Some(
        "The enabled flag, the default role, the SCIM base URL, the verified SCIM domains and the live tokens, without their secrets. Owners only, session only (D-EA-15). Needs no feature, so the page stays readable after a lapse.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .responses(responses(
        "200",
        json_of::<ScimSettings>("The org's SCIM settings."),
        AUTH_ERRORS,
    ))
    .build();
    let patch = operation(
        "update_org_scim",
        "orgs",
        "Turn the org's SCIM provisioning on or off, or change the default role",
    )
    .description(Some(
        "Turning SCIM on or changing the default role needs the `scim` feature, and turning SCIM on needs a verified domain (409). Turning it off always works. The default role is `viewer` or `member`. While SCIM is on, managed members cannot be changed by hand (D-EA-18). Owners only, session only.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .request_body(Some(json_body("UpdateScimSettings")))
    .responses(responses(
        "200",
        json_of::<ScimSettings>("The SCIM settings."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let create_token = operation(
        "create_org_scim_token",
        "orgs",
        "Create a SCIM bearer token",
    )
    .description(Some(
        "Creates a `scim-cetra-…` token for the identity provider. The secret is in this response only. At most two tokens are live at once (409 otherwise). Needs the `scim` feature. Owners only, session only.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .request_body(Some(json_body("CreateScimTokenRequest")))
    .responses(responses(
        "201",
        json_of::<CreatedScimToken>("The token, with its secret."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let revoke_token = operation(
        "revoke_org_scim_token",
        "orgs",
        "Revoke a SCIM bearer token",
    )
    .description(Some(
        "The token stops working at once. Works without the feature. Owners only, session only.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![
        path_string("org", "Org UUID or slug."),
        path_uuid("id", "Token id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The token is revoked.")
            .build(),
        AUTH_ERRORS,
    ))
    .build();
    let list_groups = operation(
        "list_org_scim_groups",
        "orgs",
        "List the SCIM groups and their role mappings",
    )
    .description(Some(
        "Every SCIM group of the org, by display name, with its member count and its role mapping (D-EA-14). Owners only, session only.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![path_string("org", "Org UUID or slug.")]))
    .responses(responses(
        "200",
        json_of::<ScimGroupList>("The org's SCIM groups."),
        AUTH_ERRORS,
    ))
    .build();
    let put_group_mapping = operation(
        "put_org_scim_group_mapping",
        "orgs",
        "Set a SCIM group's role mapping",
    )
    .description(Some(
        "Replaces the group's org role and project roles (D-EA-14). A member gets the highest of their groups' roles, else the default role; owners are never changed. The org role is `admin`, `member` or `viewer`; `owner` is a 400. Raising a role needs the `scim` feature, and writing a project role needs `project_roles`; lowering works without them (D-EA-19). A mapping with neither an org role nor a project is a 400: delete it instead. While SCIM is on, the group's members are recomputed. Owners only, session only.",
    ))
    .securities(Some(session_only.clone()))
    .parameters(Some(vec![
        path_string("org", "Org UUID or slug."),
        path_uuid("id", "Group id."),
    ]))
    .request_body(Some(json_body("PutScimGroupMapping")))
    .responses(responses(
        "200",
        json_of::<ScimGroup>("The group, with its new mapping."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let delete_group_mapping = operation(
        "delete_org_scim_group_mapping",
        "orgs",
        "Remove a SCIM group's role mapping",
    )
    .description(Some(
        "The group's members get the roles of their other groups, else the default role. Always works, so a lapsed org can take roles away. A group with no mapping also answers 204. Owners only, session only.",
    ))
    .securities(Some(session_only))
    .parameters(Some(vec![
        path_string("org", "Org UUID or slug."),
        path_uuid("id", "Group id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The mapping is removed.")
            .build(),
        AUTH_ERRORS,
    ))
    .build();
    let mut settings = PathItem::new(HttpMethod::Get, get);
    settings.patch = Some(patch);
    let mut group_mapping = PathItem::new(HttpMethod::Put, put_group_mapping);
    group_mapping.delete = Some(delete_group_mapping);
    paths
        .path("/api/v1/orgs/{org}/scim", settings)
        .path(
            "/api/v1/orgs/{org}/scim/tokens",
            PathItem::new(HttpMethod::Post, create_token),
        )
        .path(
            "/api/v1/orgs/{org}/scim/tokens/{id}",
            PathItem::new(HttpMethod::Delete, revoke_token),
        )
        .path(
            "/api/v1/orgs/{org}/scim/groups",
            PathItem::new(HttpMethod::Get, list_groups),
        )
        .path("/api/v1/orgs/{org}/scim/groups/{id}/mapping", group_mapping)
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
    let post_internal_device_code = operation(
        "post_internal_device_code",
        "internal",
        "Register a device code the control plane issued",
    )
    .description(Some(
        "Needs a `control_plane` bearer token with the `device` scope (D-AO-14). The control plane sends the lowercase hex SHA-256 of a device code it issued, its user code, purpose, host and expiry; the cell then answers that code as if it had issued it. An identical retry of a pending code answers 204 again. Any other clash with a registered code (the same hash or the same user code, with other details) answers 409. A malformed field answers 400. Registered codes count against the same 10 000 pending codes as issued ones: a full cell answers 503.",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "bearer",
        [] as [&str; 0],
    )]))
    .request_body(Some(json_body("InternalDeviceCode")))
    .responses(responses(
        "204",
        ResponseBuilder::new()
            .description("The code is registered, or the identical registration was already there.")
            .build(),
        &[400, 401, 403, 409, 503],
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
        .path(
            "/api/internal/v1/device/codes",
            PathItem::new(HttpMethod::Post, post_internal_device_code),
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
    let agent_context = operation(
        "post_agent_context",
        "ingest",
        "Send a coding agent's session context",
    )
    .description(Some(
        "Called by `cetra agents hook`. Needs an `sk` key with the `ingest` scope. Later spans of the session carry the repository, branch and person (D-CA-9).",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "bearer",
        [] as [&str; 0],
    )]))
    .request_body(Some(json_body("AgentContextBody")))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Stored.").build(),
        &[400, 401, 402, 403, 413],
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
    let facets = operation("post_facets", "traces", "Get facets")
        .description(Some(format!(
            "{EXCLUSIONS} Answers 1 to 20 fields over the rows the list would show: the window, the default exclusions, `search` and `filter`, given once for every field, but not the list's cursor, sort or limit. The answer has one result per field, in request order, with either `facet` or `error`: a bad field, `limit` or `q`, or a field that times out, fails only that field. A bad filter, `search` on sessions, a bad window, or 0 or more than 20 fields fail the whole request. A field is a column of the view, or `map['key']` of a map column; a bare map column facets its keys. `kind` is `values` (strings, booleans, lists and `map['key']`), `keys` (a bare map) or `histogram` (numbers: min, max, approximate p50, p95 and p99, and 20 equal buckets below p99 plus one bucket from p99 to max). `total` counts rows, `missing` the rows with no value (null, or an empty list or map). Values are counted per row: a list counts a row once under each distinct element. Keys starting with `cetra.` are left out. `q` keeps values containing it, ignoring case. Spans count stored copies separately. Sessions take no `search`. Nothing is cached on the server. A read: a read-only personal access token and a `read` key may call it."
        )))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("FacetsRequest")))
        .responses(responses(
            "200",
            json_of::<FacetsResponse>("One result per field, in request order."),
            &[400, 401, 403, 404, 413, 415, 422, 429, 503],
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
            "/v1/agent-context".to_owned(),
            PathItem::new(HttpMethod::Post, agent_context),
        )
        .path(
            format!("{PROJECT}/schema"),
            PathItem::new(HttpMethod::Get, schema),
        )
        .path(
            format!("{PROJECT}/facets"),
            PathItem::new(HttpMethod::Post, facets),
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

fn issue_source_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Issue source id.");
    let list = operation("list_issue_sources", "issues", "List issue sources")
        .description(Some(
            "Any project member can read. `org_active_sources` counts the enabled sources of the whole org for the plan's active-source limit (D-IC-13).",
        ))
        .parameters(Some(with_query::<PageParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("IssueSourcePage", "One page of issue sources."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_issue_source", "issues", "Create an issue source")
        .description(Some(
            "Project members and above. API keys cannot call it. 403 `plan_limit` when the org is at its active-source limit.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateIssueSourceRequest")))
        .responses(responses(
            "201",
            json_of::<IssueSource>("The created source."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let get = operation("get_issue_source", "issues", "Get an issue source")
        .description(Some("Any project member can read."))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "200",
            json_of::<IssueSource>("The source."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let patch = operation("patch_issue_source", "issues", "Update an issue source")
        .description(Some(
            "Project members and above. API keys cannot call it. `kind` cannot change. Enabling a source checks the active-source limit.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("UpdateIssueSourceRequest")))
        .responses(responses(
            "200",
            json_of::<IssueSource>("The updated source."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let refit = operation(
        "post_issue_source_refit",
        "issues",
        "Refit an issue source now",
    )
        .description(Some(
            "Project members and above. API keys cannot call it. Sets the source's next refit to now, so the next sweep refits it. 400 for an `error` source, or a disabled or paused one. The refit is recorded as `manual`. 409 when a refit started less than 10 minutes ago or one is already due.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses(
            "202",
            json_of::<IssueSource>("The source, with `next_refit_at` set to now."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .build();
    let refits = operation(
        "list_issue_source_refits",
        "issues",
        "List the refits of an issue source",
    )
    .description(Some(
        "Any project member can read. The newest 50 refits of the source, newest first.",
    ))
    .parameters(Some(vec![project_param(), id.clone()]))
    .responses(responses(
        "200",
        json_of::<IssueRefitPage>("The refits, newest first."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let delete = operation("delete_issue_source", "issues", "Delete an issue source")
        .description(Some(
            "Project members and above. API keys cannot call it. The source's issues and their events are deleted with it; the occurrence scores stay (D-IC-3).",
        ))
        .parameters(Some(vec![project_param(), id]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/issue-sources"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/issue-sources"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/issue-sources/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/issue-sources/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/issue-sources/{{id}}/refit"),
            PathItem::new(HttpMethod::Post, refit),
        )
        .path(
            format!("{PROJECT}/issue-sources/{{id}}/refits"),
            PathItem::new(HttpMethod::Get, refits),
        )
        .path(
            format!("{PROJECT}/issue-sources/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
}

fn issue_paths(paths: PathsBuilder) -> PathsBuilder {
    let id = path_uuid("id", "Issue id.");
    let list = operation("list_issues", "issues", "List issues")
        .description(Some(
            "Any project member can read. `count` and `users` cover the period `[from, to)` (default the last 24 hours). Merged issues are listed only when `state` names `merged`. Blank `ids` means no filter.",
        ))
        .parameters(Some(with_query::<IssueListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("IssuePage", "One page of issues."),
            READ_ERRORS,
        ))
        .build();
    let get = operation("get_issue", "issues", "Get an issue")
        .description(Some(
            "Any project member can read. `count` and `users` cover the last 24 hours. `merged_from` lists the issues merged into this one.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .responses(responses("200", json_of::<Issue>("The issue."), READ_ERRORS))
        .build();
    let patch = operation("patch_issue", "issues", "Update an issue")
        .description(Some(
            "Project members and above. API keys and personal access tokens cannot call it. `title` renames the issue and records a `renamed` event. `description` records no event; `null` clears it. `state` resolves, ignores or reopens the issue and records a `state_changed` event; a resolved issue that recurs is reopened as regressed. `merged` is refused: merge with the merge operation. Setting a value it already has changes nothing. A merged issue cannot change (409).",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("UpdateIssueRequest")))
        .responses(responses(
            "200",
            json_of::<Issue>("The updated issue, with its counts over the last 24 hours."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .build();
    let merge = operation("post_issue_merge", "issues", "Merge an issue into another")
        .description(Some(
            "Project members and above. API keys and personal access tokens cannot call it. Folds the issue into `into`, an issue of the same source that is not merged. The issues merged into it move to `into` too, so chains stay one level deep. Its occurrences count on `into`. Answers the target issue. 409 when both are the same issue, the issues are in different sources, this issue is already merged, or the target is merged.",
        ))
        .parameters(Some(vec![project_param(), id.clone()]))
        .request_body(Some(json_body("MergeIssueRequest")))
        .responses(responses(
            "200",
            json_of::<Issue>("The target issue, with its counts over the last 24 hours."),
            &[400, 401, 403, 404, 409, 429],
        ))
        .securities(Some(vec![SecurityRequirement::new(
            "session",
            [] as [&str; 0],
        )]))
        .build();
    let dataset_items = operation(
        "post_issue_dataset_items",
        "issues",
        "Add an issue's occurrences to a dataset",
    )
    .description(Some(
        "Project members and above, a read-write personal access token, or an API key with `write` (D-IC-22). Copies the most recent occurrence traces of the issue and of the issues merged into it, one item per trace and up to `limit` (default and maximum 100), into the dataset as one new version, as `items/from-traces` does. An occurrence with an error span uses that span, else the trace root. Items get the metadata `cetra.issue_id`. A trace that cannot be copied is listed in `skipped`; no occurrence in the data window or no copyable trace is a 400.",
    ))
    .parameters(Some(vec![project_param(), id.clone()]))
    .request_body(Some(json_body("IssueDatasetItemsRequest")))
    .responses(responses(
        "201",
        json_of::<FromTracesResult>("The new version, the items added and the traces skipped."),
        &[400, 401, 403, 404, 409, 429, 503],
    ))
    .build();
    let queue_items = operation(
        "post_issue_queue_items",
        "issues",
        "Send an issue's occurrences to an annotation queue",
    )
    .description(Some(
        "Project members and above, a read-write personal access token, or an API key with `write` (D-IC-30). Adds the targets of the most recent occurrences of the issue and of the issues merged into it (the issue and its 7 busiest merged issues over the last 14 days, newest first) to the annotation queue `queue`, up to `limit` targets (default 100, at most 1 000), through the same path as `POST …/annotation-queues/{id}/items`. A trace queue gets each occurrence's trace, a span queue its span, a session queue its session; each target once. An occurrence without that target is listed in `skipped`, its `index` being its position among the occurrences read newest first; so is a target that is unknown, hidden by the data window, an evaluation trace or blocked by a deletion. A target already in the queue counts as `existing`. The answer is 201 whatever the counts. 400 on `limit`, or on `issue` when the issue has no occurrence in the data window; 404 for a missing issue or queue; a queue that would pass 100 000 items is 422 and nothing is added.",
    ))
    .parameters(Some(vec![project_param(), id.clone()]))
    .request_body(Some(json_body("IssueQueueItemsRequest")))
    .responses(responses(
        "201",
        json_of::<AddedQueueItems>("The counts and the skipped targets."),
        &[400, 401, 403, 404, 422, 429, 503],
    ))
    .build();
    let occurrences = operation(
        "get_issue_occurrences",
        "issues",
        "Get the occurrences of an issue",
    )
    .description(Some(
        "Any project member can read. `days` has one entry per UTC day of `[from, to)` (default the last 14 days, at most 90) with an occurrence. `examples` are the newest 20 occurrences of the issue and of the issues merged into it, not limited to the period.",
    ))
    .parameters(Some(with_query::<IssueOccurrenceParams>(vec![
        project_param(),
        id.clone(),
    ])))
    .responses(responses(
        "200",
        json_of::<IssueOccurrences>("The occurrences."),
        READ_ERRORS,
    ))
    .build();
    let events = operation("get_issue_events", "issues", "List the events of an issue")
        .description(Some(
            "Any project member can read. The newest 100 events of the issue, newest first.",
        ))
        .parameters(Some(vec![project_param(), id]))
        .responses(responses(
            "200",
            json_of::<IssueEventPage>("The events."),
            QUERY_ERRORS,
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/issues"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}/merge"),
            PathItem::new(HttpMethod::Post, merge),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}/dataset-items"),
            PathItem::new(HttpMethod::Post, dataset_items),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}/queue-items"),
            PathItem::new(HttpMethod::Post, queue_items),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}/occurrences"),
            PathItem::new(HttpMethod::Get, occurrences),
        )
        .path(
            format!("{PROJECT}/issues/{{id}}/events"),
            PathItem::new(HttpMethod::Get, events),
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

fn playground_paths(paths: PathsBuilder) -> PathsBuilder {
    let run = operation(
        "post_playground_run",
        "playground",
        "Stream a playground run",
    )
    .description(Some(
        "Members and above, through a session or a read-write personal access token. API keys cannot call it. Answers `text/event-stream`: `start` `{trace_id}`, then `text` `{delta}` and `tool_call` `{id, name, arguments}` events, then one last `done` `{finish_reason, usage, cost_usd, latency_ms, ttft_ms, output}` or `error` `{class, message}` (class `retryable`, `pause` or `permanent`). A `ping` comes every 15 seconds. A response format is answered in `text` events. Tools are never run. A tool named `auto`, `none` or `required` cannot be forced by name through `tool_choice`. The run is traced into the reserved environment `cetra-playground`, which is not billed. The connection is used whatever its status, and a failure never marks it failing.",
    ))
    .parameters(Some(vec![project_param()]))
    .request_body(Some(json_body("PlaygroundRunRequest")))
    .responses(responses(
        "200",
        sse("The run's events, as `text/event-stream`."),
        &[400, 401, 403, 404, 413, 429, 503],
    ))
    .build();
    paths.path(
        format!("{PROJECT}/playground/runs"),
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
    let version_number = ParameterBuilder::new()
        .name("version")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Version number, starting at 1."))
        .schema(Some(ObjectBuilder::new().schema_type(Type::Integer)))
        .build();
    let changes_list = operation(
        "list_dataset_version_changes",
        "datasets",
        "List the items a version changed",
    )
    .description(Some(
        "Any project member, or an API key with `read`. The items the version added, edited or deleted, each with its revision before and after, paged by item id (D-J-24). A version outside 1 to the current one is 404.",
    ))
    .parameters(Some(with_query::<ItemChangeListParams>(vec![
        project_param(),
        dataset.clone(),
        version_number,
    ])))
    .responses(responses(
        "200",
        json_named("ItemChangePage", "One page of changes, by item id."),
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
    let items_edit = operation("patch_dataset_items", "datasets", "Edit items in one batch")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-23). Creates one new version holding every item that changes; items that change nothing are left out. A batch with any failing item writes nothing; the error lists the failing items.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("EditItemsRequest")))
        .responses(responses(
            "200",
            json_of::<EditItemsResponse>("The new version, or null when nothing changed, and the items."),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    let items_delete = operation("post_dataset_items_delete", "datasets", "Delete items in one batch")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-23). Creates one new version in which the listed items are gone; older versions still hold them.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("DeleteItemsRequest")))
        .responses(responses(
            "200",
            json_of::<DatasetVersion>("The new version."),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    let restore = operation("restore_dataset", "datasets", "Restore the items of an earlier version")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-24). Creates one new version whose live items are those of the given earlier version. A version that changes nothing is a 400 on `version` and writes nothing.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("RestoreRequest")))
        .responses(responses(
            "200",
            json_of::<DatasetVersion>("The new version."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let from_traces = operation("post_dataset_items_from_traces", "datasets", "Add items from several traces")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5, D-J-25). Copies 1 to 100 traces as `from-trace` does, in one new version. A trace that cannot be copied is listed in `skipped`; no copyable trace is a 400.",
        ))
        .parameters(Some(vec![project_param(), dataset.clone()]))
        .request_body(Some(json_body("FromTracesRequest")))
        .responses(responses(
            "201",
            json_of::<FromTracesResult>("The new version, the items added and the traces skipped."),
            &[400, 401, 403, 404, 409, 413, 429, 503],
        ))
        .build();
    let trends = operation(
        "list_experiment_trends",
        "datasets",
        "The latest experiments of several datasets, with score means",
    )
    .description(Some(
        "Any project member, or an API key with `read`. For each dataset of `ids` in the project, its 10 newest visible experiments with result and error counts, and their numeric and boolean score means (D-J-26). Storage busy or slow answers without `scores` and with `scores_unavailable: true`.",
    ))
    .parameters(Some(with_query::<ExperimentTrendParams>(vec![project_param()])))
    .responses(responses(
        "200",
        json_of::<ExperimentTrends>("The trends of each dataset, in request order."),
        QUERY_ERRORS,
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
            format!("{PROJECT}/datasets/{{dataset}}/versions/{{version}}/changes"),
            PathItem::new(HttpMethod::Get, changes_list),
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
            format!("{PROJECT}/datasets/{{dataset}}/items"),
            PathItem::new(HttpMethod::Patch, items_edit),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/delete"),
            PathItem::new(HttpMethod::Post, items_delete),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/restore"),
            PathItem::new(HttpMethod::Post, restore),
        )
        .path(
            format!("{PROJECT}/datasets/{{dataset}}/items/from-traces"),
            PathItem::new(HttpMethod::Post, from_traces),
        )
        .path(
            format!("{PROJECT}/dataset-trends"),
            PathItem::new(HttpMethod::Get, trends),
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

/// The body of an audit export: NDJSON lines or CSV rows, as an attachment.
fn audit_export_result() -> Response {
    ResponseBuilder::new()
        .description("The records, one per line (NDJSON) or one per row (CSV), with `Content-Disposition: attachment`.")
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

const PROMPT_READ: &str = "Any project member, or an API key with `read`.";
const PROMPT_READ_VERSIONS: &str =
    "Any project member, or an API key with `read`. At most 50 versions per page.";
const PROMPT_WRITE: &str = "Project members and above, a read-write personal access token, or an API key with `write` (D-V-13). Protected labels need a project admin or a `write` key (D-V-10).";

/// The prompt routes (D-V-2, D-V-7, D-V-9, D-V-12, D-V-13). Reads need project view; writes
/// need `ManagePrompts`.
fn prompt_paths(paths: PathsBuilder) -> PathsBuilder {
    let prompt = path_string("prompt", "Prompt name.");
    let version = ParameterBuilder::new()
        .name("version")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Version number, starting at 1."))
        .schema(Some(ObjectBuilder::new().schema_type(Type::Integer)))
        .build();
    let list = operation("list_prompts", "prompts", "List prompts")
        .description(Some(PROMPT_READ))
        .parameters(Some(with_query::<PromptListParams>(vec![project_param()])))
        .responses(responses(
            "200",
            json_named("PromptPage", "One page of prompts."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("post_prompt", "prompts", "Create a prompt")
        .description(Some(PROMPT_WRITE))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreatePromptRequest")))
        .responses(responses(
            "201",
            json_of::<PromptVersion>("The prompt's first version."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let get = operation("get_prompt", "prompts", "Get a prompt")
        .description(Some(PROMPT_READ))
        .parameters(Some(vec![project_param(), prompt.clone()]))
        .responses(responses(
            "200",
            json_of::<Prompt>("The prompt."),
            &[401, 403, 404, 429],
        ))
        .build();
    let patch = operation("patch_prompt", "prompts", "Update a prompt")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-V-13). Changes the description and the tags. The name, the type and the versions never change.",
        ))
        .parameters(Some(vec![project_param(), prompt.clone()]))
        .request_body(Some(json_body("UpdatePromptRequest")))
        .responses(responses(
            "200",
            json_of::<Prompt>("The updated prompt."),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    let delete = operation("delete_prompt", "prompts", "Delete a prompt")
        .description(Some(PROMPT_WRITE))
        .parameters(Some(vec![project_param(), prompt.clone()]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[401, 403, 404, 429],
        ))
        .build();
    let resolve = operation(
        "get_prompt_resolve",
        "prompts",
        "Resolve a prompt to a version",
    )
    .description(Some(
        "Any project member, or an API key with `read`. Answers the version that a label (default `production`), `latest` or `version` points at (D-V-9). The answer carries a strong `ETag`; a request whose `If-None-Match` matches gets `304` (D-V-12). The tag covers the whole body, including the computed `latest` label, so it changes when a newer version is saved.",
    ))
    .parameters(Some(with_query::<ResolveParams>(vec![
        project_param(),
        prompt.clone(),
    ])))
    .responses(responses_with(
        [
            (
                "200",
                ResponseBuilder::new()
                    .description("The resolved version. The `ETag` header holds its strong tag.")
                    .content(
                        "application/json",
                        Content::new(Some(Ref::from_schema_name("PromptVersion"))),
                    )
                    .build(),
            ),
            (
                "304",
                ResponseBuilder::new()
                    .description("Not modified: the If-None-Match tag matches.")
                    .build(),
            ),
        ],
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let version_list = operation(
        "list_prompt_versions",
        "prompts",
        "List the versions of a prompt",
    )
    .description(Some(PROMPT_READ_VERSIONS))
    .parameters(Some(with_query::<PageParams>(vec![
        project_param(),
        prompt.clone(),
    ])))
    .responses(responses(
        "200",
        json_named("PromptVersionPage", "One page of versions, newest first."),
        QUERY_ERRORS,
    ))
    .build();
    let version_create = operation("post_prompt_version", "prompts", "Create a version")
        .description(Some(PROMPT_WRITE))
        .parameters(Some(vec![project_param(), prompt.clone()]))
        .request_body(Some(json_body("CreateVersionRequest")))
        .responses(responses(
            "201",
            json_of::<PromptVersion>("The new version."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let version_get = operation("get_prompt_version", "prompts", "Get a version")
        .description(Some(PROMPT_READ))
        .parameters(Some(vec![project_param(), prompt.clone(), version]))
        .responses(responses(
            "200",
            json_of::<PromptVersion>("The version."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let label = path_string("label", "Label name.");
    let label_set = operation("put_prompt_label", "prompts", "Move a label")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-V-13). Points the label at the version, creating it when it is new; a protected label needs a project admin or a `write` key (D-V-10). `latest` is computed and cannot be set.",
        ))
        .parameters(Some(vec![project_param(), prompt.clone(), label.clone()]))
        .request_body(Some(json_body("SetLabelRequest")))
        .responses(responses(
            "200",
            json_of::<Prompt>("The prompt, with its labels."),
            &[400, 401, 403, 404, 409, 413, 429],
        ))
        .build();
    let label_delete = operation("delete_prompt_label", "prompts", "Remove a label")
        .description(Some(
            "Project members and above, a read-write personal access token, or an API key with `write` (D-V-13). The versions stay; a protected label needs a project admin or a `write` key (D-V-10).",
        ))
        .parameters(Some(vec![project_param(), prompt.clone(), label]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Removed.").build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let metrics = operation(
        "get_prompt_metrics",
        "prompts",
        "Get per-version prompt metrics",
    )
    .description(Some(format!(
        "{PROMPT_READ} Default window: the last 7 days; at most 90 days (D-V-17)."
    )))
    .parameters(Some(with_query::<PromptMetricsParams>(vec![
        project_param(),
        prompt.clone(),
    ])))
    .responses(responses(
        "200",
        json_of::<PromptMetrics>("Per-version metrics of the prompt over the window."),
        READ_ERRORS,
    ))
    .build();
    let label_events = operation(
        "list_prompt_label_events",
        "prompts",
        "List the label history of a prompt",
    )
    .description(Some(PROMPT_READ))
    .parameters(Some(with_query::<PageParams>(vec![
        project_param(),
        prompt,
    ])))
    .responses(responses(
        "200",
        json_named(
            "PromptLabelEventPage",
            "One page of label moves, newest first.",
        ),
        QUERY_ERRORS,
    ))
    .build();
    let settings_get = operation("get_prompt_settings", "prompts", "Get the prompt settings")
        .description(Some(PROMPT_READ))
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_of::<PromptSettings>("The project's protected labels."),
            &[401, 403, 404, 429],
        ))
        .build();
    let settings_put = operation("put_prompt_settings", "prompts", "Set the prompt settings")
        .description(Some("Project admins (D-V-10)."))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("PromptSettings")))
        .responses(responses(
            "200",
            json_of::<PromptSettings>("The stored protected labels."),
            &[400, 401, 403, 404, 413, 429],
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/prompts"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/prompts"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}"),
            PathItem::new(HttpMethod::Patch, patch),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/resolve"),
            PathItem::new(HttpMethod::Get, resolve),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/versions"),
            PathItem::new(HttpMethod::Get, version_list),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/versions"),
            PathItem::new(HttpMethod::Post, version_create),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/versions/{{version}}"),
            PathItem::new(HttpMethod::Get, version_get),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/labels/{{label}}"),
            PathItem::new(HttpMethod::Put, label_set),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/labels/{{label}}"),
            PathItem::new(HttpMethod::Delete, label_delete),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/metrics"),
            PathItem::new(HttpMethod::Get, metrics),
        )
        .path(
            format!("{PROJECT}/prompts/{{prompt}}/label-events"),
            PathItem::new(HttpMethod::Get, label_events),
        )
        .path(
            format!("{PROJECT}/prompt-settings"),
            PathItem::new(HttpMethod::Get, settings_get),
        )
        .path(
            format!("{PROJECT}/prompt-settings"),
            PathItem::new(HttpMethod::Put, settings_put),
        )
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

fn annotation_queue_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation(
        "list_annotation_queues",
        "annotation_queues",
        "List annotation queues",
    )
    .description(Some(
        "Queues of the project, by name then id, with their item counts over the data window. `target_trace_id` keeps the queues that hold a visible trace or span item of that trace; `target_session_id` the queues that hold a visible item of that session; both at once is 400. Readable with a read key and a read-only token.",
    ))
    .parameters(Some(with_query::<AnnotationQueueListParams>(vec![
        project_param(),
    ])))
    .responses(responses(
        "200",
        json_named("AnnotationQueuePage", "One page of annotation queues."),
        QUERY_ERRORS,
    ))
    .build();
    let create = operation(
        "create_annotation_queue",
        "annotation_queues",
        "Create an annotation queue",
    )
    .description(Some(
        "Creates a queue at version 1 with 1 to 50 distinct score definitions of the project, in review order. The name is unique in the project (409). A project holds at most 100 queues (409), and the org's plan may cap them (403 `plan_limit`). A session or a read-write personal access token with role member or above, or a `write` API key.",
    ))
    .parameters(Some(vec![project_param()]))
    .request_body(Some(json_body("CreateAnnotationQueue")))
    .responses(responses(
        "201",
        json_of::<AnnotationQueue>("The created queue."),
        &[400, 401, 403, 404, 409, 413, 415, 429],
    ))
    .build();
    let get = operation(
        "get_annotation_queue",
        "annotation_queues",
        "Get an annotation queue",
    )
    .description(Some(
        "One queue with its item counts over the data window. Any other id is 404. Readable with a read key and a read-only token.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AnnotationQueue>("The queue."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let update = operation(
        "update_annotation_queue",
        "annotation_queues",
        "Update an annotation queue",
    )
    .description(Some(
        "Changes the fields given; omitted fields are unchanged, and `\"\"` clears the description or the instructions. The score definitions, when given, replace the whole list. `version` must be the stored version, else 409 with the stored version in `detail`. A successful update raises the version by one. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .request_body(Some(json_body("UpdateAnnotationQueue")))
    .responses(responses(
        "200",
        json_of::<AnnotationQueue>("The updated queue."),
        &[400, 401, 403, 404, 409, 413, 415, 429],
    ))
    .build();
    let delete = operation(
        "delete_annotation_queue",
        "annotation_queues",
        "Delete an annotation queue",
    )
    .description(Some(
        "Deletes the queue and its items. The score definitions and the scores stay. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Deleted.").build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let list_items = operation(
        "list_annotation_queue_items",
        "annotation_queues",
        "List annotation queue items",
    )
    .description(Some(
        "Items of the queue by id, optionally of one status, over the data window. An absent queue is 404. Readable with a read key and a read-only token.",
    ))
    .parameters(Some(with_query::<AnnotationQueueItemListParams>(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ])))
    .responses(responses(
        "200",
        json_named("AnnotationQueueItemPage", "One page of queue items."),
        QUERY_ERRORS,
    ))
    .build();
    let get_item = operation(
        "get_annotation_queue_item",
        "annotation_queues",
        "Get an annotation queue item",
    )
    .description(Some(
        "One item of the queue. An unknown id, an item of another queue and an item the data window hides are all 404. Readable with a read key and a read-only token.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AnnotationQueueItem>("The item."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let add_items = operation(
        "add_annotation_queue_items",
        "annotation_queues",
        "Add items to an annotation queue",
    )
    .description(Some(
        "Adds 1 to 1 000 targets by id. A trace queue takes a `trace_id`, a span queue a `trace_id` and a `span_id`, a session queue a `session_id`. A target that is unknown, hidden by the data window, an evaluation trace, of the wrong kind for the queue, or blocked by a deletion is listed in `skipped` and not added. A target already in the queue counts as `existing`. The answer is 201 whatever the counts. A queue that would pass 100 000 items is 422 and nothing is added. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .request_body(Some(json_body("AddQueueItems")))
    .responses(responses(
        "201",
        json_of::<AddedQueueItems>("The counts and the skipped targets."),
        &[400, 401, 403, 404, 413, 415, 422, 429, 503],
    ))
    .build();
    let preview_matching = operation(
        "preview_annotation_queue_matching",
        "annotation_queues",
        "Count the targets a filter would add to an annotation queue",
    )
    .description(Some(
        "Counts what the queue kind's list filter matches over `from` to `to`: traces for a trace queue, spans for a span queue, sessions for a session queue. A session queue takes no `search`. At most 10 000 are counted (`capped` says more matched). Evaluation traces are never matched. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .request_body(Some(json_body("QueueMatchingRequest")))
    .responses(responses(
        "200",
        json_of::<QueueMatchingPreview>("The number of matching targets."),
        &[400, 401, 403, 404, 413, 415, 429, 503],
    ))
    .build();
    let add_matching = operation(
        "add_annotation_queue_matching",
        "annotation_queues",
        "Add the targets a filter matches to an annotation queue",
    )
    .description(Some(
        "Adds what the queue kind's list filter matches over `from` to `to`, the same selection as the preview: at most 10 000 targets per call (`capped` says more matched; which are added is not specified). Evaluation traces are never matched. A target already in the queue counts as `existing`; a target a deletion blocks counts as `blocked` and is not added. The answer is 201 whatever the counts. A queue that would pass 100 000 items is 422 and nothing is added. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .request_body(Some(json_body("QueueMatchingRequest")))
    .responses(responses(
        "201",
        json_of::<QueueMatchingAdded>("The counts of the add."),
        &[400, 401, 403, 404, 413, 415, 422, 429, 503],
    ))
    .build();
    let remove_item = operation(
        "remove_annotation_queue_item",
        "annotation_queues",
        "Remove an annotation queue item",
    )
    .description(Some(
        "Removes one item from the queue. The score definitions and the scores stay. Same roles as create.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Removed.").build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let next = operation(
        "next_annotation_queue_item",
        "annotation_queues",
        "Claim the next item of an annotation queue",
    )
    .description(Some(
        "Leases the first free pending item after `after` (by id), wrapping to the first when none follows, for 5 minutes, and releases the caller's other leases in the queue. Answers 204 with no body when nothing is free. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
    ]))
    .request_body(Some(json_body("NextQueueItem")))
    .responses(responses_with(
        [
            ("200", json_of::<AnnotationQueueItem>("The claimed item.")),
            ("204", ResponseBuilder::new().description("Nothing is free.").build()),
        ],
        &[400, 401, 403, 404, 413, 415, 429],
    ))
    .build();
    let lease = operation(
        "lease_annotation_queue_item",
        "annotation_queues",
        "Lease an annotation queue item",
    )
    .description(Some(
        "Takes or renews the 5-minute lease of a pending item that is free, expired or already the caller's. Another reviewer's live lease is 409 with the holder in `errors`. Any body is ignored. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AnnotationQueueItem>("The leased item."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let release = operation(
        "release_annotation_queue_item",
        "annotation_queues",
        "Release an annotation queue item lease",
    )
    .description(Some(
        "Clears the lease when the caller holds it; otherwise changes nothing. Answers 204 either way. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Released.").build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let complete = operation(
        "complete_annotation_queue_item",
        "annotation_queues",
        "Complete an annotation queue item",
    )
    .description(Some(
        "Marks a pending item completed by the caller and clears its lease. Refused with 409 while another reviewer holds a live lease, or when the item is not pending. Any body is ignored. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AnnotationQueueItem>("The completed item."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    let skip = operation(
        "skip_annotation_queue_item",
        "annotation_queues",
        "Skip an annotation queue item",
    )
    .description(Some(
        "Marks a pending item skipped, with an optional reason, and clears its lease. The same 409s as complete. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .request_body(Some(json_body("SkipQueueItem")))
    .responses(responses(
        "200",
        json_of::<AnnotationQueueItem>("The skipped item."),
        &[400, 401, 403, 404, 409, 413, 415, 429],
    ))
    .build();
    let reopen = operation(
        "reopen_annotation_queue_item",
        "annotation_queues",
        "Reopen an annotation queue item",
    )
    .description(Some(
        "Sets a completed or skipped item back to pending, clearing its reason and done fields. A pending item is 409. Any body is ignored. A session or a read-write personal access token with role member or above; API keys cannot call it.",
    ))
    .parameters(Some(vec![
        project_param(),
        path_uuid("id", "Annotation queue id."),
        path_uuid("item_id", "Annotation queue item id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AnnotationQueueItem>("The reopened item."),
        &[400, 401, 403, 404, 409, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/annotation-queues"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/annotation-queues"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items"),
            PathItem::new(HttpMethod::Get, list_items),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items"),
            PathItem::new(HttpMethod::Post, add_items),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/matching/preview"),
            PathItem::new(HttpMethod::Post, preview_matching),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/matching"),
            PathItem::new(HttpMethod::Post, add_matching),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}"),
            PathItem::new(HttpMethod::Get, get_item),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}"),
            PathItem::new(HttpMethod::Delete, remove_item),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/next"),
            PathItem::new(HttpMethod::Post, next),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}/lease"),
            PathItem::new(HttpMethod::Post, lease),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}/lease"),
            PathItem::new(HttpMethod::Delete, release),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}/complete"),
            PathItem::new(HttpMethod::Post, complete),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}/skip"),
            PathItem::new(HttpMethod::Post, skip),
        )
        .path(
            format!("{PROJECT}/annotation-queues/{{id}}/items/{{item_id}}/reopen"),
            PathItem::new(HttpMethod::Post, reopen),
        )
}

fn dashboard_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation("list_dashboards", "dashboards", "List dashboards")
        .description(Some(
            "Dashboards of the project, by name then id. Leaves out the definition; get one dashboard for its widgets. Readable with a read key and a read-only token.",
        ))
        .parameters(Some(with_query::<DashboardListParams>(vec![
            project_param(),
        ])))
        .responses(responses(
            "200",
            json_named("DashboardPage", "One page of dashboards."),
            QUERY_ERRORS,
        ))
        .build();
    let create = operation("create_dashboard", "dashboards", "Create a dashboard")
        .description(Some(
            "Creates a dashboard at version 1. Every widget query is checked, and every filter is parsed, before the write. A project holds at most 100 dashboards (409). A session or a read-write personal access token with role member or above; API keys cannot write dashboards.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("CreateDashboard")))
        .responses(responses(
            "201",
            json_of::<Dashboard>("The created dashboard."),
            &[400, 401, 403, 404, 409, 413, 415, 429],
        ))
        .build();
    let get = operation("get_dashboard", "dashboards", "Get a dashboard")
        .description(Some(
            "One dashboard with its definition. Any other id is 404.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Dashboard id."),
        ]))
        .responses(responses(
            "200",
            json_of::<Dashboard>("The dashboard."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let update = operation("update_dashboard", "dashboards", "Update a dashboard")
        .description(Some(
            "Changes the fields given; omitted fields are unchanged. `version` must be the stored version, else 409 with the stored version in `detail`. A successful update raises the version by one. Same roles as create.",
        ))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Dashboard id."),
        ]))
        .request_body(Some(json_body("UpdateDashboard")))
        .responses(responses(
            "200",
            json_of::<Dashboard>("The updated dashboard."),
            &[400, 401, 403, 404, 409, 413, 415, 429],
        ))
        .build();
    let query = operation("query_widget", "dashboards", "Get one widget's data")
        .description(Some(
            "Answers one widget query over `[from, to)`: a series (one value per bucket) or a total (one value over the range, with `compare` also the previous range). Spec 05 § Dashboard queries. The dashboard's `filter` is a traces expression, and the default exclusions apply: evaluation, playground and assistant traces (`environment = cetra-eval`, `environment = cetra-playground` or `environment = cetra-assistant`) and experiment traces are left out unless `include_eval` or `include_experiments` is set. A read: a read-only personal access token and a `read` key may call it. Results are cached for 10 seconds per project and body.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("WidgetQueryRequest")))
        .responses(responses(
            "200",
            json_of::<WidgetQueryResponse>("The widget's answer, tagged by `shape`."),
            &[400, 401, 403, 404, 413, 415, 422, 429, 503],
        ))
        .build();
    let get_overview = operation("get_overview", "dashboards", "Get the project's Overview")
        .description(Some(
            "The Overview the project stored with a save: its widgets and version. Without a stored one, 404, and the client shows the built-in preset. Readable with a read key and a read-only token.",
        ))
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "200",
            json_of::<Dashboard>("The stored Overview."),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let save_overview = operation("save_overview", "dashboards", "Save the project's Overview")
        .description(Some(
            "Stores the widgets as the project's Overview. With nothing stored and no `version`, creates it at version 1 (201). Otherwise `version` must be the stored version: the save updates it (200) and raises the version by one. Any other `version` is 409 with the stored version in `detail`; without a stored Overview, a `version` is also 409 with 0. Every widget query is checked before the write. Same roles as create.",
        ))
        .parameters(Some(vec![project_param()]))
        .request_body(Some(json_body("SaveOverview")))
        .responses(responses_with(
            [
                ("200", json_of::<Dashboard>("The updated Overview.")),
                ("201", json_of::<Dashboard>("The created Overview.")),
            ],
            &[400, 401, 403, 404, 409, 413, 415, 429],
        ))
        .build();
    let reset_overview = operation("reset_overview", "dashboards", "Reset the project's Overview")
        .description(Some(
            "Deletes the project's stored Overview, so the built-in preset shows again. Answers 204 whether or not one was stored. Same roles as create.",
        ))
        .parameters(Some(vec![project_param()]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Reset.").build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    let delete = operation("delete_dashboard", "dashboards", "Delete a dashboard")
        .description(Some("Deletes the dashboard. Same roles as create."))
        .parameters(Some(vec![
            project_param(),
            path_uuid("id", "Dashboard id."),
        ]))
        .responses(responses(
            "204",
            ResponseBuilder::new().description("Deleted.").build(),
            &[400, 401, 403, 404, 429],
        ))
        .build();
    paths
        .path(
            format!("{PROJECT}/dashboards"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/dashboards"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/dashboards/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/dashboards/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/dashboards/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/dashboards/query"),
            PathItem::new(HttpMethod::Post, query),
        )
        .path(
            format!("{PROJECT}/dashboards/overview"),
            PathItem::new(HttpMethod::Get, get_overview),
        )
        .path(
            format!("{PROJECT}/dashboards/overview"),
            PathItem::new(HttpMethod::Put, save_overview),
        )
        .path(
            format!("{PROJECT}/dashboards/overview"),
            PathItem::new(HttpMethod::Delete, reset_overview),
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
             When `alerts.email_recipients` is `members` (the default under open signup), every email \
             recipient must be a verified member of the org (400 on recipients).",
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
            "When `alerts.email_recipients` is `members` (the default under open signup), every email \
             recipient must be a verified member of the org (400 on recipients).",
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
    "Evaluation, playground and assistant traces (`environment = ",
    cetra_core::cetra_name!("-eval"),
    "`, `environment = ",
    cetra_core::cetra_name!("-playground"),
    "` or `environment = ",
    cetra_core::cetra_name!("-assistant"),
    "`) and experiment traces are excluded unless `include_eval` or `include_experiments` is set."
);

/// The access of the assistant routes (D-AS-4, D-AS-10).
const ASSISTANT_ACCESS: &str =
    "Project members and above, with a session; only the conversation's author (D-AS-4, D-AS-10).";

fn assistant_paths(paths: PathsBuilder) -> PathsBuilder {
    let list = operation(
        "list_assistant_conversations",
        "assistant",
        "List the caller's assistant conversations",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} The conversations, newest activity first, without their messages. Not paged: at most 100 per user and project."
    )))
    .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
    .parameters(Some(vec![project_param()]))
    .responses(responses(
        "200",
        json_named(
            "AssistantConversationPage",
            "The caller's conversations.",
        ),
        &[401, 403, 404, 429],
    ))
    .build();
    let create = operation(
        "post_assistant_conversation",
        "assistant",
        "Create an assistant conversation",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} Takes no body. The conversation starts with an empty title."
    )))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![project_param()]))
    .responses(responses(
        "201",
        json_of::<AssistantConversation>("The created conversation."),
        &[401, 403, 404, 429],
    ))
    .build();
    let get = operation(
        "get_assistant_conversation",
        "assistant",
        "Get an assistant conversation",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} The conversation with its messages, oldest first. Any other id is 404."
    )))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![
        project_param(),
        path_string("id", "Conversation id."),
    ]))
    .responses(responses(
        "200",
        json_of::<AssistantConversationDetail>("The conversation with its messages."),
        &[401, 403, 404, 429],
    ))
    .build();
    let update = operation(
        "patch_assistant_conversation",
        "assistant",
        "Rename an assistant conversation",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} Sets the title, 1 to 80 characters after trimming. The conversation keeps its place in the list."
    )))
    .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
    .parameters(Some(vec![
        project_param(),
        path_string("id", "Conversation id."),
    ]))
    .request_body(Some(json_body("UpdateAssistantConversationRequest")))
    .responses(responses(
        "200",
        json_of::<AssistantConversation>("The renamed conversation."),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    let delete = operation(
        "delete_assistant_conversation",
        "assistant",
        "Delete an assistant conversation",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} Deletes the conversation and its messages."
    )))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![
        project_param(),
        path_string("id", "Conversation id."),
    ]))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Deleted.").build(),
        &[401, 403, 404, 429],
    ))
    .build();
    let seq = ParameterBuilder::new()
        .name("seq")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .description(Some("Message position, from 1."))
        .schema(Some(ObjectBuilder::new().schema_type(Type::Integer)))
        .build();
    let turn = operation(
        "post_assistant_turn",
        "assistant",
        "Stream an assistant turn",
    )
    .description(Some(
        "Members and above, with a session (D-AS-4). Streams one turn (D-AS-9); aborting the request cancels it. Not units (D-AS-11).",
    ))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![
        project_param(),
        path_string("id", "Conversation id."),
    ]))
    .request_body(Some(json_body("AssistantTurnRequest")))
    .responses(responses(
        "200",
        sse("The turn's events, as `text/event-stream`: `start`, `text`, `tool_call`, `tool_result` and `draft` events, then one `done` or `error`. A `ping` comes every 15 seconds."),
        &[400, 401, 403, 404, 409, 413, 429, 503],
    ))
    .build();
    let feedback = operation(
        "put_assistant_feedback",
        "assistant",
        "Rate an assistant message",
    )
    .description(Some(format!(
        "{ASSISTANT_ACCESS} Sets the rating of an assistant message to `up` or `down`; `null` clears it. A user or tool message is `400`. Any other message is `404`."
    )))
    .securities(Some(vec![SecurityRequirement::new(
        "session",
        [] as [&str; 0],
    )]))
    .parameters(Some(vec![
        project_param(),
        path_string("id", "Conversation id."),
        seq,
    ]))
    .request_body(Some(json_body("AssistantFeedbackRequest")))
    .responses(responses(
        "204",
        ResponseBuilder::new().description("Saved.").build(),
        &[400, 401, 403, 404, 429],
    ))
    .build();
    paths
        .path(
            format!("{PROJECT}/assistant/conversations"),
            PathItem::new(HttpMethod::Get, list),
        )
        .path(
            format!("{PROJECT}/assistant/conversations"),
            PathItem::new(HttpMethod::Post, create),
        )
        .path(
            format!("{PROJECT}/assistant/conversations/{{id}}"),
            PathItem::new(HttpMethod::Get, get),
        )
        .path(
            format!("{PROJECT}/assistant/conversations/{{id}}"),
            PathItem::new(HttpMethod::Patch, update),
        )
        .path(
            format!("{PROJECT}/assistant/conversations/{{id}}"),
            PathItem::new(HttpMethod::Delete, delete),
        )
        .path(
            format!("{PROJECT}/assistant/conversations/{{id}}/messages/{{seq}}/feedback"),
            PathItem::new(HttpMethod::Put, feedback),
        )
        .path(
            format!("{PROJECT}/assistant/conversations/{{id}}/turns"),
            PathItem::new(HttpMethod::Post, turn),
        )
}

/// The org's assistant setting and its test, and the panel's status (D-AS-3, D-AS-4, D-AS-24).
fn assistant_settings_paths(paths: PathsBuilder) -> PathsBuilder {
    let org = path_string("org", "Org UUID or slug.");
    let session = || Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]);
    let get = operation(
        "get_org_assistant",
        "assistant",
        "Get the organization's assistant setting",
    )
    .description(Some(
        "Organization members and above, and viewers. Answers `enabled: false` while the org never saved one (D-AS-3).",
    ))
    .parameters(Some(vec![org.clone()]))
    .responses(responses(
        "200",
        json_of::<AssistantSettings>("The setting."),
        &[401, 403, 404, 429],
    ))
    .build();
    let put = operation(
        "put_org_assistant",
        "assistant",
        "Save the organization's assistant setting",
    )
    .description(Some(
        "Organization admins and owners, with a session. Enabling needs a connection and a model, and runs the tool-calling test first; a failed test saves nothing (D-AS-3, D-AS-24). Disabling needs no test.",
    ))
    .securities(session())
    .parameters(Some(vec![org.clone()]))
    .request_body(Some(json_body("PutAssistantSettings")))
    .responses(responses(
        "200",
        json_of::<AssistantSettings>("The saved setting."),
        &[400, 401, 403, 404, 429, 503],
    ))
    .build();
    let test = operation(
        "post_org_assistant_test",
        "assistant",
        "Test a connection for the assistant",
    )
    .description(Some(
        "Organization admins and owners, with a session. Runs the tool-calling test on the connection and saves nothing (D-AS-24). A provider failure is `ok: false`, not an error status.",
    ))
    .securities(session())
    .parameters(Some(vec![org]))
    .request_body(Some(json_body("AssistantTestRequest")))
    .responses(responses(
        "200",
        json_of::<LlmConnectionTestResult>(
            "The outcome. A provider failure is ok: false, not an error status.",
        ),
        &[400, 401, 403, 404, 429, 503],
    ))
    .build();
    let status = operation(
        "get_project_assistant",
        "assistant",
        "Get whether the caller can use the assistant",
    )
    .description(Some(
        "Project members and above, with a session. Answers `usable` and, when it is false, the reason: `forbidden`, `restore_mode`, `disabled` or `no_connection` (D-AS-4, D-AS-16).",
    ))
    .securities(session())
    .parameters(Some(vec![project_param()]))
    .responses(responses(
        "200",
        json_of::<AssistantStatus>("The caller's status."),
        &[401, 403, 404, 429],
    ))
    .build();
    let mut setting = PathItem::new(HttpMethod::Get, get);
    setting.put = Some(put);
    paths
        .path("/api/v1/orgs/{org}/assistant", setting)
        .path(
            "/api/v1/orgs/{org}/assistant/test",
            PathItem::new(HttpMethod::Post, test),
        )
        .path(
            format!("{PROJECT}/assistant"),
            PathItem::new(HttpMethod::Get, status),
        )
}

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
