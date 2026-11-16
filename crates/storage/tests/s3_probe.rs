//! What our S3 code relies on, probed against a real provider (R4.8, spec 03 §2 and §10, D119).
//!
//! Every probe is `#[ignore]`d and skips (prints and returns) unless `CETRA_TEST_S3_ENDPOINT` is
//! set. Run them against a provider with:
//!
//! ```text
//! cargo test --release -p cetra-storage --all-features --test s3_probe -- --ignored --nocapture
//! ```
//!
//! Environment: `CETRA_TEST_S3_ENDPOINT`, `CETRA_TEST_S3_BUCKET`, `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, `CETRA_TEST_S3_REGION` (default `us-east-1`),
//! `CETRA_TEST_S3_PATH_STYLE` (default `true`; `false` for Hetzner). `CETRA_TEST_S3_EXPECT_LIFECYCLE=1`
//! makes `lifecycle_rule_present` assert that the abort rule exists.
//!
//! Each probe works under a prefix `probe-<ms>/` and deletes it at the end. Each prints one line,
//! `probe <name>: <result>`, for `docs/research/storage/hardening.md`. A probe asserts what our code
//! relies on and only prints what is information. `leave_an_incomplete_upload` is the exception: it
//! leaves an upload open on purpose, under `lifecycle-probe/`, for the two-day check of R4.10.

// The probe lines are printed for `hardening.md`; read them with `--nocapture`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::collections::BTreeSet;
use std::fmt::Display;
use std::io::Write;
use std::process::{Command, Stdio};

use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{Error, ObjectStore, ObjectStoreExt, PutMode, PutOptions};

const MIB: usize = 1024 * 1024;

/// The provider and the prefix one probe works under.
struct Probe {
    endpoint: String,
    bucket: String,
    region: String,
    path_style: bool,
    store: AmazonS3,
    prefix: String,
}

impl Probe {
    /// `None`, after a line saying so, when `CETRA_TEST_S3_ENDPOINT` is unset.
    fn from_env() -> Option<Self> {
        let Ok(endpoint) = std::env::var("CETRA_TEST_S3_ENDPOINT") else {
            println!("skipped: CETRA_TEST_S3_ENDPOINT is not set");
            return None;
        };
        let bucket = std::env::var("CETRA_TEST_S3_BUCKET").expect("CETRA_TEST_S3_BUCKET");
        let region =
            std::env::var("CETRA_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
        let path_style = !matches!(
            std::env::var("CETRA_TEST_S3_PATH_STYLE").as_deref(),
            Ok("false")
        );
        let store = AmazonS3Builder::from_env()
            .with_bucket_name(&bucket)
            .with_endpoint(&endpoint)
            .with_region(&region)
            .with_virtual_hosted_style_request(!path_style)
            .with_allow_http(endpoint.starts_with("http://"))
            .build()
            .expect("an S3 client for the probe");
        Some(Self {
            endpoint,
            bucket,
            region,
            path_style,
            store,
            prefix: format!("probe-{}", now_ms()),
        })
    }

    fn key(&self, name: &str) -> Path {
        Path::from(format!("{}/{name}", self.prefix))
    }

    /// Deletes the probe's objects, then panics if the probe failed or the deletion did.
    async fn finish(&self, outcome: Result<(), String>) {
        let deleted = delete_under(&self.store, &self.prefix).await;
        outcome.expect("probe failed");
        deleted.expect("delete the probe's objects");
    }

    /// The multipart uploads under `prefix`, as `ListMultipartUploads` reports them: (key, upload id).
    fn incomplete_uploads(&self, prefix: &str) -> Result<Vec<(String, String)>, String> {
        let query = format!("prefix={}&uploads", prefix.replace('/', "%2F"));
        let (status, body) = self.bucket_get(&query)?;
        if status != 200 {
            return Err(format!("ListMultipartUploads answered {status}"));
        }
        Ok(body
            .split("<Upload>")
            .skip(1)
            .filter_map(|block| {
                Some((
                    xml_text(block, "Key")?.to_owned(),
                    xml_text(block, "UploadId")?.to_owned(),
                ))
            })
            .collect())
    }

    /// A signed `GET` of the bucket with `query`, through curl's built-in SigV4 signing:
    /// `object_store` has no call for the bucket's subresources. The credentials go to curl on stdin
    /// (`-K -`), never on a command line. The query's parameters must be in sorted order, which is
    /// the order curl signs. Returns the HTTP status and the body.
    fn bucket_get(&self, query: &str) -> Result<(u16, String), String> {
        let endpoint = self.endpoint.trim_end_matches('/');
        let url = if self.path_style {
            format!("{endpoint}/{}?{query}", self.bucket)
        } else {
            let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
            format!("{scheme}://{}.{host}/?{query}", self.bucket)
        };
        let key = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| "AWS_ACCESS_KEY_ID is not set")?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| "AWS_SECRET_ACCESS_KEY is not set")?;
        let mut child = Command::new("curl")
            .args(["-s", "-K", "-", "--aws-sigv4"])
            .arg(format!("aws:amz:{}:s3", self.region))
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
        let status = status
            .trim()
            .parse::<u16>()
            .map_err(|_| "curl printed no HTTP status".to_owned())?;
        Ok((status, body.to_owned()))
    }
}

