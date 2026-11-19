//! The demo org (D187, spec 08 § Demo org): one org of the instance that every signed-in user
//! who is not an instance admin reads as an implicit `viewer`, unless `org_members` has a row
//! for them. Nothing here writes a row.

use cetra_core::ids::OrgId;
use uuid::Uuid;

use crate::api::account::OrgRole;
use crate::auth::principal::AuthUser;
use crate::config::ServerConfig;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::members::org_role;
use crate::meta::orgs::{OrgRow, org_by_id, orgs_of_user};

/// A caller's role in one org. `implicit` is `true` when the role comes from the demo org and
/// `org_members` has no row for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrgRoleOf {
    /// The role the access checks use.
    pub(crate) role: OrgRole,
    /// Whether the role is the implicit demo role.
    pub(crate) implicit: bool,
}

/// Set on the request of an implicit visitor, for the demo limits (spec 08 § Demo org). The
/// value is the user id.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DemoVisitor(pub(crate) Uuid);

/// Whether `org` is the demo org (`demo.org`). Its flag in the API is `true` for every caller,
/// members included.
pub(crate) fn is_demo_org(config: &ServerConfig, org: OrgId) -> bool {
    config.demo.org == Some(org)
}

/// The caller's role in `org_id`: the stored row, else the implicit viewer role on the demo org
/// for anyone but an instance admin (D187), else `None`.
///
/// # Errors
///
/// Returns [`MetaError`] when `meta.db` cannot be read.
pub(crate) async fn org_role_for(
    state: &AppState,
    org_id: OrgId,
    user: &AuthUser,
) -> Result<Option<OrgRoleOf>, MetaError> {
    if let Some(role) = org_role(state.meta.reader(), org_id, user.id).await? {
        return Ok(Some(OrgRoleOf {
            role,
            implicit: false,
        }));
    }
    if is_demo_org(&state.config, org_id) && !user.is_instance_admin {
        return Ok(Some(OrgRoleOf {
            role: OrgRole::Viewer,
            implicit: true,
        }));
    }
    Ok(None)
}

