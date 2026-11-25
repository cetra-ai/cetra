//! The query runtime (D-R-39): a Tokio runtime of its own for DataFusion execution, so a
//! query or a compaction job never holds a worker of the runtime ingest runs on.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::dataframe::DataFrame;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use futures::{Stream, StreamExt};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};

/// Name of every thread of the query runtime.
pub(crate) const THREAD_NAME: &str = "cetra-query";
/// Nice levels the query threads add to the process's on Linux (D-R-39).
#[cfg(target_os = "linux")]
pub(crate) const NICE_INCREMENT: i32 = 10;
/// Stack of each query thread: DataFusion plans recursively (the main runtime uses the
/// same 8 MiB, `crates/cli/src/main.rs`).
const STACK_BYTES: usize = 8 * 1024 * 1024;
/// Batches a bridged stream buffers ahead of its reader. They sit outside DataFusion's
/// memory pool: two batches, at most 2 × 8,192 rows.
const STREAM_BUFFER: usize = 2;
/// The error a caller gets when the query runtime has shut down.
pub(crate) const STOPPED: &str = "the query runtime has stopped";

/// A handle on the query runtime. Cloning shares it; the runtime shuts down when the
/// last clone is dropped, without waiting for its tasks (`Runtime::shutdown_background`),
/// so dropping it inside an async context is allowed.
#[derive(Clone)]
pub struct QueryRuntime {
    inner: Arc<Owned>,
}

struct Owned {
    /// `Some` until `Drop` takes it.
    runtime: Option<Runtime>,
    handle: Handle,
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for QueryRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryRuntime")
            .finish_non_exhaustive()
    }
}

impl QueryRuntime {
    /// Builds the runtime: `threads` workers named [`THREAD_NAME`], 8 MiB stacks, I/O and
    /// time drivers on, and [`lower_priority`] as `on_thread_start`.
    pub(crate) fn new(threads: NonZeroUsize) -> std::io::Result<Self> {
        // The nice value of the thread that builds the runtime: the query threads are
        // lowered relative to it.
        #[cfg(target_os = "linux")]
        let base = rustix::process::getpriority_process(None).unwrap_or(0);
        #[cfg(not(target_os = "linux"))]
        let base = 0;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads.get())
            .thread_name(THREAD_NAME)
            .thread_stack_size(STACK_BYTES)
            .enable_all()
            .on_thread_start(move || lower_priority(base))
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self {
            inner: Arc::new(Owned {
                runtime: Some(runtime),
                handle,
            }),
        })
    }

    /// The runtime's handle, for `JoinSet::spawn_on`.
    pub(crate) fn handle(&self) -> &Handle {
        &self.inner.handle
    }

    /// Runs `future` on the query runtime and returns its output.
    pub(crate) async fn run<F, T>(&self, future: F) -> Result<T, Stopped>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let task = self.inner.handle.spawn(future);
        let _abort = AbortOnDrop(task.abort_handle());
        match task.await {
            Ok(value) => Ok(value),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => Err(Stopped),
        }
    }

    /// Runs `frame.collect()` on the query runtime.
    pub(crate) async fn collect(
        &self,
        frame: DataFrame,
    ) -> Result<Vec<RecordBatch>, DataFusionError> {
        match self.run(async move { frame.collect().await }).await {
            Ok(result) => result,
            Err(Stopped) => Err(DataFusionError::Execution(STOPPED.to_owned())),
        }
    }

    /// Runs `frame.execute_stream()` on the query runtime and returns a stream that
    /// reads the batches it produces there.
    pub(crate) async fn execute_stream(
        &self,
        frame: DataFrame,
    ) -> Result<SendableRecordBatchStream, DataFusionError> {
        let (schema_tx, schema_rx) = oneshot::channel::<Result<SchemaRef, DataFusionError>>();
        let (batch_tx, batch_rx) =
            mpsc::channel::<Result<RecordBatch, DataFusionError>>(STREAM_BUFFER);
        // The whole of `execute_stream()` runs on the query runtime: some operators (for
        // example `CoalescePartitionsExec`) spawn their partition tasks inside `execute`,
        // on the runtime that calls it.
        let task = self.inner.handle.spawn(async move {
            let mut stream = match frame.execute_stream().await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = schema_tx.send(Err(error));
                    return;
                }
            };
            if schema_tx.send(Ok(stream.schema())).is_err() {
                return;
            }
            while let Some(item) = stream.next().await {
                // The reader dropped the stream: returning drops the DataFusion stream,
                // which cancels the plan.
                if batch_tx.send(item).await.is_err() {
                    return;
                }
            }
        });
        let abort = AbortOnDrop(task.abort_handle());
        let schema = match schema_rx.await {
            Ok(Ok(schema)) => schema,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                // The task ended without sending a schema: it panicked or was cancelled.
                return match task.await {
                    Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                    _ => Err(DataFusionError::Execution(STOPPED.to_owned())),
                };
            }
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            Bridged {
                receiver: batch_rx,
                task: Some(task),
                _abort: abort,
            },
        )))
    }
}

