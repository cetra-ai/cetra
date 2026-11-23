// SPDX-License-Identifier: Elastic-2.0
//! The audit log's read API and exports (track EA M3, D-EA-7 to D-EA-10, D-EA-19).
//!
//! An org's log is read by its admins and owners (`OrgPerm::ViewAudit`) with the `audit_log`
//! feature. The instance log (`org_id` null) is read by instance admins with the licence or
//! on a cloud cell (plan P-5).

pub(crate) mod export;
pub(crate) mod read;

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::routing::get;

use self::read::Scope;
use crate::api::audit::{AuditEvent, AuditListParams};
use crate::api::common::Page;
use crate::api::entitlements::Feature;
use crate::auth::roles::OrgPerm;
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};

/// The audit routes: the org and instance lists and their exports.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/audit-events",
            access: Access::Org {
                perm: OrgPerm::ViewAudit,
                session_only: false,
            },
            documented: true,
            handler: get(list_org),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/audit-events/export",
            access: Access::Org {
                perm: OrgPerm::ViewAudit,
                session_only: false,
            },
            documented: true,
            handler: get(export::export_org),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/admin/audit-events",
            access: Access::InstanceAdmin,
            documented: true,
            handler: get(list_instance),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/admin/audit-events/export",
            access: Access::InstanceAdmin,
            documented: true,
            handler: get(export::export_instance),
        },
    ]
}

/// Whether the instance log may be read: the licence lists `audit_log` (in force, active or
/// in grace), or this is a cloud cell, whose log needs no licence (plan P-5, D-EA-19).
pub(crate) fn instance_gate(state: &AppState, now: i64) -> bool {
    state.config.entitlements.control_plane
        || state
            .entitlements
            .license()
            .state
            .active_features(now)
            .contains(Feature::AuditLog)
}

/// [`instance_gate`] as a result: `plan_required` when it is closed.
///
/// # Errors
/// [`ApiError::PlanRequired`] when the gate is closed.
pub(crate) fn require_instance(state: &AppState) -> Result<(), ApiError> {
    if instance_gate(state, state.clock.now_ns()) {
        Ok(())
    } else {
        Err(ApiError::PlanRequired {
            detail: Feature::AuditLog.locked_detail(),
        })
    }
}

/// `GET /api/v1/orgs/{org}/audit-events`: the org's records, newest first.
async fn list_org(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<AuditListParams>,
) -> Result<error::Json<Page<AuditEvent>>, ApiError> {
    let org = access.org.id;
    require(&state, org, Feature::AuditLog)?;
    list(&state, Scope::Org(org), &params).await
}

/// `GET /api/v1/admin/audit-events`: the instance's records, newest first.
async fn list_instance(
    State(state): State<AppState>,
    error::Query(params): error::Query<AuditListParams>,
) -> Result<error::Json<Page<AuditEvent>>, ApiError> {
    require_instance(&state)?;
    list(&state, Scope::Instance, &params).await
}

