// SPDX-License-Identifier: Elastic-2.0
//! The Provisioning API: an org's SCIM settings and bearer tokens (track EA M5, D-EA-15,
//! D-EA-18). Owners only, session only. Reading the settings needs no feature, so the page
//! shows locked after a lapse; enabling and tokens need `scim`; turning SCIM off and
//! revoking a token never do.

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, patch, post};
use cetra_core::ids::OrgId;
use serde::Deserialize;
use uuid::Uuid;

use super::domains;
use super::recompute::{self, RecomputeScope};
use super::store::{self, ScimTokenRow};
use crate::api::account::OrgRole;
use crate::api::common::FieldError;
use crate::api::entitlements::Feature;
use crate::api::scim::{
    CreateScimTokenRequest, CreatedScimToken, ScimSettings, ScimToken, UpdateScimSettings,
};
use crate::audit::catalog::ScimAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::{self, OrgPerm};
use crate::auth::secret::{self, KeyKind};
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;

/// The Provisioning API routes.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/scim",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: get(get_scim),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/orgs/{org}/scim",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: patch(update_scim),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/scim/tokens",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: post(create_token),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/orgs/{org}/scim/tokens/{id}",
            access: Access::Org {
                perm: OrgPerm::ManageProvisioning,
                session_only: true,
            },
            documented: true,
            handler: delete(revoke_token),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct TokenPath {
    id: Uuid,
}

/// What a settings write did inside its transaction.
enum Update {
    Done,
    /// Enabling needs a verified SCIM domain, and the org has none.
    NoDomain,
}

/// What a token create did inside its transaction: `None` when the org already has two live
/// tokens.
type Created = Option<ScimTokenRow>;

async fn get_scim(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<ScimSettings>, ApiError> {
    Ok(error::Json(view(&state, access.org.id).await?))
}

async fn update_scim(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<UpdateScimSettings>,
) -> Result<error::Json<ScimSettings>, ApiError> {
    let org = access.org.id;
    if let Some(role) = body.default_role
        && !matches!(role, OrgRole::Viewer | OrgRole::Member)
    {
        return Err(field_error("default_role", "must be viewer or member"));
    }
    // The feature is checked against the settings as they were read; the write below
    // re-reads them, so a racing write changes what is recorded, not what is allowed.
    let snapshot = store::org_scim(state.meta.reader(), org).await?;
    let enabling = body.enabled == Some(true) && !snapshot.enabled;
    let role_changes = body
        .default_role
        .is_some_and(|role| role != snapshot.default_role);
    if enabling || role_changes {
        require(&state, org, Feature::Scim)?;
    }
    // D-EA-13: on a self-hosted instance, SCIM also needs org SSO. The cloud's control plane
    // grants SCIM alone.
    if enabling && !state.config.entitlements.control_plane {
        require(&state, org, Feature::OrgSso)?;
    }
    let now = state.clock.now_ns();
    let domains_state = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let current = store::org_scim_tx(conn, org).await?;
                let enabled = body.enabled.unwrap_or(current.enabled);
                let default_role = body.default_role.unwrap_or(current.default_role);
                if enabled && !current.enabled {
                    let listed = domains::scim_domains_tx(&domains_state, conn, org, now).await?;
                    if listed.is_empty() {
                        return Ok(Update::NoDomain);
                    }
                }
                // Saving the values the org already has changes nothing, so it records nothing.
                if enabled == current.enabled && default_role == current.default_role {
                    return Ok(Update::Done);
                }
                store::put_org_scim(conn, org, enabled, default_role, now).await?;
                if enabled != current.enabled {
                    let action = if enabled {
                        ScimAction::Enabled
                    } else {
                        ScimAction::Disabled
                    };
                    audit::record(conn, &ctx, Event::new(action).org(org), now).await?;
                }
                if default_role != current.default_role {
                    let action = ScimAction::DefaultRoleChanged {
                        old_role: roles::org_role_str(current.default_role),
                        new_role: roles::org_role_str(default_role),
                    };
                    audit::record(conn, &ctx, Event::new(action).org(org), now).await?;
                    // P-3: the managed members the new default reaches are recomputed in this
                    // transaction, but only while SCIM is on (P-6).
                    if enabled {
                        let scope = RecomputeScope::of(&domains_state, org, default_role);
                        let user_ids = store::active_user_ids_tx(conn, org).await?;
                        recompute::recompute_tx(conn, &ctx, &scope, &user_ids).await?;
                    }
                }
                Ok(Update::Done)
            })
        })
        .await?;
    match outcome {
        Update::Done => Ok(error::Json(view(&state, org).await?)),
        Update::NoDomain => Err(ApiError::Conflict {
            detail: "Verify a domain first.",
        }),
    }
}

