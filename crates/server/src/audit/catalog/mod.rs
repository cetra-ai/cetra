//! The audit catalog (D-EA-4): one enum per name prefix, each in its own file.
//!
//! Names are dotted and append-only. A variant's `rename` is its stored name, so renaming
//! a variant in Rust does not change the record.

mod alert_channel;
mod alert_rule;
mod annotation_queue;
mod api_key;
mod assistant;
mod audit;
mod auth;
mod dashboard;
mod dataset;
mod deletion;
mod evaluator;
mod instance;
mod invite;
mod issue_source;
mod llm_connection;
mod mcp;
mod member;
mod model_price;
mod online_rule;
mod org;
mod pat;
mod project;
mod prompt;
mod scim;
mod score_definition;
mod sql;
mod sso;
mod user;

pub(crate) use alert_channel::AlertChannelAction;
pub(crate) use alert_rule::AlertRuleAction;
pub(crate) use annotation_queue::AnnotationQueueAction;
pub(crate) use api_key::ApiKeyAction;
pub(crate) use assistant::AssistantAction;
pub(crate) use audit::AuditAction;
pub(crate) use auth::{AuthAction, LoginMethod, SignupMethod};
pub(crate) use dashboard::DashboardAction;
pub(crate) use dataset::DatasetAction;
pub(crate) use deletion::DeletionAction;
pub(crate) use evaluator::EvaluatorAction;
pub(crate) use instance::InstanceAction;
pub(crate) use invite::InviteAction;
pub(crate) use issue_source::IssueSourceAction;
pub(crate) use llm_connection::LlmConnectionAction;
pub(crate) use mcp::McpAction;
pub(crate) use member::{JoinVia, MemberAction};
pub(crate) use model_price::ModelPriceAction;
pub(crate) use online_rule::OnlineRuleAction;
pub(crate) use org::OrgAction;
pub(crate) use pat::PatAction;
pub(crate) use project::ProjectAction;
pub(crate) use prompt::PromptAction;
pub(crate) use scim::ScimAction;
pub(crate) use score_definition::ScoreDefinitionAction;
pub(crate) use sql::SqlAction;
pub(crate) use sso::SsoAction;
pub(crate) use user::UserAction;

/// Every audit action (D-EA-4). Serializes as `{"action": "<group>.<name>", "details": {…}}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum Action {
    Auth(AuthAction),
    Org(OrgAction),
    Project(ProjectAction),
    Member(MemberAction),
    Invite(InviteAction),
    ApiKey(ApiKeyAction),
    Pat(PatAction),
    Sso(SsoAction),
    Scim(ScimAction),
    User(UserAction),
    Instance(InstanceAction),
    Deletion(DeletionAction),
    Evaluator(EvaluatorAction),
    OnlineRule(OnlineRuleAction),
    ScoreDefinition(ScoreDefinitionAction),
    AlertRule(AlertRuleAction),
    AlertChannel(AlertChannelAction),
    LlmConnection(LlmConnectionAction),
    ModelPrice(ModelPriceAction),
    Assistant(AssistantAction),
    Prompt(PromptAction),
    Dataset(DatasetAction),
    IssueSource(IssueSourceAction),
    Dashboard(DashboardAction),
    AnnotationQueue(AnnotationQueueAction),
    Sql(SqlAction),
    Mcp(McpAction),
    Audit(AuditAction),
}

impl From<AuthAction> for Action {
    fn from(action: AuthAction) -> Self {
        Self::Auth(action)
    }
}

impl From<OrgAction> for Action {
    fn from(action: OrgAction) -> Self {
        Self::Org(action)
    }
}

impl From<ProjectAction> for Action {
    fn from(action: ProjectAction) -> Self {
        Self::Project(action)
    }
}

impl From<MemberAction> for Action {
    fn from(action: MemberAction) -> Self {
        Self::Member(action)
    }
}

impl From<InviteAction> for Action {
    fn from(action: InviteAction) -> Self {
        Self::Invite(action)
    }
}

impl From<ApiKeyAction> for Action {
    fn from(action: ApiKeyAction) -> Self {
        Self::ApiKey(action)
    }
}

impl From<PatAction> for Action {
    fn from(action: PatAction) -> Self {
        Self::Pat(action)
    }
}

impl From<SsoAction> for Action {
    fn from(action: SsoAction) -> Self {
        Self::Sso(action)
    }
}

impl From<ScimAction> for Action {
    fn from(action: ScimAction) -> Self {
        Self::Scim(action)
    }
}

impl From<UserAction> for Action {
    fn from(action: UserAction) -> Self {
        Self::User(action)
    }
}

impl From<InstanceAction> for Action {
    fn from(action: InstanceAction) -> Self {
        Self::Instance(action)
    }
}

impl From<DeletionAction> for Action {
    fn from(action: DeletionAction) -> Self {
        Self::Deletion(action)
    }
}

impl From<EvaluatorAction> for Action {
    fn from(action: EvaluatorAction) -> Self {
        Self::Evaluator(action)
    }
}

impl From<OnlineRuleAction> for Action {
    fn from(action: OnlineRuleAction) -> Self {
        Self::OnlineRule(action)
    }
}

impl From<ScoreDefinitionAction> for Action {
    fn from(action: ScoreDefinitionAction) -> Self {
        Self::ScoreDefinition(action)
    }
}

impl From<AlertRuleAction> for Action {
    fn from(action: AlertRuleAction) -> Self {
        Self::AlertRule(action)
    }
}

impl From<AlertChannelAction> for Action {
    fn from(action: AlertChannelAction) -> Self {
        Self::AlertChannel(action)
    }
}

impl From<LlmConnectionAction> for Action {
    fn from(action: LlmConnectionAction) -> Self {
        Self::LlmConnection(action)
    }
}

impl From<ModelPriceAction> for Action {
    fn from(action: ModelPriceAction) -> Self {
        Self::ModelPrice(action)
    }
}

impl From<AssistantAction> for Action {
    fn from(action: AssistantAction) -> Self {
        Self::Assistant(action)
    }
}

impl From<PromptAction> for Action {
    fn from(action: PromptAction) -> Self {
        Self::Prompt(action)
    }
}

impl From<DatasetAction> for Action {
    fn from(action: DatasetAction) -> Self {
        Self::Dataset(action)
    }
}

impl From<IssueSourceAction> for Action {
    fn from(action: IssueSourceAction) -> Self {
        Self::IssueSource(action)
    }
}

impl From<DashboardAction> for Action {
    fn from(action: DashboardAction) -> Self {
        Self::Dashboard(action)
    }
}

impl From<AnnotationQueueAction> for Action {
    fn from(action: AnnotationQueueAction) -> Self {
        Self::AnnotationQueue(action)
    }
}

impl From<SqlAction> for Action {
    fn from(action: SqlAction) -> Self {
        Self::Sql(action)
    }
}

impl From<McpAction> for Action {
    fn from(action: McpAction) -> Self {
        Self::Mcp(action)
    }
}

impl From<AuditAction> for Action {
    fn from(action: AuditAction) -> Self {
        Self::Audit(action)
    }
}
