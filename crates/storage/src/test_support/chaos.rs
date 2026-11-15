//! Injected S3 faults for the storage tests (R4.1, D-R-8).
//!
//! [`ChaosStore`] wraps an [`ObjectStore`] and fails or delays the calls it forwards. It is a second
//! implementation of the external trait, so the remote `put_durable` path runs in CI without AWS
//! (D119). It sits above `object_store`'s S3 client and its retries: a fault is what our code sees
//! after the client has given up, so every injected error is `object_store::Error::Generic` with
//! store `"S3"`.

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

/// A store that injects faults into the calls it forwards to `inner`. Clones share their state.
#[derive(Debug, Clone)]
pub(crate) struct ChaosStore {
    inner: Arc<dyn ObjectStore>,
    state: Arc<Mutex<ChaosState>>,
}

#[derive(Debug, Default)]
struct ChaosState {
    /// Faults queued per call kind, consumed from the front.
    scripted: BTreeMap<Call, VecDeque<Fault>>,
    /// Call kinds that fail before they reach the store until `release`.
    held: BTreeSet<Call>,
    /// Calls per kind, failed or not.
    calls: BTreeMap<Call, u64>,
    /// Faults that failed a call (delays excluded).
    injected: u64,
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
        assert!(
            call == Call::Get || !matches!(fault, Fault::Truncated | Fault::Corrupt),
            "{fault:?} applies to Get only, not {call:?}"
        );
        self.lock()
            .scripted
            .entry(call)
            .or_default()
            .push_back(fault);
    }

    /// Fails every call of these kinds before it reaches the store, until [`Self::release`].
    pub(crate) fn hold(&self, calls: &[Call]) {
        self.lock().held.extend(calls.iter().copied());
    }

    /// Ends every [`Self::hold`], and drops every queued fault, so a test starts its next step
    /// clean.
    pub(crate) fn release(&self) {
        let mut state = self.lock();
        state.held.clear();
        state.scripted.clear();
    }

    /// Calls of this kind so far, failed or not.
    pub(crate) fn count(&self, call: Call) -> u64 {
        self.lock().calls.get(&call).copied().unwrap_or(0)
    }

    /// Faults that failed a call so far (delays excluded).
    pub(crate) fn injected(&self) -> u64 {
        self.lock().injected
    }

    fn lock(&self) -> MutexGuard<'_, ChaosState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a call of this kind without deciding a fault.
    fn record(&self, call: Call) {
        *self.lock().calls.entry(call).or_default() += 1;
    }

    /// Counts a call and decides its fault: a held call fails, otherwise the next queued fault of
    /// its kind applies.
    fn next_fault(&self, call: Call) -> Option<Fault> {
        let mut state = self.lock();
        *state.calls.entry(call).or_default() += 1;
        let fault = if state.held.contains(&call) {
            Some(Fault::Before(Status::SlowDown))
        } else {
            state.scripted.get_mut(&call).and_then(VecDeque::pop_front)
        };
        if fault
            .as_ref()
            .is_some_and(|fault| !matches!(fault, Fault::Delay(_)))
        {
            state.injected += 1;
        }
        fault
    }
}

impl fmt::Display for ChaosStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ChaosStore({})", self.inner.as_ref())
    }
}

/// The multipart upload of a [`ChaosStore`]: its parts and completion go through the same queues.
struct ChaosUpload {
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

#[async_trait]
impl MultipartUpload for ChaosUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        match self.store.next_fault(Call::PutPart) {
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
        let fault = self.store.next_fault(Call::Complete);
        with_fault(fault, Call::Complete, self.inner.complete()).await
    }

    async fn abort(&mut self) -> Result<()> {
        let fault = self.store.next_fault(Call::Abort);
        with_fault(fault, Call::Abort, self.inner.abort()).await
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
        let fault = self.next_fault(Call::Put);
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
        let fault = self.next_fault(Call::MultipartStart);
        let inner = with_fault(
            fault,
            Call::MultipartStart,
            self.inner.put_multipart_opts(location, opts),
        )
        .await?;
        Ok(Box::new(ChaosUpload {
            inner,
            store: self.clone(),
        }))
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let call = if options.head { Call::Head } else { Call::Get };
        match self.next_fault(call) {
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
                    let fault = store.next_fault(Call::Delete);
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
}