/// The `user = "..."` value of a curl config line: `\` and `"` are escaped, as curl reads quoted
/// strings with backslash escapes.
fn curl_credentials(key: &str, secret: &str) -> String {
    format!("{key}:{secret}")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// `read_after_write`: a fresh key is readable at once, with its bytes and its size. Flush then query
/// and `verify_file`'s `head` rely on it (spec 03 §10).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn read_after_write() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = read_after_write_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn read_after_write_checks(probe: &Probe) -> Result<(), String> {
    for index in 0..100 {
        let path = probe.key(&format!("read-{index:03}"));
        let body = Bytes::from(format!("read after write {index}"));
        probe
            .store
            .put(&path, body.clone().into())
            .await
            .map_err(|error| format!("put {index}: {error}"))?;
        let read = probe
            .store
            .get(&path)
            .await
            .map_err(|error| format!("get {index}: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("read the body {index}: {error}"))?;
        if read != body {
            return Err(format!("get {index} returned other bytes than were put"));
        }
        let meta = probe
            .store
            .head(&path)
            .await
            .map_err(|error| format!("head {index}: {error}"))?;
        if meta.size != u64::try_from(body.len()).expect("a body length fits in u64") {
            return Err(format!("head {index} reported size {}", meta.size));
        }
    }
    report("read_after_write", "100 of 100 fresh keys read back");
    Ok(())
}

/// `delete_missing_is_ok`: GC deletes idempotently, so a key that was never written is no error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn delete_missing_is_ok() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let deleted = probe.store.delete(&probe.key("never-written")).await;
    report("delete_missing_is_ok", answer(&deleted));
    probe
        .finish(deleted.map_err(|error| format!("delete of a missing key: {error}")))
        .await;
}

/// `head_after_delete_is_not_found`: GC's `delete_len` and verification's missing-object rule
/// (spec 03 §10) rely on `NotFound` after a delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn head_after_delete_is_not_found() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = head_after_delete_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn head_after_delete_checks(probe: &Probe) -> Result<(), String> {
    let path = probe.key("deleted");
    probe
        .store
        .put(&path, Bytes::from_static(b"gone soon").into())
        .await
        .map_err(|error| format!("put: {error}"))?;
    probe
        .store
        .delete(&path)
        .await
        .map_err(|error| format!("delete: {error}"))?;
    match probe.store.head(&path).await {
        Err(Error::NotFound { .. }) => {
            report("head_after_delete_is_not_found", "NotFound");
            Ok(())
        }
        other => Err(format!("head after delete answered {}", answer(&other))),
    }
}

/// `list_after_write`: a listing shows every key written under a prefix, each once. N's
/// `collect_orphans` and the audit rely on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn list_after_write() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = list_after_write_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn list_after_write_checks(probe: &Probe) -> Result<(), String> {
    let directory = probe.key("listed");
    for index in 0..50 {
        probe
            .store
            .put(
                &probe.key(&format!("listed/key-{index:02}")),
                Bytes::from_static(b"listed").into(),
            )
            .await
            .map_err(|error| format!("put {index}: {error}"))?;
    }
    let listed: Vec<Path> = probe
        .store
        .list(Some(&directory))
        .map_ok(|meta| meta.location)
        .try_collect()
        .await
        .map_err(|error| format!("list: {error}"))?;
    let unique: BTreeSet<&Path> = listed.iter().collect();
    if listed.len() != 50 || unique.len() != 50 {
        return Err(format!(
            "a listing of 50 keys showed {} entries, {} distinct",
            listed.len(),
            unique.len()
        ));
    }
    report("list_after_write", "50 of 50 keys listed, each once");
    Ok(())
}

