//! The event catalog (D-TL-15). Names are dotted and append-only: a name is never renamed or reused, so a dashboard keeps its meaning. Properties are code-defined values (enums, booleans, our entity ids): never names, free text, URLs, filter values, attribute keys or payload. `fixtures/analytics/catalog.json` is the golden file; any change shows in review.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::api::account::OrgRole;
use crate::api::alerts::ChannelKind;
use crate::api::annotation_queues::QueueTarget;
use crate::api::connections::LlmAdapter;
use crate::api::datasets::{ExportFormat, ImportFormat};
use crate::api::evaluators::EvaluatorKind;
use crate::api::issues::IssueSourceKind;
use crate::api::keys::ApiKeyKind;
use crate::api::online_rules::RuleTarget;
use crate::api::prompts::PromptKind;
use crate::api::saved_views::SavedViewKind;

/// Where an event is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// The web UI, through `POST /api/v1/analytics/events`.
    Web,
    /// An API handler, whatever client called it.
    Server,
}

/// One catalog entry: what the golden file fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventSpec {
    /// The dotted name.
    pub(crate) name: &'static str,
    /// Where it is recorded. Read by the golden test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) source: Source,
    /// Its property keys, sorted. Read by the golden test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) properties: &'static [&'static str],
}

/// Context properties a sink may add to an event besides its own: `project_id` when the
/// event has a project, `via` for server events. Read by the golden test.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const CONTEXT_PROPERTIES: [&str; 2] = ["project_id", "via"];

/// Every event, sorted by name. Append-only.
pub(crate) const CATALOG: &[EventSpec] = &[
    spec("alert_rule.created", Source::Server, &[]),
    spec("annotation_queue.created", Source::Server, &["target"]),
    spec("annotation_queue.item_completed", Source::Server, &[]),
    spec("api_key.created", Source::Server, &["kind"]),
    spec("assistant.panel_opened", Source::Web, &[]),
    spec("assistant.turn_sent", Source::Server, &[]),
    spec("backfill.started", Source::Server, &[]),
    spec("channel.created", Source::Server, &["kind"]),
    spec("command_palette.opened", Source::Web, &[]),
    spec("dashboard.created", Source::Server, &[]),
    spec("dashboard.edited", Source::Server, &[]),
    spec("dataset.created", Source::Server, &[]),
    spec("dataset.exported", Source::Server, &["format"]),
    spec("dataset.imported", Source::Server, &["format"]),
    spec("dataset.items_added", Source::Server, &["source"]),
    spec("deletion.requested", Source::Server, &[]),
    spec("device.approved", Source::Server, &[]),
    spec("evaluator.created", Source::Server, &["kind"]),
    spec("evaluator.run", Source::Server, &[]),
    spec("experiment.created", Source::Server, &[]),
    spec("invite.created", Source::Server, &["role"]),
    spec("issue_source.created", Source::Server, &["kind"]),
    spec("list_layout.changed", Source::Web, &["layout"]),
    spec("live_tail.toggled", Source::Web, &["on"]),
    spec("llm_connection.created", Source::Server, &["adapter"]),
    spec("mcp.tool_called", Source::Server, &["ok", "tool"]),
    spec("online_rule.created", Source::Server, &["target"]),
    spec("org.created", Source::Server, &[]),
    spec("page.viewed", Source::Web, &["route"]),
    spec("personal_token.created", Source::Server, &["write"]),
    spec("playground.run", Source::Server, &[]),
    spec("project.created", Source::Server, &[]),
    spec("prompt.created", Source::Server, &["kind"]),
    spec("prompt.version_created", Source::Server, &[]),
    spec("saved_view.created", Source::Server, &["kind"]),
    spec("scores.created", Source::Server, &[]),
    spec("sql_console.run", Source::Server, &[]),
    spec("theme.changed", Source::Web, &["theme"]),
    spec("trace.viewed", Source::Server, &[]),
    spec("user.signed_in", Source::Server, &["method"]),
];

const fn spec(
    name: &'static str,
    source: Source,
    properties: &'static [&'static str],
) -> EventSpec {
    EventSpec {
        name,
        source,
        properties,
    }
}

