//! The device authorization store (RFC 8628, D-AO-3, D-AO-5): pending codes and the
//! grants waiting for their CLI. In memory only: a restart loses pending codes, and the
//! plaintext secrets of a grant never reach the disk or a log.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use cetra_core::ids::ProjectId;

use crate::auth::AuthError;
use crate::auth::secret::{hash_token, new_opaque_token, random_bytes};

/// A code lives 10 minutes.
pub(crate) const CODE_TTL_NS: i64 = 600 * 1_000_000_000;
/// The first polling interval, in seconds.
pub(crate) const POLL_INTERVAL_S: u64 = 5;
/// What `slow_down` adds to a code's interval, in seconds.
pub(crate) const SLOW_DOWN_STEP_S: u64 = 5;
/// How long an approval may take between `claim` and `complete`.
pub(crate) const APPROVING_TTL_NS: i64 = 60 * 1_000_000_000;
/// At most this many codes at once (pending, being approved or approved and not collected).
pub(crate) const MAX_CODES: usize = 10_000;
/// How far past `CODE_TTL_NS` a registered code may expire: the clock skew between the control
/// plane that issued it and this cell.
const REGISTER_SKEW_NS: i64 = 30 * 1_000_000_000;
/// The user-code letters: no vowels, so a code never spells a word (RFC 8628 § 6.1).
pub(crate) const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Login,
    Init,
}

/// What `issue` hands to the CLI. `Debug` hides the device code.
pub(crate) struct Issued {
    /// Plaintext, 43 base64url characters.
    pub(crate) device_code: String,
    /// `XXXX-XXXX`.
    pub(crate) user_code: String,
}

impl fmt::Debug for Issued {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Issued")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .finish()
    }
}

/// The project of an `init` grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GrantProject {
    pub(crate) id: ProjectId,
    pub(crate) org_slug: String,
    pub(crate) slug: String,
    pub(crate) name: String,
}

/// The secrets minted at approval. `Debug` hides `token` and `ingest_key`.
pub(crate) struct Grant {
    pub(crate) token: String,
    pub(crate) project: Option<GrantProject>,
    pub(crate) ingest_key: Option<String>,
}

impl fmt::Debug for Grant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Grant")
            .field("token", &"<redacted>")
            .field("project", &self.project)
            .field(
                "ingest_key",
                &self.ingest_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// A pending code as the approval page sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) purpose: Purpose,
    pub(crate) host: Option<String>,
    pub(crate) created_at: i64,
}

