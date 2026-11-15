//! S3 faults over TCP (R4.7, spec 03 §2): toxiproxy in front of an S3 endpoint, so that the S3
//! client's own retry loop runs for real. `ChaosStore` (in-process) never triggers that loop.
//!
//! Every test is `#[ignore]`d: run them with `just s3-chaos`. Each one also skips (prints and
//! returns) when `LLMOBS_TEST_S3_ENDPOINT` or `LLMOBS_TEST_TOXIPROXY` is unset.
//!
//! Environment:
//! - `LLMOBS_TEST_S3_ENDPOINT`: the S3 endpoint, Versity's address, e.g. `http://127.0.0.1:7070`;
//! - `LLMOBS_TEST_S3_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`;
//! - `LLMOBS_TEST_S3_REGION` (default `us-east-1`) and `LLMOBS_TEST_S3_PATH_STYLE` (default `true`);
//! - `LLMOBS_TEST_TOXIPROXY`: the toxiproxy API, e.g. `http://127.0.0.1:8474`;
//! - `LLMOBS_TEST_TOXIPROXY_TOKEN`: optional, sent as a bearer token.
//!
//! Setup. Versity Gateway serves a directory as S3 (see `tests/s3.rs`):
//!
//! ```text
//! docker run --rm --name llmobs-s3 -p 7070:7070 --tmpfs /data:rw,mode=1777,size=4g -e ROOT_ACCESS_KEY=llmobs -e ROOT_SECRET_KEY=llmobs-secret -e VGW_BACKEND=posix -e VGW_BACKEND_ARGS=/data -e VGW_ARGS="--port :7070" versity/versitygw:v1.11.0
//! curl --aws-sigv4 "aws:amz:us-east-1:s3" --user llmobs:llmobs-secret -X PUT http://127.0.0.1:7070/llmobs-test
//! docker run --rm --name llmobs-toxiproxy --net=host ghcr.io/shopify/toxiproxy:2.13.0 -host 127.0.0.1
//! ```
//!
//! `-host 127.0.0.1` keeps the API (`:8474`) and the proxies on loopback. With it no token is
//! needed; if one is set anyway, `LLMOBS_TEST_TOXIPROXY_TOKEN` must match. Without Docker, run the
//! release binary `toxiproxy-server-linux-amd64` (v2.13.0) with `-host 127.0.0.1`.
//!
//! Verify both image tags and the toxiproxy flags when M4 runs (`docker pull`); if a newer
//! release exists, use it and record the versions in `docs/research/storage/hardening.md`.
//!
//! Each test has its own proxy, `llmobs-<test>`, and its own prefix, `s3-chaos-<test>-<ms>`. A
//! test deletes its proxy first, ignoring a 404, so a proxy left by a failed run cannot make
//! `proxy` fail. At the end it deletes the proxy, then the prefix, calling Versity directly.

// The probe lines (timings, retry and GC pass counts) are printed for `hardening.md`; read them
// with `--nocapture`, as `just s3-chaos` runs them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arrow::array::{Array, StringArray};
use bytesize::ByteSize;
use datafusion::execution::context::{SessionConfig, SessionContext};
use futures::TryStreamExt;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::span::Span;
use llmobs_storage::config::{
    FlushConfig, MemtableConfig, ObjectStoreUrl, S3Config, StorageConfig,
};
use llmobs_storage::error::AppendError;
use llmobs_storage::{ProjectQuery, Storage};
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, ObjectStoreExt};
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use sqlx::sqlite::SqliteConnectOptions;

/// The bound of `shutdown` (plan § R4.7): 30 s for compaction and 30 s for flushes, plus margin.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(75);
const MIB: u64 = 1024 * 1024;

/// What a test needs from the environment. `None` from `from_env` means the test is skipped.
struct Env {
    /// The S3 endpoint as the test reaches it directly (Versity).
    endpoint: String,
    bucket: String,
    region: String,
    path_style: bool,
    toxi: Toxiproxy,
}

