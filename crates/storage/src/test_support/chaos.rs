//! Injected S3 faults for the storage tests (R4.1, R4.2, D-R-8).
//!
//! [`ChaosStore`] wraps an [`ObjectStore`] and fails or delays the calls it forwards. It is a second
//! implementation of the external trait, so the remote `put_durable` path runs in CI without AWS
//! (D119). It sits above `object_store`'s S3 client and its retries: a fault is what our code sees
//! after the client has given up, so every injected error is `object_store::Error::Generic` with
//! store `"S3"`.
//!
//! A call's fault comes from, in order: a [`ChaosStore::hold`], a [`ChaosStore::push_at`], the
//! queue of [`ChaosStore::push`], and then the seeded schedule of [`ChaosStore::set_schedule`].
//! Every upload is recorded in a ledger ([`ChaosStore::uploads`]), and the last calls with a fault
//! are kept in a journal ([`ChaosStore::journal`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
    UploadPart,
};

/// Which kind of store call a fault applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Call {
    /// `put_opts`.
    Put,
    /// `put_multipart_opts`.
    MultipartStart,
    /// `MultipartUpload::put_part`.
    PutPart,
    /// `MultipartUpload::complete`.
    Complete,
    /// `MultipartUpload::abort`.
    Abort,
    /// `get_opts` without `head`.
    Get,
    /// `get_opts` with `head` (what `ObjectStoreExt::head` sends).
    Head,
    /// Each path of `delete_stream`.
    Delete,
    /// `list` and `list_with_delimiter`. Counted, never faulted: storage never lists the bucket.
    List,
    /// `copy_opts`. Counted, never faulted.
    Copy,
}

/// The S3 status a fault simulates, after the client's retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// 503 SlowDown.
    SlowDown,
    /// 500 InternalError.
    Internal,
}

impl Status {
    fn code(self) -> &'static str {
        match self {
            Self::SlowDown => "503 SlowDown",
            Self::Internal => "500 InternalError",
        }
    }
}

/// One injected fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Waits, then runs the call normally. Not a failure.
    Delay(Duration),
    /// Fails without reaching the inner store.
    Before(Status),
    /// Runs the call on the inner store, then fails: the effect happened (the ambiguous upload of
    /// spec 03 §2). On `MultipartStart` the created upload is dropped, so it stays open.
    After(Status),
    /// `Get` only: the body yields its first half, then an error.
    Truncated,
    /// `Get` only: `Ok`, with one byte flipped.
    Corrupt,
}

/// Probabilities of the random faults, per call (R4.2). Each check is one draw in `[0, 1)`
/// compared with its rate, so a rate of `0.0` never hits.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Rates {
    /// Any call that takes checks: a `Delay` of up to `max_delay`.
    pub(crate) delay: f64,
    pub(crate) max_delay: Duration,
    /// Put, MultipartStart, PutPart, Complete and Delete: `Before`.
    pub(crate) before: f64,
    /// Put, MultipartStart, PutPart, Complete and Delete: `After`.
    pub(crate) after: f64,
    /// Abort: `Before`, so the upload stays open.
    pub(crate) abort: f64,
    /// Get and Head: `Before`.
    pub(crate) read_error: f64,
    /// Get: `Truncated`.
    pub(crate) truncated: f64,
}

impl Rates {
    /// The model's S3 mode (R4.5 may tune these by measurement).
    pub(crate) const MODEL: Rates = Rates {
        delay: 0.2,
        max_delay: Duration::from_millis(5),
        before: 0.06,
        after: 0.03,
        abort: 0.3,
        read_error: 0.04,
        truncated: 0.04,
    };
}

/// A store that injects faults into the calls it forwards to `inner`. Clones share their state.
#[derive(Debug, Clone)]
pub(crate) struct ChaosStore {
    inner: Arc<dyn ObjectStore>,
    state: Arc<Mutex<ChaosState>>,
}

/// How many recent entries [`ChaosStore::journal`] keeps.
const JOURNAL_LEN: usize = 200;

