//! The in-app assistant (track AS). M1: conversation expiry and tool-result stubbing (D-AS-10, D-AS-12).

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::assistant::{STUB_BATCH, expire_conversations, stub_tool_results};

/// A conversation idle this long is deleted (D-AS-10): 30 days.
pub(crate) const CONVERSATION_TTL_NS: i64 = 30 * 24 * 60 * 60 * 1_000_000_000;
/// How often the job runs.
const EXPIRY_EVERY: Duration = Duration::from_secs(60 * 60);

/// What one run removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ExpiryOutcome {
    pub(crate) expired: u64,
    pub(crate) stubbed: u64,
}

/// One run: deletes the conversations idle for [`CONVERSATION_TTL_NS`], then stubs every due tool result,
/// one batch of [`STUB_BATCH`] per write transaction.
pub(crate) async fn expire_once(state: &AppState) -> Result<ExpiryOutcome, MetaError> {
    let now = state.clock.now_ns();
    let idle_before = now.saturating_sub(CONVERSATION_TTL_NS);
    let expired = state
        .meta
        .write(move |conn| Box::pin(async move { expire_conversations(conn, idle_before).await }))
        .await?;
    let mut stubbed = 0;
    loop {
        let batch = state
            .meta
            .write(move |conn| Box::pin(async move { stub_tool_results(conn, now).await }))
            .await?;
        stubbed += batch;
        if batch < STUB_BATCH.unsigned_abs() {
            break;
        }
    }
    Ok(ExpiryOutcome { expired, stubbed })
}

/// Runs [`expire_once`] at start, then every hour.
pub(crate) fn spawn_expiry(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(EXPIRY_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match expire_once(&state).await {
                Ok(outcome) if outcome.expired > 0 || outcome.stubbed > 0 => {
                    let ExpiryOutcome { expired, stubbed } = outcome;
                    tracing::info!(expired, stubbed, "assistant conversations expired");
                }
                Ok(_) => {}
                Err(error) => tracing::error!(error = %error, "assistant expiry failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::ProjectId;
    use sqlx::SqliteConnection;
    use uuid::Uuid;

    use super::*;
    use crate::meta::assistant::{insert_conversation, insert_message_raw, list_conversations};
    use crate::test_support::TestApp;

    const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;
    /// The test clock's start for these tests.
    const T0: i64 = 1_800_000_000_000_000_000;
    const TOOL_PARTS: &str = r#"[{"type":"tool_result","tool":"lookup","result":{"rows":3}}]"#;

    async fn create_at(app: &TestApp, project: ProjectId, user: Uuid, at: i64) -> Uuid {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_conversation(conn, Uuid::now_v7(), project, user, at)
                        .await
                        .map(|row| row.id)
                })
            })
            .await
            .unwrap()
    }

    async fn set_activity_on(
        conn: &mut SqliteConnection,
        id: Uuid,
        at: i64,
    ) -> Result<(), MetaError> {
        sqlx::query("UPDATE assistant_conversations SET last_active_at = ? WHERE id = ?")
            .bind(at)
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn set_activity(app: &TestApp, id: Uuid, at: i64) {
        app.state
            .meta
            .write(move |conn| Box::pin(async move { set_activity_on(conn, id, at).await }))
            .await
            .unwrap();
    }

    async fn insert_tool_messages_on(
        conn: &mut SqliteConnection,
        conversation: Uuid,
        count: i64,
        stub_after: i64,
    ) -> Result<(), MetaError> {
        for seq in 1..=count {
            insert_message_raw(
                conn,
                conversation,
                seq,
                "tool",
                TOOL_PARTS,
                None,
                Some(stub_after),
                T0,
            )
            .await?;
        }
        Ok(())
    }

    /// The raw `parts` text and `stub_after` of one message.
    async fn message_row(app: &TestApp, conversation: Uuid, seq: i64) -> (String, Option<i64>) {
        sqlx::query_as(
            "SELECT parts, stub_after FROM assistant_messages \
             WHERE conversation_id = ? AND seq = ?",
        )
        .bind(conversation.to_string())
        .bind(seq)
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    async fn listed(app: &TestApp, project: ProjectId, user: Uuid) -> Vec<Uuid> {
        list_conversations(app.state.meta.reader(), project, user)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn expires_idle_conversations() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (project, user) = (seeded.project.id, seeded.user_id);
        app.state.clock.set(T0);
        let idle = create_at(&app, project, user, T0).await;
        let active = create_at(&app, project, user, T0).await;
        set_activity(&app, active, T0 + 10 * DAY_NS).await;

        app.state.clock.set(T0 + CONVERSATION_TTL_NS + 1);
        let outcome = expire_once(&app.state).await.unwrap();
        assert_eq!(
            outcome,
            ExpiryOutcome {
                expired: 1,
                stubbed: 0
            }
        );
        assert_eq!(listed(&app, project, user).await, vec![active]);
        assert!(!listed(&app, project, user).await.contains(&idle));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_a_conversation_active_exactly_at_the_limit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (project, user) = (seeded.project.id, seeded.user_id);
        app.state.clock.set(T0);
        let kept = create_at(&app, project, user, T0).await;

        app.state.clock.set(T0 + CONVERSATION_TTL_NS);
        let outcome = expire_once(&app.state).await.unwrap();
        assert_eq!(
            outcome,
            ExpiryOutcome {
                expired: 0,
                stubbed: 0
            }
        );
        assert_eq!(listed(&app, project, user).await, vec![kept]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stubs_when_due() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (project, user) = (seeded.project.id, seeded.user_id);
        app.state.clock.set(T0);
        let conversation = create_at(&app, project, user, T0).await;
        let stub_after = T0 + DAY_NS;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_message_raw(
                        conn,
                        conversation,
                        1,
                        "tool",
                        TOOL_PARTS,
                        None,
                        Some(stub_after),
                        T0,
                    )
                    .await
                })
            })
            .await
            .unwrap();

        app.state.clock.set(stub_after - 1);
        assert_eq!(
            expire_once(&app.state).await.unwrap(),
            ExpiryOutcome {
                expired: 0,
                stubbed: 0
            }
        );
        assert_eq!(
            message_row(&app, conversation, 1).await,
            (TOOL_PARTS.to_owned(), Some(stub_after))
        );

        app.state.clock.set(stub_after);
        assert_eq!(
            expire_once(&app.state).await.unwrap(),
            ExpiryOutcome {
                expired: 0,
                stubbed: 1
            }
        );
        let (parts, stub_after_now) = message_row(&app, conversation, 1).await;
        assert_eq!(stub_after_now, None);
        let parts: serde_json::Value = serde_json::from_str(&parts).unwrap();
        assert!(parts[0]["result"].is_null());
        assert_eq!(parts[0]["stubbed"], serde_json::Value::Bool(true));
        assert_eq!(parts[0]["tool"], "lookup");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stubs_more_than_a_batch_in_one_run() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (project, user) = (seeded.project.id, seeded.user_id);
        app.state.clock.set(T0);
        let conversation = create_at(&app, project, user, T0).await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_tool_messages_on(conn, conversation, 501, T0).await })
            })
            .await
            .unwrap();

        let outcome = expire_once(&app.state).await.unwrap();
        assert_eq!(
            outcome,
            ExpiryOutcome {
                expired: 0,
                stubbed: 501
            }
        );
        let still_due: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assistant_messages \
             WHERE conversation_id = ? AND stub_after IS NOT NULL",
        )
        .bind(conversation.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(still_due, 0);
    }
}