impl Env {
    fn from_env() -> Option<Self> {
        let Ok(endpoint) = std::env::var("LLMOBS_TEST_S3_ENDPOINT") else {
            println!("skipped: LLMOBS_TEST_S3_ENDPOINT is not set");
            return None;
        };
        let Ok(api) = std::env::var("LLMOBS_TEST_TOXIPROXY") else {
            println!("skipped: LLMOBS_TEST_TOXIPROXY is not set");
            return None;
        };
        let bucket = std::env::var("LLMOBS_TEST_S3_BUCKET").expect("LLMOBS_TEST_S3_BUCKET");
        let region = std::env::var("LLMOBS_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let path_style =
            std::env::var("LLMOBS_TEST_S3_PATH_STYLE").map_or(true, |value| value != "false");
        let token = std::env::var("LLMOBS_TEST_TOXIPROXY_TOKEN").ok();
        Some(Self {
            endpoint,
            bucket,
            region,
            path_style,
            toxi: Toxiproxy::new(&api, token),
        })
    }
}

/// The toxiproxy API the suite uses (v2.13.0).
struct Toxiproxy {
    api: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Toxiproxy {
    fn new(api: &str, token: Option<String>) -> Self {
        Self {
            api: api.trim_end_matches('/').to_owned(),
            token,
            http: reqwest::Client::new(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, format!("{}{path}", self.api));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// `POST /proxies`: a proxy on a free loopback port of its own. Returns the address it listens on.
    async fn proxy(&self, name: &str, upstream: &str) -> Result<String, String> {
        let body = json!({
            "name": name,
            "listen": "127.0.0.1:0",
            "upstream": upstream,
            "enabled": true,
        });
        let response = self
            .request(reqwest::Method::POST, "/proxies")
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("create proxy {name}: {error}"))?;
        let response = check(response, "create a proxy")?;
        let created: Value = response
            .json()
            .await
            .map_err(|error| format!("read the proxy answer: {error}"))?;
        created["listen"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "the proxy answer has no listen address".to_owned())
    }

    /// `POST /proxies/{proxy}/toxics`.
    async fn toxic(
        &self,
        proxy: &str,
        name: &str,
        kind: &str,
        stream: &str,
        toxicity: f64,
        attributes: Value,
    ) -> Result<(), String> {
        let body = json!({
            "name": name,
            "type": kind,
            "stream": stream,
            "toxicity": toxicity,
            "attributes": attributes,
        });
        let response = self
            .request(reqwest::Method::POST, &format!("/proxies/{proxy}/toxics"))
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("add toxic {name}: {error}"))?;
        check(response, "add a toxic").map(drop)
    }

    /// `DELETE /proxies/{proxy}/toxics/{name}`.
    async fn remove(&self, proxy: &str, name: &str) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/proxies/{proxy}/toxics/{name}"),
            )
            .send()
            .await
            .map_err(|error| format!("remove toxic {name}: {error}"))?;
        check(response, "remove a toxic").map(drop)
    }

    /// `POST /proxies/{proxy}` with `enabled`: `false` refuses connections (the `down` toxic).
    async fn enabled(&self, proxy: &str, on: bool) -> Result<(), String> {
        let response = self
            .request(reqwest::Method::POST, &format!("/proxies/{proxy}"))
            .json(&json!({ "enabled": on }))
            .send()
            .await
            .map_err(|error| format!("set proxy {proxy} enabled: {error}"))?;
        check(response, "enable or disable a proxy").map(drop)
    }

    /// `DELETE /proxies/{proxy}`. A 404 (no such proxy) is success.
    async fn delete(&self, proxy: &str) -> Result<(), String> {
        let response = self
            .request(reqwest::Method::DELETE, &format!("/proxies/{proxy}"))
            .send()
            .await
            .map_err(|error| format!("delete proxy {proxy}: {error}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        check(response, "delete a proxy").map(drop)
    }
}

/// `response` when its status is a success, else an error that names the status only.
fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(format!("toxiproxy did not {what}: {}", response.status()))
    }
}

/// One test's proxy, prefix and storage endpoint.
struct Case {
    env: Env,
    /// The proxy's name, `llmobs-<test>`.
    proxy: String,
    /// The object prefix of this test, `s3-chaos-<test>-<ms>`.
    prefix: String,
    /// The storage endpoint through the proxy, `http://<listen>`.
    endpoint: String,
}