/// The query runtime shut down before the work finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the query runtime has stopped")]
pub(crate) struct Stopped;

/// `on_thread_start` of the query runtime: on Linux, sets the calling thread's nice
/// value to `base + NICE_INCREMENT` (at most 19), where `base` is the nice value of the
/// thread that built the runtime. Absolute, not relative: blocking-pool threads are
/// spawned from query threads and inherit their nice value, so `current + 10` would
/// stack. Elsewhere does nothing. A failure is logged at debug and ignored: the thread
/// then runs at the priority it inherited.
#[cfg(target_os = "linux")]
pub(crate) fn lower_priority(base: i32) {
    let nice = (base + NICE_INCREMENT).min(19);
    // On Linux `PRIO_PROCESS` with a thread id changes that thread only.
    if let Err(error) = rustix::process::setpriority_process(Some(rustix::thread::gettid()), nice)
    {
        tracing::debug!(%error, "lowering the query thread priority");
    }
}

/// `on_thread_start` of the query runtime. Elsewhere it does nothing.
#[cfg(not(target_os = "linux"))]
pub(crate) fn lower_priority(base: i32) {
    let _ = base;
}

/// Aborts its task when dropped. Aborting a finished task is a no-op.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The stream [`QueryRuntime::execute_stream`] returns: it reads the batches the task on
/// the query runtime sends. Dropping it aborts that task.
struct Bridged {
    receiver: mpsc::Receiver<Result<RecordBatch, DataFusionError>>,
    /// `Some` until the task has finished and its result was taken.
    task: Option<JoinHandle<()>>,
    _abort: AbortOnDrop,
}