/// `abort_after_complete_keeps_the_object`: an abort after `complete` does not remove the object.
/// R4.3's fix relies on it: a failed `complete` is aborted, and the object it may have created stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn abort_after_complete_keeps_the_object() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = abort_after_complete_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn abort_after_complete_checks(probe: &Probe) -> Result<(), String> {
    let path = probe.key("complete-then-abort");
    let mut upload = probe
        .store
        .put_multipart(&path)
        .await
        .map_err(|error| format!("start: {error}"))?;
    let part = Bytes::from(vec![7_u8; 5 * MIB]);
    for _ in 0..2 {
        upload
            .put_part(part.clone().into())
            .await
            .map_err(|error| format!("put_part: {error}"))?;
    }
    upload
        .put_part(Bytes::from(vec![8_u8; 1024]).into())
        .await
        .map_err(|error| format!("put_part of the last part: {error}"))?;
    upload
        .complete()
        .await
        .map_err(|error| format!("complete: {error}"))?;
    let aborted = upload.abort().await;
    report(
        "abort_after_complete_keeps_the_object",
        format!("abort answered {}", answer(&aborted)),
    );
    let meta = probe
        .store
        .head(&path)
        .await
        .map_err(|error| format!("the object is gone after an abort: {error}"))?;
    let expected = u64::try_from(10 * MIB + 1024).expect("a size fits in u64");
    if meta.size != expected {
        return Err(format!(
            "the object has size {}, expected {expected}",
            meta.size
        ));
    }
    Ok(())
}

/// `multipart_part_sizes`: parts of 16 MiB (our size) complete. The probe also sends a 4 MiB part
/// that is not the last one and prints whether the provider refuses it (`EntityTooSmall`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn multipart_part_sizes() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = multipart_part_size_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn multipart_part_size_checks(probe: &Probe) -> Result<(), String> {
    let ours = probe.key("ours-16mib");
    let mut upload = probe
        .store
        .put_multipart(&ours)
        .await
        .map_err(|error| format!("start: {error}"))?;
    let part = Bytes::from(vec![5_u8; 16 * MIB]);
    for _ in 0..2 {
        upload
            .put_part(part.clone().into())
            .await
            .map_err(|error| format!("put_part of 16 MiB: {error}"))?;
    }
    upload
        .complete()
        .await
        .map_err(|error| format!("complete of 16 MiB parts: {error}"))?;

    let short = probe.key("short-part");
    let mut upload = probe
        .store
        .put_multipart(&short)
        .await
        .map_err(|error| format!("start: {error}"))?;
    let refused = async {
        upload
            .put_part(Bytes::from(vec![4_u8; 4 * MIB]).into())
            .await?;
        upload.put_part(Bytes::from(vec![6_u8; MIB]).into()).await?;
        upload.complete().await
    }
    .await;
    match refused {
        Ok(_) => report(
            "multipart_part_sizes",
            "16 MiB parts completed; a 4 MiB non-last part was accepted",
        ),
        Err(error) => {
            report(
                "multipart_part_sizes",
                format!("16 MiB parts completed; a 4 MiB non-last part was refused: {error}"),
            );
            // The upload was not completed: abort it so it does not stay on the bucket.
            let _ = upload.abort().await;
        }
    }
    Ok(())
}

/// `multipart_part_count`: 40 parts of 5 MiB (above our 32 parts of 16 MiB for a 512 MiB file)
/// complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn multipart_part_count() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = multipart_part_count_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn multipart_part_count_checks(probe: &Probe) -> Result<(), String> {
    let path = probe.key("forty-parts");
    let mut upload = probe
        .store
        .put_multipart(&path)
        .await
        .map_err(|error| format!("start: {error}"))?;
    let part = Bytes::from(vec![3_u8; 5 * MIB]);
    for index in 0..40 {
        upload
            .put_part(part.clone().into())
            .await
            .map_err(|error| format!("put_part {index}: {error}"))?;
    }
    upload
        .complete()
        .await
        .map_err(|error| format!("complete of 40 parts: {error}"))?;
    report("multipart_part_count", "40 of 40 parts completed");
    Ok(())
}

/// `conditional_put`: information for a future multi-node catalog (spec 03 §11). Storage does not use
/// `PutMode::Create` today, so the probe only prints what the provider answers to a second create.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn conditional_put() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let path = probe.key("conditional");
    let options = PutOptions {
        mode: PutMode::Create,
        ..PutOptions::default()
    };
    let first = probe
        .store
        .put_opts(&path, Bytes::from_static(b"first").into(), options.clone())
        .await;
    let second = probe
        .store
        .put_opts(&path, Bytes::from_static(b"second").into(), options)
        .await;
    report(
        "conditional_put",
        format!(
            "first create {}, second create {}",
            answer(&first),
            answer(&second)
        ),
    );
    probe.finish(Ok(())).await;
}