/// The answer to one poll. `Debug` is derived: `Grant`'s own `Debug` hides the secrets.
#[derive(Debug)]
pub(crate) enum Poll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Granted(Grant),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DeviceError {
    /// `MAX_CODES` codes exist.
    #[error("too many device codes")]
    Full,
    /// The code is unknown, expired, or not in the state the call needs.
    #[error("device code not found")]
    NotFound,
    /// A registration's field is malformed; the name is the field's.
    #[error("{0}")]
    Invalid(&'static str),
    /// The registration's device code or user code is already registered, with other details.
    #[error("device code already registered")]
    Taken,
    #[error(transparent)]
    Auth(#[from] AuthError),
}

pub(crate) struct DeviceStore {
    inner: Mutex<Inner>,
}

impl DeviceStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                by_device: HashMap::new(),
                by_user: HashMap::new(),
                next_ticket: 1,
            }),
        }
    }

    /// A new pending code.
    pub(crate) fn issue(
        &self,
        purpose: Purpose,
        host: Option<String>,
        now: i64,
    ) -> Result<Issued, DeviceError> {
        let mut inner = lock(&self.inner);
        if inner.by_device.len() >= MAX_CODES {
            inner.sweep(now);
        }
        if inner.by_device.len() >= MAX_CODES {
            return Err(DeviceError::Full);
        }
        let token = new_opaque_token()?;
        let mut letters = None;
        for _ in 0..5 {
            let candidate = draw_letters()?;
            if !inner.by_user.contains_key(&candidate) {
                letters = Some(candidate);
                break;
            }
        }
        // Five clashes in a row can only mean a broken RNG or a full table.
        let letters = letters.ok_or(DeviceError::Full)?;
        let entry = Entry {
            user_code: letters.clone(),
            purpose,
            host,
            created_at: now,
            expires_at: now.saturating_add(CODE_TTL_NS),
            interval_s: POLL_INTERVAL_S,
            last_poll: None,
            state: State::Pending,
        };
        inner.by_user.insert(letters.clone(), token.hash.clone());
        inner.by_device.insert(token.hash, entry);
        Ok(Issued {
            device_code: token.plaintext,
            user_code: format!("{}-{}", &letters[..4], &letters[4..]),
        })
    }

    /// A code the control plane issued (D-AO-14): stored as if this cell had issued it.
    /// `device_code_sha256` is the lowercase hex SHA-256 of the device code, 64 characters.
    /// An identical retry of a pending registration is `Ok`; any other clash is `Taken`.
    pub(crate) fn register(
        &self,
        device_code_sha256: &str,
        user_code: &str,
        purpose: Purpose,
        host: Option<String>,
        expires_at: i64,
        now: i64,
    ) -> Result<(), DeviceError> {
        if !is_sha256_hex(device_code_sha256) {
            return Err(DeviceError::Invalid("device_code_sha256"));
        }
        let letters = normalize_user_code(user_code).ok_or(DeviceError::Invalid("user_code"))?;
        let latest = now
            .saturating_add(CODE_TTL_NS)
            .saturating_add(REGISTER_SKEW_NS);
        if now >= expires_at || expires_at > latest {
            return Err(DeviceError::Invalid("expires_at"));
        }
        let mut inner = lock(&self.inner);
        inner.sweep(now);
        if let Some(existing) = inner.by_device.get(device_code_sha256) {
            let identical = matches!(existing.state, State::Pending)
                && existing.user_code == letters
                && existing.purpose == purpose
                && existing.host == host
                && existing.expires_at == expires_at;
            return if identical {
                Ok(())
            } else {
                Err(DeviceError::Taken)
            };
        }
        if inner.by_user.contains_key(&letters) {
            return Err(DeviceError::Taken);
        }
        // Only a new entry counts against the cap, so an identical retry at the cap is still `Ok`.
        if inner.by_device.len() >= MAX_CODES {
            return Err(DeviceError::Full);
        }
        let entry = Entry {
            user_code: letters.clone(),
            purpose,
            host,
            created_at: now,
            expires_at,
            interval_s: POLL_INTERVAL_S,
            last_poll: None,
            state: State::Pending,
        };
        inner.by_user.insert(letters, device_code_sha256.to_owned());
        inner.by_device.insert(device_code_sha256.to_owned(), entry);
        Ok(())
    }

    /// A pending, unexpired code; `None` otherwise.
    pub(crate) fn pending(&self, user_code: &str, now: i64) -> Option<Pending> {
        let mut inner = lock(&self.inner);
        let hash = inner.find(user_code, now)?;
        let entry = inner.by_device.get(&hash)?;
        matches!(entry.state, State::Pending).then(|| entry.view())
    }

    /// Pending → approving. While approving (at most `APPROVING_TTL_NS`), `deny` and `claim`
    /// refuse it and a poll answers `Pending`. An approval older than that falls back to
    /// pending: its handler was dropped (a closed tab) and never called `release` or `complete`.
    /// Returns the pending view and the approval's ticket, which `release` and `complete` need.
    pub(crate) fn claim(&self, user_code: &str, now: i64) -> Result<(Pending, u64), DeviceError> {
        let mut inner = lock(&self.inner);
        let hash = inner.find(user_code, now).ok_or(DeviceError::NotFound)?;
        let ticket = inner.next_ticket;
        let entry = inner
            .by_device
            .get_mut(&hash)
            .ok_or(DeviceError::NotFound)?;
        if !matches!(entry.state, State::Pending) {
            return Err(DeviceError::NotFound);
        }
        entry.state = State::Approving { since: now, ticket };
        let view = entry.view();
        inner.next_ticket = ticket.wrapping_add(1);
        Ok((view, ticket))
    }

    /// Approving → pending, after the minting failed. Does nothing unless the code is approving
    /// under `ticket`, so a late release never undoes a later approval.
    pub(crate) fn release(&self, user_code: &str, ticket: u64) {
        let mut inner = lock(&self.inner);
        if let Some(entry) = inner.by_user_mut(user_code)
            && matches!(entry.state, State::Approving { ticket: held, .. } if held == ticket)
        {
            entry.state = State::Pending;
        }
    }

    /// Approving → approved, with the grant. Refused with `NotFound` unless the code is
    /// approving under `ticket`: an approval that went stale and was claimed again gets no grant.
    pub(crate) fn complete(
        &self,
        user_code: &str,
        ticket: u64,
        grant: Grant,
    ) -> Result<(), DeviceError> {
        let mut inner = lock(&self.inner);
        let entry = inner.by_user_mut(user_code).ok_or(DeviceError::NotFound)?;
        if !matches!(entry.state, State::Approving { ticket: held, .. } if held == ticket) {
            return Err(DeviceError::NotFound);
        }
        entry.state = State::Approved(grant);
        Ok(())
    }

    /// Pending → denied.
    pub(crate) fn deny(&self, user_code: &str, now: i64) -> Result<(), DeviceError> {
        let mut inner = lock(&self.inner);
        let hash = inner.find(user_code, now).ok_or(DeviceError::NotFound)?;
        let entry = inner
            .by_device
            .get_mut(&hash)
            .ok_or(DeviceError::NotFound)?;
        if !matches!(entry.state, State::Pending) {
            return Err(DeviceError::NotFound);
        }
        entry.state = State::Denied;
        Ok(())
    }

    /// One poll by the CLI.
    pub(crate) fn poll(&self, device_code: &str, now: i64) -> Poll {
        let mut inner = lock(&self.inner);
        let hash = hash_token(device_code);
        let Some(entry) = inner.by_device.get_mut(&hash) else {
            return Poll::Expired;
        };
        settle(entry, now);
        let lapsed = now >= entry.expires_at && matches!(entry.state, State::Pending);
        let finished = matches!(entry.state, State::Approved(_) | State::Denied);
        if lapsed {
            inner.remove(&hash);
            return Poll::Expired;
        }
        if finished {
            // The grant leaves the store with this poll, so a second poll is `Expired`.
            return match inner.remove(&hash).map(|removed| removed.state) {
                Some(State::Approved(grant)) => Poll::Granted(grant),
                _ => Poll::Denied,
            };
        }
        // Still `Pending` or `Approving`: the poll is too soon when the last one was within
        // the current interval.
        let slow = entry
            .last_poll
            .is_some_and(|last| now.saturating_sub(last) < interval_ns(entry.interval_s));
        entry.last_poll = Some(now);
        if slow {
            entry.interval_s = entry.interval_s.saturating_add(SLOW_DOWN_STEP_S);
            return Poll::SlowDown;
        }
        Poll::Pending
    }

    /// Drops expired entries that are not approving.
    pub(crate) fn sweep(&self, now: i64) {
        lock(&self.inner).sweep(now);
    }
}