impl Stream for Bridged {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Every field is `Unpin`, so the stream can be accessed directly.
        let this = self.get_mut();
        match this.receiver.poll_recv(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                // The task dropped its sender: its result says whether it finished or
                // failed. A failure must not look like a short result.
                let Some(task) = this.task.as_mut() else {
                    return Poll::Ready(None);
                };
                match Pin::new(task).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.task = None;
                        Poll::Ready(None)
                    }
                    Poll::Ready(Err(error)) if error.is_panic() => {
                        std::panic::resume_unwind(error.into_panic())
                    }
                    Poll::Ready(Err(_)) => {
                        this.task = None;
                        Poll::Ready(Some(Err(DataFusionError::Execution(
                            STOPPED.to_owned(),
                        ))))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use datafusion::logical_expr::{ColumnarValue, Volatility, create_udf};
    use datafusion::prelude::SessionContext;

    use super::*;

    fn query_runtime(threads: usize) -> QueryRuntime {
        QueryRuntime::new(NonZeroUsize::new(threads).unwrap()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_returns_the_output_from_a_query_thread() {
        let runtime = query_runtime(2);
        let name = runtime
            .run(async { std::thread::current().name().map(str::to_owned) })
            .await;
        assert_eq!(name, Ok(Some("cetra-query".to_owned())));
    }

    /// Sends a message when dropped, so a test can see that its future was dropped.
    struct Notify(std::sync::mpsc::Sender<()>);

    impl Drop for Notify {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_run_aborts_the_task() {
        let runtime = query_runtime(2);
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            runtime.run(async move {
                let _guard = Notify(tx);
                std::future::pending::<()>().await
            }),
        )
        .await;
        rx.recv_timeout(Duration::from_secs(2))
            .expect("the aborted task's future was dropped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[should_panic(expected = "boom")]
    async fn a_panic_reaches_the_caller() {
        let runtime = query_runtime(2);
        let _: Result<(), Stopped> = runtime.run(async { panic!("boom") }).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_after_the_runtime_is_gone_is_stopped() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        // A plain drop shuts a runtime down synchronously, which is not allowed on an
        // async thread.
        tokio::task::spawn_blocking(move || drop(runtime))
            .await
            .unwrap();
        let query = QueryRuntime {
            inner: Arc::new(Owned {
                runtime: None,
                handle,
            }),
        };
        assert_eq!(query.run(async {}).await, Err(Stopped));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_and_execute_stream_return_the_rows() {
        let runtime = query_runtime(2);
        let ctx = SessionContext::new();
        let sql = "SELECT * FROM (VALUES (1), (2), (3)) AS t(x)";

        let frame = ctx.sql(sql).await.unwrap();
        let batches: Vec<RecordBatch> = runtime.collect(frame).await.unwrap();
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 3);

        let frame = ctx.sql(sql).await.unwrap();
        let mut stream = runtime.execute_stream(frame).await.unwrap();
        assert!(stream.schema().field_with_name("x").is_ok());
        let mut rows = 0;
        while let Some(batch) = stream.next().await {
            rows += batch.unwrap().num_rows();
        }
        assert_eq!(rows, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_stream_reports_an_execution_error() {
        let runtime = query_runtime(2);
        let ctx = SessionContext::new();
        let frame = ctx.sql("SELECT CAST('a' AS INT)").await.unwrap();
        match runtime.execute_stream(frame).await {
            Err(_) => {}
            Ok(mut stream) => {
                let first = stream.next().await.unwrap();
                assert!(first.is_err(), "the cast must fail, got {first:?}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[should_panic(expected = "udf boom")]
    async fn a_panic_mid_stream_reaches_the_reader() {
        let runtime = query_runtime(2);
        let ctx = SessionContext::new();
        let boom = create_udf(
            "boom",
            vec![datafusion::arrow::datatypes::DataType::Int64],
            datafusion::arrow::datatypes::DataType::Int64,
            Volatility::Volatile,
            Arc::new(|_: &[ColumnarValue]| -> datafusion::error::Result<ColumnarValue> {
                panic!("udf boom")
            }),
        );
        ctx.register_udf(boom);
        let frame = ctx
            .sql("SELECT boom(value) FROM generate_series(1, 10)")
            .await
            .unwrap();
        let mut stream = runtime.execute_stream(frame).await.unwrap();
        while let Some(item) = stream.next().await {
            item.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_stream_stops_the_producer() {
        let runtime = query_runtime(2);
        let ctx = SessionContext::new();
        let frame = ctx
            .sql("SELECT * FROM generate_series(1, 100000000)")
            .await
            .unwrap();
        let mut stream = runtime.execute_stream(frame).await.unwrap();
        assert!(stream.next().await.is_some());
        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.handle().metrics().num_alive_tasks() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the producer task ended after the stream was dropped");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_threads_run_at_a_lower_priority() {
        let base = rustix::process::getpriority_process(None).unwrap();
        let runtime = query_runtime(2);
        let nice = runtime
            .run(async {
                rustix::process::getpriority_process(Some(rustix::thread::gettid())).unwrap()
            })
            .await
            .unwrap();
        assert_eq!(nice, (base + NICE_INCREMENT).min(19));
    }
}
