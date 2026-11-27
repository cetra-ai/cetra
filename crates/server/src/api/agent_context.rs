//! The body of `POST /v1/agent-context`, which `cetra agents hook` sends (D-CA-9).

use serde::Deserialize;
use utoipa::ToSchema;

use super::online_rules::present;

/// A coding agent's session context, sent by `cetra agents hook` (D-CA-9).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentContextBody {
    /// `claude-code`, `codex`, `gemini-cli`, `copilot` or `qwen-code`.
    pub agent: String,
    /// The id the agent puts on its spans, 1 to 256 characters.
    pub session_id: String,
    /// The `origin` URL without user-info. Absent keeps the stored value; `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub repository: Option<Option<String>>,
    /// The branch, 1 to 256 characters. Absent keeps; `null` clears.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub branch: Option<Option<String>>,
    /// The person, 1 to 256 characters, lowercased by the server. Absent keeps; `null` clears.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub user: Option<Option<String>>,
}
