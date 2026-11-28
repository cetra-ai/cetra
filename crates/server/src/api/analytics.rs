//! Product analytics events from the web UI (track TL M6, spec 09 § Product analytics).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;

/// `POST /api/v1/analytics/events`. At most 50 events are read; the rest are dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AnalyticsEventsRequest {
    /// Catalog events of the web UI.
    pub events: Vec<AnalyticsEventInput>,
}

/// One web event. The server keeps it only when `name` is a web event of the catalog with
/// valid properties, and the user can read `org` and `project`.
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AnalyticsEventInput {
    /// A web event name of the catalog, e.g. `page.viewed`.
    pub name: String,
    /// The page's org, as in the URL (slug or id). Resolved by the server, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// The page's project, as in the URL (slug or id). Resolved by the server, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The event's properties; unknown keys are dropped.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schema(value_type = Object)]
    pub properties: Map<String, Value>,
}

/// Name and property count only: property values are not logged.
impl fmt::Debug for AnalyticsEventInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnalyticsEventInput")
            .field("name", &self.name)
            .field("properties", &self.properties.len())
            .finish_non_exhaustive()
    }
}
