//! Usage notices at 80 % and 100 % of `units_included` (D-P-18, D-P-38).
//!
//! Each notice is one `usage_notices` row per org, month and threshold, so it is sent once.
//! The owners get one email, for the highest threshold newly recorded.

use cetra_core::ids::OrgId;

use crate::api::entitlements::UsageNotice;
use crate::auth::OrgRole;
use crate::email::Email;
use crate::entitlements::units;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::members::org_members;
use crate::usage::month_bounds;

/// The first threshold, in percent of `units_included`.
const WARNING: u8 = 80;
/// The second threshold.
const REACHED: u8 = 100;

/// Records and sends the month's 80 % / 100 % notices (D-P-38). Returns how many were recorded.
///
/// Every active org is checked, so orgs without a cap are counted too. An org without
/// `units_included` is skipped. With no mailer the rows are still written and nothing is sent.
/// An org whose check fails is logged (its id only) and the others are still checked.
///
/// # Errors
///
/// Returns [`MetaError`] when the list of orgs cannot be read.
pub(crate) async fn notices_once(state: &AppState) -> Result<u32, MetaError> {
    let now = state.clock.now_ns();
    let month = month_bounds(now).0.format("%Y-%m").to_string();
    let orgs: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, name, slug FROM orgs WHERE state = 'active'")
            .fetch_all(state.meta.reader())
            .await?;
    let mut recorded = 0_u32;
    for (id, name, slug) in orgs {
        match notify_org(state, now, &month, &id, &name, &slug).await {
            Ok(count) => recorded = recorded.saturating_add(count),
            Err(error) => {
                tracing::warn!(org = %id, error = %error, "usage notice check failed; continuing");
            }
        }
    }
    Ok(recorded)
}