async fn create_token(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateScimTokenRequest>,
) -> Result<(StatusCode, error::Json<CreatedScimToken>), ApiError> {
    let user = principal::session_user(&principal)?;
    let org = access.org.id;
    require(&state, org, Feature::Scim)?;
    let name = checked_name(&body.name)?;
    let minted =
        secret::new_key(KeyKind::Scim).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let now = state.clock.now_ns();
    let creator = user.id;
    let hash = minted.hash.clone();
    let prefix = minted.prefix.clone();
    let created: Created = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let row = match store::insert_token(conn, org, &name, &prefix, &hash, creator, now)
                    .await
                {
                    Ok(row) => row,
                    Err(MetaError::Conflict("at most two live SCIM tokens")) => return Ok(None),
                    Err(error) => return Err(error),
                };
                let event = Event::new(ScimAction::TokenCreated)
                    .org(org)
                    .target(TargetKind::ScimToken, row.id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(Some(row))
            })
        })
        .await?;
    let Some(row) = created else {
        return Err(ApiError::Conflict {
            detail: "Revoke a token first.",
        });
    };
    Ok((
        StatusCode::CREATED,
        error::Json(CreatedScimToken {
            token: token_json(&row),
            secret: minted.plaintext,
        }),
    ))
}

async fn revoke_token(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Path(path): error::Path<TokenPath>,
) -> Result<StatusCode, ApiError> {
    let org = access.org.id;
    let id = path.id;
    let now = state.clock.now_ns();
    let hash = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let hash = store::revoke_token(conn, org, id, now).await?;
                if hash.is_some() {
                    let event = Event::new(ScimAction::TokenRevoked)
                        .org(org)
                        .target(TargetKind::ScimToken, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(hash)
            })
        })
        .await?;
    // Evict after commit: a token revoked here stops authenticating at once (D-EA-15).
    let Some(hash) = hash else {
        return Err(ApiError::NotFound);
    };
    state.scim_cache.evict(&hash);
    Ok(StatusCode::NO_CONTENT)
}

/// The settings as the Provisioning API shows them: the stored flags, the base URL, the
/// verified SCIM domains and the live tokens.
async fn view(state: &AppState, org: OrgId) -> Result<ScimSettings, ApiError> {
    let row = store::org_scim(state.meta.reader(), org).await?;
    let tokens = store::list_tokens(state.meta.reader(), org).await?;
    let now = state.clock.now_ns();
    let mut conn = state
        .meta
        .reader()
        .acquire()
        .await
        .map_err(MetaError::from)?;
    let listed = domains::scim_domains_tx(state, &mut conn, org, now).await?;
    Ok(ScimSettings {
        enabled: row.enabled,
        default_role: row.default_role,
        base_url: state.config.public_url.join("/scim/v2"),
        verified_domains: u32::try_from(listed.len()).unwrap_or(u32::MAX),
        tokens: tokens.iter().map(token_json).collect(),
    })
}

fn token_json(row: &ScimTokenRow) -> ScimToken {
    ScimToken {
        id: row.id,
        name: row.name.clone(),
        prefix: row.prefix.clone(),
        created_at: row.created_at,
        created_by: row.created_by,
        last_used_at: row.last_used_at,
    }
}

/// The token name rule of `routes/tokens.rs`: trimmed, 1 to 100 characters.
fn checked_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(field_error("name", "must be 1 to 100 characters"));
    }
    Ok(name.to_owned())
}