/// The UI's page route templates: the TanStack Router ids of its leaf routes, sorted, except
/// the public pre-sign-in pages (`/login`, `/signup`, `/reset`, `/verify`, `/setup/`,
/// `/invite/$token`), which the client never reports. `page.viewed` accepts only these. A new
/// signed-in page adds its id here and to the golden file.
pub(crate) const PAGE_ROUTES: &[&str] = &[
    "/",
    "/$org/",
    "/$org/$project/",
    "/$org/$project/alerts/",
    "/$org/$project/alerts/$ruleId/",
    "/$org/$project/alerts/$ruleId/edit",
    "/$org/$project/alerts/new",
    "/$org/$project/annotation-queues/",
    "/$org/$project/annotation-queues/$queueId/",
    "/$org/$project/annotation-queues/$queueId/review/$itemId",
    "/$org/$project/dashboard/",
    "/$org/$project/dashboard/$dashboardId",
    "/$org/$project/datasets/",
    "/$org/$project/datasets/$datasetId",
    "/$org/$project/evaluators/",
    "/$org/$project/evaluators/$evaluatorId",
    "/$org/$project/evaluators/new",
    "/$org/$project/experiments/",
    "/$org/$project/experiments/$experimentId",
    "/$org/$project/experiments/compare",
    "/$org/$project/issue-sources/",
    "/$org/$project/issue-sources/$sourceId/",
    "/$org/$project/issue-sources/$sourceId/edit",
    "/$org/$project/issue-sources/new",
    "/$org/$project/issues/",
    "/$org/$project/issues/$issueId",
    "/$org/$project/online-rules/",
    "/$org/$project/online-rules/$ruleId/",
    "/$org/$project/online-rules/$ruleId/edit",
    "/$org/$project/online-rules/new",
    "/$org/$project/playground/",
    "/$org/$project/prompts/",
    "/$org/$project/prompts/$prompt",
    "/$org/$project/sessions/",
    "/$org/$project/sessions/$sessionId",
    "/$org/$project/settings/",
    "/$org/$project/settings/api-keys",
    "/$org/$project/settings/channels",
    "/$org/$project/settings/data",
    "/$org/$project/settings/general",
    "/$org/$project/settings/members",
    "/$org/$project/settings/models",
    "/$org/$project/settings/score-definitions",
    "/$org/$project/spans/",
    "/$org/$project/sql/",
    "/$org/$project/traces/",
    "/$org/$project/traces/$traceId",
    "/$org/$project/users/",
    "/$org/$project/users/$userId",
    "/$org/settings/",
    "/$org/settings/assistant",
    "/$org/settings/audit-log",
    "/$org/settings/general",
    "/$org/settings/llm-connections",
    "/$org/settings/members",
    "/$org/settings/models",
    "/$org/settings/plan",
    "/$org/settings/provisioning",
    "/$org/settings/sso",
    "/account",
    "/admin/",
    "/admin/audit-log",
    "/admin/settings",
    "/admin/users",
    "/demo",
    "/device",
    "/setup/workspace",
    "/sso/no-seat",
];

/// The traces list layout (`web/src/lib/list-layout.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ListLayout {
    Drawer,
    Bottom,
    Page,
}

/// The UI theme (`web/src/components/theme-provider.tsx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    Light,
    Dark,
    System,
}

/// How dataset items were added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemSource {
    /// `POST …/items`.
    Manual,
    /// `POST …/items/from-trace`.
    Trace,
    /// `POST …/items/from-traces`.
    Traces,
    /// `POST …/issues/{id}/dataset-items`.
    Issue,
}

/// How a session started (`auth::session::SessionOrigin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignInMethod {
    /// A password: login, signup, setup, or the first session after a reset.
    Password,
    /// GitHub, Google or an `[[auth.oidc]]` provider.
    Provider,
    /// An org's own identity provider (D-P-59).
    OrgProvider,
}

