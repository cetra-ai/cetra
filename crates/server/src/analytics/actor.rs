//! Who an event is about (D-TL-15): the user and org come from the session and the access checks,
//! never from the browser's claim. The request's `DNT: 1` or `Sec-GPC: 1` is kept in a task-local
//! for the length of the request; anywhere else the answer is "opted out", so a forgotten scope
//! never sends.

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use cetra_core::ids::{OrgId, ProjectId};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::principal::Principal;
use crate::auth::roles::OrgRole;
use crate::http::access::{OrgAccess, ProjectAccess};

tokio::task_local! {
    /// The current request opted out of per-user analytics (`DNT: 1` or `Sec-GPC: 1`).
    static OPTED_OUT: bool;
}

/// How a server event's caller authenticated (the `via` context property).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Via {
    /// A web session (the UI).
    Session,
    /// A personal access token (CLI, MCP, scripts).
    Pat,
    /// A project API key (SDKs).
    ApiKey,
    /// The cloud control plane.
    ControlPlane,
    /// A SCIM bearer token of an org (provisioning; no user, so counters only).
    Scim,
}

/// Who an event is about. Carries ids and enums only: no email, name, slug or address can
/// reach a sink through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Actor {
    /// The user; `None` for API keys and the control plane (counters only).
    pub(crate) user_id: Option<Uuid>,
    /// How a server event's caller authenticated; `None` for web events.
    pub(crate) via: Option<Via>,
    /// The org, when the action has one.
    pub(crate) org_id: Option<OrgId>,
    /// The user's role in that org, when known without another query.
    pub(crate) org_role: Option<OrgRole>,
    /// The project, when the action has one.
    pub(crate) project_id: Option<ProjectId>,
    /// The request opted out of per-user analytics: no PostHog event, counters only.
    pub(crate) opted_out: bool,
}

impl Actor {
    /// The caller of this request, without an org.
    pub(crate) fn principal(principal: &Principal) -> Self {
        let (user_id, via) = match principal {
            Principal::Session { user, .. } => (Some(user.id), Via::Session),
            Principal::Pat { user, .. } => (Some(user.id), Via::Pat),
            Principal::ApiKey { .. } => (None, Via::ApiKey),
            Principal::ControlPlane { .. } => (None, Via::ControlPlane),
            Principal::Scim { .. } => (None, Via::Scim),
        };
        Self {
            user_id,
            via: Some(via),
            org_id: None,
            org_role: None,
            project_id: None,
            opted_out: request_opted_out(),
        }
    }

    /// The caller on a project route: the project and its org.
    pub(crate) fn project(principal: &Principal, access: &ProjectAccess) -> Self {
        Self {
            org_id: Some(access.project.org_id),
            project_id: Some(access.project.id),
            ..Self::principal(principal)
        }
    }

    /// The caller on an org route: the org and the caller's role there.
    pub(crate) fn org(principal: &Principal, access: &OrgAccess) -> Self {
        Self {
            org_id: Some(access.org.id),
            org_role: Some(access.role),
            ..Self::principal(principal)
        }
    }

    /// A user known by id only (a session that just started, an MCP tool call).
    pub(crate) fn user(user_id: Uuid, via: Via) -> Self {
        Self {
            user_id: Some(user_id),
            via: Some(via),
            org_id: None,
            org_role: None,
            project_id: None,
            opted_out: request_opted_out(),
        }
    }
}

/// `DNT: 1` or `Sec-GPC: 1` (values trimmed).
pub(crate) fn opts_out(headers: &HeaderMap) -> bool {
    ["dnt", "sec-gpc"].iter().any(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim() == "1")
    })
}

/// Whether the current request opted out. Outside a request scope: `true` (fail closed).
pub(crate) fn request_opted_out() -> bool {
    OPTED_OUT.try_with(|opted_out| *opted_out).unwrap_or(true)
}