/// `XXXX-XXXX` or `xxxxxxxx` or `xxxx-xxxx` → `XXXXXXXX` when it is 8 alphabet letters.
pub(crate) fn normalize_user_code(raw: &str) -> Option<String> {
    let letters = match raw.split_once('-') {
        None => raw.to_owned(),
        Some((left, right)) if left.len() == 4 && right.len() == 4 => format!("{left}{right}"),
        Some(_) => return None,
    };
    if letters.len() != 8 {
        return None;
    }
    let upper = letters.to_ascii_uppercase();
    let all_letters = upper.bytes().all(|byte| USER_CODE_ALPHABET.contains(&byte));
    all_letters.then_some(upper)
}

/// 64 lowercase hexadecimal characters: the SHA-256 that `hash_token` writes.
fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One code's state and its bookkeeping.
struct Inner {
    /// Keyed by `hash_token(device_code)`.
    by_device: HashMap<String, Entry>,
    /// From the normalized user code to that hash.
    by_user: HashMap<String, String>,
    /// The ticket of the next approval. Wraps; a ticket is only compared with the one held.
    next_ticket: u64,
}

struct Entry {
    /// Normalized: 8 letters, no dash.
    user_code: String,
    purpose: Purpose,
    host: Option<String>,
    created_at: i64,
    expires_at: i64,
    interval_s: u64,
    last_poll: Option<i64>,
    state: State,
}