/// One product event. Field types are the property types; `&'static str` values come from
/// code-defined lists (`PAGE_ROUTES`, the MCP tool names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// An alert rule is created.
    AlertRuleCreated,
    /// An annotation queue is created.
    AnnotationQueueCreated { target: QueueTarget },
    /// A reviewer completes a queue item.
    AnnotationQueueItemCompleted,
    /// A project API key is created.
    ApiKeyCreated { kind: ApiKeyKind },
    /// The assistant panel opens.
    AssistantPanelOpened,
    /// A message is sent to the assistant.
    AssistantTurnSent,
    /// An online-rule backfill starts.
    BackfillStarted,
    /// A notification channel is created.
    ChannelCreated { kind: ChannelKind },
    /// The command palette opens.
    CommandPaletteOpened,
    /// A dashboard is created.
    DashboardCreated,
    /// A dashboard or the overview is saved.
    DashboardEdited,
    /// A dataset is created.
    DatasetCreated,
    /// A dataset export is downloaded.
    DatasetExported { format: ExportFormat },
    /// A file is imported into a dataset.
    DatasetImported { format: ImportFormat },
    /// Items are added to a dataset.
    DatasetItemsAdded { source: ItemSource },
    /// A GDPR deletion is requested.
    DeletionRequested,
    /// A device sign-in (`cetra login`, `cetra-cli init`) is approved.
    DeviceApproved,
    /// An evaluator is created.
    EvaluatorCreated { kind: EvaluatorKind },
    /// An evaluator run (eval job) starts.
    EvaluatorRun,
    /// An experiment is created.
    ExperimentCreated,
    /// An org invite is created.
    InviteCreated { role: OrgRole },
    /// An issue source is created.
    IssueSourceCreated { kind: IssueSourceKind },
    /// The list layout changes.
    ListLayoutChanged { layout: ListLayout },
    /// The Live switch of a list is flipped.
    LiveTailToggled { on: bool },
    /// An LLM connection is created.
    LlmConnectionCreated { adapter: LlmAdapter },
    /// An MCP tool call ends (not when cancelled).
    McpToolCalled { tool: &'static str, ok: bool },
    /// An online rule is created.
    OnlineRuleCreated { target: RuleTarget },
    /// An org is created.
    OrgCreated,
    /// The UI shows a page (path change).
    PageViewed { route: &'static str },
    /// A personal access token is created.
    PersonalTokenCreated { write: bool },
    /// A playground run starts.
    PlaygroundRun,
    /// A project is created.
    ProjectCreated,
    /// A prompt is created.
    PromptCreated { kind: PromptKind },
    /// A prompt version is created.
    PromptVersionCreated,
    /// A saved view or SQL query is saved.
    SavedViewCreated { kind: SavedViewKind },
    /// Scores are written through a project route.
    ScoresCreated,
    /// A SQL console query succeeds.
    SqlConsoleRun,
    /// The theme is picked in the menu.
    ThemeChanged { theme: Theme },
    /// A trace's detail is read.
    TraceViewed,
    /// A web session starts.
    UserSignedIn { method: SignInMethod },
}

impl Event {
    /// The dotted catalog name.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::AlertRuleCreated => "alert_rule.created",
            Self::AnnotationQueueCreated { .. } => "annotation_queue.created",
            Self::AnnotationQueueItemCompleted => "annotation_queue.item_completed",
            Self::ApiKeyCreated { .. } => "api_key.created",
            Self::AssistantPanelOpened => "assistant.panel_opened",
            Self::AssistantTurnSent => "assistant.turn_sent",
            Self::BackfillStarted => "backfill.started",
            Self::ChannelCreated { .. } => "channel.created",
            Self::CommandPaletteOpened => "command_palette.opened",
            Self::DashboardCreated => "dashboard.created",
            Self::DashboardEdited => "dashboard.edited",
            Self::DatasetCreated => "dataset.created",
            Self::DatasetExported { .. } => "dataset.exported",
            Self::DatasetImported { .. } => "dataset.imported",
            Self::DatasetItemsAdded { .. } => "dataset.items_added",
            Self::DeletionRequested => "deletion.requested",
            Self::DeviceApproved => "device.approved",
            Self::EvaluatorCreated { .. } => "evaluator.created",
            Self::EvaluatorRun => "evaluator.run",
            Self::ExperimentCreated => "experiment.created",
            Self::InviteCreated { .. } => "invite.created",
            Self::IssueSourceCreated { .. } => "issue_source.created",
            Self::ListLayoutChanged { .. } => "list_layout.changed",
            Self::LiveTailToggled { .. } => "live_tail.toggled",
            Self::LlmConnectionCreated { .. } => "llm_connection.created",
            Self::McpToolCalled { .. } => "mcp.tool_called",
            Self::OnlineRuleCreated { .. } => "online_rule.created",
            Self::OrgCreated => "org.created",
            Self::PageViewed { .. } => "page.viewed",
            Self::PersonalTokenCreated { .. } => "personal_token.created",
            Self::PlaygroundRun => "playground.run",
            Self::ProjectCreated => "project.created",
            Self::PromptCreated { .. } => "prompt.created",
            Self::PromptVersionCreated => "prompt.version_created",
            Self::SavedViewCreated { .. } => "saved_view.created",
            Self::ScoresCreated => "scores.created",
            Self::SqlConsoleRun => "sql_console.run",
            Self::ThemeChanged { .. } => "theme.changed",
            Self::TraceViewed => "trace.viewed",
            Self::UserSignedIn { .. } => "user.signed_in",
        }
    }

    /// Where the event is recorded.
    pub(crate) fn source(&self) -> Source {
        match self {
            Self::AssistantPanelOpened
            | Self::CommandPaletteOpened
            | Self::ListLayoutChanged { .. }
            | Self::LiveTailToggled { .. }
            | Self::PageViewed { .. }
            | Self::ThemeChanged { .. } => Source::Web,
            Self::AlertRuleCreated
            | Self::AnnotationQueueCreated { .. }
            | Self::AnnotationQueueItemCompleted
            | Self::ApiKeyCreated { .. }
            | Self::AssistantTurnSent
            | Self::BackfillStarted
            | Self::ChannelCreated { .. }
            | Self::DashboardCreated
            | Self::DashboardEdited
            | Self::DatasetCreated
            | Self::DatasetExported { .. }
            | Self::DatasetImported { .. }
            | Self::DatasetItemsAdded { .. }
            | Self::DeletionRequested
            | Self::DeviceApproved
            | Self::EvaluatorCreated { .. }
            | Self::EvaluatorRun
            | Self::ExperimentCreated
            | Self::InviteCreated { .. }
            | Self::IssueSourceCreated { .. }
            | Self::LlmConnectionCreated { .. }
            | Self::McpToolCalled { .. }
            | Self::OnlineRuleCreated { .. }
            | Self::OrgCreated
            | Self::PersonalTokenCreated { .. }
            | Self::PlaygroundRun
            | Self::ProjectCreated
            | Self::PromptCreated { .. }
            | Self::PromptVersionCreated
            | Self::SavedViewCreated { .. }
            | Self::ScoresCreated
            | Self::SqlConsoleRun
            | Self::TraceViewed
            | Self::UserSignedIn { .. } => Source::Server,
        }
    }

    /// The event's own properties, in the table's key order. Values are strings, booleans or
    /// numbers, never null.
    pub(crate) fn properties(&self) -> Map<String, Value> {
        let mut map = Map::new();
        match self {
            Self::AnnotationQueueCreated { target } => {
                map.insert("target".to_owned(), enum_value(target));
            }
            Self::ApiKeyCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::ChannelCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::DatasetExported { format } => {
                map.insert("format".to_owned(), enum_value(format));
            }
            Self::DatasetImported { format } => {
                map.insert("format".to_owned(), enum_value(format));
            }
            Self::DatasetItemsAdded { source } => {
                map.insert("source".to_owned(), enum_value(source));
            }
            Self::EvaluatorCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::InviteCreated { role } => {
                map.insert("role".to_owned(), enum_value(role));
            }
            Self::IssueSourceCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::ListLayoutChanged { layout } => {
                map.insert("layout".to_owned(), enum_value(layout));
            }
            Self::LiveTailToggled { on } => {
                map.insert("on".to_owned(), Value::Bool(*on));
            }
            Self::LlmConnectionCreated { adapter } => {
                map.insert("adapter".to_owned(), enum_value(adapter));
            }
            Self::McpToolCalled { tool, ok } => {
                map.insert("ok".to_owned(), Value::Bool(*ok));
                map.insert("tool".to_owned(), Value::from(*tool));
            }
            Self::OnlineRuleCreated { target } => {
                map.insert("target".to_owned(), enum_value(target));
            }
            Self::PageViewed { route } => {
                map.insert("route".to_owned(), Value::from(*route));
            }
            Self::PersonalTokenCreated { write } => {
                map.insert("write".to_owned(), Value::Bool(*write));
            }
            Self::PromptCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::SavedViewCreated { kind } => {
                map.insert("kind".to_owned(), enum_value(kind));
            }
            Self::ThemeChanged { theme } => {
                map.insert("theme".to_owned(), enum_value(theme));
            }
            Self::UserSignedIn { method } => {
                map.insert("method".to_owned(), enum_value(method));
            }
            Self::AlertRuleCreated
            | Self::AnnotationQueueItemCompleted
            | Self::AssistantPanelOpened
            | Self::AssistantTurnSent
            | Self::BackfillStarted
            | Self::CommandPaletteOpened
            | Self::DashboardCreated
            | Self::DashboardEdited
            | Self::DatasetCreated
            | Self::DeletionRequested
            | Self::DeviceApproved
            | Self::EvaluatorRun
            | Self::ExperimentCreated
            | Self::OrgCreated
            | Self::PlaygroundRun
            | Self::ProjectCreated
            | Self::PromptVersionCreated
            | Self::ScoresCreated
            | Self::SqlConsoleRun
            | Self::TraceViewed => {}
        }
        map
    }
}