impl Case {
    async fn start(env: Env, test: &str) -> Result<Self, String> {
        let proxy = format!("llmobs-{test}");
        env.toxi.delete(&proxy).await?;
        let upstream = env
            .endpoint
            .trim_end_matches('/')
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_owned();
        let listen = env.toxi.proxy(&proxy, &upstream).await?;
        Ok(Self {
            endpoint: format!("http://{listen}"),
            prefix: format!("s3-chaos-{test}-{}", now_ms()),
            proxy,
            env,
        })
    }

    /// The storage config of this test, through the proxy: flushes every second, memtables of
    /// 64 MiB, files of 16 MiB at most (plan § R4.7).
    fn config(&self, dir: &Path) -> StorageConfig {
        config(dir, &self.env, &self.prefix, &self.endpoint)
    }

    /// The same data directory read directly against Versity, as the tail of each test uses.
    fn direct_config(&self, dir: &Path) -> StorageConfig {
        config(dir, &self.env, &self.prefix, &self.env.endpoint)
    }

    async fn toxic(
        &self,
        name: &str,
        kind: &str,
        stream: &str,
        toxicity: f64,
        attributes: Value,
    ) -> Result<(), String> {
        self.env
            .toxi
            .toxic(&self.proxy, name, kind, stream, toxicity, attributes)
            .await
    }

    async fn remove(&self, name: &str) -> Result<(), String> {
        self.env.toxi.remove(&self.proxy, name).await
    }

    /// Lets a flush commit: waits until the catalog has a file, at most `limit`.
    async fn wait_for_a_file(&self, dir: &Path, limit: Duration) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        loop {
            if catalog_rows(dir).await? > 0 {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!("no file was committed within {limit:?}"));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Multipart uploads still open under this test's prefix, polled every second for up to 10 s.
    /// Returns the last count. A count above 0 fails unless `allow` is set.
    async fn check_uploads(&self, allow: bool) -> Result<usize, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut count = incomplete_uploads(&self.env, &self.prefix)?;
        while count > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            count = incomplete_uploads(&self.env, &self.prefix)?;
        }
        if count > 0 && !allow {
            return Err(format!(
                "{count} incomplete uploads remain under the prefix"
            ));
        }
        Ok(count)
    }

    /// Deletes the proxy, then every object under the prefix, calling Versity directly.
    async fn finish(self) -> Result<(), String> {
        self.env.toxi.delete(&self.proxy).await?;
        delete_prefix(&self.env, &self.prefix).await
    }
}

fn config(dir: &Path, env: &Env, prefix: &str, endpoint: &str) -> StorageConfig {
    StorageConfig {
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: Some(ObjectStoreUrl::S3 {
            bucket: env.bucket.clone(),
            prefix: prefix.to_owned(),
        }),
        s3: S3Config {
            region: Some(env.region.clone()),
            endpoint: Some(endpoint.to_owned()),
            force_path_style: env.path_style,
            allow_http: endpoint.starts_with("http://"),
        },
        wal: Default::default(),
        flush: FlushConfig {
            max_bytes: ByteSize::mib(16),
            max_age: Duration::from_secs(1),
            upload_concurrency: 2,
        },
        memtable: MemtableConfig {
            hard_limit: ByteSize::mib(64),
        },
        query: Default::default(),
        anchor: Default::default(),
        gc: Default::default(),
        compaction: Default::default(),
        deletion: Default::default(),
    }
}

/// `shutdown`, which must return within [`SHUTDOWN_LIMIT`].
async fn shut_down(storage: &Storage) -> Result<(), String> {
    match tokio::time::timeout(SHUTDOWN_LIMIT, storage.shutdown()).await {
        Ok(result) => result.map_err(|error| format!("shutdown: {error}")),
        Err(_) => Err(format!("shutdown did not return within {SHUTDOWN_LIMIT:?}")),
    }
}