enum State {
    Pending,
    /// `ticket` identifies this approval among the ones the code gets over time.
    Approving {
        since: i64,
        ticket: u64,
    },
    Denied,
    Approved(Grant),
}

impl Entry {
    fn view(&self) -> Pending {
        Pending {
            purpose: self.purpose,
            host: self.host.clone(),
            created_at: self.created_at,
        }
    }
}

impl Inner {
    /// The hash of the code `user_code` names, when it is pending, approving or approved and
    /// not expired. An expired code that is not approving is `None`.
    fn find(&mut self, user_code: &str, now: i64) -> Option<String> {
        let letters = normalize_user_code(user_code)?;
        let hash = self.by_user.get(&letters)?.clone();
        let entry = self.by_device.get_mut(&hash)?;
        settle(entry, now);
        let approving = matches!(entry.state, State::Approving { .. });
        if now >= entry.expires_at && !approving {
            return None;
        }
        Some(hash)
    }

    /// The entry of a normalized or raw user code, whatever its state.
    fn by_user_mut(&mut self, user_code: &str) -> Option<&mut Entry> {
        let letters = normalize_user_code(user_code)?;
        let hash = self.by_user.get(&letters)?;
        self.by_device.get_mut(hash)
    }

    /// Removes an entry from both maps.
    fn remove(&mut self, hash: &str) -> Option<Entry> {
        let entry = self.by_device.remove(hash)?;
        self.by_user.remove(&entry.user_code);
        Some(entry)
    }

    fn sweep(&mut self, now: i64) {
        for entry in self.by_device.values_mut() {
            settle(entry, now);
        }
        let expired: Vec<String> = self
            .by_device
            .iter()
            .filter(|(_, entry)| match entry.state {
                State::Pending | State::Denied => entry.expires_at <= now,
                // A grant nobody collected within one minute after the code's expiry is dropped.
                State::Approved(_) => entry.expires_at.saturating_add(APPROVING_TTL_NS) <= now,
                State::Approving { .. } => false,
            })
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in expired {
            self.remove(&hash);
        }
    }
}

/// An approval older than `APPROVING_TTL_NS` falls back to pending.
fn settle(entry: &mut Entry, now: i64) {
    let stale = match entry.state {
        State::Approving { since, .. } => now >= since.saturating_add(APPROVING_TTL_NS),
        _ => false,
    };
    if stale {
        entry.state = State::Pending;
    }
}

fn interval_ns(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000_000)
}

/// Eight letters of [`USER_CODE_ALPHABET`], drawn without bias: a byte of 240 or more is
/// skipped, since 240 is the largest multiple of 20 below 256.
fn draw_letters() -> Result<String, AuthError> {
    let mut letters = String::with_capacity(8);
    while letters.len() < 8 {
        for byte in random_bytes(16)? {
            if byte < 240 {
                letters.push(char::from(USER_CODE_ALPHABET[usize::from(byte % 20)]));
                if letters.len() == 8 {
                    break;
                }
            }
        }
    }
    Ok(letters)
}