/// A unit enum's serde string. Every property enum serializes to a string, so the
/// `Null` fallback is never taken; `property_values_are_scalars` checks it.
fn enum_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The catalog's `&'static` spelling of `name`, if it is a catalog name.
pub(crate) fn static_name(name: &str) -> Option<&'static str> {
    CATALOG
        .iter()
        .find(|spec| spec.name == name)
        .map(|spec| spec.name)
}

/// A web event from `POST /api/v1/analytics/events`: `None` for a server event's name, an
/// unknown name, a missing or mistyped property, or a route that is not in [`PAGE_ROUTES`].
/// Unknown properties are ignored. A stored route is the catalog's constant, never the input.
/// The route is not checked against the event's `org`/`project`: they are independent values.
pub(crate) fn parse_web(name: &str, properties: &Map<String, Value>) -> Option<Event> {
    match name {
        "page.viewed" => {
            let route = properties.get("route")?.as_str()?;
            let route = PAGE_ROUTES.iter().copied().find(|known| *known == route)?;
            Some(Event::PageViewed { route })
        }
        "command_palette.opened" => Some(Event::CommandPaletteOpened),
        "live_tail.toggled" => Some(Event::LiveTailToggled {
            on: properties.get("on")?.as_bool()?,
        }),
        "list_layout.changed" => Some(Event::ListLayoutChanged {
            layout: ListLayout::deserialize(properties.get("layout")?).ok()?,
        }),
        "theme.changed" => Some(Event::ThemeChanged {
            theme: Theme::deserialize(properties.get("theme")?).ok()?,
        }),
        "assistant.panel_opened" => Some(Event::AssistantPanelOpened),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// One sample of each of the 40 variants.
    fn every_event() -> Vec<Event> {
        vec![
            Event::AlertRuleCreated,
            Event::AnnotationQueueCreated {
                target: QueueTarget::Trace,
            },
            Event::AnnotationQueueItemCompleted,
            Event::ApiKeyCreated {
                kind: ApiKeyKind::Secret,
            },
            Event::AssistantPanelOpened,
            Event::AssistantTurnSent,
            Event::BackfillStarted,
            Event::ChannelCreated {
                kind: ChannelKind::Webhook,
            },
            Event::CommandPaletteOpened,
            Event::DashboardCreated,
            Event::DashboardEdited,
            Event::DatasetCreated,
            Event::DatasetExported {
                format: ExportFormat::Jsonl,
            },
            Event::DatasetImported {
                format: ImportFormat::Csv,
            },
            Event::DatasetItemsAdded {
                source: ItemSource::Manual,
            },
            Event::DeletionRequested,
            Event::DeviceApproved,
            Event::EvaluatorCreated {
                kind: EvaluatorKind::LlmJudge,
            },
            Event::EvaluatorRun,
            Event::ExperimentCreated,
            Event::InviteCreated {
                role: OrgRole::Member,
            },
            Event::IssueSourceCreated {
                kind: IssueSourceKind::Error,
            },
            Event::ListLayoutChanged {
                layout: ListLayout::Bottom,
            },
            Event::LiveTailToggled { on: true },
            Event::LlmConnectionCreated {
                adapter: LlmAdapter::Openai,
            },
            Event::McpToolCalled {
                tool: "query_sql",
                ok: true,
            },
            Event::OnlineRuleCreated {
                target: RuleTarget::Trace,
            },
            Event::OrgCreated,
            Event::PageViewed {
                route: PAGE_ROUTES[0],
            },
            Event::PersonalTokenCreated { write: false },
            Event::PlaygroundRun,
            Event::ProjectCreated,
            Event::PromptCreated {
                kind: PromptKind::Text,
            },
            Event::PromptVersionCreated,
            Event::SavedViewCreated {
                kind: SavedViewKind::Sql,
            },
            Event::ScoresCreated,
            Event::SqlConsoleRun,
            Event::ThemeChanged {
                theme: Theme::System,
            },
            Event::TraceViewed,
            Event::UserSignedIn {
                method: SignInMethod::Password,
            },
        ]
    }

    fn props(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn is_dotted_name(name: &str) -> bool {
        let Some((left, right)) = name.split_once('.') else {
            return false;
        };
        let part = |text: &str| {
            let mut bytes = text.bytes();
            matches!(bytes.next(), Some(b'a'..=b'z'))
                && bytes.all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        };
        part(left) && part(right)
    }

    #[test]
    fn the_catalog_matches_the_golden_file() {
        let golden: Value =
            serde_json::from_str(include_str!("../../../../fixtures/analytics/catalog.json"))
                .expect("the golden file is JSON");
        let events: Vec<Value> = CATALOG
            .iter()
            .map(|spec| json!({"name": spec.name, "source": spec.source, "properties": spec.properties}))
            .collect();
        let expected = json!({
            "events": events,
            "context": CONTEXT_PROPERTIES,
            "page_routes": PAGE_ROUTES,
        });
        assert_eq!(
            golden, expected,
            "update fixtures/analytics/catalog.json and the web catalog together"
        );
    }

    #[test]
    fn catalog_names_are_sorted_unique_and_dotted() {
        assert_eq!(CATALOG.len(), 40);
        for pair in CATALOG.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} < {}",
                pair[0].name,
                pair[1].name
            );
        }
        for spec in CATALOG {
            assert!(is_dotted_name(spec.name), "bad name {}", spec.name);
            assert!(
                spec.properties.windows(2).all(|pair| pair[0] < pair[1]),
                "properties of {} are not sorted",
                spec.name
            );
        }
    }

    #[test]
    fn every_variant_is_in_the_catalog() {
        let events = every_event();
        assert_eq!(events.len(), 40);
        let mut names: Vec<&str> = events.iter().map(Event::name).collect();
        names.sort_unstable();
        let catalog: Vec<&str> = CATALOG.iter().map(|spec| spec.name).collect();
        assert_eq!(names, catalog);
    }

    #[test]
    fn properties_match_the_spec() {
        for event in every_event() {
            let spec = CATALOG
                .iter()
                .find(|spec| spec.name == event.name())
                .expect("every variant is in the catalog");
            assert_eq!(event.source(), spec.source, "source of {}", spec.name);
            let mut keys: Vec<String> = event.properties().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, spec.properties, "properties of {}", spec.name);
        }
    }

    #[test]
    fn property_values_are_scalars() {
        for event in every_event() {
            for (key, value) in event.properties() {
                assert!(
                    value.is_string() || value.is_boolean() || value.is_number(),
                    "{} {key} is not a scalar",
                    event.name()
                );
            }
        }
    }

    #[test]
    fn page_routes_are_sorted_unique_templates() {
        assert_eq!(PAGE_ROUTES.len(), 68);
        for pair in PAGE_ROUTES.windows(2) {
            assert!(pair[0] < pair[1], "{} < {}", pair[0], pair[1]);
        }
        for public in [
            "/login",
            "/signup",
            "/reset",
            "/verify",
            "/setup/",
            "/invite/$token",
        ] {
            assert!(!PAGE_ROUTES.contains(&public), "{public} is public");
        }
        for route in PAGE_ROUTES {
            assert!(route.starts_with('/'), "{route}");
            assert!(!route.contains("//"), "{route}");
            assert!(!route.contains('?') && !route.contains('#'), "{route}");
            for segment in route.split('/').filter(|segment| !segment.is_empty()) {
                let valid = match segment.strip_prefix('$') {
                    Some(param) => {
                        !param.is_empty() && param.bytes().all(|byte| byte.is_ascii_alphabetic())
                    }
                    None => segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    }),
                };
                assert!(valid, "bad segment {segment} in {route}");
            }
        }
    }

    #[test]
    fn parse_web_accepts_each_web_event() {
        let cases = [
            (
                "page.viewed",
                json!({"route": "/$org/$project/traces/"}),
                Event::PageViewed {
                    route: "/$org/$project/traces/",
                },
            ),
            (
                "command_palette.opened",
                json!({}),
                Event::CommandPaletteOpened,
            ),
            (
                "live_tail.toggled",
                json!({"on": true}),
                Event::LiveTailToggled { on: true },
            ),
            (
                "list_layout.changed",
                json!({"layout": "bottom"}),
                Event::ListLayoutChanged {
                    layout: ListLayout::Bottom,
                },
            ),
            (
                "theme.changed",
                json!({"theme": "system"}),
                Event::ThemeChanged {
                    theme: Theme::System,
                },
            ),
            (
                "assistant.panel_opened",
                json!({}),
                Event::AssistantPanelOpened,
            ),
        ];
        for (name, properties, expected) in cases {
            assert_eq!(
                parse_web(name, &props(properties)),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn parse_web_refuses_server_and_unknown_names() {
        let empty = Map::new();
        for name in [
            "dataset.created",
            "org.created",
            "user.signed_in",
            "custom.event",
            "",
            "PAGE.VIEWED",
        ] {
            assert_eq!(parse_web(name, &empty), None, "{name}");
        }
    }

    #[test]
    fn parse_web_refuses_a_url_as_route() {
        for route in [
            "/acme/support-bot/traces",
            "/$org/$project/traces",
            "https://cetra.example/acme",
            "/$org/$project/traces/?q=ada@example.com",
        ] {
            let properties = props(json!({"route": route}));
            assert_eq!(parse_web("page.viewed", &properties), None, "{route}");
        }
    }

    #[test]
    fn parse_web_drops_unknown_properties() {
        let properties = props(json!({
            "route": "/$org/$project/traces/",
            "email": "ada@example.com",
            "note": "free text",
        }));
        let event = parse_web("page.viewed", &properties).expect("a valid page view");
        let kept = event.properties();
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key("route"));
        let text = Value::Object(kept).to_string();
        assert!(!text.contains("ada@"));
        assert!(!text.contains("free text"));
    }

    #[test]
    fn parse_web_refuses_wrong_types() {
        let cases = [
            ("live_tail.toggled", json!({"on": "true"})),
            ("live_tail.toggled", json!({})),
            ("list_layout.changed", json!({"layout": "grid"})),
            ("theme.changed", json!({"theme": 1})),
            ("page.viewed", json!({"route": 5})),
        ];
        for (name, properties) in cases {
            assert_eq!(parse_web(name, &props(properties)), None, "{name}");
        }
    }

    #[test]
    fn the_route_value_is_the_catalog_constant() {
        // The input is a heap string, dropped with its event before the route is read: the
        // stored route outlives it, so it is not a borrow of the input.
        let event = {
            let properties = props(json!({"route": String::from("/$org/")}));
            parse_web("page.viewed", &properties)
        };
        match event {
            Some(Event::PageViewed { route }) => {
                assert_eq!(route, "/$org/");
                // By value: a `const` is copied into every codegen unit that reads it, so two
                // spellings of the same entry need not share an address.
                assert_eq!(route, PAGE_ROUTES[1]);
                assert!(PAGE_ROUTES.contains(&route));
            }
            other => panic!("expected a page view, got {other:?}"),
        }
    }

    #[test]
    fn static_name_finds_catalog_names() {
        assert_eq!(static_name("trace.viewed"), Some("trace.viewed"));
        assert_eq!(static_name("trace.view"), None);
    }

    #[test]
    fn the_telemetry_page_lists_every_event() {
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../website/src/content/docs/docs/self-hosting/telemetry.md"
        ))
        .expect("the telemetry page exists");
        for spec in CATALOG {
            let backticked = format!("`{}`", spec.name);
            assert!(
                page.contains(&backticked),
                "the telemetry page lacks {backticked}"
            );
        }
    }
}