/// The tail of every test: a fresh open of `dir` directly against Versity counts `acknowledged`
/// rows of the project.
async fn reopen_direct(case: &Case, dir: &Path, acknowledged: usize) -> Result<(), String> {
    let storage = Storage::open(case.direct_config(dir))
        .await
        .map_err(|error| format!("reopen directly against the store: {error}"))?;
    let rows = count_rows(&storage).await?;
    shut_down(&storage).await?;
    drop(storage);
    if rows != acknowledged {
        return Err(format!(
            "rows read after a direct reopen: {rows}, acknowledged: {acknowledged}"
        ));
    }
    Ok(())
}

/// Appends `rows` spans of `payload` bytes of output each, and returns the rows acknowledged.
async fn append(storage: &Storage, rows: usize, payload: usize) -> Result<usize, String> {
    storage
        .append_spans(project(), spans(rows, payload))
        .await
        .map_err(|error| format!("append {rows} spans: {error}"))?;
    Ok(rows)
}

/// Every span row the project reads, through a query of its snapshot.
async fn count_rows(storage: &Storage) -> Result<usize, String> {
    let query = storage
        .query(project(), llmobs_storage::QueryOptions::new(None, None))
        .await
        .map_err(|error| format!("query: {error}"))?;
    Ok(span_count(&query).await)
}

/// Rows of the catalog's `data_files` table, read through a fresh read-only connection.
async fn catalog_rows(dir: &Path) -> Result<i64, String> {
    let mut connection = catalog(dir).await?;
    sqlx::query_scalar("SELECT COUNT(*) FROM data_files")
        .fetch_one(&mut connection)
        .await
        .map_err(|error| error.to_string())
}

/// Rows of `data_files` that GC has not yet deleted.
async fn obsolete_rows(dir: &Path) -> Result<i64, String> {
    let mut connection = catalog(dir).await?;
    sqlx::query_scalar("SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'")
        .fetch_one(&mut connection)
        .await
        .map_err(|error| error.to_string())
}

async fn catalog(dir: &Path) -> Result<sqlx::SqliteConnection, String> {
    SqliteConnectOptions::new()
        .filename(dir.join("catalog.db"))
        .read_only(true)
        .busy_timeout(Duration::from_secs(5))
        .connect()
        .await
        .map_err(|error| error.to_string())
}

/// The size of the largest catalogued file, in bytes.
async fn largest_object(dir: &Path) -> Result<i64, String> {
    let mut connection = catalog(dir).await?;
    let size: Option<i64> = sqlx::query_scalar("SELECT MAX(size_bytes) FROM data_files")
        .fetch_one(&mut connection)
        .await
        .map_err(|error| error.to_string())?;
    size.ok_or_else(|| "catalog has no files".to_owned())
}