/// A poisoned mutex still holds plain maps, so the guard is recovered.
fn lock(mutex: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const T0: i64 = 1_800_000_000_000_000_000;

    fn at(seconds: i64) -> i64 {
        T0 + seconds * 1_000_000_000
    }

    fn issue_at(store: &DeviceStore, now: i64) -> Issued {
        store
            .issue(Purpose::Login, Some("laptop".to_owned()), now)
            .expect("a code is issued")
    }

    fn grant(token: &str) -> Grant {
        Grant {
            token: token.to_owned(),
            project: None,
            ingest_key: None,
        }
    }

    fn entries(store: &DeviceStore) -> usize {
        lock(&store.inner).by_device.len()
    }

    fn is_user_code(code: &str) -> bool {
        let bytes = code.as_bytes();
        bytes.len() == 9
            && bytes[4] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| index == 4 || USER_CODE_ALPHABET.contains(byte))
    }

    #[test]
    fn issue_returns_a_device_code_and_a_user_code_of_the_alphabet() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        assert_eq!(issued.device_code.len(), 43);
        assert!(is_user_code(&issued.user_code), "{}", issued.user_code);
    }

    #[test]
    fn a_thousand_issues_give_a_thousand_user_codes() {
        let store = DeviceStore::new();
        let mut codes = HashSet::new();
        for _ in 0..1000 {
            codes.insert(issue_at(&store, at(0)).user_code);
        }
        assert_eq!(codes.len(), 1000);
    }

    #[test]
    fn normalize_accepts_the_three_spellings_and_refuses_the_rest() {
        assert_eq!(
            normalize_user_code("bcdf-ghjk").as_deref(),
            Some("BCDFGHJK")
        );
        assert_eq!(normalize_user_code("BCDFGHJK").as_deref(), Some("BCDFGHJK"));
        assert_eq!(
            normalize_user_code("BCDF-GHJK").as_deref(),
            Some("BCDFGHJK")
        );
        for bad in ["BCDF-GHJ", "BCDF-GHJA", "BCDF GHJK", ""] {
            assert_eq!(normalize_user_code(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn polling_slows_down_and_the_interval_grows() {
        let store = DeviceStore::new();
        let code = issue_at(&store, at(0)).device_code;
        assert!(matches!(store.poll(&code, at(0)), Poll::Pending));
        assert!(matches!(store.poll(&code, at(1)), Poll::SlowDown));
        // The interval is now 10 s, counted from the slow poll.
        assert!(matches!(store.poll(&code, at(11)), Poll::Pending));
        assert!(matches!(store.poll(&code, at(16)), Poll::SlowDown));
    }

    #[test]
    fn approve_and_collect_hands_over_the_grant_once() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let (_, ticket) = store.claim(&issued.user_code, at(1)).expect("claimed");
        store
            .complete(&issued.user_code, ticket, grant("pat-cetra-one"))
            .expect("completed");
        let Poll::Granted(collected) = store.poll(&issued.device_code, at(2)) else {
            panic!("the approved code is granted");
        };
        assert_eq!(collected.token, "pat-cetra-one");
        assert!(matches!(
            store.poll(&issued.device_code, at(3)),
            Poll::Expired
        ));
    }

    #[test]
    fn while_approving_a_poll_is_pending_and_deny_and_claim_are_refused() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        store.claim(&issued.user_code, at(0)).expect("claimed");
        assert!(matches!(
            store.poll(&issued.device_code, at(1)),
            Poll::Pending
        ));
        assert!(matches!(
            store.deny(&issued.user_code, at(2)),
            Err(DeviceError::NotFound)
        ));
        assert!(matches!(
            store.claim(&issued.user_code, at(2)),
            Err(DeviceError::NotFound)
        ));
    }

    #[test]
    fn an_approval_started_near_expiry_survives_a_sweep_and_completes() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        // Claimed 30 s before the code expires; the sweep runs 10 s after the expiry.
        let (_, ticket) = store.claim(&issued.user_code, at(570)).expect("claimed");
        store.sweep(at(610));
        store
            .complete(&issued.user_code, ticket, grant("pat-cetra-late"))
            .expect("still approving, so the entry is kept");
    }

    #[test]
    fn a_stale_approval_falls_back_to_pending() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        store.claim(&issued.user_code, at(0)).expect("claimed");
        assert!(matches!(
            store.claim(&issued.user_code, at(30)),
            Err(DeviceError::NotFound)
        ));
        store
            .claim(&issued.user_code, at(61))
            .expect("the first approval was dropped");
    }

    #[test]
    fn a_stale_approval_past_the_expiry_is_swept() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        store.claim(&issued.user_code, at(100)).expect("claimed");
        // Past the expiry, and 61 s after the claim.
        store.sweep(at(601));
        assert_eq!(entries(&store), 0);
        assert!(matches!(
            store.poll(&issued.device_code, at(601)),
            Poll::Expired
        ));
    }

    #[test]
    fn a_denial_is_seen_even_after_the_expiry() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        store.deny(&issued.user_code, at(1)).expect("denied");
        assert!(matches!(
            store.poll(&issued.device_code, at(601)),
            Poll::Denied
        ));
        assert!(matches!(
            store.poll(&issued.device_code, at(602)),
            Poll::Expired
        ));
    }

    #[test]
    fn release_puts_an_approval_back_to_pending() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let (_, ticket) = store.claim(&issued.user_code, at(0)).expect("claimed");
        store.release(&issued.user_code, ticket);
        assert!(store.pending(&issued.user_code, at(1)).is_some());
        store
            .claim(&issued.user_code, at(1))
            .expect("a second claim works");
    }

    #[test]
    fn a_late_release_does_not_undo_a_later_approval() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let (_, first) = store.claim(&issued.user_code, at(0)).expect("claimed");
        // The first approval is stale after 60 s; the second claim takes the code.
        let (_, second) = store
            .claim(&issued.user_code, at(61))
            .expect("claimed again");
        assert_ne!(first, second);

        store.release(&issued.user_code, first);
        assert!(matches!(
            store.poll(&issued.device_code, at(62)),
            Poll::Pending
        ));
        store
            .complete(&issued.user_code, second, grant("pat-cetra-second"))
            .expect("the second approval still holds the code");
        let Poll::Granted(collected) = store.poll(&issued.device_code, at(63)) else {
            panic!("the second approval is granted");
        };
        assert_eq!(collected.token, "pat-cetra-second");
    }

    #[test]
    fn a_late_complete_with_a_stale_ticket_gets_no_grant() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let (_, first) = store.claim(&issued.user_code, at(0)).expect("claimed");
        store
            .claim(&issued.user_code, at(61))
            .expect("claimed again");
        assert!(matches!(
            store.complete(&issued.user_code, first, grant("pat-cetra-late-first")),
            Err(DeviceError::NotFound)
        ));
    }

    #[test]
    fn deny_then_poll_is_denied_then_expired() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        store.deny(&issued.user_code, at(0)).expect("denied");
        assert!(matches!(
            store.poll(&issued.device_code, at(0)),
            Poll::Denied
        ));
        assert!(matches!(
            store.poll(&issued.device_code, at(0)),
            Poll::Expired
        ));
    }

    #[test]
    fn an_expired_code_is_unknown_to_every_call_but_the_poll() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let end = at(600);
        assert!(store.pending(&issued.user_code, end).is_none());
        assert!(matches!(
            store.claim(&issued.user_code, end),
            Err(DeviceError::NotFound)
        ));
        assert!(matches!(
            store.poll(&issued.device_code, end),
            Poll::Expired
        ));
    }

    #[test]
    fn sweep_drops_expired_pending_codes_and_uncollected_grants_a_minute_later() {
        let store = DeviceStore::new();
        let _pending = issue_at(&store, at(0));
        let issued = issue_at(&store, at(0));
        let (_, ticket) = store.claim(&issued.user_code, at(0)).expect("claimed");
        store
            .complete(&issued.user_code, ticket, grant("pat-cetra-sweep"))
            .expect("completed");
        store.sweep(at(600));
        assert_eq!(
            entries(&store),
            1,
            "the pending code is gone, the grant stays"
        );
        store.sweep(at(659));
        assert_eq!(
            entries(&store),
            1,
            "the grant is kept until one minute after expiry"
        );
        store.sweep(at(660));
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn a_full_store_sweeps_before_it_refuses() {
        let store = DeviceStore::new();
        for _ in 0..MAX_CODES {
            issue_at(&store, at(0));
        }
        assert!(matches!(
            store.issue(Purpose::Login, None, at(0)),
            Err(DeviceError::Full)
        ));
        store
            .issue(Purpose::Init, None, at(600))
            .expect("the expired codes are swept first");
        assert_eq!(entries(&store), 1);
    }

    #[test]
    fn debug_hides_the_device_code_and_the_secrets() {
        let store = DeviceStore::new();
        let issued = issue_at(&store, at(0));
        let shown = format!("{issued:?}");
        assert!(!shown.contains(&issued.device_code), "{shown}");

        let secret = Grant {
            token: "pat-cetra-DEBUGSECRET".to_owned(),
            project: Some(GrantProject {
                id: ProjectId::new_v7(),
                org_slug: "acme".to_owned(),
                slug: "support-bot".to_owned(),
                name: "Support bot".to_owned(),
            }),
            ingest_key: Some("sk-cetra-DEBUGKEY".to_owned()),
        };
        let shown = format!("{secret:?}");
        assert!(!shown.contains("pat-cetra-DEBUGSECRET"), "{shown}");
        assert!(!shown.contains("sk-cetra-DEBUGKEY"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    const SKEW_NS: i64 = 30 * 1_000_000_000;

    /// The hash a control plane registers for `device_code`.
    fn registered(device_code: &str) -> String {
        hash_token(device_code)
    }

    #[test]
    fn a_registered_code_is_pending_and_a_lower_case_user_code_is_normalized() {
        let store = DeviceStore::new();
        store
            .register(
                &registered("device-code-one"),
                "bcdf-ghjk",
                Purpose::Login,
                Some("laptop".to_owned()),
                at(600),
                at(0),
            )
            .expect("registered");
        let pending = store.pending("BCDF-GHJK", at(1)).expect("pending");
        assert_eq!(pending.purpose, Purpose::Login);
        assert_eq!(pending.host.as_deref(), Some("laptop"));
        assert_eq!(pending.created_at, at(0));
        assert!(store.pending("bcdf-ghjk", at(1)).is_some());
    }

    #[test]
    fn the_same_registration_twice_is_ok_and_counts_once() {
        let store = DeviceStore::new();
        let hash = registered("device-code-two");
        store
            .register(
                &hash,
                "BCDF-GHJK",
                Purpose::Init,
                Some("laptop".to_owned()),
                at(600),
                at(0),
            )
            .expect("first");
        store
            .register(
                &hash,
                "BCDF-GHJK",
                Purpose::Init,
                Some("laptop".to_owned()),
                at(600),
                at(1),
            )
            .expect("the retry is identical");
        assert_eq!(entries(&store), 1);
    }

    #[test]
    fn a_retry_that_differs_in_purpose_host_or_expiry_is_taken() {
        let store = DeviceStore::new();
        let hash = registered("device-code-three");
        store
            .register(&hash, "BCDF-GHJK", Purpose::Login, None, at(600), at(0))
            .expect("first");
        assert!(matches!(
            store.register(&hash, "BCDF-GHJK", Purpose::Init, None, at(600), at(1)),
            Err(DeviceError::Taken)
        ));
        assert!(matches!(
            store.register(
                &hash,
                "BCDF-GHJK",
                Purpose::Login,
                Some("other".to_owned()),
                at(600),
                at(1)
            ),
            Err(DeviceError::Taken)
        ));
        assert!(matches!(
            store.register(&hash, "BCDF-GHJK", Purpose::Login, None, at(601), at(1)),
            Err(DeviceError::Taken)
        ));
    }

    #[test]
    fn the_same_hash_with_another_user_code_is_taken() {
        let store = DeviceStore::new();
        let hash = registered("device-code-four");
        store
            .register(&hash, "BCDF-GHJK", Purpose::Login, None, at(600), at(0))
            .expect("first");
        assert!(matches!(
            store.register(&hash, "BCDF-GHJL", Purpose::Login, None, at(600), at(0)),
            Err(DeviceError::Taken)
        ));
    }

    #[test]
    fn the_same_user_code_with_another_hash_is_taken() {
        let store = DeviceStore::new();
        store
            .register(
                &registered("device-code-five"),
                "BCDF-GHJK",
                Purpose::Login,
                None,
                at(600),
                at(0),
            )
            .expect("first");
        assert!(matches!(
            store.register(
                &registered("device-code-six"),
                "bcdf-ghjk",
                Purpose::Login,
                None,
                at(600),
                at(0)
            ),
            Err(DeviceError::Taken)
        ));
    }

    #[test]
    fn an_expired_unswept_code_does_not_block_a_registration_eleven_minutes_later() {
        let store = DeviceStore::new();
        store
            .register(
                &registered("device-code-seven"),
                "BCDF-GHJK",
                Purpose::Login,
                None,
                at(600),
                at(0),
            )
            .expect("first");
        store
            .register(
                &registered("device-code-eight"),
                "BCDF-GHJK",
                Purpose::Login,
                None,
                at(1260),
                at(660),
            )
            .expect("the expired code is swept first");
        assert_eq!(entries(&store), 1);
    }

    #[test]
    fn expires_at_must_be_after_now_and_at_most_the_ttl_plus_the_skew() {
        let store = DeviceStore::new();
        let hash = registered("device-code-nine");
        let now = at(0);
        let latest = now + CODE_TTL_NS + SKEW_NS;
        assert!(matches!(
            store.register(&hash, "BCDF-GHJK", Purpose::Login, None, now, now),
            Err(DeviceError::Invalid("expires_at"))
        ));
        assert!(matches!(
            store.register(&hash, "BCDF-GHJK", Purpose::Login, None, latest + 1, now),
            Err(DeviceError::Invalid("expires_at"))
        ));
        assert!(matches!(
            store.register(&hash, "BCDF-GHJK", Purpose::Login, None, at(660), now),
            Err(DeviceError::Invalid("expires_at"))
        ));
        store
            .register(&hash, "BCDF-GHJK", Purpose::Login, None, latest, now)
            .expect("the latest allowed expiry");
        assert_eq!(entries(&store), 1);
    }

    #[test]
    fn a_malformed_hash_or_user_code_is_invalid_and_stores_nothing() {
        let store = DeviceStore::new();
        let now = at(0);
        let expires = at(600);
        let good = registered("device-code-ten");
        for hash in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            String::new(),
        ] {
            assert!(
                matches!(
                    store.register(&hash, "BCDF-GHJK", Purpose::Login, None, expires, now),
                    Err(DeviceError::Invalid("device_code_sha256"))
                ),
                "{hash}"
            );
        }
        assert!(matches!(
            store.register(&good, "BCDA-GHJK", Purpose::Login, None, expires, now),
            Err(DeviceError::Invalid("user_code"))
        ));
        assert!(matches!(
            store.register(&good, "BCDF-GHJ", Purpose::Login, None, expires, now),
            Err(DeviceError::Invalid("user_code"))
        ));
        assert_eq!(entries(&store), 0);
    }

    #[test]
    fn a_registered_code_is_approved_and_granted_to_its_device_code() {
        let store = DeviceStore::new();
        let device_code = "device-code-eleven";
        store
            .register(
                &registered(device_code),
                "BCDF-GHJK",
                Purpose::Init,
                Some("laptop".to_owned()),
                at(600),
                at(0),
            )
            .expect("registered");
        let (_, ticket) = store.claim("BCDF-GHJK", at(1)).expect("claimed");
        store
            .complete("BCDF-GHJK", ticket, grant("pat-cetra-registered"))
            .expect("completed");
        let Poll::Granted(collected) = store.poll(device_code, at(2)) else {
            panic!("the registered code is granted");
        };
        assert_eq!(collected.token, "pat-cetra-registered");
        assert!(matches!(store.poll(device_code, at(3)), Poll::Expired));
    }

    #[test]
    fn a_full_store_refuses_a_new_registration_but_not_an_identical_retry() {
        let store = DeviceStore::new();
        let hash = registered("device-code-twelve");
        store
            .register(&hash, "BCDF-GHJK", Purpose::Login, None, at(600), at(0))
            .expect("registered");
        for _ in 1..MAX_CODES {
            issue_at(&store, at(0));
        }
        assert!(matches!(
            store.register(
                &registered("device-code-thirteen"),
                "BCDF-GHJL",
                Purpose::Login,
                None,
                at(600),
                at(0)
            ),
            Err(DeviceError::Full)
        ));
        store
            .register(&hash, "BCDF-GHJK", Purpose::Login, None, at(600), at(0))
            .expect("the identical retry is ok at the cap");
    }

    #[test]
    fn a_control_plane_hash_is_plain_lower_case_sha256() {
        use sha2::{Digest, Sha256};
        // The control plane computes the hash itself (D-AO-14), so pin it to SHA-256 directly,
        // not to `hash_token`, which the other tests use.
        let digest = Sha256::digest(b"device-code-pinned");
        let sha256_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(sha256_hex.len(), 64);
        assert_eq!(sha256_hex, hash_token("device-code-pinned"));

        let store = DeviceStore::new();
        store
            .register(
                &sha256_hex,
                "BCDF-GHJK",
                Purpose::Login,
                None,
                at(600),
                at(0),
            )
            .expect("registered");
        // A poll with the plain device code reaches the entry: it is pending, not expired.
        assert!(matches!(
            store.poll("device-code-pinned", at(1)),
            Poll::Pending
        ));
    }
}