/// One page of `scope`, filtered and resolved (plan P-1, P-3).
async fn list(
    state: &AppState,
    scope: Scope,
    params: &AuditListParams,
) -> Result<error::Json<Page<AuditEvent>>, ApiError> {
    let filters = read::filters(
        params.from.as_ref(),
        params.to.as_ref(),
        params.action.as_deref(),
        params.actor_id.as_deref(),
        params.target_id.as_deref(),
        params.project_id,
        params.outcome,
    )?;
    let before = params
        .cursor
        .as_deref()
        .map(read::decode_cursor)
        .transpose()?;
    let limit = params.limit.get();
    // One row more than the page shows tells whether a next page exists.
    let mut rows = read::page(state.meta.reader(), &scope, &filters, before, limit + 1).await?;
    let next_cursor = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last().map(|row| read::encode_cursor(row.seq))
    } else {
        None
    };
    let data = read::resolve(state.meta.reader(), &scope, &rows).await?;
    Ok(error::Json(Page { data, next_cursor }))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use base64::Engine;
    use cetra_core::ids::OrgId;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::api::account::OrgRole;
    use crate::api::audit::{AuditEvent, AuditOutcome, AuditRefStatus};
    use crate::api::common::{Page, ProblemCode};
    use crate::audit::catalog::{
        ApiKeyAction, DatasetAction, InstanceAction, JoinVia, MemberAction,
    };
    use crate::audit::{Actor, Ctx, Event, TargetKind, Via, record};
    use crate::test_support::{Seeded, TestApp, TestResponse};

    /// 2026-01-01T00:00:00Z in epoch milliseconds: the base of the test times.
    const BASE_MS: i64 = 1_767_225_600_000;

    /// `ms` milliseconds after [`BASE_MS`], in nanoseconds.
    fn at_ms(ms: i64) -> i64 {
        (BASE_MS + ms) * 1_000_000
    }

    /// The app, with `features` licensed (none means no licence), and `seed()`.
    async fn app_with(features: &[&str]) -> (TestApp, Seeded) {
        let app = if features.is_empty() {
            TestApp::new().await
        } else {
            TestApp::with_license(features).await
        };
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// Ada (the seed's owner and instance admin) acting through a session.
    fn ada_ctx(seeded: &Seeded) -> Ctx {
        Ctx {
            actor: Actor::User {
                id: seeded.user_id,
                via: Via::Session,
                credential_id: None,
            },
            ip: Some("203.0.113.7".parse().expect("ip")),
            user_agent: Some("test-agent/1.0".to_owned()),
        }
    }

    /// Writes `count` copies of `event`, at `at` plus one nanosecond each, in one transaction.
    async fn record_many(app: &TestApp, ctx: &Ctx, event: Event, at: i64, count: usize) {
        let ctx = ctx.clone();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for offset in 0..count {
                        record(conn, &ctx, event.clone(), at + offset as i64).await?;
                    }
                    Ok(())
                })
            })
            .await
            .expect("records are written");
    }

    /// One org record of a role change of `target`, by Ada, at `at`.
    fn role_changed(seeded: &Seeded, target: Uuid) -> Event {
        Event::new(MemberAction::RoleChanged {
            old_role: "member",
            new_role: "admin",
        })
        .org(seeded.org_id)
        .target(TargetKind::User, target)
    }

    fn org_path(org: OrgId, query: &str) -> String {
        format!("/api/v1/orgs/{org}/audit-events{query}")
    }

    fn export_path(org: OrgId, query: &str) -> String {
        format!("/api/v1/orgs/{org}/audit-events/export{query}")
    }

    async fn get(app: &TestApp, cookie: &str, path: &str) -> TestResponse {
        app.request(Method::GET, path)
            .header("cookie", cookie)
            .send()
            .await
    }

    /// The first page of `seeded`'s org log for `query`.
    async fn rows(app: &TestApp, seeded: &Seeded, query: &str) -> Vec<AuditEvent> {
        let path = org_path(seeded.org_id, query);
        page_of(app, &seeded.cookie, &path).await.data
    }

    async fn page_of(app: &TestApp, cookie: &str, path: &str) -> Page<AuditEvent> {
        let response = get(app, cookie, path).await;
        assert_eq!(response.status, 200, "{path}");
        response.json()
    }

    /// The text of a CSV or NDJSON body, as lines.
    fn lines_of(response: &TestResponse) -> Vec<String> {
        String::from_utf8(response.body.to_vec())
            .expect("utf-8")
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    async fn count_exports(app: &TestApp) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'audit.export_started'",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .expect("count")
    }

    #[tokio::test]
    async fn admins_read_the_org_log_newest_first() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let ctx = ada_ctx(&seeded);
        let bob = Uuid::from_u128(0xb0b);
        for ms in 1..=3 {
            let event = role_changed(&seeded, bob);
            record_many(&app, &ctx, event, at_ms(ms), 1).await;
        }
        let path = org_path(seeded.org_id, "?action=member.");
        let page = page_of(&app, &seeded.cookie, &path).await;
        let ats: Vec<i64> = page.data.iter().map(|event| event.at).collect();
        assert_eq!(ats, vec![at_ms(3), at_ms(2), at_ms(1)]);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.data[0].actor.status, AuditRefStatus::Present);
        assert_eq!(page.data[0].actor.email.as_deref(), Some("ada@example.com"));
    }

    #[tokio::test]
    async fn members_and_api_keys_cannot_read() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let member = app
            .add_member(&seeded, "member@acme.test", OrgRole::Member)
            .await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let path = org_path(seeded.org_id, "");
        assert_eq!(get(&app, &member.cookie, &path).await.status, 403);
        assert_eq!(get(&app, &admin.cookie, &path).await.status, 200);
        let key = app
            .request(Method::GET, &path)
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(key.status, 404);
    }

    #[tokio::test]
    async fn without_the_feature_it_is_plan_required() {
        let (app, seeded) = app_with(&[]).await;
        let path = org_path(seeded.org_id, "");
        let response = get(&app, &seeded.cookie, &path).await;
        assert_eq!(response.status, 403);
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::PlanRequired);
        assert_eq!(
            problem.detail.as_deref(),
            Some(Feature::AuditLog.locked_detail())
        );
        let export = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=csv"),
        )
        .await;
        assert_eq!(export.status, 403);
        assert_eq!(export.problem().code, ProblemCode::PlanRequired);
    }

    #[tokio::test]
    async fn filters_by_action_group_actor_target_project_outcome_and_time() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let ctx = ada_ctx(&seeded);
        let bob = Uuid::from_u128(0xb0b);
        let key = Uuid::from_u128(0xce7);
        let added = Event::new(MemberAction::Added {
            role: "viewer",
            via: JoinVia::SsoJit,
        })
        .org(seeded.org_id)
        .target(TargetKind::User, bob);
        let key_created = Event::new(ApiKeyAction::Created {
            kind: "secret",
            scopes: vec!["read"],
        })
        .org(seeded.org_id)
        .project(seeded.project.id)
        .target(TargetKind::ApiKey, key);
        let failed = role_changed(&seeded, bob).failed();
        let dataset = Event::new(DatasetAction::Created).org(seeded.org_id);
        record_many(&app, &ctx, added, at_ms(1), 1).await;
        record_many(&app, &ctx, key_created, at_ms(2), 1).await;
        record_many(&app, &ctx, failed, at_ms(3), 1).await;
        record_many(&app, &Ctx::system(), dataset, at_ms(4), 1).await;

        let group = rows(&app, &seeded, "?action=member.").await;
        assert_eq!(group.len(), 2);
        assert!(
            group
                .iter()
                .all(|event| event.action.starts_with("member."))
        );
        assert_eq!(
            rows(&app, &seeded, "?action=api_key.created").await.len(),
            1
        );
        let listed = rows(&app, &seeded, "?action=member.added,api_key.created").await;
        assert_eq!(listed.len(), 2);
        let ada = seeded.user_id;
        assert_eq!(
            rows(&app, &seeded, &format!("?actor_id={ada}")).await.len(),
            3
        );
        assert_eq!(
            rows(&app, &seeded, &format!("?target_id={bob}"))
                .await
                .len(),
            2
        );
        let project = seeded.project.id;
        assert_eq!(
            rows(&app, &seeded, &format!("?project_id={project}"))
                .await
                .len(),
            1
        );
        let failures = rows(&app, &seeded, "?outcome=failure").await;
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].outcome, AuditOutcome::Failure);
        // `from` is inclusive, `to` exclusive, in epoch milliseconds.
        let window = rows(
            &app,
            &seeded,
            &format!("?from={}&to={}", BASE_MS + 2, BASE_MS + 4),
        )
        .await;
        assert_eq!(window.len(), 2);
    }

    #[tokio::test]
    async fn invalid_action_filter_is_400() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let too_many = vec!["auth.login"; 21].join(",");
        let bad = [
            "?action=Member.Role".to_owned(),
            "?action=member..".to_owned(),
            "?action=a.b.c".to_owned(),
            "?action=member.,".to_owned(),
            format!("?action={too_many}"),
        ];
        for query in bad {
            let response = get(&app, &seeded.cookie, &org_path(seeded.org_id, &query)).await;
            assert_eq!(response.status, 400, "{query}");
            let text = String::from_utf8_lossy(&response.body).to_string();
            assert!(text.contains("\"action\""), "{query}: {text}");
        }
    }

    #[tokio::test]
    async fn paging_by_cursor_never_skips_or_repeats() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let ctx = ada_ctx(&seeded);
        record_many(
            &app,
            &ctx,
            role_changed(&seeded, Uuid::from_u128(0xb0b)),
            at_ms(1),
            250,
        )
        .await;
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0;
        let mut cursor: Option<String> = None;
        loop {
            let query = match &cursor {
                Some(next) => format!("?action=member.&limit=100&cursor={next}"),
                None => "?action=member.&limit=100".to_owned(),
            };
            let page = page_of(&app, &seeded.cookie, &org_path(seeded.org_id, &query)).await;
            pages += 1;
            for event in &page.data {
                assert!(seen.insert(event.id), "{} repeated", event.id);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen.len(), 250);
    }

    #[tokio::test]
    async fn former_members_and_deleted_users_show_no_email() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let former = app
            .add_member(&seeded, "former@acme.test", OrgRole::Member)
            .await;
        let erased = app
            .add_member(&seeded, "erased@acme.test", OrgRole::Member)
            .await;
        let ghost = Uuid::now_v7();
        let former_id = former.id;
        let erased_id = erased.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM org_members WHERE user_id = ?")
                        .bind(former_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("DELETE FROM users WHERE id = ?")
                        .bind(erased_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .expect("rows are changed");
        // The former member and the erased user act, and the ghost is a deleted actor.
        for (index, actor) in [former_id, erased_id, ghost].into_iter().enumerate() {
            let as_actor = Ctx {
                actor: Actor::User {
                    id: actor,
                    via: Via::Session,
                    credential_id: None,
                },
                ip: None,
                user_agent: None,
            };
            let event = role_changed(&seeded, seeded.user_id);
            record_many(&app, &as_actor, event, at_ms(10 + index as i64), 1).await;
        }
        let page = page_of(
            &app,
            &seeded.cookie,
            &org_path(seeded.org_id, "?action=member."),
        )
        .await;
        let status_of = |id: Uuid| {
            page.data
                .iter()
                .find(|event| event.actor.id.as_deref() == Some(id.to_string().as_str()))
                .map(|event| {
                    (
                        event.actor.status,
                        event.actor.name.clone(),
                        event.actor.email.clone(),
                    )
                })
                .expect("a row for the actor")
        };
        assert_eq!(
            status_of(former_id),
            (AuditRefStatus::FormerMember, None, None)
        );
        assert_eq!(status_of(erased_id), (AuditRefStatus::Deleted, None, None));
        assert_eq!(status_of(ghost), (AuditRefStatus::Deleted, None, None));
    }

    #[tokio::test]
    async fn the_instance_log_holds_only_instance_records() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let org_event = role_changed(&seeded, seeded.user_id);
        record_many(&app, &ada_ctx(&seeded), org_event, at_ms(1), 1).await;
        record_many(
            &app,
            &Ctx::system(),
            Event::new(InstanceAction::InitApplied),
            at_ms(2),
            1,
        )
        .await;
        let org_only = page_of(
            &app,
            &seeded.cookie,
            "/api/v1/admin/audit-events?action=member.",
        )
        .await;
        assert!(org_only.data.is_empty());
        let instance = page_of(
            &app,
            &seeded.cookie,
            "/api/v1/admin/audit-events?action=instance.",
        )
        .await;
        assert_eq!(instance.data.len(), 1);
        assert_eq!(instance.data[0].action, "instance.init_applied");
        assert_eq!(instance.data[0].actor.kind, "system");
        assert_eq!(instance.data[0].actor.id, None);
        assert_eq!(instance.data[0].actor.status, AuditRefStatus::Present);
    }

    #[tokio::test]
    async fn instance_admins_cannot_read_an_org_log_without_membership() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let outsider = app
            .add_member(&seeded, "outsider@acme.test", OrgRole::Member)
            .await;
        let outsider_id = outsider.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM org_members WHERE user_id = ?")
                        .bind(outsider_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("UPDATE users SET is_instance_admin = 1 WHERE id = ?")
                        .bind(outsider_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .expect("rows are changed");
        let path = org_path(seeded.org_id, "");
        assert_eq!(get(&app, &outsider.cookie, &path).await.status, 404);
        let instance = get(&app, &outsider.cookie, "/api/v1/admin/audit-events").await;
        assert_eq!(instance.status, 200);
    }

    #[tokio::test]
    async fn instance_log_needs_the_licence_or_a_cloud_cell() {
        // The default test config is a cloud cell, so this one is a self-hosted instance.
        let app = TestApp::with_config(|config| config.entitlements.control_plane = false).await;
        let seeded = app.seed().await;
        let path = "/api/v1/admin/audit-events";
        let locked = get(&app, &seeded.cookie, path).await;
        assert_eq!(locked.status, 403);
        assert_eq!(locked.problem().code, ProblemCode::PlanRequired);

        let (licensed, seeded) = app_with(&["audit_log"]).await;
        assert_eq!(get(&licensed, &seeded.cookie, path).await.status, 200);

        let cloud = TestApp::with_config(|config| config.entitlements.control_plane = true).await;
        let seeded = cloud.seed().await;
        assert_eq!(get(&cloud, &seeded.cookie, path).await.status, 200);
    }

    #[tokio::test]
    async fn details_never_carry_an_email() {
        for file in ["org-audit-events.json", "admin-audit-events.json"] {
            let path = format!(
                "{}/../../fixtures/api/static/{file}",
                env!("CARGO_MANIFEST_DIR")
            );
            let text = std::fs::read_to_string(&path).expect("fixture");
            let page: Value = serde_json::from_str(&text).expect("json");
            for event in page["data"].as_array().expect("data") {
                let details = event["details"].to_string();
                assert!(!details.contains('@'), "{file}: {details}");
            }
        }
    }

    #[tokio::test]
    async fn instance_scope_resolves_no_org_object_names() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let event = Event::new(InstanceAction::InitApplied)
            .target(TargetKind::Project, seeded.project.id)
            .project(seeded.project.id);
        record_many(&app, &Ctx::system(), event, at_ms(1), 1).await;
        let page = page_of(&app, &seeded.cookie, "/api/v1/admin/audit-events").await;
        let row = &page.data[0];
        let target = row.target.as_ref().expect("target");
        assert_eq!(target.status, AuditRefStatus::Unresolved);
        assert_eq!(target.name, None);
        let project = row.project.as_ref().expect("project");
        assert_eq!(project.status, AuditRefStatus::Unresolved);
        assert_eq!(project.name, None);
    }

    #[tokio::test]
    async fn instance_project_filter_matches_nothing() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let event = Event::new(InstanceAction::InitApplied).project(seeded.project.id);
        record_many(&app, &Ctx::system(), event, at_ms(1), 1).await;
        let path = format!(
            "/api/v1/admin/audit-events?project_id={}",
            seeded.project.id
        );
        let page = page_of(&app, &seeded.cookie, &path).await;
        assert!(page.data.is_empty());
    }

    #[tokio::test]
    async fn csv_export_has_the_header_and_escapes_formulas() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let ctx = Ctx {
            actor: Actor::User {
                id: seeded.user_id,
                via: Via::Session,
                credential_id: None,
            },
            ip: None,
            user_agent: Some("=cmd()".to_owned()),
        };
        let event = Event::new(MemberAction::Added {
            role: "viewer",
            via: JoinVia::SsoJit,
        })
        .org(seeded.org_id)
        .target(TargetKind::User, seeded.user_id);
        record_many(&app, &ctx, event, at_ms(1), 1).await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=csv&action=member."),
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("content-type"),
            Some("text/csv; charset=utf-8")
        );
        let disposition = response.header("content-disposition").expect("disposition");
        assert!(disposition.starts_with("attachment; filename=\"audit-events-"));
        assert!(disposition.ends_with(".csv\""));
        assert_eq!(response.header("cache-control"), Some("no-store"));
        let body = String::from_utf8(response.body.to_vec()).expect("utf-8");
        let header = "at,action,outcome,actor_kind,actor_id,actor_name,actor_email,actor_status,via,credential_id,target_kind,target_id,target_name,target_email,target_status,project_id,project_name,ip,user_agent,details\r\n";
        assert!(body.starts_with(header), "{body}");
        assert!(body.contains(",'=cmd(),"), "{body}");
    }

    #[tokio::test]
    async fn ndjson_export_lines_parse_as_audit_events() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        record_many(
            &app,
            &ada_ctx(&seeded),
            role_changed(&seeded, Uuid::from_u128(0xb0b)),
            at_ms(1),
            3,
        )
        .await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=ndjson&action=member."),
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("content-type"),
            Some("application/x-ndjson")
        );
        let lines = lines_of(&response);
        assert_eq!(lines.len(), 3);
        for line in lines {
            let event: AuditEvent = serde_json::from_str(&line).expect("an audit event");
            assert_eq!(event.action, "member.role_changed");
        }
    }

    #[tokio::test]
    async fn export_is_recorded_before_streaming() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(
                seeded.org_id,
                &format!("?format=csv&action=member.&from={}", BASE_MS),
            ),
        )
        .await;
        assert_eq!(response.status, 200);
        let details: String = sqlx::query_scalar(
            "SELECT details FROM audit_events WHERE action = 'audit.export_started'",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .expect("one export record");
        let details: Value = serde_json::from_str(&details).expect("json");
        assert_eq!(details["format"], "csv");
        assert_eq!(details["filters"], json!(["from", "action"]));
        assert!(!details.to_string().contains("member."));
    }

    #[tokio::test]
    async fn export_respects_filters() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        let ctx = ada_ctx(&seeded);
        record_many(
            &app,
            &ctx,
            role_changed(&seeded, Uuid::from_u128(0xb0b)),
            at_ms(1),
            2,
        )
        .await;
        let key = Event::new(ApiKeyAction::Revoked)
            .org(seeded.org_id)
            .target(TargetKind::ApiKey, Uuid::from_u128(0xce7));
        record_many(&app, &ctx, key, at_ms(2), 1).await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=ndjson&action=api_key."),
        )
        .await;
        let lines = lines_of(&response);
        assert_eq!(lines.len(), 1);
        let event: AuditEvent = serde_json::from_str(&lines[0]).expect("an event");
        assert_eq!(event.action, "api_key.revoked");
    }

    #[tokio::test]
    async fn export_without_the_feature_is_plan_required() {
        let (app, seeded) = app_with(&[]).await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=csv"),
        )
        .await;
        assert_eq!(response.status, 403);
        assert_eq!(response.problem().code, ProblemCode::PlanRequired);
        assert_eq!(count_exports(&app).await, 0);
    }

    #[tokio::test]
    async fn export_stops_at_the_cap() {
        let (app, seeded) = app_with(&["audit_log"]).await;
        record_many(
            &app,
            &ada_ctx(&seeded),
            role_changed(&seeded, Uuid::from_u128(0xb0b)),
            at_ms(1),
            2_500,
        )
        .await;
        let response = get(
            &app,
            &seeded.cookie,
            &export_path(seeded.org_id, "?format=ndjson"),
        )
        .await;
        assert_eq!(response.status, 200);
        // The cap is 1,500 in tests (`MAX_EXPORT_ROWS`), and the export's own record counts.
        assert_eq!(lines_of(&response).len(), 1_500);
    }

    #[test]
    fn the_cursor_round_trips_and_rejects_garbage() {
        assert_eq!(read::decode_cursor(&read::encode_cursor(42)).ok(), Some(42));
        assert!(read::decode_cursor("not a cursor!").is_err());
        // Valid base64 of text that is not the `{"seq": N}` object.
        let not_json = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("garbage");
        assert!(read::decode_cursor(&not_json).is_err());
    }
}
