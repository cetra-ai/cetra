//! `mcp.*` actions: MCP tool calls that name a project (D-EA-22). The hash is set only for
//! `query_sql`.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "action", content = "details")]
pub(crate) enum McpAction {
    #[serde(rename = "mcp.tool_called")]
    #[serde(rename_all = "snake_case")]
    ToolCalled {
        tool: &'static str,
        query_sha256: Option<String>,
    },
}
