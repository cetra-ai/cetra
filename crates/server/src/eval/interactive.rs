//! Interactive calls on an LLM connection: a playground run (and later tracks' interactive calls)
//! takes the connection's concurrency slot and a rate token like a queued job, but waits for them
//! instead of polling, so it goes ahead of rule jobs (D-V-35).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, sleep, timeout_at};

use super::dispatch::connection_limit;
use crate::http::AppState;
use crate::meta::connections::ConnectionRow;

/// How often a waiter re-checks for a rate token.
const TOKEN_POLL: Duration = Duration::from_millis(100);

/// The connection is busy for longer than the caller wants to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the connection is busy")]
pub(crate) struct Busy;

/// Holds the connection's concurrency slot until dropped. The rate token is spent.
#[derive(Debug)]
pub(crate) struct InteractivePermit {
    _slot: OwnedSemaphorePermit,
}

/// Waits up to `wait` for a rate token of `connection` (polled every 100 ms with the state's
/// clock), without holding a slot, then for a slot (FIFO, ahead of the dispatcher's
/// `try_acquire`) with the time that is left, and returns the permit.
///
/// A token is spent when it is taken, so a call that gets its token and then no slot before the
/// deadline answers `Busy` and the token is lost (it is not given back). At the deadline without a
/// token or a slot: `Busy`, holding nothing.
///
/// # Errors
///
/// Returns [`Busy`] when no token or no slot is free before `wait` is over.
pub(crate) async fn acquire(
    state: &AppState,
    connection: &ConnectionRow,
    wait: Duration,
) -> Result<InteractivePermit, Busy> {
    let deadline = Instant::now() + wait;
    let limit = connection_limit(
        &state.eval,
        connection.id,
        (connection.max_concurrency, connection.requests_per_minute),
        state.clock.now_ns(),
    );
    loop {
        if limit.take_token(state.clock.now_ns()) {
            break;
        }
        if Instant::now() + TOKEN_POLL > deadline {
            return Err(Busy);
        }
        sleep(TOKEN_POLL).await;
    }
    let slot = match timeout_at(deadline, Arc::clone(limit.slots()).acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return Err(Busy),
    };
    Ok(InteractivePermit { _slot: slot })
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::OrgId;
    use uuid::Uuid;

    use super::*;
    use crate::test_support::TestApp;

    /// A connection row with the given limits. `acquire` reads only its id and limits.
    fn row(max_concurrency: u32, requests_per_minute: u32) -> ConnectionRow {
        ConnectionRow {
            id: Uuid::now_v7(),
            org_id: OrgId::new_v7(),
            name: "Test".to_owned(),
            adapter: "openai".to_owned(),
            base_url: None,
            secret_enc: Vec::new(),
            api_key_hint: String::new(),
            extra_headers_enc: None,
            extra_header_names: Vec::new(),
            default_model: None,
            structured_output: "json_schema".to_owned(),
            max_concurrency,
            requests_per_minute,
            status: "ok".to_owned(),
            last_error: None,
            last_error_at: None,
            last_tested_at: None,
            last_test_ok: None,
            created_by: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn takes_a_slot_and_a_token() {
        let app = TestApp::new().await;
        let row = row(2, 300);
        let permit = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("admitted");
        let limit = connection_limit(&app.state.eval, row.id, (2, 300), 0);
        assert_eq!(limit.slots().available_permits(), 1);
        drop(permit);
        assert_eq!(limit.slots().available_permits(), 2);
    }

    #[tokio::test]
    async fn waits_for_a_slot() {
        let app = TestApp::new().await;
        let row = row(1, 300);
        let first = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the first slot");
        let (second, ()) = tokio::join!(acquire(&app.state, &row, Duration::from_secs(2)), async {
            sleep(Duration::from_millis(200)).await;
            drop(first);
        });
        assert!(second.is_ok(), "the waiter gets the slot once it is free");
    }

    #[tokio::test]
    async fn busy_at_the_deadline() {
        let app = TestApp::new().await;
        let row = row(1, 300);
        let _held = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the first slot");
        let refused = acquire(&app.state, &row, Duration::from_millis(200))
            .await
            .err();
        assert_eq!(refused, Some(Busy));
    }

    #[tokio::test]
    async fn busy_without_tokens() {
        let app = TestApp::new().await;
        // One request a minute: a capacity of one token, and the clock is frozen.
        let row = row(2, 1);
        let first = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the first token");
        drop(first);
        let refused = acquire(&app.state, &row, Duration::from_millis(200))
            .await
            .err();
        assert_eq!(refused, Some(Busy));
        // The refused call holds no slot.
        let limit = connection_limit(&app.state.eval, row.id, (2, 1), 0);
        assert_eq!(limit.slots().available_permits(), 2);
    }

    #[tokio::test]
    async fn a_waiter_for_a_token_holds_no_slot() {
        let app = TestApp::new().await;
        // One request a minute, two slots: the first call takes the token and a slot.
        let row = row(2, 1);
        let first = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the first token");
        let limit = connection_limit(&app.state.eval, row.id, (2, 1), 0);
        let (refused, free) = tokio::join!(
            acquire(&app.state, &row, Duration::from_millis(300)),
            async {
                sleep(Duration::from_millis(100)).await;
                limit.slots().available_permits()
            }
        );
        assert_eq!(refused.err(), Some(Busy));
        assert_eq!(free, 1, "the call waiting for a token holds no slot");
        drop(first);
    }

    #[tokio::test]
    async fn dispatcher_sees_the_slot_taken() {
        let app = TestApp::new().await;
        let row = row(1, 300);
        let permit = acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("admitted");
        let limit = connection_limit(&app.state.eval, row.id, (1, 300), 0);
        assert!(
            Arc::clone(limit.slots()).try_acquire_owned().is_err(),
            "the dispatcher's try_acquire finds no slot"
        );
        drop(permit);
        assert!(Arc::clone(limit.slots()).try_acquire_owned().is_ok());
    }
}