/// `incomplete_upload_is_listed`: an upload that is started and not completed is listed by
/// `ListMultipartUploads`, and gone once aborted. The lifecycle rule relies on the listing being real.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn incomplete_upload_is_listed() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let outcome = incomplete_upload_checks(&probe).await;
    probe.finish(outcome).await;
}

async fn incomplete_upload_checks(probe: &Probe) -> Result<(), String> {
    let path = probe.key("incomplete");
    let key = path.to_string();
    let mut upload = probe
        .store
        .put_multipart(&path)
        .await
        .map_err(|error| format!("start: {error}"))?;
    upload
        .put_part(Bytes::from(vec![1_u8; 5 * MIB]).into())
        .await
        .map_err(|error| format!("put_part: {error}"))?;
    let listed = probe.incomplete_uploads(&probe.prefix)?;
    if !listed.iter().any(|(listed_key, _)| *listed_key == key) {
        return Err(format!(
            "the open upload is not listed: {} entries",
            listed.len()
        ));
    }
    upload
        .abort()
        .await
        .map_err(|error| format!("abort: {error}"))?;
    let after = probe.incomplete_uploads(&probe.prefix)?;
    if after.iter().any(|(listed_key, _)| *listed_key == key) {
        return Err("the aborted upload is still listed".to_owned());
    }
    report(
        "incomplete_upload_is_listed",
        "listed while open, gone once aborted",
    );
    Ok(())
}

/// `lifecycle_rule_present`: prints the bucket's lifecycle configuration. With
/// `CETRA_TEST_S3_EXPECT_LIFECYCLE=1` it asserts the abort rule exists (`docs/self-hosting/s3.md`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs CETRA_TEST_S3_ENDPOINT; see the module docs"]
async fn lifecycle_rule_present() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let (status, body) = probe
        .bucket_get("lifecycle")
        .expect("GET ?lifecycle through curl");
    let has_rule = body.contains("<AbortIncompleteMultipartUpload>");
    report(
        "lifecycle_rule_present",
        format!("GET ?lifecycle answered {status}: {}", body.trim()),
    );
    if std::env::var("CETRA_TEST_S3_EXPECT_LIFECYCLE").is_ok_and(|value| value == "1") {
        assert!(
            has_rule,
            "the bucket has no AbortIncompleteMultipartUpload rule (answered {status})"
        );
    }
}

/// `leave_an_incomplete_upload` (R4.8, run by name): starts one upload under `lifecycle-probe/`,
/// leaves it open on purpose and prints its upload id. R4.10 checks two days later that the listing
/// under that prefix is empty, which tests the one-day abort rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "leaves an upload open on purpose for R4.10's two-day check; run it by name"]
async fn leave_an_incomplete_upload() {
    let Some(probe) = Probe::from_env() else {
        return;
    };
    let path = Path::from(format!("lifecycle-probe/{}", now_ms()));
    let key = path.to_string();
    let mut upload = probe
        .store
        .put_multipart(&path)
        .await
        .expect("start the lifecycle-probe upload");
    upload
        .put_part(Bytes::from(vec![2_u8; 5 * MIB]).into())
        .await
        .expect("put one part of the lifecycle-probe upload");
    // Dropped without `complete` or `abort`: the upload stays open on the bucket.
    drop(upload);
    let listed = probe
        .incomplete_uploads("lifecycle-probe/")
        .expect("list the lifecycle-probe uploads");
    let id = listed
        .iter()
        .find(|(listed_key, _)| *listed_key == key)
        .map_or("not listed".to_owned(), |(_, id)| id.clone());
    report(
        "leave_an_incomplete_upload",
        format!("left open under lifecycle-probe/, upload id {id}"),
    );
}

/// Deletes every object under `prefix`, as `tests/s3.rs` does.
async fn delete_under(store: &AmazonS3, prefix: &str) -> Result<(), String> {
    let root = Path::from(prefix);
    let locations: Vec<Path> = store
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

/// The text of the first `<name>…</name>` in `block`.
fn xml_text<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&format!("</{name}>"))? + start;
    Some(&block[start..end])
}

/// What an `object_store` call answered, for a probe line.
fn answer<T>(result: &object_store::Result<T>) -> String {
    match result {
        Ok(_) => "Ok".to_owned(),
        Err(error) => format!("error: {error}"),
    }
}

fn report(name: &str, result: impl Display) {
    println!("probe {name}: {result}");
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
