//! `GET /api/internal/v1/deletions`: the control plane reads the applied project and org drops,
//! oldest first (D-M-17, D-P-49).

use axum::extract::State;
use axum::routing::get;

use crate::api::common::Page;
use crate::api::internal::{InternalDrop, InternalDropKind, InternalDropsQuery};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::deletions::{AppliedDrop, DeletionKind, applied_drops};
use crate::routes::time_cursor::{decode_time_cursor, encode_time_cursor};

const DEFAULT_LIMIT: u32 = 1_000;
const MAX_LIMIT: u32 = 10_000;

/// The internal deletions route. The control plane holds a `control_plane` token with scope
/// `deletions`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::GET,
        path: "/api/internal/v1/deletions",
        access: Access::ControlPlane { scope: "deletions" },
        documented: true,
        handler: get(list_drops),
    }]
}

/// `GET /api/internal/v1/deletions`. Applied drops ordered by `(applied_at, id)`, keyset-paged.
async fn list_drops(
    State(state): State<AppState>,
    error::Query(params): error::Query<InternalDropsQuery>,
) -> Result<error::Json<Page<InternalDrop>>, ApiError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(field_error("limit", "between 1 and 10000"));
    }
    let after = params
        .cursor
        .as_deref()
        .map(decode_time_cursor)
        .transpose()?;
    let kind = params.kind.map(|kind| match kind {
        InternalDropKind::Project => DeletionKind::Project,
        InternalDropKind::Org => DeletionKind::Org,
    });
    // One row more than the page tells whether a next page exists.
    let mut rows = applied_drops(state.meta.reader(), kind, after, limit + 1).await?;
    let page_size = usize::try_from(limit).unwrap_or(usize::MAX);
    let has_next = rows.len() > page_size;
    rows.truncate(page_size);
    let next_cursor = match (has_next, rows.last()) {
        (true, Some(last)) => Some(encode_time_cursor(last.applied_at, &last.id.to_string())?),
        _ => None,
    };
    Ok(error::Json(Page {
        data: rows.iter().map(drop_json).collect(),
        next_cursor,
    }))
}

fn drop_json(row: &AppliedDrop) -> InternalDrop {
    InternalDrop {
        id: row.id,
        kind: match row.kind {
            DeletionKind::Project => InternalDropKind::Project,
            _ => InternalDropKind::Org,
        },
        org_id: row.org_id.as_uuid(),
        project_id: row.project_id.map(|project| project.as_uuid()),
        applied_at: row.applied_at,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Method;
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;
    use crate::meta::deletions::{
        ActorKind, DeletionState, NewDeletion, RequestDetails, insert_deletion, mark_applied,
    };
    use crate::test_support::{TestApp, TestResponse};
    use llmobs_core::ids::{OrgId, ProjectId};

    /// Inserts one deletion of `kind` in `project` (a project for `project` and `trace_ids`), and
    /// applies it at `applied_at` when given. Returns its id.
    async fn insert(
        app: &TestApp,
        kind: DeletionKind,
        project: Option<ProjectId>,
        applied_at: Option<i64>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let state = match kind {
            DeletionKind::Project | DeletionKind::Org => DeletionState::Pending,
            _ => DeletionState::Resolving,
        };
        let new = NewDeletion {
            id,
            org_id: OrgId::new_v7(),
            project_id: project,
            kind,
            user_id: None,
            request: RequestDetails::default(),
            actor_kind: ActorKind::User,
            actor_id: Uuid::now_v7(),
            state,
            created_at: 1,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_deletion(conn, &new).await?;
                    if let Some(at) = applied_at {
                        mark_applied(conn, id, at).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        id
    }

    /// `GET /api/internal/v1/deletions{query}` with a control-plane token of `scope`.
    async fn list(app: &TestApp, scope: &str, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("/api/internal/v1/deletions{query}"))
            .control_plane(scope)
            .send()
            .await
    }

    /// The ids of a page's rows, in order.
    fn ids(page: &Value) -> Vec<String> {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Two applied drops (a project at 10, an org at 20), an applied `trace_ids` deletion at 15
    /// and a pending org drop. Returns the project drop's and the org drop's ids.
    async fn seed_drops(app: &TestApp) -> (Uuid, Uuid) {
        let project = insert(
            app,
            DeletionKind::Project,
            Some(ProjectId::new_v7()),
            Some(10),
        )
        .await;
        let org = insert(app, DeletionKind::Org, None, Some(20)).await;
        insert(
            app,
            DeletionKind::TraceIds,
            Some(ProjectId::new_v7()),
            Some(15),
        )
        .await;
        insert(app, DeletionKind::Org, None, None).await;
        (project, org)
    }

    #[tokio::test]
    async fn lists_applied_drops_oldest_first() {
        let app = TestApp::new().await;
        let (project, org) = seed_drops(&app).await;

        let response = list(&app, "deletions", "").await;
        assert_eq!(response.status, 200);
        let page: Value = response.json();
        assert_eq!(ids(&page), vec![project.to_string(), org.to_string()]);
        assert_eq!(page["data"][0]["kind"], "project");
        assert!(page["data"][0]["project_id"].is_string());
        assert_eq!(page["data"][1]["kind"], "org");
        assert!(page["data"][1].get("project_id").is_none());
        assert!(page["data"][1]["applied_at"].is_string());
        assert!(page["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn kind_narrows_the_feed() {
        let app = TestApp::new().await;
        let (_, org) = seed_drops(&app).await;

        let page: Value = list(&app, "deletions", "?kind=org").await.json();
        assert_eq!(ids(&page), vec![org.to_string()]);
    }

    #[tokio::test]
    async fn pages_follow_the_cursor_to_the_end() {
        let app = TestApp::new().await;
        let (project, org) = seed_drops(&app).await;

        let first: Value = list(&app, "deletions", "?limit=1").await.json();
        assert_eq!(ids(&first), vec![project.to_string()]);
        let cursor = first["next_cursor"]
            .as_str()
            .expect("a next page")
            .to_owned();

        let second: Value = list(&app, "deletions", &format!("?limit=1&cursor={cursor}"))
            .await
            .json();
        assert_eq!(ids(&second), vec![org.to_string()]);
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn refuses_bad_input_and_other_scopes() {
        let app = TestApp::new().await;
        seed_drops(&app).await;

        assert_eq!(list(&app, "deletions", "?cursor=garbage").await.status, 400);
        assert_eq!(list(&app, "deletions", "?kind=user").await.status, 400);
        assert_eq!(list(&app, "deletions", "?limit=0").await.status, 400);
        assert_eq!(list(&app, "deletions", "?limit=10001").await.status, 400);
        assert_eq!(list(&app, "orgs", "").await.status, 403);
    }

    #[tokio::test]
    async fn the_feed_is_off_without_the_control_plane() {
        let app = TestApp::with_config(|c| c.entitlements.control_plane = false).await;
        seed_drops(&app).await;

        assert_eq!(list(&app, "deletions", "").await.status, 404);
    }
}