fn field_error(field: &str, message: &str) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![FieldError {
            field: field.to_owned(),
            message: message.to_owned(),
            position: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use super::*;
    use crate::api::common::{Page, ProblemCode};
    use crate::api::members::OrgMember;
    use crate::ee::org_sso::dns;
    use crate::ee::org_sso::store as sso_store;
    use crate::test_support::{Member, Seeded, TestApp};

    const PATH: &str = "/api/v1/orgs/acme/scim";
    const TOKENS: &str = "/api/v1/orgs/acme/scim/tokens";

    /// An app with `features` licensed (none means no licence), `seed()` and a non-admin
    /// owner `owner@acme.test`.
    async fn app_with(features: &[&str]) -> (TestApp, Seeded, Member) {
        let app = if features.is_empty() {
            TestApp::new().await
        } else {
            TestApp::with_license(features).await
        };
        let seeded = app.seed().await;
        let owner = app
            .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
            .await;
        (app, seeded, owner)
    }

    /// A verified SCIM domain of `org`, written straight to `meta.db`.
    async fn verify_domain(app: &TestApp, org: OrgId, domain: &str) {
        let id = Uuid::now_v7();
        let domain = domain.to_owned();
        let token = dns::new_token().expect("token");
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row = sso_store::DomainRow {
                        id,
                        org_id: org,
                        domain,
                        token,
                        verified_at: None,
                        last_checked_at: None,
                        created_at: now,
                    };
                    sso_store::insert_domain(conn, &row).await?;
                    sso_store::mark_verified(conn, id, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// Writes the org's SCIM row without going through the route.
    async fn set_scim(app: &TestApp, org: OrgId, enabled: bool, role: OrgRole) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::put_org_scim(conn, org, enabled, role, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    /// Makes `user` a provisioned (managed) member of `org`, as a SCIM create does.
    async fn provision(app: &TestApp, org: OrgId, user: Uuid, email: &str) {
        let now = app.state.clock.now_ns();
        let fields = store::ScimUserFields {
            user_name: email.to_owned(),
            external_id: None,
            given_name: None,
            family_name: None,
            display_name: None,
            department: None,
            employee_number: None,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_user(conn, org, user, &fields, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn get_settings(app: &TestApp, cookie: &str) -> ScimSettings {
        let response = app
            .request(Method::GET, PATH)
            .header("cookie", cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        response.json()
    }

    async fn send_patch(
        app: &TestApp,
        cookie: &str,
        body: &Value,
    ) -> crate::test_support::TestResponse {
        app.request(Method::PATCH, PATH)
            .header("cookie", cookie)
            .json(body)
            .send()
            .await
    }

    async fn create_token(
        app: &TestApp,
        cookie: &str,
        name: &str,
    ) -> crate::test_support::TestResponse {
        app.request(Method::POST, TOKENS)
            .header("cookie", cookie)
            .json(&json!({ "name": name }))
            .send()
            .await
    }

    #[tokio::test]
    async fn settings_are_owner_only_and_session_only() {
        let (app, seeded, owner) = app_with(&[]).await;
        let admin = app
            .add_member(&seeded, "admin@acme.test", OrgRole::Admin)
            .await;
        let by_admin = app
            .request(Method::GET, PATH)
            .header("cookie", &admin.cookie)
            .send()
            .await;
        assert_eq!(by_admin.status, StatusCode::FORBIDDEN);
        let token = app.personal_token(owner.id, true).await;
        let by_token = app
            .request(Method::GET, PATH)
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await;
        assert_eq!(by_token.status, StatusCode::FORBIDDEN);
        let by_owner = get_settings(&app, &owner.cookie).await;
        assert!(!by_owner.enabled);
    }

    #[tokio::test]
    async fn enabling_needs_the_feature_and_a_domain() {
        let (app, _, owner) = app_with(&[]).await;
        let locked = send_patch(&app, &owner.cookie, &json!({ "enabled": true })).await;
        assert_eq!(locked.status, StatusCode::FORBIDDEN);
        assert_eq!(locked.problem().code, ProblemCode::PlanRequired);

        let (app, seeded, owner) = app_with(&["scim", "org_sso"]).await;
        let no_domain = send_patch(&app, &owner.cookie, &json!({ "enabled": true })).await;
        assert_eq!(no_domain.status, StatusCode::CONFLICT);
        assert_eq!(
            no_domain.problem().detail.as_deref(),
            Some("Verify a domain first.")
        );
        verify_domain(&app, seeded.org_id, "acme.com").await;
        let on = send_patch(&app, &owner.cookie, &json!({ "enabled": true })).await;
        assert_eq!(on.status, StatusCode::OK);
        assert!(on.json::<ScimSettings>().enabled);
        assert_eq!(get_settings(&app, &owner.cookie).await.verified_domains, 1);
    }

    #[tokio::test]
    async fn self_hosted_enabling_also_needs_org_sso() {
        // Off the cloud (D-EA-13): the licence must list org SSO as well as SCIM.
        let key = crate::test_support::license_key(&["scim"]);
        let app = TestApp::with_config(move |config| {
            config.entitlements.control_plane = false;
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await;
        let seeded = app.seed().await;
        let owner = app
            .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
            .await;
        verify_domain(&app, seeded.org_id, "acme.com").await;
        let refused = send_patch(&app, &owner.cookie, &json!({ "enabled": true })).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.problem().code, ProblemCode::PlanRequired);
        assert!(!get_settings(&app, &owner.cookie).await.enabled);
    }

    #[tokio::test]
    async fn disabling_works_when_lapsed() {
        let (app, seeded, owner) = app_with(&[]).await;
        set_scim(&app, seeded.org_id, true, OrgRole::Viewer).await;
        let off = send_patch(&app, &owner.cookie, &json!({ "enabled": false })).await;
        assert_eq!(off.status, StatusCode::OK);
        assert!(!off.json::<ScimSettings>().enabled);
        // Changing the default role needs the feature, so a lapsed org cannot.
        let role = send_patch(&app, &owner.cookie, &json!({ "default_role": "member" })).await;
        assert_eq!(role.status, StatusCode::FORBIDDEN);
        let bad = send_patch(&app, &owner.cookie, &json!({ "default_role": "admin" })).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_create_shows_the_secret_once_and_caps_at_two() {
        let (app, _, owner) = app_with(&["scim"]).await;
        let first = create_token(&app, &owner.cookie, "Okta").await;
        assert_eq!(first.status, StatusCode::CREATED);
        let created: CreatedScimToken = first.json();
        assert!(created.secret.starts_with("scim-cetra-"));
        assert!(created.secret.starts_with(&created.token.prefix));
        assert_eq!(
            create_token(&app, &owner.cookie, "Entra ID").await.status,
            StatusCode::CREATED
        );
        let third = create_token(&app, &owner.cookie, "Third").await;
        assert_eq!(third.status, StatusCode::CONFLICT);
        assert_eq!(
            third.problem().detail.as_deref(),
            Some("Revoke a token first.")
        );
        let listed = get_settings(&app, &owner.cookie).await;
        assert_eq!(listed.tokens.len(), 2);
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains(&created.secret)
        );
        let blank = create_token(&app, &owner.cookie, "   ").await;
        assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_create_needs_the_feature() {
        let (app, _, owner) = app_with(&[]).await;
        let locked = create_token(&app, &owner.cookie, "Okta").await;
        assert_eq!(locked.status, StatusCode::FORBIDDEN);
        assert_eq!(locked.problem().code, ProblemCode::PlanRequired);
    }

    #[tokio::test]
    async fn revoked_token_stops_working_at_once() {
        let (app, seeded, owner) = app_with(&["scim"]).await;
        verify_domain(&app, seeded.org_id, "acme.com").await;
        set_scim(&app, seeded.org_id, true, OrgRole::Viewer).await;
        let created: CreatedScimToken = create_token(&app, &owner.cookie, "Okta").await.json();
        let bearer = format!("Bearer {}", created.secret);
        let before = app
            .request(Method::GET, "/scim/v2/Users")
            .header("authorization", &bearer)
            .send()
            .await;
        assert_eq!(before.status, StatusCode::OK);
        let revoked = app
            .request(Method::DELETE, &format!("{TOKENS}/{}", created.token.id))
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT);
        let after = app
            .request(Method::GET, "/scim/v2/Users")
            .header("authorization", &bearer)
            .send()
            .await;
        assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    }

    /// An app with SCIM on, `acme.com` verified, and a managed member `ada@acme.com`.
    async fn managed_app(features: &[&str]) -> (TestApp, Seeded, Member, Member) {
        let (app, seeded, owner) = app_with(features).await;
        verify_domain(&app, seeded.org_id, "acme.com").await;
        set_scim(&app, seeded.org_id, true, OrgRole::Viewer).await;
        let ada = app
            .add_member(&seeded, "ada@acme.com", OrgRole::Member)
            .await;
        provision(&app, seeded.org_id, ada.id, "ada@acme.com").await;
        (app, seeded, owner, ada)
    }

    #[tokio::test]
    async fn managed_member_role_change_is_409_scim_managed() {
        let (app, _, owner, ada) = managed_app(&["scim"]).await;
        let changed = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{}", ada.id),
            )
            .header("cookie", &owner.cookie)
            .json(&json!({ "role": "viewer" }))
            .send()
            .await;
        assert_eq!(changed.status, StatusCode::CONFLICT);
        assert_eq!(changed.problem().code, ProblemCode::ScimManaged);
    }

    #[tokio::test]
    async fn managed_member_removal_and_self_leave_are_409() {
        let (app, _, owner, ada) = managed_app(&["scim"]).await;
        let removed = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{}", ada.id),
            )
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(removed.status, StatusCode::CONFLICT);
        assert_eq!(removed.problem().code, ProblemCode::ScimManaged);
        let left = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/members/{}", ada.id),
            )
            .header("cookie", &ada.cookie)
            .send()
            .await;
        assert_eq!(left.status, StatusCode::CONFLICT);
        assert_eq!(left.problem().code, ProblemCode::ScimManaged);
    }

    #[tokio::test]
    async fn managed_member_override_is_409() {
        let (app, seeded, owner, ada) = managed_app(&["scim", "project_roles"]).await;
        let project = seeded.project.id;
        let overridden = app
            .request(
                Method::PUT,
                &format!("/api/v1/projects/{project}/members/{}", ada.id),
            )
            .header("cookie", &owner.cookie)
            .json(&json!({ "role": "viewer" }))
            .send()
            .await;
        assert_eq!(overridden.status, StatusCode::CONFLICT);
        assert_eq!(overridden.problem().code, ProblemCode::ScimManaged);
    }

    #[tokio::test]
    async fn invites_to_verified_domains_are_409_while_scim_is_on() {
        let (app, _, owner, _) = managed_app(&["scim"]).await;
        let refused = app
            .request(Method::POST, "/api/v1/orgs/acme/invites")
            .header("cookie", &owner.cookie)
            .json(&json!({ "email": "new@acme.com", "role": "member" }))
            .send()
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert_eq!(refused.problem().code, ProblemCode::ScimManaged);
        let outsider = app
            .request(Method::POST, "/api/v1/orgs/acme/invites")
            .header("cookie", &owner.cookie)
            .json(&json!({ "email": "new@outside.test", "role": "member" }))
            .send()
            .await;
        assert_eq!(outsider.status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn locks_lift_when_scim_is_disabled() {
        let (app, _, owner, ada) = managed_app(&["scim"]).await;
        let off = send_patch(&app, &owner.cookie, &json!({ "enabled": false })).await;
        assert_eq!(off.status, StatusCode::OK);
        let changed = app
            .request(
                Method::PATCH,
                &format!("/api/v1/orgs/acme/members/{}", ada.id),
            )
            .header("cookie", &owner.cookie)
            .json(&json!({ "role": "viewer" }))
            .send()
            .await;
        assert_eq!(changed.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn member_list_marks_managed_members() {
        let (app, _, owner, ada) = managed_app(&["scim"]).await;
        let response = app
            .request(Method::GET, "/api/v1/orgs/acme/members")
            .header("cookie", &owner.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let page: Page<OrgMember> = response.json();
        let row = page
            .data
            .iter()
            .find(|member| member.user.id == ada.id)
            .expect("ada is listed");
        assert!(row.scim_managed);
        let owner_row = page
            .data
            .iter()
            .find(|member| member.user.id == owner.id)
            .expect("the owner is listed");
        assert!(!owner_row.scim_managed);
    }
}
