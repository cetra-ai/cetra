//! Issue source API types (spec 09 § Issues, D-IC-1, D-IC-3, D-IC-13).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr};
use utoipa::{PartialSchema, ToSchema};
use uuid::Uuid;

use super::online_rules::present;

/// What an issue source reads. M3 adds kinds (D-IC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSourceKind {
    /// Error spans of completed traces, fingerprinted into issues (D-IC-2).
    Error,
}

/// An issue source. Spec 09 § Issues.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IssueSource {
    /// Source id.
    pub id: Uuid,
    /// The project the source belongs to.
    pub project_id: Uuid,
    /// Display name, unique in the project. 1 to 128 characters.
    pub name: String,
    /// What the source reads. Immutable.
    pub kind: IssueSourceKind,
    /// Spans filter for `error` sources. `null` when there is none.
    pub filter: Option<String>,
    /// Whether the source runs. A disabled source counts for no plan limit.
    pub enabled: bool,
    /// Why the sweeper skips the source. `null` when it runs.
    pub paused_reason: Option<String>,
    /// User who created the source. `null` when that user is unknown.
    pub created_by: Option<Uuid>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
    /// Issues of this source that are open now.
    pub open_issues: u64,
}

/// `GET /api/v1/projects/{project}/issue-sources` answer. It carries the org's enabled-source
/// count (D-IC-13).
///
/// Its schema is written by hand, like `OnlineRulePage`'s, so that `org_active_sources` is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueSourcePage {
    /// Sources in this page.
    pub data: Vec<IssueSource>,
    /// Opaque cursor for the next page, or null when this is the last page.
    pub next_cursor: Option<String>,
    /// Enabled sources of the whole org, counted for the plan's active-source limit.
    pub org_active_sources: u64,
}

impl PartialSchema for IssueSourcePage {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .description(Some(
                "One page of issue sources. `next_cursor` is null when there is no next page.",
            ))
            .property(
                "data",
                ArrayBuilder::new()
                    .description(Some("Sources in this page."))
                    .items(Ref::from_schema_name(IssueSource::name())),
            )
            .required("data")
            .property(
                "next_cursor",
                ObjectBuilder::new()
                    .schema_type(SchemaType::from_iter([Type::String, Type::Null]))
                    .description(Some(
                        "Opaque cursor for the next page, or null when this is the last page.",
                    )),
            )
            .required("next_cursor")
            .property(
                "org_active_sources",
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .minimum(Some(0))
                    .description(Some(
                        "Enabled sources of the whole org, counted for the plan's active-source limit.",
                    )),
            )
            .required("org_active_sources")
            .into()
    }
}

impl ToSchema for IssueSourcePage {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("IssueSourcePage")
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        schemas.push((IssueSource::name().into_owned(), IssueSource::schema()));
        IssueSource::schemas(schemas);
    }
}

/// `POST /api/v1/projects/{project}/issue-sources` body. Absent optional fields take their defaults.
///
/// Optional fields refuse `null` (spec 09 § Conventions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueSourceRequest {
    /// Display name, 1 to 128 characters after trimming.
    pub name: String,
    /// What the source reads.
    pub kind: IssueSourceKind,
    /// Spans filter. Absent means no filter.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub filter: Option<String>,
    /// Whether the source runs. Default true.
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// `PATCH /api/v1/projects/{project}/issue-sources/{id}` body. Absent means unchanged.
///
/// There is no `kind`: sending it is a 400 (unknown field). Optional fields refuse `null`,
/// except `filter`, where `null` clears it.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueSourceRequest {
    /// Display name, 1 to 128 characters after trimming.
    #[serde(default, deserialize_with = "present")]
    pub name: Option<String>,
    /// Spans filter. `null` or `""` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub filter: Option<Option<String>>,
    /// Turns the source on or off.
    #[serde(default, deserialize_with = "present")]
    pub enabled: Option<bool>,
}

fn yes() -> bool {
    true
}
