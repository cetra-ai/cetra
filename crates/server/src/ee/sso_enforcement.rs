// SPDX-License-Identifier: Elastic-2.0
//! Instance-wide SSO enforcement, `auth.enforce_sso_domains` (spec 08, D-P-4, D-P-8).
//!
//! It applies when the configured key is a genuine licence that lists `sso_enforcement`,
//! expired or not: a lapse never re-opens password login for those domains.

use crate::entitlements::Feature;
use crate::http::AppState;

/// Whether `email` (already normalized) must sign in through SSO.
pub(crate) fn requires_sso(state: &AppState, email: &str) -> bool {
    applies(state)
        && crate::routes::auth::domain_listed(&state.config.auth.enforce_sso_domains, email)
}

/// Whether the configured domains are enforced at all.
pub(crate) fn applies(state: &AppState) -> bool {
    state
        .entitlements
        .license()
        .listed_features()
        .contains(Feature::SsoEnforcement)
}

/// Warns once at startup when domains are configured but not enforced. No domain is logged.
pub(crate) fn warn_if_unlicensed(state: &AppState) {
    if !state.config.auth.enforce_sso_domains.is_empty() && !applies(state) {
        tracing::warn!(
            domains = state.config.auth.enforce_sso_domains.len(),
            "auth.enforce_sso_domains is set but not enforced: it needs an enterprise licence"
        );
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Method;

    use crate::api::common::ProblemCode;
    use crate::bootstrap;
    use crate::test_support::{TestApp, capture_logs, license_key, license_key_until};

    const PASSWORD: &str = "cetra-ok-1";

    fn login_body(email: &str) -> serde_json::Value {
        serde_json::json!({ "email": email, "password": PASSWORD })
    }

    fn sso_config(config: &mut crate::config::ServerConfig) {
        config.auth.enforce_sso_domains = vec!["sso.example".to_owned()];
    }

    #[tokio::test]
    async fn domains_without_a_license_are_not_enforced() {
        let app = TestApp::with_config(sso_config).await;
        let response = app
            .request(Method::POST, "/api/auth/login")
            .json(&login_body("x@sso.example"))
            .send()
            .await;
        assert_eq!(response.status, 401);
        assert_ne!(response.problem().code, ProblemCode::SsoRequired);
        assert!(!super::applies(&app.state));
    }

    #[tokio::test]
    async fn expired_license_keeps_enforcing() {
        let key = license_key_until(
            &["sso_enforcement"],
            "2025-12-01T00:00:00Z",
            "2025-12-31T00:00:00Z",
        );
        let app = TestApp::with_config(move |config| {
            sso_config(config);
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await;
        let response = app
            .request(Method::POST, "/api/auth/login")
            .json(&login_body("x@sso.example"))
            .send()
            .await;
        assert_eq!(response.status, 403);
        assert_eq!(response.problem().code, ProblemCode::SsoRequired);
    }

    #[tokio::test]
    async fn license_without_the_feature_is_not_enough() {
        let key = license_key(&["project_roles"]);
        let app = TestApp::with_config(move |config| {
            sso_config(config);
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await;
        assert!(!super::applies(&app.state));
        let response = app
            .request(Method::POST, "/api/auth/login")
            .json(&login_body("x@sso.example"))
            .send()
            .await;
        assert_eq!(response.status, 401);
        assert_ne!(response.problem().code, ProblemCode::SsoRequired);
    }

    #[tokio::test]
    async fn warning_is_logged_without_domains() {
        let app = TestApp::with_config(sso_config).await;
        let ((), logs) = capture_logs(async { super::warn_if_unlicensed(&app.state) }).await;
        assert!(logs.contains("not enforced"), "{logs}");
        assert!(logs.contains("domains=1"), "{logs}");
        assert!(!logs.contains("sso.example"), "{logs}");
    }

    #[tokio::test]
    async fn admin_settings_report_enforcement() {
        let unlicensed = TestApp::with_config(sso_config).await;
        assert!(!admin_settings_applied(&unlicensed).await);

        let key = license_key(&["sso_enforcement"]);
        let licensed = TestApp::with_config(move |config| {
            sso_config(config);
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await;
        assert!(admin_settings_applied(&licensed).await);
    }

    /// Sets up the instance admin and returns the `GET /api/v1/admin/settings` answer's
    /// `enforce_sso_applied`.
    async fn admin_settings_applied(app: &TestApp) -> bool {
        bootstrap::prepare(&app.state).await.unwrap();
        let token = app.state.setup.token().unwrap().expect("setup token");
        let created = app
            .request(Method::POST, "/api/auth/setup")
            .json(&serde_json::json!({
                "setup_token": token,
                "email": "ada@example.com",
                "password": PASSWORD,
                "name": "Ada",
                "org_name": "Acme",
                "project_name": "Support bot",
            }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let cookie = created
            .header("set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let settings = app
            .request(Method::GET, "/api/v1/admin/settings")
            .header("cookie", &cookie)
            .send()
            .await;
        assert_eq!(settings.status, 200);
        settings.json::<serde_json::Value>()["enforce_sso_applied"]
            .as_bool()
            .unwrap()
    }

    /// Auth review #12 (D-P-69): listing a domain also stops the password sessions, personal
    /// access tokens and resets of that domain, not only new logins.
    #[tokio::test]
    async fn enforced_domains_lose_password_sessions_tokens_and_resets() {
        use crate::auth::OrgRole;
        use crate::auth::secret::{self, KeyKind};
        use crate::auth::session::{SessionOrigin, start_session};
        use crate::meta::personal_tokens::{self, NewToken};

        let key = license_key(&["sso_enforcement"]);
        let app = TestApp::with_mailer_config(move |config| {
            sso_config(config);
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await;
        let seeded = app.seed().await;
        let listed = crate::test_support::member_in_org(
            &app,
            seeded.org_id,
            "x@sso.example",
            OrgRole::Member,
        )
        .await;
        let other = crate::test_support::member_in_org(
            &app,
            seeded.org_id,
            "y@other.example",
            OrgRole::Member,
        )
        .await;
        let me = |cookie: String| {
            let app = &app;
            async move {
                app.request(Method::GET, "/api/auth/me")
                    .header("cookie", &cookie)
                    .send()
                    .await
                    .status
            }
        };
        // A session no provider started stops resolving; other domains are untouched.
        assert_eq!(me(listed.cookie.clone()).await, 401);
        assert_eq!(me(other.cookie.clone()).await, 200);
        // A provider's session passes.
        let header = start_session(&app.state, listed.id, None, None, SessionOrigin::Provider)
            .await
            .unwrap();
        let provider_cookie = header
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(me(provider_cookie).await, 200);

        // A personal access token of the domain is refused.
        let minted = secret::new_key(KeyKind::Personal).unwrap();
        let bearer = format!("Bearer {}", minted.plaintext);
        let now = app.state.clock.now_ns();
        let user_id = listed.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    personal_tokens::insert_token(
                        conn,
                        NewToken {
                            user_id,
                            name: "ci".to_owned(),
                            prefix: minted.prefix,
                            hash: minted.hash,
                            scopes: vec![crate::auth::roles::KeyScope::Read],
                            expires_at: None,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let pat = app
            .request(Method::GET, "/api/auth/me")
            .header("authorization", &bearer)
            .send()
            .await;
        assert_eq!(pat.status, 401);

        // Forgot sends nothing, and a reset token minted earlier is refused.
        let forgot = app
            .request(Method::POST, "/api/auth/password/forgot")
            .json(&serde_json::json!({ "email": "x@sso.example" }))
            .send()
            .await;
        assert_eq!(forgot.status, 204);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(app.sent().is_empty());
        let token = secret::new_opaque_token().unwrap();
        let hash = token.hash.clone();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::password_resets::insert_reset(
                        conn,
                        &hash,
                        user_id,
                        now,
                        now + 3_600_000_000_000,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let reset = app
            .request(Method::POST, "/api/auth/password/reset")
            .json(&serde_json::json!({ "token": token.plaintext, "password": "new-cetra-1" }))
            .send()
            .await;
        assert_eq!(reset.status, 403);
        assert_eq!(reset.problem().code, ProblemCode::SsoRequired);
    }
}
