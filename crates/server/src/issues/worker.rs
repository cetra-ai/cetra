//! The IC candidate workers (track IC M3, D-IC-12): a loop that claims ready candidates and runs
//! each one on a worker slot, and the startup and janitor interruption of candidates left
//! `running`.

use std::sync::Arc;
use std::time::Duration;

use cetra_core::ids::TraceId;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use tokio::sync::watch;
use uuid::Uuid;

use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issues::{
    CandidateFinish, claim_candidate, finish_candidate, interrupt_candidates, ready_candidates,
};

/// How often the loop looks for ready candidates when nothing wakes it.
const TICK: Duration = Duration::from_secs(1);

/// Candidates read from the store per free worker slot.
const CANDIDATES_PER_SLOT: u32 = 4;

/// How long a candidate waits when its run is stopped before it can be run again (M3.4b stub).
const REQUEUE_NS: i64 = 60_000_000_000;

/// A candidate `running` for longer than this is interrupted by the janitor (D-I-12). The same
/// window as the eval janitor's `STALE_NS`.
const STALE_NS: i64 = 600_000_000_000;

/// Runs the candidate loop until `stop` is set. A pass runs when the tick fires or a worker
/// wakes the loop. Each claimed candidate runs on its own task, holding one `issue_workers`
/// slot.
pub(crate) async fn run_workers(state: AppState, mut stop: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return,
            _ = interval.tick() => {}
            () = state.issue_wake.notified() => {}
        }
        if let Err(error) = run_pass(&state).await {
            tracing::error!(error = %error, "issue worker pass failed");
        }
    }
}

/// One pass: reads the ready candidates of connections with a free slot and token, claims as
/// many as there are free worker slots, and starts each. Returns how many started.
async fn run_pass(state: &AppState) -> Result<usize, MetaError> {
    let now = state.clock.now_ns();
    let free = state.issue_workers.available_permits();
    if free == 0 {
        return Ok(0);
    }
    let saturated = state.eval.saturated_connections(now);
    let read = u32::try_from(free)
        .unwrap_or(u32::MAX)
        .saturating_mul(CANDIDATES_PER_SLOT);
    let ready = ready_candidates(state.meta.reader(), now, &saturated, read).await?;
    if ready.is_empty() {
        return Ok(0);
    }
    let mut picked = Vec::new();
    for candidate in ready {
        let Ok(permit) = Arc::clone(&state.issue_workers).try_acquire_owned() else {
            break;
        };
        picked.push((candidate.id, permit));
    }
    let ids: Vec<Uuid> = picked.iter().map(|(id, _)| *id).collect();
    let claims = state
        .meta
        .write(move |conn| Box::pin(async move { claim_all(conn, ids, now).await }))
        .await?;

    let mut started = 0;
    for ((id, permit), claim) in picked.into_iter().zip(claims) {
        // A `None` claim: the candidate left `queued` since the read. Its permit drops here.
        let Some(claims) = claim else { continue };
        let state = state.clone();
        tokio::spawn(async move {
            run_candidate(&state, id, claims).await;
            drop(permit);
            state.issues_wake();
        });
        started += 1;
    }
    Ok(started)
}

async fn claim_all(
    conn: &mut SqliteConnection,
    ids: Vec<Uuid>,
    now: i64,
) -> Result<Vec<Option<u32>>, MetaError> {
    let mut claims = Vec::with_capacity(ids.len());
    for id in ids {
        claims.push(claim_candidate(conn, id, now).await?);
    }
    Ok(claims)
}

/// Runs one claimed candidate. M3.4b stub: the candidate goes back to `queued` for a minute.
/// M3.4c replaces the body with the real run.
async fn run_candidate(state: &AppState, id: Uuid, claims: u32) {
    let now = state.clock.now_ns();
    let result = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                finish_candidate(
                    conn,
                    id,
                    claims,
                    CandidateFinish::Requeue {
                        not_before: now.saturating_add(REQUEUE_NS),
                    },
                    0.0,
                    now,
                )
                .await
            })
        })
        .await;
    if let Err(error) = result {
        tracing::error!(candidate_id = %id, error = %error, "issue candidate could not be requeued");
    }
}

/// Interrupts the candidates `running` since before `now − STALE_NS` (D-I-12, the janitor's
/// pass). Returns how many rows changed.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn janitor_once(state: &AppState, now: i64) -> Result<u64, MetaError> {
    let older_than = now.saturating_sub(STALE_NS);
    let changed = state
        .meta
        .write(move |conn| {
            Box::pin(async move { interrupt_candidates(conn, older_than, now).await })
        })
        .await?;
    if changed > 0 {
        state.issues_wake();
    }
    Ok(changed)
}

/// The trace id of one IC call (A10, D-IC-17): the first 16 bytes of SHA-256 over
/// `cetra.issue-call.v1`, the owner (a candidate or refit id), `claims` and `call`, both as `u32`
/// big-endian. `call` is 1 for the summary, 2 for the embedding and `3 + index` for a naming call.
/// A refit uses `claims` 0.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn call_trace_id(owner: Uuid, claims: u32, call: u32) -> TraceId {
    let mut hasher = Sha256::new();
    hasher.update(b"cetra.issue-call.v1");
    hasher.update(owner.as_bytes());
    hasher.update(claims.to_be_bytes());
    hasher.update(call.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    // The all-zero case is replaced above, so the only error of `from_bytes` cannot happen.
    TraceId::from_bytes(bytes).expect("a non-zero trace id")
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::call_trace_id;

    #[test]
    fn call_trace_id_is_deterministic() {
        let owner = Uuid::from_u128(7);
        assert_eq!(call_trace_id(owner, 1, 1), call_trace_id(owner, 1, 1));
    }

    #[test]
    fn call_trace_id_differs_by_owner_claims_and_call() {
        let owner = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        let base = call_trace_id(owner, 1, 1);
        assert_ne!(base, call_trace_id(other, 1, 1));
        assert_ne!(base, call_trace_id(owner, 2, 1));
        assert_ne!(base, call_trace_id(owner, 1, 2));
        assert_ne!(call_trace_id(owner, 1, 3), call_trace_id(owner, 1, 4));
    }

    #[test]
    fn call_trace_id_is_never_all_zero() {
        for n in 0..256_u128 {
            let id = call_trace_id(Uuid::from_u128(n), 0, u32::try_from(n).unwrap_or(0));
            assert_ne!(id.as_bytes(), &[0u8; 16]);
        }
    }
}