#[derive(Debug, Default)]
struct ChaosState {
    /// Faults queued per call kind, consumed from the front.
    scripted: BTreeMap<Call, VecDeque<Fault>>,
    /// Faults for one call, by kind and 1-based ordinal (`push_at`).
    scripted_at: BTreeMap<(Call, u64), Fault>,
    /// Call kinds that fail before they reach the store until `release`.
    held: BTreeSet<Call>,
    /// Calls per kind, failed or not.
    calls: BTreeMap<Call, u64>,
    /// Faults that failed a call (delays excluded).
    injected: u64,
    schedule: Schedule,
    /// Every upload started through this store, by id.
    uploads: BTreeMap<u64, UploadRecord>,
    next_upload: u64,
    /// Set while a life ends (`set_ending`): uploads dropped open then count as cancelled.
    ending: bool,
    /// The last [`JOURNAL_LEN`] calls that got a fault or a delay, and every multipart start,
    /// complete and abort.
    journal: VecDeque<String>,
}

/// The seeded schedule: its rates, its generator and whether it is on.
#[derive(Debug, Default)]
struct Schedule {
    rates: Rates,
    rng: Rng,
    active: bool,
}

/// One check of the schedule: the fault it selects when it hits.
#[derive(Debug, Clone, Copy)]
enum Pick {
    Before,
    After,
    Truncated,
    Delay,
}

impl Schedule {
    /// The random fault of a call of kind `call`, if any. Each call kind runs its checks in the
    /// order of the R4.2 table, and the first hit wins: later checks do not draw.
    fn draw(&mut self, call: Call) -> Option<Fault> {
        let rates = self.rates;
        let checks = match call {
            Call::Put | Call::MultipartStart | Call::PutPart | Call::Complete | Call::Delete => {
                vec![
                    (rates.before, Pick::Before),
                    (rates.after, Pick::After),
                    (rates.delay, Pick::Delay),
                ]
            }
            Call::Get => vec![
                (rates.read_error, Pick::Before),
                (rates.truncated, Pick::Truncated),
                (rates.delay, Pick::Delay),
            ],
            Call::Head => vec![(rates.read_error, Pick::Before), (rates.delay, Pick::Delay)],
            Call::Abort => vec![(rates.abort, Pick::Before), (rates.delay, Pick::Delay)],
            Call::List | Call::Copy => Vec::new(),
        };
        let pick = checks
            .into_iter()
            .find(|(rate, _)| self.rng.unit() < *rate)?
            .1;
        Some(match pick {
            Pick::Before => Fault::Before(self.status()),
            Pick::After => Fault::After(self.status()),
            Pick::Truncated => Fault::Truncated,
            Pick::Delay => Fault::Delay(rates.max_delay.mul_f64(self.rng.unit())),
        })
    }

    /// One draw: below one half is `SlowDown`, otherwise `Internal`.
    fn status(&mut self) -> Status {
        if self.rng.unit() < 0.5 {
            Status::SlowDown
        } else {
            Status::Internal
        }
    }
}

/// SplitMix64, the generator of the schedule (R4.2): no random-number crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// Where an upload stands, as far as the store can tell (the ledger of R4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadState {
    /// Started; neither completed nor aborted yet.
    Open,
    /// `complete` returned Ok.
    Completed,
    /// `complete` returned an error, and nothing aborted the upload since.
    CompleteFailed,
    /// `abort` returned Ok.
    Aborted,
    /// `abort` returned an error: the upload stays on the bucket until a lifecycle rule removes it.
    AbortFailed,
    /// `CreateMultipartUpload` took effect but its answer was lost (`After` on `MultipartStart`):
    /// no caller holds it.
    Orphaned,
    /// Dropped while `Open`: the future that owned it went away.
    DroppedOpen,
}

/// One upload in the ledger.
#[derive(Debug, Clone)]
pub(crate) struct UploadRecord {
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) state: UploadState,
    /// `DroppedOpen` only: dropped while a life was ending (`set_ending`).
    pub(crate) cancelled: bool,
}