/// The active orgs the user reads, with their role and whether each is the demo org. The demo
/// org comes last, whether the user has a row there or reads it implicitly. The sort is stable,
/// so the other orgs keep the order of `orgs_of_user`.
///
/// # Errors
///
/// Returns [`MetaError`] when `meta.db` cannot be read or a stored value does not decode.
pub(crate) async fn orgs_for(
    state: &AppState,
    user_id: Uuid,
    is_instance_admin: bool,
) -> Result<Vec<(OrgRow, OrgRole, bool)>, MetaError> {
    let demo = state.config.demo.org;
    let mut rows = Vec::new();
    let mut member_of_demo = false;
    for (org, role) in orgs_of_user(state.meta.reader(), user_id).await? {
        let is_demo = Some(org.id) == demo;
        member_of_demo |= is_demo;
        rows.push((org, role, is_demo));
    }
    if let Some(id) = demo.filter(|_| !is_instance_admin && !member_of_demo)
        && let Some(org) = org_by_id(state.meta.reader(), id).await?
        && org.state == "active"
    {
        rows.push((org, OrgRole::Viewer, true));
    }
    rows.sort_by_key(|(_, _, is_demo)| *is_demo);
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::ids::{OrgId, ProjectId};
    use serde_json::{Value, json};

    use crate::api::account::OrgRole;
    use crate::config::DemoConfig;
    use crate::meta::members::{add_org_member, org_role};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::users::{NewUser, insert_user};
    use crate::test_support::{Member, TestApp, TestResponse, member_in_org, session_cookie_via};

    /// The demo org `cetra-demo` with one project, `demo`, a real owner and a real member.
    pub(crate) struct World {
        pub(crate) app: TestApp,
        pub(crate) demo: OrgId,
        pub(crate) project: ProjectId,
        pub(crate) owner: Member,
        pub(crate) member: Member,
    }

    pub(crate) async fn world() -> World {
        world_with(|_| {}).await
    }

    /// [`world`] with `tweak` applied to the demo config.
    pub(crate) async fn world_with(tweak: impl FnOnce(&mut DemoConfig)) -> World {
        let demo = OrgId::new_v7();
        let app = TestApp::with_config(|config| {
            config.demo.org = Some(demo);
            tweak(&mut config.demo);
        })
        .await;
        let now = app.state.clock.now_ns();
        let project = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, demo, "Cetra Demo", "cetra-demo", now).await?;
                    insert_project(conn, demo, "Demo", "demo", None, now).await
                })
            })
            .await
            .unwrap();
        let owner = member_in_org(&app, demo, "owner@example.com", OrgRole::Owner).await;
        let member = member_in_org(&app, demo, "member@example.com", OrgRole::Member).await;
        World {
            app,
            demo,
            project: project.id,
            owner,
            member,
        }
    }

    /// A signed-in user with no row in any org, or an instance admin when `admin` is set.
    pub(crate) async fn signed_in(app: &TestApp, email: &str, admin: bool) -> Member {
        let now = app.state.clock.now_ns();
        let email = email.to_owned();
        let id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email,
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: admin,
                        },
                        now,
                    )
                    .await?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        let cookie = session_cookie_via(app, id, None).await;
        Member { id, cookie }
    }

    async fn get_as(app: &TestApp, path: &str, who: &Member) -> TestResponse {
        app.request(Method::GET, path)
            .header("cookie", &who.cookie)
            .send()
            .await
    }

    async fn post_as(app: &TestApp, path: &str, who: &Member, body: &Value) -> TestResponse {
        app.request(Method::POST, path)
            .header("cookie", &who.cookie)
            .json(body)
            .send()
            .await
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    #[tokio::test]
    async fn a_visitor_reads_the_demo_org_and_its_project() {
        let w = world().await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let org = get_as(&w.app, &format!("/api/v1/orgs/{}", w.demo), &visitor).await;
        assert_eq!(org.status, StatusCode::OK, "{}", text(&org));
        let body: Value = org.json();
        assert_eq!(body["role"], "viewer");
        assert_eq!(body["demo"], true);
        let project = get_as(&w.app, &format!("/api/v1/projects/{}", w.project), &visitor).await;
        assert_eq!(project.status, StatusCode::OK, "{}", text(&project));
    }

    #[tokio::test]
    async fn a_visitor_runs_sql_but_not_exports_or_member_routes() {
        let w = world().await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let sql = format!("/api/v1/projects/{}/sql", w.project);
        let query = json!({ "query": "SELECT 1 AS one" });
        let run = post_as(&w.app, &sql, &visitor, &query).await;
        assert_eq!(run.status, StatusCode::OK, "{}", text(&run));
        let export = w
            .app
            .request(Method::POST, &sql)
            .header("cookie", &visitor.cookie)
            .header("accept", "text/csv")
            .json(&query)
            .send()
            .await;
        assert_eq!(export.status, StatusCode::FORBIDDEN, "{}", text(&export));
        for path in [
            format!("/api/v1/projects/{}/score-definitions", w.project),
            format!("/api/v1/projects/{}/api-keys", w.project),
        ] {
            let response = post_as(&w.app, &path, &visitor, &json!({})).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN, "POST {path}");
        }
        let drop = w
            .app
            .request(Method::DELETE, &format!("/api/v1/projects/{}", w.project))
            .header("cookie", &visitor.cookie)
            .send()
            .await;
        assert_eq!(drop.status, StatusCode::FORBIDDEN, "{}", text(&drop));
    }

    #[tokio::test]
    async fn a_real_member_keeps_their_role() {
        let w = world().await;
        let org = get_as(&w.app, &format!("/api/v1/orgs/{}", w.demo), &w.member).await;
        assert_eq!(org.status, StatusCode::OK, "{}", text(&org));
        let body: Value = org.json();
        assert_eq!(body["role"], "member");
        assert_eq!(body["demo"], true);
        let path = format!("/api/v1/projects/{}/score-definitions", w.project);
        let defs = post_as(&w.app, &path, &w.member, &json!({})).await;
        assert_ne!(defs.status, StatusCode::FORBIDDEN, "{}", text(&defs));
        let reader = w.app.state.meta.reader();
        assert_eq!(
            org_role(reader, w.demo, w.member.id).await.unwrap(),
            Some(OrgRole::Member)
        );
    }

    #[tokio::test]
    async fn an_instance_admin_without_a_row_gets_404() {
        let w = world().await;
        let admin = signed_in(&w.app, "root@example.com", true).await;
        let org = get_as(&w.app, &format!("/api/v1/orgs/{}", w.demo), &admin).await;
        assert_eq!(org.status, StatusCode::NOT_FOUND, "{}", text(&org));
        let project = get_as(&w.app, &format!("/api/v1/projects/{}", w.project), &admin).await;
        assert_eq!(project.status, StatusCode::NOT_FOUND, "{}", text(&project));
    }

    #[tokio::test]
    async fn without_a_demo_org_a_user_without_a_row_gets_404() {
        let app = TestApp::new().await;
        let org = OrgId::new_v7();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_org(conn, org, "Acme", "acme", now).await })
            })
            .await
            .unwrap();
        let visitor = signed_in(&app, "visitor@example.com", false).await;
        let response = get_as(&app, &format!("/api/v1/orgs/{org}"), &visitor).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{}",
            text(&response)
        );
    }

    #[tokio::test]
    async fn me_and_the_org_list_put_the_demo_org_last() {
        let w = world().await;
        let dual = signed_in(&w.app, "dual@example.com", false).await;
        let own = OrgId::new_v7();
        let now = w.app.state.clock.now_ns();
        let dual_id = dual.id;
        w.app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_org(conn, own, "Zeta", "zeta", now).await?;
                    add_org_member(conn, own, dual_id, OrgRole::Owner, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let me = get_as(&w.app, "/api/auth/me", &dual).await;
        assert_eq!(me.status, StatusCode::OK, "{}", text(&me));
        let me: Value = me.json();
        let orgs = me["orgs"].as_array().unwrap();
        assert_eq!(orgs.len(), 2, "{me}");
        assert_eq!(orgs[0]["slug"], "zeta");
        assert_eq!(orgs[0]["role"], "owner");
        assert_eq!(orgs[0]["demo"], false);
        assert_eq!(orgs[1]["slug"], "cetra-demo");
        assert_eq!(orgs[1]["role"], "viewer");
        assert_eq!(orgs[1]["demo"], true);

        let first = get_as(&w.app, "/api/v1/orgs?limit=1", &dual).await;
        assert_eq!(first.status, StatusCode::OK, "{}", text(&first));
        let first: Value = first.json();
        assert_eq!(first["data"][0]["slug"], "zeta");
        let cursor = first["next_cursor"].as_str().unwrap();
        let second = get_as(
            &w.app,
            &format!("/api/v1/orgs?limit=1&cursor={cursor}"),
            &dual,
        )
        .await;
        assert_eq!(second.status, StatusCode::OK, "{}", text(&second));
        let second: Value = second.json();
        assert_eq!(second["data"].as_array().unwrap().len(), 1, "{second}");
        assert_eq!(second["data"][0]["slug"], "cetra-demo");
        assert_eq!(second["data"][0]["demo"], true);
        assert!(second["next_cursor"].is_null(), "{second}");
    }

    #[tokio::test]
    async fn the_member_list_shows_real_members_only() {
        let w = world().await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let list = get_as(
            &w.app,
            &format!("/api/v1/orgs/{}/members", w.demo),
            &visitor,
        )
        .await;
        assert_eq!(list.status, StatusCode::OK, "{}", text(&list));
        let body: Value = list.json();
        assert_eq!(body["data"].as_array().unwrap().len(), 2, "{body}");
        assert!(!text(&list).contains(&visitor.id.to_string()));
        assert!(text(&list).contains(&w.owner.id.to_string()));
    }

    #[tokio::test]
    async fn a_visitor_cannot_leave_or_change_members() {
        let w = world().await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let leave = w
            .app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/{}/members/{}", w.demo, visitor.id),
            )
            .header("cookie", &visitor.cookie)
            .send()
            .await;
        assert_eq!(leave.status, StatusCode::FORBIDDEN, "{}", text(&leave));
        let change = w
            .app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/{}/members/{}", w.demo, w.member.id),
            )
            .header("cookie", &visitor.cookie)
            .json(&json!({ "role": "admin" }))
            .send()
            .await;
        assert_eq!(change.status, StatusCode::FORBIDDEN, "{}", text(&change));
        let reader = w.app.state.meta.reader();
        assert_eq!(org_role(reader, w.demo, visitor.id).await.unwrap(), None);
        assert_eq!(
            org_role(reader, w.demo, w.member.id).await.unwrap(),
            Some(OrgRole::Member)
        );
    }

    #[tokio::test]
    async fn the_control_plane_lookup_does_not_return_the_demo_org_to_a_visitor() {
        let w = world().await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let lookup = w
            .app
            .request(Method::POST, "/api/internal/v1/memberships/lookup")
            .control_plane("members")
            .json(&json!({ "subject": "nobody", "email": "visitor@example.com" }))
            .send()
            .await;
        assert_eq!(lookup.status, StatusCode::OK, "{}", text(&lookup));
        let body: Value = lookup.json();
        assert_eq!(body["user_id"], visitor.id.to_string());
        assert_eq!(body["orgs"].as_array().unwrap().len(), 0, "{body}");
    }

    #[tokio::test]
    async fn an_implicit_visitor_over_the_rate_gets_429_and_a_member_is_not_limited() {
        let w = world_with(|demo| demo.requests_per_minute = 2).await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let path = format!("/api/v1/orgs/{}", w.demo);
        for _ in 0..2 {
            assert_eq!(get_as(&w.app, &path, &visitor).await.status, StatusCode::OK);
        }
        // Two tokens a minute: the next one is 30 s away.
        let refused = get_as(&w.app, &path, &visitor).await;
        assert_eq!(
            refused.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            text(&refused)
        );
        assert_eq!(refused.header("retry-after"), Some("30"));
        assert_eq!(refused.json::<Value>()["code"], "rate_limited");

        for _ in 0..5 {
            assert_eq!(
                get_as(&w.app, &path, &w.member).await.status,
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn held_slots_refuse_a_visitor_until_they_are_freed() {
        let w = world_with(|demo| {
            demo.user_in_flight = 1;
            demo.org_in_flight = 1;
        })
        .await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let other = signed_in(&w.app, "other@example.com", false).await;
        let path = format!("/api/v1/orgs/{}", w.demo);
        let now = w.app.state.clock.now_ns();

        // Another visitor holds the org's only slot.
        let held = w.app.state.limits.demo.admit(other.id, now).unwrap();
        let refused = get_as(&w.app, &path, &visitor).await;
        assert_eq!(
            refused.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            text(&refused)
        );
        assert_eq!(refused.json::<Value>()["code"], "rate_limited");
        drop(held);

        // Requests one after another each find the slot free again.
        assert_eq!(get_as(&w.app, &path, &visitor).await.status, StatusCode::OK);
        assert_eq!(get_as(&w.app, &path, &visitor).await.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_implicit_visitors_mcp_call_takes_no_demo_slot() {
        let w = world_with(|demo| {
            demo.user_in_flight = 1;
            demo.org_in_flight = 1;
        })
        .await;
        let visitor = signed_in(&w.app, "visitor@example.com", false).await;
        let now = w.app.state.clock.now_ns();
        let _held = w.app.state.limits.demo.admit(visitor.id, now).unwrap();
        let token = w.app.personal_token(visitor.id, false).await;
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let response = crate::mcp::test_helpers::rpc(&w.app, Some(token.as_str()), &list).await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
    }
}