/// Middleware: runs the rest of the request with its opt-out in [`OPTED_OUT`].
pub(crate) async fn privacy_signal(request: Request, next: Next) -> Response {
    let opted_out = opts_out(request.headers());
    OPTED_OUT.scope(opted_out, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;
    use crate::auth::principal::AuthUser;
    use crate::auth::roles::KeyScopes;
    use crate::auth::secret::KeyKind;
    use crate::meta::orgs::OrgRow;
    use crate::test_support::TestApp;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().expect("test header value is valid"));
        }
        map
    }

    fn user(id: Uuid) -> AuthUser {
        AuthUser {
            id,
            is_instance_admin: false,
            email: "ada@example.com".to_owned(),
        }
    }

    #[test]
    fn opts_out_reads_dnt_and_gpc() {
        assert!(opts_out(&headers(&[("dnt", "1")])));
        assert!(opts_out(&headers(&[("sec-gpc", "1")])));
        assert!(opts_out(&headers(&[("dnt", " 1 ")])));
        assert!(!opts_out(&headers(&[("dnt", "0")])));
        assert!(!opts_out(&headers(&[("dnt", "yes")])));
        assert!(!opts_out(&headers(&[("sec-gpc", "0")])));
        assert!(!opts_out(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn outside_a_request_is_opted_out() {
        assert!(request_opted_out());
    }

    #[tokio::test]
    async fn the_middleware_scopes_the_signal() {
        let app = Router::new()
            .route("/", get(|| async { request_opted_out().to_string() }))
            .layer(axum::middleware::from_fn(privacy_signal));

        let cases = [
            (vec![("DNT", "1")], "true"),
            (vec![("Sec-GPC", "1")], "true"),
            (vec![], "false"),
        ];
        for (pairs, expected) in cases {
            let mut builder = Request::builder().uri("/");
            for (name, value) in pairs {
                builder = builder.header(name, value);
            }
            let request = builder.body(Body::empty()).expect("test request is valid");
            let response = app.clone().oneshot(request).await.expect("router answers");
            let bytes = axum::body::to_bytes(response.into_body(), 64)
                .await
                .expect("body is small");
            assert_eq!(std::str::from_utf8(&bytes).expect("utf-8 body"), expected);
        }
    }

    #[test]
    fn principal_actors() {
        let user_id = Uuid::now_v7();
        let session = Principal::Session {
            user: user(user_id),
            session_hash: "h".to_owned(),
            sso_org: None,
        };
        let pat = Principal::Pat {
            user: user(user_id),
            token_id: Uuid::now_v7(),
            write: false,
        };
        let api_key = Principal::ApiKey {
            key_id: Uuid::now_v7(),
            project_id: ProjectId::new_v7(),
            kind: KeyKind::Secret,
            scopes: KeyScopes::from_scopes(&[]),
        };
        let control_plane = Principal::ControlPlane {
            token_id: Uuid::now_v7(),
            scope: "ingest".to_owned(),
            method: "GET".to_owned(),
            path: "/".to_owned(),
        };

        let session_actor = Actor::principal(&session);
        assert_eq!(session_actor.user_id, Some(user_id));
        assert_eq!(session_actor.via, Some(Via::Session));
        let pat_actor = Actor::principal(&pat);
        assert_eq!(pat_actor.user_id, Some(user_id));
        assert_eq!(pat_actor.via, Some(Via::Pat));
        let api_key_actor = Actor::principal(&api_key);
        assert_eq!(api_key_actor.user_id, None);
        assert_eq!(api_key_actor.via, Some(Via::ApiKey));
        let control_plane_actor = Actor::principal(&control_plane);
        assert_eq!(control_plane_actor.user_id, None);
        assert_eq!(control_plane_actor.via, Some(Via::ControlPlane));

        // Outside a request scope every actor is opted out.
        for actor in [session_actor, pat_actor, api_key_actor, control_plane_actor] {
            assert!(actor.opted_out);
        }
    }

    #[tokio::test]
    async fn project_and_org_actors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org_id = seeded.org_id;
        let project_id = seeded.project.id;
        let principal = Principal::Session {
            user: user(seeded.user_id),
            session_hash: "h".to_owned(),
            sso_org: None,
        };

        let project_access = ProjectAccess {
            project: seeded.project,
            role: None,
            implicit: false,
        };
        let project_actor = Actor::project(&principal, &project_access);
        assert_eq!(project_actor.org_id, Some(org_id));
        assert_eq!(project_actor.project_id, Some(project_id));
        assert_eq!(project_actor.org_role, None);

        let org_access = OrgAccess {
            org: OrgRow {
                id: org_id,
                name: "Acme".to_owned(),
                slug: "acme".to_owned(),
                state: "active".to_owned(),
                created_at: 0,
                updated_at: 0,
            },
            role: OrgRole::Admin,
        };
        let org_actor = Actor::org(&principal, &org_access);
        assert_eq!(org_actor.org_id, Some(org_id));
        assert_eq!(org_actor.org_role, Some(OrgRole::Admin));
        assert_eq!(org_actor.project_id, None);
    }

    #[test]
    fn the_actor_has_no_text_fields() {
        let principal = Principal::Session {
            user: user(Uuid::now_v7()),
            session_hash: "h".to_owned(),
            sso_org: None,
        };
        let actor = Actor::principal(&principal);
        assert!(!format!("{actor:?}").contains("ada@"));
    }
}