/// The error of an injected fault: `Generic`, with a source that names the simulated status.
#[derive(Debug, thiserror::Error)]
#[error("injected {code} on {call:?} (after the client's retries)")]
struct Injected {
    code: &'static str,
    call: Call,
}

fn injected(status: Status, call: Call) -> object_store::Error {
    object_store::Error::Generic {
        store: "S3",
        source: Box::new(Injected {
            code: status.code(),
            call,
        }),
    }
}

/// Runs `run` under `fault`: fails it before it runs, runs it and then fails it, or delays it.
/// `Truncated` and `Corrupt` act on the body of a `get`, so here they run the call unchanged.
async fn with_fault<T>(
    fault: Option<Fault>,
    call: Call,
    run: impl Future<Output = Result<T>> + Send,
) -> Result<T> {
    match fault {
        Some(Fault::Before(status)) => Err(injected(status, call)),
        Some(Fault::After(status)) => {
            run.await?;
            Err(injected(status, call))
        }
        Some(Fault::Delay(duration)) => {
            tokio::time::sleep(duration).await;
            run.await
        }
        Some(Fault::Truncated | Fault::Corrupt) | None => run.await,
    }
}

/// The body of `result` cut after its first half, then an error: a connection cut mid-body on
/// every retry, as `AmazonS3` reports it after its body retries.
async fn truncated(result: GetResult) -> Result<GetResult> {
    let (meta, range, attributes) = (
        result.meta.clone(),
        result.range.clone(),
        result.attributes.clone(),
    );
    let body = result.bytes().await?;
    let half = body.slice(..body.len() / 2);
    let error = injected(Status::Internal, Call::Get);
    let stream: BoxStream<'static, Result<Bytes>> =
        futures::stream::iter(vec![Ok(half), Err(error)]).boxed();
    Ok(GetResult {
        payload: GetResultPayload::Stream(stream),
        meta,
        range,
        attributes,
    })
}

/// The body of `result` with its middle byte flipped (an empty body is unchanged).
async fn corrupt(result: GetResult) -> Result<GetResult> {
    let (meta, range, attributes) = (
        result.meta.clone(),
        result.range.clone(),
        result.attributes.clone(),
    );
    let mut body = result.bytes().await?.to_vec();
    if !body.is_empty() {
        let middle = body.len() / 2;
        body[middle] ^= 0xFF;
    }
    let stream: BoxStream<'static, Result<Bytes>> =
        futures::stream::iter(vec![Ok(Bytes::from(body))]).boxed();
    Ok(GetResult {
        payload: GetResultPayload::Stream(stream),
        meta,
        range,
        attributes,
    })
}

/// Panics unless `fault` can apply to `call`: `Truncated` and `Corrupt` act on a `get` body only.
fn check_applies(call: Call, fault: &Fault) {
    assert!(
        call == Call::Get || !matches!(fault, Fault::Truncated | Fault::Corrupt),
        "{fault:?} applies to Get only, not {call:?}"
    );
}