/// One org's notices for `month`: records the new thresholds and mails the owners once.
async fn notify_org(
    state: &AppState,
    now: i64,
    month: &str,
    id: &str,
    name: &str,
    slug: &str,
) -> Result<u32, MetaError> {
    let org: OrgId = id.parse().map_err(|_| MetaError::Corrupt {
        table: "orgs",
        column: "id",
    })?;
    let entitlements = state.entitlements.resolve(org, now);
    let Some(included) = entitlements.limits.units_included.filter(|n| *n > 0) else {
        return Ok(0);
    };
    let units = state.usage.month_units(&state.meta, org, now).await?;
    let thresholds: &'static [u8] = match units::notice(units, Some(included)) {
        UsageNotice::None => return Ok(0),
        UsageNotice::Warning => &[WARNING],
        UsageNotice::Reached => &[REACHED, WARNING],
    };
    let month = month.to_owned();
    let newly: Vec<u8> = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let mut newly = Vec::new();
                for &threshold in thresholds {
                    let result = sqlx::query(
                        "INSERT INTO usage_notices (org_id, month, threshold, sent_at) \
                         VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                    )
                    .bind(org.to_string())
                    .bind(&month)
                    .bind(i64::from(threshold))
                    .bind(now)
                    .execute(&mut *conn)
                    .await?;
                    if result.rows_affected() == 1 {
                        newly.push(threshold);
                    }
                }
                Ok(newly)
            })
        })
        .await?;
    let Some(&threshold) = newly.iter().max() else {
        return Ok(0);
    };
    if let Some(mailer) = &state.mailer {
        let paid = entitlements
            .limits
            .units_cap
            .is_none_or(|cap| cap > included);
        let url = state
            .config
            .public_url
            .join(&format!("/{slug}/settings/plan"));
        let owners = org_members(state.meta.reader(), org).await?;
        for owner in owners
            .into_iter()
            .filter(|member| member.role == OrgRole::Owner && member.user.disabled_at.is_none())
        {
            mailer.send_in_background(
                owner.user.email,
                Email::UsageNotice {
                    org_name: name.to_owned(),
                    threshold,
                    units,
                    included,
                    paid,
                    url: url.clone(),
                },
            );
        }
    }
    Ok(u32::try_from(newly.len()).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::email::SentMail;
    use crate::test_support::{TestApp, set_limits};

    use super::notices_once;

    /// Waits up to a second for the background sends to reach `count` messages.
    async fn wait_for_sent(app: &TestApp, count: usize) -> Vec<SentMail> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while app.sent().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.sent()
    }

    async fn recorded_rows(app: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_notices")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn each_threshold_is_recorded_and_mailed_once() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 85, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        let sent = wait_for_sent(&app, 1).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "ada@example.com");
        assert_eq!(sent[0].subject, "cetra: Acme used 80% of its monthly units");
        assert!(
            sent[0]
                .text
                .contains("has used 85 of its 100 monthly units")
        );

        // The same month again: nothing new is recorded or sent.
        assert_eq!(notices_once(&app.state).await.unwrap(), 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(app.sent().len(), 1);

        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 20, 0, 0);
        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        let sent = wait_for_sent(&app, 2).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1].subject,
            "cetra: Acme used 100% of its monthly units"
        );
        assert!(sent[1].text.contains("reached its 100 monthly units"));
        assert_eq!(recorded_rows(&app).await, 2);
    }

    #[tokio::test]
    async fn a_jump_past_100_in_one_tick_mails_once_for_100() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 200}),
        );
        // February: nothing counted yet.
        app.state.clock.advance(Duration::from_secs(40 * 86_400));
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 120, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 2);
        let sent = wait_for_sent(&app, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(app.sent().len(), 1, "{sent:?}");
        assert_eq!(
            app.sent()[0].subject,
            "cetra: Acme used 100% of its monthly units"
        );
        assert_eq!(recorded_rows(&app).await, 2);
    }

    #[tokio::test]
    async fn only_owners_are_mailed() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        app.add_member(&seeded, "grace@example.com", crate::auth::OrgRole::Member)
            .await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 85, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        let sent = wait_for_sent(&app, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "ada@example.com");
    }

    #[tokio::test]
    async fn no_included_units_records_nothing() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_cap": 10}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 500, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 0);
        assert_eq!(recorded_rows(&app).await, 0);
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn disabled_owners_are_not_mailed() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        app.add_member(&seeded, "grace@example.com", crate::auth::OrgRole::Owner)
            .await;
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE users SET disabled_at = 1 WHERE email = 'ada@example.com'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 85, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        let sent = wait_for_sent(&app, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].to, "grace@example.com");
    }

    #[tokio::test]
    async fn a_failing_org_is_skipped_and_the_others_are_notified() {
        let app = TestApp::with_mailer().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        // A second org whose notice insert always fails.
        let broken = cetra_core::ids::OrgId::new_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::orgs::insert_org(conn, broken, "Beta", "beta", 0).await?;
                    sqlx::query(
                        "CREATE TRIGGER fail_beta_notice BEFORE INSERT ON usage_notices \
                         WHEN NEW.org_id = (SELECT id FROM orgs WHERE slug = 'beta') \
                         BEGIN SELECT RAISE(ABORT, 'test failure'); END",
                    )
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        set_limits(
            &app,
            broken,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(broken, seeded.project.id, now, 85, 0, 0);
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 85, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        let sent = wait_for_sent(&app, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].to, "ada@example.com");
        assert!(sent[0].text.contains("Acme"));
    }

    #[tokio::test]
    async fn without_a_mailer_the_rows_are_still_written() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_included": 100, "units_cap": 100}),
        );
        let now = app.state.clock.now_ns();
        app.state
            .usage
            .add(seeded.org_id, seeded.project.id, now, 85, 0, 0);

        assert_eq!(notices_once(&app.state).await.unwrap(), 1);
        assert_eq!(notices_once(&app.state).await.unwrap(), 0);
        assert_eq!(recorded_rows(&app).await, 1);
    }
}