/// The multipart uploads under `prefix`, as `ListMultipartUploads` reports them. `object_store`
/// has no call for it, so this runs curl with its built-in SigV4 signing. The keys go to curl on
/// stdin (`-K -`), never on a command line. The URL follows `LLMOBS_TEST_S3_PATH_STYLE`, with the
/// query parameters in sorted order, which is the order curl signs. The prefix is sent with a
/// trailing `/` (encoded), so `s3-chaos-x-1` does not match `s3-chaos-x-12`. The answer must be a
/// 200 `ListMultipartUploadsResult`: any other status is an error, not an empty list.
fn incomplete_uploads(env: &Env, prefix: &str) -> Result<usize, String> {
    let query = format!("prefix={prefix}%2F&uploads");
    let endpoint = env.endpoint.trim_end_matches('/');
    let url = if env.path_style {
        format!("{endpoint}/{}?{query}", env.bucket)
    } else {
        let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
        format!("{scheme}://{}.{host}/?{query}", env.bucket)
    };
    let key = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| "AWS_ACCESS_KEY_ID is not set")?;
    let secret =
        std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| "AWS_SECRET_ACCESS_KEY is not set")?;
    let mut child = Command::new("curl")
        .args(["-s", "-K", "-", "--aws-sigv4"])
        .arg(format!("aws:amz:{}:s3", env.region))
        .args(["-w", "\n%{http_code}"])
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("run curl: {error}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .expect("curl was spawned with a stdin pipe");
        stdin
            .write_all(format!("user = \"{}\"\n", curl_credentials(&key, &secret)).as_bytes())
            .map_err(|error| format!("write the credentials to curl: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for curl: {error}"))?;
    if !output.status.success() {
        return Err(format!("curl exited with {}", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text
        .rsplit_once('\n')
        .ok_or("curl printed no HTTP status")?;
    let status = status.trim();
    if status != "200" || !body.contains("<ListMultipartUploadsResult") {
        return Err(format!("ListMultipartUploads answered {status}"));
    }
    Ok(body.matches("<Upload>").count())
}

/// The `user = "..."` value of a curl config line: `\` and `"` are escaped, as curl reads quoted
/// strings with backslash escapes.
fn curl_credentials(key: &str, secret: &str) -> String {
    format!("{key}:{secret}")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Deletes every object under `prefix`, calling Versity directly (copied from `tests/s3.rs`).
async fn delete_prefix(env: &Env, prefix: &str) -> Result<(), String> {
    let http = env.endpoint.starts_with("http://");
    let store = AmazonS3Builder::from_env()
        .with_bucket_name(&env.bucket)
        .with_endpoint(&env.endpoint)
        .with_region(&env.region)
        .with_virtual_hosted_style_request(!env.path_style)
        .with_allow_http(http)
        .build()
        .map_err(|error| error.to_string())?;
    let root = object_store::path::Path::from(prefix);
    let locations: Vec<object_store::path::Path> = store
        .list(Some(&root))
        .map_ok(|meta| meta.location)
        .try_collect()
        .await
        .map_err(|error| error.to_string())?;
    for location in locations {
        store
            .delete(&location)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

/// `rows` spans of one trace, each with `payload` bytes of output (copied from `tests/s3.rs`).
fn spans(rows: usize, payload: usize) -> arrow::array::RecordBatch {
    let project_id = project();
    let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736"
        .parse::<TraceId>()
        .unwrap();
    let spans: Vec<Span> = (0..rows)
        .map(|row| {
            let mut bytes = (u64::try_from(row).unwrap_or(0).saturating_add(1)).to_be_bytes();
            if bytes == [0; 8] {
                bytes[7] = 1;
            }
            let start = 1_000 + i64::try_from(row).unwrap_or(0);
            Span {
                project_id,
                trace_id,
                span_id: SpanId::from_bytes(bytes).unwrap(),
                parent_span_id: None,
                name: format!("s3-chaos/{row}"),
                span_kind: SpanKind::Span,
                start_time: start,
                end_time: start + 100,
                status: SpanStatus::Ok,
                status_message: None,
                service_name: None,
                session_id: None,
                user_id: None,
                environment: None,
                release: None,
                tags: Vec::new(),
                metadata: BTreeMap::new(),
                provider: None,
                request_model: None,
                response_model: None,
                operation: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                total_tokens: None,
                cost_input: None,
                cost_output: None,
                cost_total: None,
                cost_source: None,
                ttft_ns: None,
                request_params: None,
                input: None,
                input_format: None,
                output: (payload > 0).then(|| filler(row, payload)),
                output_format: None,
                input_preview: None,
                output_preview: None,
                system_instructions: None,
                tool_definitions: None,
                attributes: BTreeMap::new(),
                resource_attributes: BTreeMap::new(),
                scope_name: None,
                scope_version: None,
                events: Vec::new(),
                links: Vec::new(),
                media_refs: Vec::new(),
                truncated: Vec::new(),
                conventions: Vec::new(),
                ingest_time: start + 200,
                schema_version: 1,
            }
        })
        .collect();
    spans_to_record_batch(&spans).unwrap()
}

/// Printable bytes that do not compress (copied from `tests/s3.rs`).
fn filler(row: usize, len: usize) -> String {
    let mut state = u64::try_from(row)
        .unwrap_or(1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for byte in state.to_le_bytes() {
            out.push(char::from(b'!' + (byte % 94)));
            if out.len() == len {
                break;
            }
        }
    }
    out
}

async fn span_count(query: &ProjectQuery) -> usize {
    let ctx = SessionContext::new_with_config_rt(
        SessionConfig::new(),
        std::sync::Arc::clone(&query.runtime),
    );
    ctx.register_table("spans_t", std::sync::Arc::clone(&query.spans))
        .unwrap();
    let batches = ctx
        .sql("SELECT name FROM spans_t")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    batches
        .iter()
        .map(|batch| {
            batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .len()
        })
        .sum()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch, as storage takes them.
fn now_ns() -> i64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

/// Runs a test body against its case, then deletes the case's proxy and prefix whatever the body
/// returned, and fails the test on the body's error first.
macro_rules! run_case {
    ($test:literal, $case:ident, $body:expr) => {{
        let Some(env) = Env::from_env() else { return };
        let $case = Case::start(env, $test)
            .await
            .expect("start the test's proxy");
        let started = Instant::now();
        let outcome = $body;
        println!("s3_chaos {}: {:?}", $test, started.elapsed());
        let cleanup = $case.finish().await;
        outcome.expect($test);
        cleanup.expect("delete the test's proxy and prefix");
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn latency_and_jitter() {
    run_case!(
        "latency_and_jitter",
        case,
        latency_and_jitter_run(&case).await
    );
}

/// `latency` of 200 ms plus up to 100 ms of jitter on both streams. Four appends flush, and a
/// query returns the four rows within 30 s.
async fn latency_and_jitter_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let latency = json!({ "latency": 200, "jitter": 100 });
    case.toxic(
        "latency-down",
        "latency",
        "downstream",
        1.0,
        latency.clone(),
    )
    .await?;
    case.toxic("latency-up", "latency", "upstream", 1.0, latency)
        .await?;

    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let mut acknowledged = 0;
    for _ in 0..4 {
        acknowledged += append(&storage, 1, 0).await?;
    }
    case.wait_for_a_file(dir.path(), Duration::from_secs(60))
        .await?;
    let rows = tokio::time::timeout(Duration::from_secs(30), count_rows(&storage))
        .await
        .map_err(|_| "a query did not return within 30 s".to_owned())??;
    if rows != acknowledged {
        return Err(format!(
            "query returned {rows} rows, acknowledged {acknowledged}"
        ));
    }
    shut_down(&storage).await?;
    drop(storage);
    case.check_uploads(false).await?;
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn reset_peer_on_some_connections() {
    run_case!(
        "reset_peer_on_some_connections",
        case,
        reset_peer_run(&case).await
    );
}

/// `reset_peer` on 30% of upstream connections. Twenty 1 MiB appends flush, a compaction pass
/// runs, a 12,000-row batch goes out as a multipart upload, and GC runs until the obsolete rows
/// are gone. Failed GC passes are counted: the bulk delete is not retried on a reset connection
/// (`docs/research/storage/hardening.md`, `M4-n`). Every acknowledged row must read back.
async fn reset_peer_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    case.toxic(
        "reset",
        "reset_peer",
        "upstream",
        0.3,
        json!({ "timeout": 0 }),
    )
    .await?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let mut acknowledged = 0;
    for _ in 0..20 {
        acknowledged += append(&storage, 16, 64 * 1024).await?;
    }
    case.wait_for_a_file(dir.path(), Duration::from_secs(120))
        .await?;

    // Past the close delay of the partitions, so compaction may close them.
    let report = storage
        .compact(now_ns() + 2 * 24 * 3600 * 1_000_000_000)
        .await
        .map_err(|error| format!("compact through the reset connections: {error}"))?;
    if report.failed > 0 {
        return Err(format!("{} compaction jobs failed", report.failed));
    }

    acknowledged += append(&storage, 12_000, 8 * 1024).await?;
    shut_down(&storage).await?;
    drop(storage);

    // GC through the same toxic. A pass may fail on a reset bulk delete; a later one must succeed.
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("reopen through the proxy: {error}"))?;
    let mut failed_passes = 0_u32;
    let mut cleaned = false;
    for _ in 0..10 {
        match storage.collect_obsolete(Duration::ZERO).await {
            Ok(_) if obsolete_rows(dir.path()).await? == 0 => {
                cleaned = true;
                break;
            }
            Ok(_) => {}
            Err(_) => failed_passes += 1,
        }
    }
    println!("s3_chaos reset_peer: {failed_passes} GC passes failed");
    if !cleaned {
        return Err("obsolete rows remain after 10 GC passes".to_owned());
    }
    shut_down(&storage).await?;
    drop(storage);

    let leaked = case.check_uploads(true).await?;
    println!("s3_chaos reset_peer: {leaked} incomplete uploads after shutdown");
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn slicer_small_packets() {
    run_case!("slicer_small_packets", case, slicer_run(&case).await);
}

/// `slicer` cuts the downstream into 64-byte packets with a 10 µs delay. A round trip reads the
/// rows back through a fresh open of the proxy.
async fn slicer_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    case.toxic(
        "slicer",
        "slicer",
        "downstream",
        1.0,
        json!({ "average_size": 64, "size_variation": 32, "delay": 10 }),
    )
    .await?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let acknowledged = append(&storage, 50, 1024).await?;
    shut_down(&storage).await?;
    drop(storage);

    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("reopen through the proxy: {error}"))?;
    let rows = count_rows(&storage).await?;
    shut_down(&storage).await?;
    drop(storage);
    if rows != acknowledged {
        return Err(format!(
            "rows read through the slicer: {rows}, acknowledged: {acknowledged}"
        ));
    }
    case.check_uploads(false).await?;
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn limit_data_cuts_large_bodies() {
    run_case!(
        "limit_data_cuts_large_bodies",
        case,
        limit_data_run(&case).await
    );
}

/// `limit_data` closes each upstream connection after 1 MiB, half the time. A multipart flush
/// (12,000 rows) must still complete: the client retries the cut parts.
async fn limit_data_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    case.toxic(
        "limit",
        "limit_data",
        "upstream",
        0.5,
        json!({ "bytes": MIB }),
    )
    .await?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let acknowledged = append(&storage, 12_000, 8 * 1024).await?;
    case.wait_for_a_file(dir.path(), Duration::from_secs(300))
        .await?;
    shut_down(&storage).await?;
    drop(storage);
    // The check of the rest of the suite (no incomplete upload) is a finding here, not a
    // failure: a lost abort or create answer is what the lifecycle rule removes.
    let leaked = case.check_uploads(true).await?;
    println!("s3_chaos limit_data_cuts_large_bodies: {leaked} incomplete uploads after shutdown");
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn blackhole_then_recovery() {
    run_case!("blackhole_then_recovery", case, blackhole_run(&case).await);
}

/// Both streams blackholed (`timeout` with 0). Appends of 1 MiB go on until `Backpressure`, the
/// memtable stays bounded, and no row commits while the toxic is on. After the toxic is removed,
/// the memtable drains within 6 minutes, and every acknowledged row reads back.
async fn blackhole_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let mut acknowledged = append(&storage, 16, 64 * 1024).await?;
    case.wait_for_a_file(dir.path(), Duration::from_secs(60))
        .await?;

    case.toxic(
        "hole-down",
        "timeout",
        "downstream",
        1.0,
        json!({ "timeout": 0 }),
    )
    .await?;
    case.toxic(
        "hole-up",
        "timeout",
        "upstream",
        1.0,
        json!({ "timeout": 0 }),
    )
    .await?;
    let committed = catalog_rows(dir.path()).await?;

    let mut backpressure = false;
    for _ in 0..200 {
        match storage.append_spans(project(), spans(16, 64 * 1024)).await {
            Ok(_) => acknowledged += 16,
            Err(AppendError::Backpressure) => {
                backpressure = true;
                break;
            }
            Err(error) => return Err(format!("append during the blackhole: {error}")),
        }
    }
    if !backpressure {
        return Err("no Backpressure after 200 appends of 1 MiB".to_owned());
    }
    let health = storage.health();
    let bound = health.memtable_hard_limit + 2 * MIB;
    if health.memtable_bytes > bound {
        return Err(format!(
            "memtable holds {} bytes, over its limit plus 2 MiB",
            health.memtable_bytes
        ));
    }

    tokio::time::sleep(Duration::from_secs(45)).await;
    let during = catalog_rows(dir.path()).await?;
    if during != committed {
        return Err(format!(
            "{} catalog rows committed during the blackhole",
            during - committed
        ));
    }

    case.remove("hole-down").await?;
    case.remove("hole-up").await?;
    let recovery = Instant::now();
    let deadline = recovery + Duration::from_secs(6 * 60);
    loop {
        match storage.append_spans(project(), spans(1, 0)).await {
            Ok(_) => {
                acknowledged += 1;
                break;
            }
            Err(AppendError::Backpressure) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => return Err(format!("the memtable did not drain: {error}")),
        }
    }
    println!(
        "s3_chaos blackhole_then_recovery: the memtable drained in {:?}",
        recovery.elapsed()
    );
    shut_down(&storage).await?;
    drop(storage);
    case.check_uploads(false).await?;
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn down_during_shutdown() {
    run_case!("down_during_shutdown", case, down_run(&case).await);
}

/// The proxy refuses connections during a shutdown. The shutdown returns within 75 s, and the rows
/// replay from the WAL once the proxy is back.
async fn down_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let acknowledged = append(&storage, 4, 0).await?;

    case.env.toxi.enabled(&case.proxy, false).await?;
    let started = Instant::now();
    let shutdown = tokio::time::timeout(SHUTDOWN_LIMIT, storage.shutdown()).await;
    let elapsed = started.elapsed();
    let Ok(result) = shutdown else {
        return Err(format!("shutdown did not return within {SHUTDOWN_LIMIT:?}"));
    };
    println!("s3_chaos down_during_shutdown: shutdown returned {result:?} in {elapsed:?}");
    drop(storage);

    case.env.toxi.enabled(&case.proxy, true).await?;
    let storage = Storage::open(case.config(dir.path()))
        .await
        .map_err(|error| format!("reopen after the outage: {error}"))?;
    let rows = count_rows(&storage).await?;
    shut_down(&storage).await?;
    drop(storage);
    if rows != acknowledged {
        return Err(format!(
            "rows after the outage: {rows}, acknowledged: {acknowledged}"
        ));
    }
    case.check_uploads(false).await?;
    reopen_direct(case, dir.path(), acknowledged).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]
async fn bandwidth_and_large_single_put() {
    run_case!(
        "bandwidth_and_large_single_put",
        case,
        bandwidth_run(&case).await
    );
}

/// At 1 MiB/s a single put of about 47 MiB takes longer than the 30 s request timeout, so every
/// attempt times out. No file commits while the toxic is on. Once it is removed the flush commits
/// within 6 minutes, as one file between 40 and 64 MiB.
async fn bandwidth_run(case: &Case) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut config = case.config(dir.path());
    config.flush.max_bytes = ByteSize::mib(256);
    config.memtable.hard_limit = ByteSize::gib(1);
    case.toxic(
        "bandwidth",
        "bandwidth",
        "upstream",
        1.0,
        json!({ "rate": 1024 }),
    )
    .await?;
    let storage = Storage::open(config)
        .await
        .map_err(|error| format!("open through the proxy: {error}"))?;
    let acknowledged = append(&storage, 6_000, 8 * 1024).await?;

    let held_until = Instant::now() + Duration::from_secs(3 * 60);
    while Instant::now() < held_until {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let rows = catalog_rows(dir.path()).await?;
        if rows != 0 {
            return Err(format!("{rows} files committed through a 1 MiB/s link"));
        }
    }

    case.remove("bandwidth").await?;
    let deadline = Instant::now() + Duration::from_secs(6 * 60);
    let committed = loop {
        if catalog_rows(dir.path()).await? > 0 {
            break Instant::now();
        }
        if Instant::now() > deadline {
            return Err("the flush did not commit within 6 minutes".to_owned());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    println!(
        "s3_chaos bandwidth_and_large_single_put: committed {:?} after the toxic was removed",
        committed.elapsed()
    );
    let size = largest_object(dir.path()).await?;
    if !(40 * MIB as i64..=64 * MIB as i64).contains(&size) {
        return Err(format!("the file is {size} bytes, outside 40 to 64 MiB"));
    }
    shut_down(&storage).await?;
    drop(storage);
    case.check_uploads(false).await?;
    reopen_direct(case, dir.path(), acknowledged).await
}