impl ChaosStore {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(ChaosState::default())),
        }
    }

    /// The store faults are injected in front of. Tests read and audit through it.
    pub(crate) fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }

    /// Queues `fault` for the next call of kind `call`.
    pub(crate) fn push(&self, call: Call, fault: Fault) {
        check_applies(call, &fault);
        self.lock()
            .scripted
            .entry(call)
            .or_default()
            .push_back(fault);
    }

    /// Injects `fault` into the `ordinal`-th call of kind `call` (1-based, counted per kind), so a
    /// test can replay one exact call. Cleared by [`Self::release`].
    pub(crate) fn push_at(&self, call: Call, ordinal: u64, fault: Fault) {
        check_applies(call, &fault);
        self.lock().scripted_at.insert((call, ordinal), fault);
    }

    /// Fails every call of these kinds before it reaches the store, until [`Self::release`].
    pub(crate) fn hold(&self, calls: &[Call]) {
        self.lock().held.extend(calls.iter().copied());
    }

    /// Ends every [`Self::hold`], and drops every queued fault (`push` and `push_at`), so a test
    /// starts its next step clean. The schedule is not affected.
    pub(crate) fn release(&self) {
        let mut state = self.lock();
        state.held.clear();
        state.scripted.clear();
        state.scripted_at.clear();
    }

    /// Sets the random schedule to `seed` and `rates`. It stays off until `set_active(true)`.
    pub(crate) fn set_schedule(&self, seed: u64, rates: Rates) {
        self.lock().schedule = Schedule {
            rates,
            rng: Rng(seed),
            active: false,
        };
    }

    /// Turns the random schedule on or off. Scripted faults and holds are not affected.
    pub(crate) fn set_active(&self, on: bool) {
        self.lock().schedule.active = on;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.lock().schedule.active
    }

    /// Marks the window in which a life ends: the model harness sets it just before `shutdown`
    /// or the drop of `Storage`, and clears it once the old runtime has shut down.
    pub(crate) fn set_ending(&self, on: bool) {
        self.lock().ending = on;
    }

    /// Calls of this kind so far, failed or not.
    pub(crate) fn count(&self, call: Call) -> u64 {
        self.lock().calls.get(&call).copied().unwrap_or(0)
    }

    /// Faults that failed a call so far (delays excluded).
    pub(crate) fn injected(&self) -> u64 {
        self.lock().injected
    }

    /// Every upload started through this store, in the order they started.
    pub(crate) fn uploads(&self) -> Vec<UploadRecord> {
        self.lock().uploads.values().cloned().collect()
    }

    /// Uploads our code left open although it could have closed them: `Open`, `CompleteFailed`,
    /// and `DroppedOpen` outside an ending window.
    pub(crate) fn leaked_uploads(&self) -> Vec<UploadRecord> {
        self.uploads()
            .into_iter()
            .filter(|record| {
                matches!(
                    record.state,
                    UploadState::Open | UploadState::CompleteFailed
                ) || (record.state == UploadState::DroppedOpen && !record.cancelled)
            })
            .collect()
    }

    /// Uploads only a lifecycle rule can remove: `AbortFailed`, `Orphaned`, and `DroppedOpen`
    /// inside an ending window.
    pub(crate) fn abandoned_uploads(&self) -> Vec<UploadRecord> {
        self.uploads()
            .into_iter()
            .filter(|record| {
                matches!(
                    record.state,
                    UploadState::AbortFailed | UploadState::Orphaned
                ) || (record.state == UploadState::DroppedOpen && record.cancelled)
            })
            .collect()
    }

    /// The recent calls that got a fault, as `"{call}#{ordinal} {path} {fault}"`.
    pub(crate) fn journal(&self) -> Vec<String> {
        self.lock().journal.iter().cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, ChaosState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a call of this kind without deciding a fault.
    fn record(&self, call: Call) {
        *self.lock().calls.entry(call).or_default() += 1;
    }

    /// Counts a call and decides its fault, and journals it when it has one.
    fn next_fault(&self, call: Call, path: &str) -> Option<Fault> {
        let mut state = self.lock();
        let ordinal = {
            let count = state.calls.entry(call).or_default();
            *count += 1;
            *count
        };
        let fault = state.decide(call, ordinal);
        if fault
            .as_ref()
            .is_some_and(|fault| !matches!(fault, Fault::Delay(_)))
        {
            state.injected += 1;
        }
        if fault.is_some() || matches!(call, Call::MultipartStart | Call::Complete | Call::Abort) {
            state.push_journal(format!("{call:?}#{ordinal} {path} {fault:?}"));
        }
        fault
    }

    /// Starts the ledger entry of an upload that is now open, and returns its id.
    fn begin_upload(&self, path: &str) -> u64 {
        let mut state = self.lock();
        let id = state.next_upload;
        state.next_upload += 1;
        state.uploads.insert(
            id,
            UploadRecord {
                id,
                path: path.to_owned(),
                state: UploadState::Open,
                cancelled: false,
            },
        );
        id
    }

    fn set_upload(&self, id: u64, next: UploadState) {
        if let Some(record) = self.lock().uploads.get_mut(&id) {
            record.state = next;
        }
    }
}

impl ChaosState {
    /// The fault of the `ordinal`-th call of `call`: a hold, then `push_at`, then the queue, then
    /// the schedule.
    fn decide(&mut self, call: Call, ordinal: u64) -> Option<Fault> {
        if self.held.contains(&call) {
            return Some(Fault::Before(Status::SlowDown));
        }
        if let Some(fault) = self.scripted_at.remove(&(call, ordinal)) {
            return Some(fault);
        }
        if let Some(fault) = self.scripted.get_mut(&call).and_then(VecDeque::pop_front) {
            return Some(fault);
        }
        if self.schedule.active {
            self.schedule.draw(call)
        } else {
            None
        }
    }

    fn push_journal(&mut self, entry: String) {
        if self.journal.len() == JOURNAL_LEN {
            self.journal.pop_front();
        }
        self.journal.push_back(entry);
    }
}

impl fmt::Display for ChaosStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ChaosStore({})", self.inner.as_ref())
    }
}

/// The multipart upload of a [`ChaosStore`]: its parts and completion go through the same queues,
/// and it updates its ledger entry `id`.
struct ChaosUpload {
    id: u64,
    path: String,
    inner: Box<dyn MultipartUpload>,
    store: ChaosStore,
}

impl fmt::Debug for ChaosUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChaosUpload")
            .finish_non_exhaustive()
    }
}

impl Drop for ChaosUpload {
    /// An upload dropped while open was dropped without an abort. It counts as cancelled when a
    /// life is ending at that moment (R4.2).
    fn drop(&mut self) {
        let mut state = self.store.lock();
        let cancelled = state.ending;
        if let Some(record) = state
            .uploads
            .get_mut(&self.id)
            .filter(|record| record.state == UploadState::Open)
        {
            record.state = UploadState::DroppedOpen;
            record.cancelled = cancelled;
        }
    }
}

#[async_trait]
impl MultipartUpload for ChaosUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        match self.store.next_fault(Call::PutPart, &self.path) {
            Some(Fault::Before(status)) => {
                Box::pin(async move { Err(injected(status, Call::PutPart)) })
            }
            Some(Fault::After(status)) => {
                // Synchronous, so the inner upload sees its parts in call order.
                let part = self.inner.put_part(data);
                Box::pin(async move {
                    part.await?;
                    Err(injected(status, Call::PutPart))
                })
            }
            Some(Fault::Delay(duration)) => {
                let part = self.inner.put_part(data);
                Box::pin(async move {
                    tokio::time::sleep(duration).await;
                    part.await
                })
            }
            Some(Fault::Truncated | Fault::Corrupt) | None => self.inner.put_part(data),
        }
    }

    async fn complete(&mut self) -> Result<PutResult> {
        let fault = self.store.next_fault(Call::Complete, &self.path);
        let result = with_fault(fault, Call::Complete, self.inner.complete()).await;
        let next = if result.is_ok() {
            UploadState::Completed
        } else {
            UploadState::CompleteFailed
        };
        self.store.set_upload(self.id, next);
        result
    }

    async fn abort(&mut self) -> Result<()> {
        let fault = self.store.next_fault(Call::Abort, &self.path);
        let result = with_fault(fault, Call::Abort, self.inner.abort()).await;
        let next = if result.is_ok() {
            UploadState::Aborted
        } else {
            UploadState::AbortFailed
        };
        self.store.set_upload(self.id, next);
        result
    }
}

#[async_trait]
impl ObjectStore for ChaosStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let fault = self.next_fault(Call::Put, location.as_ref());
        with_fault(
            fault,
            Call::Put,
            self.inner.put_opts(location, payload, opts),
        )
        .await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        let path: &str = location.as_ref();
        match self.next_fault(Call::MultipartStart, path) {
            Some(Fault::After(status)) => {
                // The upload is created, but its answer is lost: no caller holds it.
                drop(self.inner.put_multipart_opts(location, opts).await?);
                let id = self.begin_upload(path);
                self.set_upload(id, UploadState::Orphaned);
                Err(injected(status, Call::MultipartStart))
            }
            fault => {
                let inner = with_fault(
                    fault,
                    Call::MultipartStart,
                    self.inner.put_multipart_opts(location, opts),
                )
                .await?;
                let id = self.begin_upload(path);
                Ok(Box::new(ChaosUpload {
                    id,
                    path: path.to_owned(),
                    inner,
                    store: self.clone(),
                }))
            }
        }
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let call = if options.head { Call::Head } else { Call::Get };
        match self.next_fault(call, location.as_ref()) {
            Some(Fault::Truncated) => {
                truncated(self.inner.get_opts(location, options).await?).await
            }
            Some(Fault::Corrupt) => corrupt(self.inner.get_opts(location, options).await?).await,
            fault => with_fault(fault, call, self.inner.get_opts(location, options)).await,
        }
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        let store = self.clone();
        locations
            .then(move |location| {
                let store = store.clone();
                async move {
                    let path = location?;
                    let fault = store.next_fault(Call::Delete, path.as_ref());
                    let run = async {
                        store.inner.delete(&path).await?;
                        Ok(path.clone())
                    };
                    with_fault(fault, Call::Delete, run).await
                }
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.record(Call::List);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.record(Call::List);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.record(Call::Copy);
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use object_store::memory::InMemory;

    use super::*;

    fn path(name: &str) -> Path {
        Path::from(name)
    }

    fn chaos() -> ChaosStore {
        ChaosStore::new(Arc::new(InMemory::new()))
    }

    fn is_injected(error: &object_store::Error) -> bool {
        matches!(error, object_store::Error::Generic { store: "S3", .. })
    }

    /// The journal with each entry's path removed, so replays with other object names compare
    /// equal: `"{call}#{ordinal} {path} {fault}"` becomes `"{call}#{ordinal} {fault}"`.
    fn without_paths(journal: &[String]) -> Vec<String> {
        journal
            .iter()
            .map(|entry| {
                let mut parts = entry.splitn(3, ' ');
                let call = parts.next().expect("the call and ordinal");
                let _path = parts.next().expect("the path");
                let fault = parts.next().expect("the fault");
                format!("{call} {fault}")
            })
            .collect()
    }

    #[tokio::test]
    async fn before_fails_without_writing() {
        let store = chaos();
        store.push(Call::Put, Fault::Before(Status::SlowDown));
        let error = store.put(&path("a"), Bytes::from_static(b"x").into()).await;
        assert!(is_injected(&error.unwrap_err()));
        assert!(matches!(
            store.inner().head(&path("a")).await.unwrap_err(),
            object_store::Error::NotFound { .. }
        ));
        assert_eq!(store.injected(), 1);
    }

    #[tokio::test]
    async fn after_writes_then_fails() {
        let store = chaos();
        store.push(Call::Put, Fault::After(Status::SlowDown));
        let error = store.put(&path("a"), Bytes::from_static(b"x").into()).await;
        assert!(is_injected(&error.unwrap_err()));
        let bytes = store
            .inner()
            .get(&path("a"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes, Bytes::from_static(b"x"));
        assert_eq!(store.injected(), 1);
    }

    #[tokio::test]
    async fn held_calls_fail_until_released() {
        let store = chaos();
        store.push(Call::Put, Fault::Before(Status::SlowDown));
        store.push(Call::Put, Fault::Before(Status::SlowDown));
        store.release();
        store
            .put(&path("a"), Bytes::from_static(b"x").into())
            .await
            .expect("release dropped the queued faults");

        store.hold(&[Call::Put]);
        let error = store.put(&path("b"), Bytes::from_static(b"x").into()).await;
        assert!(is_injected(&error.unwrap_err()));
        store.release();
        store
            .put(&path("b"), Bytes::from_static(b"x").into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn truncated_get_errors_after_half_the_body() {
        let store = chaos();
        let body = Bytes::from(vec![7_u8; 1000]);
        store.put(&path("a"), body.clone().into()).await.unwrap();
        store.push(Call::Get, Fault::Truncated);
        let mut stream = store.get(&path("a")).await.unwrap().into_stream();
        let first = stream.next().await.expect("the first half").unwrap();
        assert_eq!(first.len(), 500);
        assert!(stream.next().await.expect("the error").is_err());
        let bytes = store.get(&path("a")).await.unwrap().bytes().await.unwrap();
        assert_eq!(bytes, body);
    }

    #[tokio::test]
    async fn corrupt_get_flips_one_byte() {
        let store = chaos();
        let body = Bytes::from(vec![7_u8; 1000]);
        store.put(&path("a"), body.clone().into()).await.unwrap();
        store.push(Call::Get, Fault::Corrupt);
        let bytes = store.get(&path("a")).await.unwrap().bytes().await.unwrap();
        assert_eq!(bytes.len(), body.len());
        let differing: Vec<usize> = (0..body.len()).filter(|&i| bytes[i] != body[i]).collect();
        assert_eq!(differing, vec![500]);
    }

    #[tokio::test]
    async fn delete_after_deletes_and_fails() {
        let store = chaos();
        store
            .put(&path("a"), Bytes::from_static(b"x").into())
            .await
            .unwrap();
        store.push(Call::Delete, Fault::After(Status::SlowDown));
        let error = store.delete(&path("a")).await;
        assert!(is_injected(&error.unwrap_err()));
        assert!(matches!(
            store.inner().head(&path("a")).await.unwrap_err(),
            object_store::Error::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn part_and_complete_faults_reach_the_upload() {
        let store = chaos();
        let mut upload = store.put_multipart(&path("a")).await.unwrap();
        store.push(Call::PutPart, Fault::Before(Status::SlowDown));
        let first = upload.put_part(vec![1_u8; 10].into()).await;
        assert!(is_injected(&first.unwrap_err()));
        upload.put_part(vec![2_u8; 10].into()).await.unwrap();

        store.push(Call::Complete, Fault::After(Status::SlowDown));
        let error = upload.complete().await;
        assert!(is_injected(&error.unwrap_err()));
        assert_eq!(store.inner().head(&path("a")).await.unwrap().size, 10);
    }

    #[tokio::test]
    async fn counts_every_call_kind() {
        let store = chaos();
        let from = path("a");
        let to = path("b");
        store
            .put(&from, Bytes::from_static(b"x").into())
            .await
            .unwrap();
        let mut upload = store.put_multipart(&to).await.unwrap();
        upload.put_part(vec![1_u8; 10].into()).await.unwrap();
        upload.complete().await.unwrap();
        store.get(&from).await.unwrap().bytes().await.unwrap();
        store.head(&from).await.unwrap();
        store.copy(&from, &path("c")).await.unwrap();
        store.delete(&path("c")).await.unwrap();
        store.list(None).try_collect::<Vec<_>>().await.unwrap();
        store.list_with_delimiter(None).await.unwrap();

        for call in [
            Call::Put,
            Call::MultipartStart,
            Call::PutPart,
            Call::Complete,
            Call::Get,
            Call::Head,
            Call::Copy,
            Call::Delete,
        ] {
            assert_eq!(store.count(call), 1, "{call:?}");
        }
        assert_eq!(store.count(Call::Abort), 0);
        // `list` and `list_with_delimiter` are both listings.
        assert_eq!(store.count(Call::List), 2);
        assert_eq!(store.injected(), 0);
    }

    /// Runs 500 puts under the model's rates from `seed`, and returns the injected count and the
    /// journal without paths.
    async fn schedule_run(seed: u64) -> (u64, Vec<String>) {
        let store = chaos();
        store.set_schedule(seed, Rates::MODEL);
        store.set_active(true);
        for index in 0..500 {
            // Failures are the point: the test reads them from the store, not from the results.
            let _ = store
                .put(&path(&format!("k{index}")), Bytes::from_static(b"x").into())
                .await;
        }
        (store.injected(), without_paths(&store.journal()))
    }

    #[tokio::test]
    async fn the_same_seed_gives_the_same_faults() {
        let (injected, journal) = schedule_run(7).await;
        let (replayed, replayed_journal) = schedule_run(7).await;
        assert!(injected > 0, "the model's rates inject faults");
        assert_eq!(injected, replayed);
        assert_eq!(journal, replayed_journal);
    }

    #[tokio::test]
    async fn an_inactive_schedule_injects_nothing() {
        let store = chaos();
        store.set_schedule(7, Rates::MODEL);
        assert!(!store.is_active());
        for index in 0..100 {
            store
                .put(&path(&format!("k{index}")), Bytes::from_static(b"x").into())
                .await
                .unwrap();
        }
        assert_eq!(store.injected(), 0);
        assert!(store.journal().is_empty());

        store.set_active(true);
        assert!(store.is_active());
    }

    #[tokio::test]
    async fn the_ledger_tracks_every_outcome() {
        let store = chaos();

        let mut completed = store.put_multipart(&path("completed")).await.unwrap();
        completed.put_part(vec![1_u8; 10].into()).await.unwrap();
        completed.complete().await.unwrap();

        let mut aborted = store.put_multipart(&path("aborted")).await.unwrap();
        aborted.abort().await.unwrap();

        let mut failed = store.put_multipart(&path("failed")).await.unwrap();
        store.push(Call::Complete, Fault::Before(Status::SlowDown));
        assert!(failed.complete().await.is_err());
        failed.abort().await.unwrap();

        drop(store.put_multipart(&path("dropped")).await.unwrap());

        store.set_ending(true);
        drop(store.put_multipart(&path("ending")).await.unwrap());
        store.set_ending(false);

        store.push(Call::MultipartStart, Fault::After(Status::SlowDown));
        assert!(store.put_multipart(&path("orphan")).await.is_err());

        let records = store.uploads();
        let ids: Vec<u64> = records.iter().map(|record| record.id).collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "ids increase: {ids:?}"
        );
        let seen: Vec<(&str, UploadState, bool)> = records
            .iter()
            .map(|record| (record.path.as_str(), record.state, record.cancelled))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("completed", UploadState::Completed, false),
                ("aborted", UploadState::Aborted, false),
                ("failed", UploadState::Aborted, false),
                ("dropped", UploadState::DroppedOpen, false),
                ("ending", UploadState::DroppedOpen, true),
                ("orphan", UploadState::Orphaned, false),
            ]
        );

        let leaked: Vec<String> = store
            .leaked_uploads()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(leaked, vec!["dropped".to_owned()]);
        let abandoned: Vec<String> = store
            .abandoned_uploads()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(abandoned, vec!["ending".to_owned(), "orphan".to_owned()]);
    }

    #[tokio::test]
    async fn push_at_hits_the_given_call() {
        let store = chaos();
        store.push_at(Call::Put, 3, Fault::Before(Status::SlowDown));
        for index in 1..=4 {
            let result = store
                .put(&path(&format!("k{index}")), Bytes::from_static(b"x").into())
                .await;
            assert_eq!(result.is_err(), index == 3, "put {index}");
        }

        // `release` clears a `push_at` that has not been reached yet.
        store.push_at(Call::Put, 6, Fault::Before(Status::SlowDown));
        store.release();
        store
            .put(&path("k5"), Bytes::from_static(b"x").into())
            .await
            .unwrap();
        store
            .put(&path("k6"), Bytes::from_static(b"x").into())
            .await
            .expect("release cleared the pending push_at");
    }
}
