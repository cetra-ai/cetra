//! The end-to-end test (D-G-20): the SDK conformance fixtures go through a real
//! server over OTLP/HTTP protobuf, OTLP/HTTP JSON and OTLP/gRPC, and every trace
//! is read back through the API. The three read-backs must agree, and each
//! fixture's read-back is snapshotted. A second test covers the live streams and
//! every read route over real HTTP.

// A failed setup step panics with its own message, as in the other integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use cetra_core::ids::TraceId;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, trace_service_client::TraceServiceClient,
};
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message;
use serde_json::{Value, json};

const ORIGIN: &str = "http://127.0.0.1:8080";
const ADMIN_EMAIL: &str = "ada@example.com";
const ADMIN_PASSWORD: &str = "cetra-e2e-1";
const INIT_KEY: &str = "sk-cetra-0123456789ABCDEFGHIJKLMNOPQRSTUV";
/// Fields that differ between runs or between projects, replaced at any depth.
const REDACTED_KEYS: &[&str] = &["ingest_time", "project_id", "score_id"];

/// A session-authenticated API client.
struct Api {
    client: reqwest::Client,
    base: String,
    cookie: String,
}

impl Api {
    async fn get(&self, path: &str) -> Value {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        read_json(response).await
    }

    /// A cookie-authenticated `GET` whose response the caller reads itself, such as a stream.
    async fn get_raw(&self, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut request = self
            .client
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap()
    }

    async fn post(&self, path: &str, body: &Value) -> Value {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("origin", ORIGIN)
            .json(body)
            .send()
            .await
            .unwrap();
        read_json(response).await
    }

    /// A `PATCH` with the same cookie and origin as `post`.
    async fn patch(&self, path: &str, body: &Value) -> Value {
        let response = self
            .client
            .patch(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("origin", ORIGIN)
            .json(body)
            .send()
            .await
            .unwrap();
        read_json(response).await
    }

    /// A `PUT` with the same cookie and origin as `post`.
    async fn put(&self, path: &str, body: &Value) -> Value {
        let response = self
            .client
            .put(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("origin", ORIGIN)
            .json(body)
            .send()
            .await
            .unwrap();
        read_json(response).await
    }

    /// A `DELETE` with the same cookie and origin as `post`.
    async fn delete(&self, path: &str) -> Value {
        let response = self
            .client
            .delete(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("origin", ORIGIN)
            .send()
            .await
            .unwrap();
        read_json(response).await
    }

    /// The status of a cookie-authenticated `GET`, without asserting success.
    async fn get_status(&self, path: &str) -> u16 {
        self.client
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

async fn read_json(response: reqwest::Response) -> Value {
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Reads `response` until its text contains `needle`, within `wait`. Returns the text read.
async fn read_until(response: &mut reqwest::Response, needle: &str, wait: Duration) -> String {
    let deadline = tokio::time::Instant::now() + wait;
    let mut text = String::new();
    while !text.contains(needle) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let chunk = tokio::time::timeout(remaining, response.chunk())
            .await
            .unwrap_or_else(|_| panic!("no {needle:?} within {wait:?}; read: {text}"))
            .unwrap()
            .unwrap_or_else(|| panic!("the stream ended before {needle:?}; read: {text}"));
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}

/// A running server with the admin signed in.
struct Server {
    running: cetra_server::serve::Running,
    http: String,
    otlp: String,
    grpc: String,
    client: reqwest::Client,
    api: Api,
    /// The `Proto` project, which `INIT_KEY` ingests into.
    proto_id: String,
}

/// Writes the config plus `extra_config` (after the `[init]` table) into `dir`, starts the
/// server with the embedded UI and signs in.
async fn start(dir: &Path, extra_config: &str) -> Server {
    let config_path = dir.join("cetra.toml");
    let data_dir = dir.join("data");
    fs::write(
        &config_path,
        format!(
            "data_dir = {:?}\nhttp_addr = \"127.0.0.1:0\"\notlp_http_addr = \"127.0.0.1:0\"\ngrpc_addr = \"127.0.0.1:0\"\npublic_url = \"{ORIGIN}\"\n\n[init]\nadmin_email = \"{ADMIN_EMAIL}\"\nadmin_password = \"{ADMIN_PASSWORD}\"\norg = \"Acme\"\nproject = \"Proto\"\nsecret_key = \"{INIT_KEY}\"\n{extra_config}",
            data_dir.display().to_string(),
        ),
    )
    .unwrap();
    let config = cetra_server::config::ServerConfig::load(Some(&config_path)).unwrap();
    let running = cetra_server::serve::bind(config, Some(cetra_ui::router()))
        .await
        .unwrap();
    let http = format!("http://{}", running.local_addr);
    let otlp = format!("http://{}", running.otlp_local_addr.unwrap());
    let grpc = format!("http://{}", running.grpc_local_addr.unwrap());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Sign in. Every later request carries the session cookie.
    let login = client
        .post(format!("{http}/api/auth/login"))
        .header("origin", ORIGIN)
        .json(&json!({"email": ADMIN_EMAIL, "password": ADMIN_PASSWORD}))
        .send()
        .await
        .unwrap();
    assert!(login.status().is_success(), "login: {}", login.status());
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let api = Api {
        client: client.clone(),
        base: http.clone(),
        cookie,
    };

    let listed = api.get("/api/v1/orgs/acme/projects").await;
    let proto_id = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["name"] == "Proto")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    Server {
        running,
        http,
        otlp,
        grpc,
        client,
        api,
        proto_id,
    }
}

/// A project under test: its id, and the bearer key that ingests into it.
struct Project {
    label: &'static str,
    id: String,
    key: String,
}

/// The `fixtures/otlp/cetra` directory.
fn fixtures_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/otlp/cetra"
    ))
}

/// The fixture directories whose names start with `sdk-`, sorted.
fn sdk_fixtures() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(fixtures_root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("sdk-"))
        })
        .collect();
    dirs.sort();
    dirs
}

/// The fixture `name`'s `input.json`, with every span's trace id rewritten to `trace`
/// (32 hex digits), so each trace is the test's own.
fn fixture_request(name: &str, trace: &str) -> ExportTraceServiceRequest {
    let bytes = fs::read(fixtures_root().join(name).join("input.json")).unwrap();
    let mut request: ExportTraceServiceRequest = serde_json::from_slice(&bytes).unwrap();
    let trace_id = TraceId::from_str(trace).unwrap().as_bytes().to_vec();
    for resource in &mut request.resource_spans {
        for scope in &mut resource.scope_spans {
            for span in &mut scope.spans {
                span.trace_id = trace_id.clone();
            }
        }
    }
    request
}

/// Sixteen bytes: fifteen zeros, then `index + 1`. The fixtures all use trace `…01`,
/// so each fixture gets its own trace.
fn trace_id_for(index: usize) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[15] = u8::try_from(index + 1).unwrap();
    id
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if REDACTED_KEYS.contains(&key.as_str()) {
                    *child = json!("[redacted]");
                } else {
                    redact(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

async fn post_otlp_http(
    client: &reqwest::Client,
    url: String,
    key: &str,
    content_type: &str,
    body: Vec<u8>,
) {
    let response = client
        .post(url)
        .header("content-type", content_type)
        .header("authorization", format!("Bearer {key}"))
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert!(status.is_success(), "OTLP/HTTP export: {status}");
}

/// Exports `request` over OTLP/HTTP with `INIT_KEY` and returns the status, without asserting it.
async fn otlp_status(server: &Server, request: &ExportTraceServiceRequest) -> u16 {
    server
        .client
        .post(format!("{}/v1/traces", server.http))
        .header("authorization", format!("Bearer {INIT_KEY}"))
        .header("content-type", "application/x-protobuf")
        .body(request.encode_to_vec())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Reads `path` until `done` holds, every 100 ms, for at most `wait`. Returns the body.
async fn poll_json(api: &Api, path: &str, done: impl Fn(&Value) -> bool, wait: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let body = api.get(path).await;
        if done(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{path} did not settle within {wait:?}: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_sdk_fixture_round_trips_over_every_transport() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "").await;

    if std::env::var_os("E2E_REQUIRE_UI").is_some() {
        assert!(
            cetra_ui::embedded(),
            "E2E_REQUIRE_UI is set, but web/dist was not embedded: build the web first"
        );
    }

    // The UI and unknown API paths.
    let root = server
        .client
        .get(format!("{}/", server.http))
        .send()
        .await
        .unwrap();
    if cetra_ui::embedded() {
        assert_eq!(root.status(), 200);
        let content_type = root.headers()["content-type"].to_str().unwrap().to_owned();
        assert!(content_type.starts_with("text/html"), "{content_type}");
        // An app deep link whose last segment has a dot (F1).
        let deep = server
            .client
            .get(format!(
                "{}/acme/proto/users/alice%40example.com",
                server.http
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(deep.status(), 200);
        let content_type = deep.headers()["content-type"].to_str().unwrap().to_owned();
        assert!(content_type.starts_with("text/html"), "{content_type}");
    } else {
        assert_eq!(root.status(), 503);
    }
    let missing = server
        .client
        .get(format!("{}/api/v1/nope", server.http))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let content_type = missing.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.starts_with("application/problem+json"),
        "{content_type}"
    );

    // Three projects, one per transport. The first is the init project.
    let mut projects = vec![Project {
        label: "protobuf",
        id: server.proto_id.clone(),
        key: INIT_KEY.to_owned(),
    }];
    for (label, name) in [("json", "Json"), ("grpc", "Grpc")] {
        let created = server
            .api
            .post("/api/v1/orgs/acme/projects", &json!({"name": name}))
            .await;
        let id = created["id"].as_str().unwrap().to_owned();
        let key_response = server
            .api
            .post(
                &format!("/api/v1/projects/{id}/api-keys"),
                &json!({"kind": "secret", "scopes": ["ingest", "read"]}),
            )
            .await;
        let key = key_response["secret"].as_str().unwrap().to_owned();
        projects.push(Project { label, id, key });
    }

    // Send every fixture to every project, each over its own transport.
    let fixtures = sdk_fixtures();
    let names: Vec<String> = fixtures
        .iter()
        .map(|dir| dir.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    let requests: Vec<ExportTraceServiceRequest> = fixtures
        .iter()
        .enumerate()
        .map(|(index, dir)| {
            let bytes = fs::read(dir.join("input.json")).unwrap();
            let mut request: ExportTraceServiceRequest = serde_json::from_slice(&bytes).unwrap();
            let trace_id = trace_id_for(index);
            for resource in &mut request.resource_spans {
                for scope in &mut resource.scope_spans {
                    for span in &mut scope.spans {
                        span.trace_id = trace_id.clone();
                    }
                }
            }
            request
        })
        .collect();

    let mut grpc_client = TraceServiceClient::connect(server.grpc.clone())
        .await
        .unwrap();
    for request in &requests {
        let proto = &projects[0];
        post_otlp_http(
            &server.client,
            format!("{}/v1/traces", server.http),
            &proto.key,
            "application/x-protobuf",
            request.encode_to_vec(),
        )
        .await;

        let json = &projects[1];
        post_otlp_http(
            &server.client,
            format!("{}/v1/traces", server.otlp),
            &json.key,
            "application/json",
            serde_json::to_vec(request).unwrap(),
        )
        .await;

        let grpc_project = &projects[2];
        let mut grpc_request = tonic::Request::new(request.clone());
        grpc_request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", grpc_project.key)
                .parse()
                .expect("ASCII token"),
        );
        grpc_client.export(grpc_request).await.unwrap();
    }
    drop(grpc_client);

    // Read every trace back through the API, then compare the three projects.
    for (index, name) in names.iter().enumerate() {
        let trace_id = format!("{:032x}", index + 1);
        let mut read_backs = Vec::new();
        for project in &projects {
            let listing = server
                .api
                .get(&format!(
                    "/api/v1/projects/{}/traces?include_eval=true&from=2025-12-31T00:00:00Z&to=2026-01-02T00:00:00Z",
                    project.id
                ))
                .await;
            assert!(
                listing["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|trace| trace["trace_id"] == trace_id),
                "{name}: {} project does not list trace {trace_id}",
                project.label
            );

            let detail = server
                .api
                .get(&format!(
                    "/api/v1/projects/{}/traces/{trace_id}",
                    project.id
                ))
                .await;
            let mut spans = Vec::new();
            for item in detail["spans"].as_array().unwrap() {
                let span_id = item["span"]["span_id"].as_str().unwrap();
                spans.push(
                    server
                        .api
                        .get(&format!(
                            "/api/v1/projects/{}/traces/{trace_id}/spans/{span_id}",
                            project.id
                        ))
                        .await,
                );
            }
            let mut value = json!({"trace": detail, "spans": spans});
            redact(&mut value);
            read_backs.push(value);
        }

        let protobuf = &read_backs[0];
        assert_eq!(
            &read_backs[1], protobuf,
            "{name}: OTLP/JSON read-back differs from protobuf"
        );
        assert_eq!(
            &read_backs[2], protobuf,
            "{name}: OTLP/gRPC read-back differs from protobuf"
        );
        insta::assert_json_snapshot!(name.as_str(), protobuf);
    }

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn live_streams_and_every_read_route_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    // The media fixture's blob is 70 bytes; the default `media.min_bytes` (4 KiB) would keep
    // it inline, and `GET /media/…` below would be a 404.
    let server = start(dir.path(), "\n[media]\nmin_bytes = \"1B\"\n").await;
    let project = format!("/api/v1/projects/{}", server.proto_id);
    let context_trace = format!("{:032x}", 1);
    let media_trace = format!("{:032x}", 2);

    // The streams open before anything is sent, so the events below must be live.
    let mut traces = server
        .api
        .get_raw(&format!("{project}/traces/stream"), &[])
        .await;
    assert_eq!(traces.status(), 200);
    let content_type = traces.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let mut spans = server
        .api
        .get_raw(&format!("{project}/traces/{context_trace}/stream"), &[])
        .await;
    assert_eq!(spans.status(), 200);
    // A `Last-Event-ID` gets a `resync` first, so the client refetches.
    let mut resumed = server
        .api
        .get_raw(
            &format!("{project}/traces/stream"),
            &[("last-event-id", "1")],
        )
        .await;
    assert_eq!(resumed.status(), 200);
    read_until(&mut resumed, "event: resync", Duration::from_secs(5)).await;
    drop(resumed);

    // The fixture with the session and user, and the one with the media blob.
    for (name, trace) in [
        ("sdk-context", &context_trace),
        ("sdk-media-bytes", &media_trace),
    ] {
        let request = fixture_request(name, trace);
        post_otlp_http(
            &server.client,
            format!("{}/v1/traces", server.http),
            INIT_KEY,
            "application/x-protobuf",
            request.encode_to_vec(),
        )
        .await;
    }

    let span_events = read_until(&mut spans, "event: span", Duration::from_secs(10)).await;
    assert!(
        span_events.contains(&context_trace),
        "no span event for {context_trace}: {span_events}"
    );
    let trace_events = read_until(&mut traces, &context_trace, Duration::from_secs(10)).await;
    assert!(
        trace_events.contains("event: trace"),
        "no trace event for {context_trace}: {trace_events}"
    );
    drop(spans);
    drop(traces);

    // A score write with the init key, then one read of every remaining route.
    let score = server
        .client
        .post(format!("{}/api/v1/scores", server.http))
        .header("authorization", format!("Bearer {INIT_KEY}"))
        .json(&json!({
            "scores": [{
                "target": {"type": "trace", "trace_id": context_trace.clone()},
                "name": "e2e",
                "value": {"data_type": "numeric", "value": 1.0}
            }]
        }))
        .send()
        .await
        .unwrap();
    let status = score.status();
    let text = score.text().await.unwrap();
    assert_eq!(status, 200, "{status}: {text}");

    let window = "from=2025-12-31T00:00:00Z&to=2026-01-02T00:00:00Z";
    let sessions = server
        .api
        .get(&format!("{project}/sessions?{window}"))
        .await;
    assert!(
        sessions["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["session_id"] == "chat-42"),
        "{sessions}"
    );
    server.api.get(&format!("{project}/sessions/chat-42")).await;
    let users = server.api.get(&format!("{project}/users?{window}")).await;
    assert!(
        users["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|user| user["user_id"] == "u-17"),
        "{users}"
    );
    server.api.get(&format!("{project}/users/u-17")).await;
    let scores = server
        .api
        .get(&format!("{project}/scores?trace_id={context_trace}"))
        .await;
    assert!(
        scores["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|score| score["name"] == "e2e"),
        "{scores}"
    );
    let total = server
        .api
        .post(
            &format!("{project}/dashboards/query"),
            &json!({
                "from": "2025-12-31T00:00:00Z",
                "to": "2026-01-02T00:00:00Z",
                "shape": "total",
                "query": {"source": "traces", "measures": [{"agg": "count"}]},
            }),
        )
        .await;
    assert!(total["rows"][0]["values"][0].as_f64().is_some(), "{total}");
    server.api.get(&format!("{project}/schema")).await;

    let media = server
        .api
        .get_raw(
            &format!(
                "{project}/media/497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581"
            ),
            &[],
        )
        .await;
    assert_eq!(media.status(), 200);
    let bytes = media.bytes().await.unwrap();
    assert_eq!(bytes.len(), 70);

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
}

/// The span id of the application span that the Agent SDK case puts over the Claude Code turn.
const APP_SPAN_HEX: &str = "a000000000000001";
const APP_SPAN_BYTES: [u8; 8] = [0xa0, 0, 0, 0, 0, 0, 0, 1];

/// The Claude Code bash-tool export (`fixtures/otlp/claude_code/bash-tool`) as session
/// `e2e-session`: every span on `trace`, moved so the earliest starts at `start_ns`, and the
/// interaction span under `parent` when one is given.
fn claude_code_request(
    trace: &str,
    start_ns: i64,
    parent: Option<&[u8]>,
) -> ExportTraceServiceRequest {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/otlp/claude_code/bash-tool/input.json"
    );
    let text = fs::read_to_string(path)
        .unwrap()
        .replace("capture-volatile-2", "e2e-session");
    let mut request: ExportTraceServiceRequest = serde_json::from_str(&text).unwrap();
    let trace_id = TraceId::from_str(trace).unwrap().as_bytes().to_vec();
    let mut spans: Vec<&mut Span> = request
        .resource_spans
        .iter_mut()
        .flat_map(|resource| resource.scope_spans.iter_mut())
        .flat_map(|scope| scope.spans.iter_mut())
        .collect();
    let first = spans
        .iter()
        .map(|span| span.start_time_unix_nano)
        .min()
        .unwrap();
    let start = u64::try_from(start_ns).unwrap();
    for span in &mut spans {
        span.trace_id = trace_id.clone();
        span.start_time_unix_nano = span.start_time_unix_nano - first + start;
        span.end_time_unix_nano = span.end_time_unix_nano - first + start;
        if let (Some(parent), "claude_code.interaction") = (parent, span.name.as_str()) {
            span.parent_span_id = parent.to_vec();
        }
    }
    request
}

/// One application span `app-request` (resource `service.name = e2e-app`) on `trace`, from
/// `start_ns` to `end_ns`, with the span id `APP_SPAN_HEX`.
fn app_request(trace: &str, start_ns: i64, end_ns: i64) -> ExportTraceServiceRequest {
    serde_json::from_value(json!({
        "resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "e2e-app"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": trace,
                "spanId": APP_SPAN_HEX,
                "name": "app-request",
                "startTimeUnixNano": start_ns.to_string(),
                "endTimeUnixNano": end_ns.to_string(),
            }]}],
        }]
    }))
    .unwrap()
}

/// `ns` nanoseconds since the Unix epoch as RFC 3339 in UTC, the form the query parameters take.
fn rfc3339(ns: i64) -> String {
    chrono::DateTime::from_timestamp_nanos(ns).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Coding agents end to end (spec 02 § Coding agents, spec 05 § Dashboard queries): an agent
/// context, a captured Claude Code export, and the same export under an application span, as
/// the Agent SDK sends it. Both traces get the context's person, repository, branch and agent,
/// and the "Coding agents" template's repository and session breakdowns group them.
#[tokio::test(flavor = "multi_thread")]
async fn coding_agents_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "").await;
    let project = format!("/api/v1/projects/{}", server.proto_id);

    // The hook's context for the session: a `.git` URL, a mixed-case person and a branch.
    let context = server
        .client
        .post(format!("{}/v1/agent-context", server.http))
        .header("authorization", format!("Bearer {INIT_KEY}"))
        .json(&json!({
            "agent": "claude-code",
            "session_id": "e2e-session",
            "repository": "https://github.com/acme/widgets.git",
            "branch": "feature/e2e",
            "user": "Dev@Example.com"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(context.status(), 204);

    // Both turns start 60 seconds ago. The application span covers the Claude Code turn.
    let base = now_ns() - 60 * NANOS_PER_SECOND;
    let plain_trace = format!("{:032x}", 21);
    let sdk_trace = format!("{:032x}", 22);
    post_otlp_http(
        &server.client,
        format!("{}/v1/traces", server.http),
        INIT_KEY,
        "application/x-protobuf",
        claude_code_request(&plain_trace, base, None).encode_to_vec(),
    )
    .await;
    let mut sdk = app_request(&sdk_trace, base, base + 2 * NANOS_PER_SECOND);
    let turn = claude_code_request(&sdk_trace, base, Some(&APP_SPAN_BYTES));
    sdk.resource_spans.extend(turn.resource_spans);
    post_otlp_http(
        &server.client,
        format!("{}/v1/traces", server.http),
        INIT_KEY,
        "application/x-protobuf",
        sdk.encode_to_vec(),
    )
    .await;

    // The range covers both turns and the dashboard queries below.
    let from = rfc3339(base - 3_600 * NANOS_PER_SECOND);
    let to = rfc3339(now_ns() + 3_600 * NANOS_PER_SECOND);

    // Both traces are listed for the session. `%20%3D%20%27` is ` = '` in the filter.
    let listed = poll_json(
        &server.api,
        &format!(
            "{project}/traces?filter=session_id%20%3D%20%27e2e-session%27&from={from}&to={to}"
        ),
        |body| body["data"].as_array().is_some_and(|rows| rows.len() == 2),
        Duration::from_secs(20),
    )
    .await;
    let mut ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["trace_id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [plain_trace.as_str(), sdk_trace.as_str()], "{listed}");
    for row in listed["data"].as_array().unwrap() {
        assert_eq!(row["user_id"], "dev@example.com", "{row}");
        assert_eq!(row["session_id"], "e2e-session", "{row}");
        assert_eq!(
            row["metadata"]["repository"], "github.com/acme/widgets",
            "{row}"
        );
        assert_eq!(row["metadata"]["branch"], "feature/e2e", "{row}");
        assert_eq!(row["metadata"]["coding_agent"], "claude-code", "{row}");
    }

    // The Agent SDK trace's root is the application span, read from the trace's spans.
    let detail = server
        .api
        .get(&format!("{project}/traces/{sdk_trace}"))
        .await;
    let root = detail["spans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["span"]["parent_span_id"].is_null())
        .unwrap();
    assert_eq!(root["span"]["name"], "app-request", "{detail}");

    // The "Top repositories by cost" and "Top sessions by cost" widgets of the template.
    let coding_agent = "metadata['coding_agent'] IS NOT NULL";
    let by_repository = server
        .api
        .post(
            &format!("{project}/dashboards/query"),
            &json!({
                "from": from,
                "to": to,
                "shape": "total",
                "query": {
                    "source": "traces",
                    "filter": format!("{coding_agent} AND metadata['repository'] IS NOT NULL"),
                    "measures": [{"agg": "sum", "field": "cost_total", "label": "Cost"}],
                    "breakdown": {"field": "metadata['repository']", "limit": 10, "other": false},
                },
            }),
        )
        .await;
    let rows = by_repository["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{by_repository}");
    assert_eq!(
        rows[0]["group"], "github.com/acme/widgets",
        "{by_repository}"
    );
    assert!(rows[0]["values"][0].as_f64().is_some(), "{by_repository}");

    let by_session = server
        .api
        .post(
            &format!("{project}/dashboards/query"),
            &json!({
                "from": from,
                "to": to,
                "shape": "total",
                "query": {
                    "source": "traces",
                    "filter": format!("{coding_agent} AND session_id IS NOT NULL"),
                    "measures": [{"agg": "sum", "field": "cost_total", "label": "Cost"}],
                    "breakdown": {"field": "session_id", "limit": 10, "other": false},
                },
            }),
        )
        .await;
    let rows = by_session["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{by_session}");
    assert_eq!(rows[0]["group"], "e2e-session", "{by_session}");

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
}

/// Track M end to end: a user deletion hides that user's traces at once and applies once
/// storage reports it; a project drop answers 404 and ends the project's data.
#[tokio::test(flavor = "multi_thread")]
async fn deletion_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "").await;
    let project = format!("/api/v1/projects/{}", server.proto_id);
    let project_body = server.api.get(&project).await;
    assert_eq!(project_body["slug"], "proto", "{project_body}");

    // Two traces of the fixture with the end user `u-17`, and one without a user.
    let user_traces = [format!("{:032x}", 11), format!("{:032x}", 12)];
    let other_trace = format!("{:032x}", 13);
    for (name, trace) in [
        ("sdk-context", &user_traces[0]),
        ("sdk-context", &user_traces[1]),
        ("sdk-span-manual", &other_trace),
    ] {
        let request = fixture_request(name, trace);
        post_otlp_http(
            &server.client,
            format!("{}/v1/traces", server.http),
            INIT_KEY,
            "application/x-protobuf",
            request.encode_to_vec(),
        )
        .await;
    }
    assert_eq!(
        server
            .api
            .get_status(&format!("{project}/traces/{}", user_traces[0]))
            .await,
        200
    );

    // The trace-list stream is open before the request, so its `resync` is live.
    let mut traces = server
        .api
        .get_raw(&format!("{project}/traces/stream"), &[])
        .await;
    assert_eq!(traces.status(), 200);

    let created = server
        .api
        .post(
            &format!("{project}/deletions"),
            &json!({"kind": "user", "user_id": "u-17"}),
        )
        .await;
    assert_eq!(created["status"], "resolving", "{created}");
    let deletion_id = created["id"].as_str().unwrap().to_owned();
    let deletion_path = format!("{project}/deletions/{deletion_id}");
    let pending = poll_json(
        &server.api,
        &deletion_path,
        |body| body["status"] == "pending",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(pending["trace_count"], 2, "{pending}");

    read_until(&mut traces, "event: resync", Duration::from_secs(5)).await;

    // The user's traces are hidden from every read; the other trace is not.
    for trace in &user_traces {
        assert_eq!(
            server
                .api
                .get_status(&format!("{project}/traces/{trace}"))
                .await,
            404
        );
    }
    assert_eq!(
        server
            .api
            .get_status(&format!("{project}/traces/{other_trace}"))
            .await,
        200
    );
    assert_eq!(
        server
            .api
            .get_status(&format!("{project}/users/u-17"))
            .await,
        404
    );

    // The window of the SDK fixtures: only the trace without a user is left.
    let preview = server
        .api
        .post(
            &format!("{project}/deletions/preview"),
            &json!({"from": "2025-12-31T00:00:00Z", "to": "2026-01-02T00:00:00Z"}),
        )
        .await;
    assert_eq!(preview["matched_traces"], 1, "{preview}");

    // Export once more with the init key right before the drop. This caches the key, so the
    // project check (not the key check) answers after the drop.
    let manual = fixture_request("sdk-span-manual", &other_trace);
    assert_eq!(otlp_status(&server, &manual).await, 200);

    let dropped = server
        .api
        .delete(&format!(
            "/api/v1/projects/{}?confirm=proto",
            server.proto_id
        ))
        .await;
    assert_eq!(dropped["kind"], "project", "{dropped}");

    assert_eq!(server.api.get_status(&project).await, 404);
    assert_eq!(otlp_status(&server, &manual).await, 404);

    // The drop applies the project deletion and, because the drop supersedes it, the user one.
    let org_deletions = poll_json(
        &server.api,
        "/api/v1/orgs/acme/deletions",
        |body| {
            let rows = body["data"].as_array();
            let applied = |kind: &str| {
                rows.is_some_and(|rows| {
                    rows.iter()
                        .any(|row| row["kind"] == kind && row["status"] == "applied")
                })
            };
            applied("project") && applied("user")
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(org_deletions["data"].is_array(), "{org_deletions}");

    drop(traces);
    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
}

/// The judge prompt of the end-to-end evaluator.
const JUDGE_PROMPT: &str = "Question: {{ input }}\nAnswer: {{ output }}";

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// One chat turn of the mock provider: the judge's numeric answer.
fn judge_turn() -> cetra_capture::Turn {
    serde_json::from_value(json!({
        "api": "openai.chat",
        "model": "gpt-test",
        "message": {"text": "{\"explanation\": \"Relevant.\", \"value\": 0.9}"},
        "finish": "stop",
        "usage": {"input": 10, "output": 3},
        "ttft_ms": 0
    }))
    .unwrap()
}

/// `sdk-generator-text`'s one root span (it has an input and an output), on `trace`,
/// starting at `start_ns` and lasting 998 ms.
fn timed_request(trace: &str, start_ns: i64) -> ExportTraceServiceRequest {
    timed_fixture("sdk-generator-text", trace, start_ns)
}

/// The fixture `name`'s spans, on `trace`, starting at `start_ns` and lasting 998 ms.
fn timed_fixture(name: &str, trace: &str, start_ns: i64) -> ExportTraceServiceRequest {
    let mut request = fixture_request(name, trace);
    let start = u64::try_from(start_ns).unwrap();
    let end = u64::try_from(start_ns + 998_000_000).unwrap();
    for resource in &mut request.resource_spans {
        for scope in &mut resource.scope_spans {
            for span in &mut scope.spans {
                span.start_time_unix_nano = start;
                span.end_time_unix_nano = end;
            }
        }
    }
    request
}

/// `sdk-observe-error`'s spans on `trace`, starting at `start_ns` and lasting 998 ms, as
/// `timed_request` does for `sdk-generator-text`. The fixture has an error span.
fn timed_error_request(trace: &str, start_ns: i64) -> ExportTraceServiceRequest {
    let mut request = fixture_request("sdk-observe-error", trace);
    let start = u64::try_from(start_ns).unwrap();
    let end = u64::try_from(start_ns + 998_000_000).unwrap();
    for resource in &mut request.resource_spans {
        for scope in &mut resource.scope_spans {
            for span in &mut scope.spans {
                span.start_time_unix_nano = start;
                span.end_time_unix_nano = end;
            }
        }
    }
    request
}

/// Nanoseconds since the Unix epoch, now.
fn now_ns() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap()
}

/// Reads `path` until its `data` has `count` rows, every 200 ms, for at most `wait`.
/// Returns the last body read, and whether it had `count` rows.
async fn rows_until(api: &Api, path: &str, count: usize, wait: Duration) -> (Value, bool) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let body = api.get(path).await;
        let done = body["data"]
            .as_array()
            .is_some_and(|rows| rows.len() == count);
        if done || tokio::time::Instant::now() >= deadline {
            return (body, done);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Track I end to end (D-G-20): a live trigger scores a new trace, and a backfill scores a
/// trace stored before the rule existed, both against the capture tool's mock provider.
#[tokio::test(flavor = "multi_thread")]
async fn online_rules_and_backfill_end_to_end() {
    // The mock provider answers the judge twice: once for the live trace, once for the backfill.
    let mock = cetra_capture::Mock::new(vec![judge_turn(), judge_turn()]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_addr = listener.local_addr().unwrap();
    let router = mock.router();
    let provider = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    // The mock listens on 127.0.0.1, a private address the judge may reach only when allowed.
    let server = start(
        dir.path(),
        "\n[security]\nallow_private_network_egress = true\n",
    )
    .await;
    let project = format!("/api/v1/projects/{}", server.proto_id);

    // A trace that is stored before any rule exists, so the live trigger does not track it.
    let old = format!("{:032x}", 31);
    post_otlp_http(
        &server.client,
        format!("{}/v1/traces", server.http),
        INIT_KEY,
        "application/x-protobuf",
        timed_request(&old, now_ns() - 20 * 60 * NANOS_PER_SECOND).encode_to_vec(),
    )
    .await;

    // The connection to the mock, the judge and the rule.
    let connection = server
        .api
        .post(
            "/api/v1/orgs/acme/llm-connections",
            &json!({
                "name": "Mock",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": format!("http://{provider_addr}/v1"),
                "default_model": "gpt-test",
                "max_concurrency": 4,
                "requests_per_minute": 600
            }),
        )
        .await;
    let evaluator = server
        .api
        .post(
            &format!("{project}/evaluators"),
            &json!({
                "name": "Relevance",
                "kind": "llm_judge",
                "config": {
                    "connection_id": connection["id"],
                    "model": "gpt-test",
                    "prompt": JUDGE_PROMPT,
                    "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                    "include_reason": true,
                    "temperature": null,
                    "max_output_tokens": 1024,
                    "timeout_ms": 60000
                },
                "output_score": {"name": "relevance", "data_type": "numeric"}
            }),
        )
        .await;
    let rule = server
        .api
        .post(
            &format!("{project}/online-rules"),
            &json!({
                "name": "Relevance on traces",
                "evaluator_id": evaluator["id"],
                "target": "trace",
                "variable_mapping": {"input": "trace.input", "output": "trace.output"},
                "sample_rate": 1.0,
                "enabled": true
            }),
        )
        .await;
    let rule_id = rule["id"].as_str().unwrap().to_owned();
    let scores_path = format!("{project}/scores?rule_id={rule_id}");

    // The sweeper tracks a project from its first tick after the rule exists, and a trace is
    // tracked from its first span: a send before that tick is missed, and a resend restarts the
    // trace's 5 s idle timer. So wait one tick and a margin, then send the live trace once.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let live = format!("{:032x}", 32);
    post_otlp_http(
        &server.client,
        format!("{}/v1/traces", server.http),
        INIT_KEY,
        "application/x-protobuf",
        timed_request(&live, now_ns() - NANOS_PER_SECOND).encode_to_vec(),
    )
    .await;
    let live_scores = poll_json(
        &server.api,
        &scores_path,
        |body| body["data"].as_array().is_some_and(|rows| !rows.is_empty()),
        Duration::from_secs(30),
    )
    .await;

    // Only the live trace is scored: the trace stored before the rule is not a live target.
    let scored: std::collections::BTreeSet<&str> = live_scores["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["target"]["trace_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        scored,
        std::collections::BTreeSet::from([live.as_str()]),
        "the live trigger must score the live trace only: {live_scores}"
    );

    // The live score: the rule's judge wrote it for the live trace.
    let row = &live_scores["data"][0];
    assert_eq!(row["target"]["trace_id"], live, "{live_scores}");
    assert_eq!(row["name"], "relevance", "{live_scores}");
    assert_eq!(row["source"], "llm_judge", "{live_scores}");

    // The estimate: the live trace is after `to`, and its judge trace is an evaluation trace,
    // so neither counts.
    let from = chrono::DateTime::from_timestamp_nanos(now_ns() - 3600 * NANOS_PER_SECOND)
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let to = chrono::DateTime::from_timestamp_nanos(now_ns() - 360 * NANOS_PER_SECOND)
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let body = json!({"from": from, "to": to, "max_targets": 100});
    let estimate_path = format!("{project}/online-rules/{rule_id}/backfills/estimate");
    let estimate = server.api.post(&estimate_path, &body).await;
    assert_eq!(estimate["matched"], 1, "{estimate}");
    assert_eq!(estimate["capped"], false, "{estimate}");
    assert_eq!(estimate["sampled"], 1, "{estimate}");
    let input_chars = u64::try_from(JUDGE_PROMPT.chars().count()).unwrap();
    assert_eq!(
        estimate["estimated_input_tokens"],
        json!((input_chars + 4000).div_ceil(4)),
        "{estimate}"
    );
    assert_eq!(estimate["estimated_output_tokens"], 1024, "{estimate}");
    assert!(estimate.get("estimated_cost_usd").is_none(), "{estimate}");

    // The backfill runs over the old trace and finishes.
    let backfills_path = format!("{project}/online-rules/{rule_id}/backfills");
    let created = server.api.post(&backfills_path, &body).await;
    assert_eq!(created["status"], "running", "{created}");
    let listed = poll_json(
        &server.api,
        &backfills_path,
        |body| body["data"][0]["status"] == "done",
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(listed["data"][0]["matched"], 1, "{listed}");
    assert_eq!(listed["data"][0]["enqueued"], 1, "{listed}");
    let (scores, done) = rows_until(&server.api, &scores_path, 2, Duration::from_secs(10)).await;
    assert!(done, "no second score: {scores}");
    assert!(
        scores["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|score| score["target"]["trace_id"] == old),
        "{scores}"
    );

    // A second backfill of the same window pays for nothing: the target is already scored.
    let again = server.api.post(&backfills_path, &body).await;
    let again_id = again["id"].as_str().unwrap().to_owned();
    let listed = poll_json(
        &server.api,
        &backfills_path,
        |body| body["data"][0]["id"] == again_id.as_str() && body["data"][0]["status"] == "done",
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(listed["data"][0]["matched"], 1, "{listed}");
    assert_eq!(listed["data"][0]["enqueued"], 0, "{listed}");

    // One judge job per target: the live trace's and the backfill's, both done.
    let jobs = server
        .api
        .get(&format!("{project}/eval-jobs?rule_id={rule_id}"))
        .await;
    let jobs = jobs["data"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    for job in jobs {
        assert_eq!(job["status"], "done", "{job}");
    }
    let mut kinds: Vec<&str> = jobs
        .iter()
        .map(|job| job["kind"].as_str().unwrap())
        .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["backfill", "rule"]);

    // The mock served exactly the two judge calls, with no error.
    let log = mock.finish();
    assert!(log.errors.is_empty(), "{:?}", log.errors);
    assert_eq!(log.served.len(), 2, "{:?}", log.served);

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
    provider.abort();
}

/// Track IC M4 end to end (D-IC-25): an error source turns a failing trace into an issue, a
/// resolved issue regresses on a second failure, its examples are copied into a dataset, and a
/// channel is attached and detached. No alert or notification is awaited: the binary's
/// notification tick runs every 60 s.
#[tokio::test(flavor = "multi_thread")]
async fn error_issues_end_to_end() {
    /// A Slack URL the channel is never sent to: nothing is notified in this test.
    const SLACK_URL: &str = "https://hooks.slack.com/services/T0/B0/XXXXabcd";

    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), "").await;
    let project = format!("/api/v1/projects/{}", server.proto_id);
    let otlp_url = format!("{}/v1/traces", server.http);

    let source = server
        .api
        .post(
            &format!("{project}/issue-sources"),
            &json!({"name": "Errors", "kind": "error"}),
        )
        .await;
    let source_id = source["id"].as_str().unwrap().to_owned();
    assert_eq!(source["channel_ids"], json!([]), "{source}");

    // The sweeper tracks the project from its first tick after the source exists, and a trace
    // is tracked from its first span: wait one tick and a margin before the first failure.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let first = format!("{:032x}", 41);
    post_otlp_http(
        &server.client,
        otlp_url.clone(),
        INIT_KEY,
        "application/x-protobuf",
        timed_fixture("sdk-observe-error", &first, now_ns() - NANOS_PER_SECOND).encode_to_vec(),
    )
    .await;

    // The trace completes after its 5 s idle timer, and its error becomes one open issue.
    let issues_path = format!("{project}/issues");
    let listed = poll_json(
        &server.api,
        &issues_path,
        |body| body["data"][0]["state"] == "open",
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1, "{listed}");
    let issue = &listed["data"][0];
    assert_eq!(issue["kind"], "error", "{listed}");
    assert_eq!(issue["count"], 1, "{listed}");
    let issue_id = issue["id"].as_str().unwrap().to_owned();
    let issue_path = format!("{issues_path}/{issue_id}");

    let occurrences = server.api.get(&format!("{issue_path}/occurrences")).await;
    assert_eq!(
        occurrences["examples"][0]["trace_id"], first,
        "{occurrences}"
    );

    // Resolved, then a failure that started after the resolution reopens the issue as regressed.
    let resolved = server
        .api
        .patch(&issue_path, &json!({"state": "resolved"}))
        .await;
    assert_eq!(resolved["state"], "resolved", "{resolved}");
    let second = format!("{:032x}", 42);
    post_otlp_http(
        &server.client,
        otlp_url,
        INIT_KEY,
        "application/x-protobuf",
        timed_fixture("sdk-observe-error", &second, now_ns()).encode_to_vec(),
    )
    .await;
    let regressed = poll_json(
        &server.api,
        &issue_path,
        |body| body["state"] == "open" && body["regressed"] == true && body["count"] == 2,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(regressed["id"], issue_id.as_str(), "{regressed}");

    // The issue's examples are copied into a dataset: both traces, each tagged with the issue.
    server
        .api
        .post(
            &format!("{project}/datasets"),
            &json!({"name": "issue-examples"}),
        )
        .await;
    let dataset_items = format!("{issue_path}/dataset-items");
    let added = server
        .api
        .post(&dataset_items, &json!({"dataset": "issue-examples"}))
        .await;
    let added_items = added["items"].as_array().unwrap();
    assert_eq!(added_items.len(), 2, "{added}");
    assert_eq!(added["skipped"], json!([]), "{added}");
    for item in added_items {
        assert_eq!(
            item["metadata"]["cetra.issue_id"],
            issue_id.as_str(),
            "{added}"
        );
    }
    let items = server
        .api
        .get(&format!("{project}/datasets/issue-examples/items"))
        .await;
    assert_eq!(items["data"].as_array().unwrap().len(), 2, "{items}");

    // A limit of one copies only the newest occurrence, as a new version.
    let newest = server
        .api
        .post(
            &dataset_items,
            &json!({"dataset": "issue-examples", "limit": 1}),
        )
        .await;
    assert_eq!(newest["items"].as_array().unwrap().len(), 1, "{newest}");

    // A channel is attached to the source, and deleting the channel detaches it.
    let channel = server
        .api
        .post(
            &format!("{project}/channels"),
            &json!({"name": "Team Slack", "kind": "slack", "url": SLACK_URL}),
        )
        .await;
    let channel_id = channel["id"].as_str().unwrap().to_owned();
    let source_path = format!("{project}/issue-sources/{source_id}");
    let attached = server
        .api
        .patch(&source_path, &json!({"channel_ids": [channel_id]}))
        .await;
    assert_eq!(attached["channel_ids"], json!([channel_id]), "{attached}");

    // A `DELETE` answers `204` with no body, so it is sent without `Api::delete`'s JSON read.
    let deleted = server
        .client
        .delete(format!("{}{project}/channels/{channel_id}", server.http))
        .header("cookie", &server.api.cookie)
        .header("origin", ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status().as_u16(), 204);
    let detached = server.api.get(&source_path).await;
    assert_eq!(detached["channel_ids"], json!([]), "{detached}");

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
}

/// Track V M6 end to end (D-V-20, D-V-22, D-V-33): a playground run streams from the capture
/// mock through the org's connection, and its trace lives in the reserved environment
/// `cetra-playground`, which the default trace list leaves out.
#[tokio::test(flavor = "multi_thread")]
async fn playground_run_end_to_end() {
    let mock = cetra_capture::Mock::new(vec![
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": "Hello from cetra."},
            "finish": "stop",
            "usage": {"input": 12, "output": 4},
            "chunks": 3,
            "ttft_ms": 0,
            "chunk_delay_ms": 5
        }))
        .unwrap(),
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_addr = listener.local_addr().unwrap();
    let router = mock.router();
    let provider = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    // The mock listens on 127.0.0.1, a private address the run may reach only when allowed.
    let server = start(
        dir.path(),
        "\n[security]\nallow_private_network_egress = true\n",
    )
    .await;
    let project = format!("/api/v1/projects/{}", server.proto_id);

    let connection = server
        .api
        .post(
            "/api/v1/orgs/acme/llm-connections",
            &json!({
                "name": "Mock",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": format!("http://{provider_addr}/v1"),
                "default_model": "gpt-test",
                "max_concurrency": 4,
                "requests_per_minute": 600
            }),
        )
        .await;

    // The admin's run: a system message with a variable, a user message, and a prompt source.
    let body = json!({
        "connection_id": connection["id"],
        "model": "gpt-test",
        "messages": [
            {"role": "system", "parts": [{"type": "text", "content": "You are a support agent for {{product}}."}]},
            {"role": "user", "parts": [{"type": "text", "content": "Say hello."}]}
        ],
        "variables": {"product": "cetra"},
        "params": {"temperature": 0.2},
        "source": {"prompt": {"name": "support-reply", "version": 2}}
    });
    let mut response = server
        .client
        .post(format!("{}{project}/playground/runs", server.http))
        .header("cookie", &server.api.cookie)
        .header("origin", ORIGIN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let events = read_until(&mut response, "event: done", Duration::from_secs(30)).await;
    assert!(events.contains("event: start"), "{events}");
    assert!(events.contains("event: text"), "{events}");
    assert!(events.contains("Hello from cetra."), "{events}");
    drop(response);

    // The run's trace is in the reserved environment: listed with evaluation traces only.
    let with_eval = poll_json(
        &server.api,
        &format!("{project}/traces?include_eval=true"),
        |body| {
            body["data"].as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["environment"] == "cetra-playground")
            })
        },
        Duration::from_secs(10),
    )
    .await;
    let playground = with_eval["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["environment"] == "cetra-playground")
        .unwrap();
    let trace_id = playground["trace_id"].as_str().unwrap().to_owned();
    let detail = server
        .api
        .get(&format!("{project}/traces/{trace_id}"))
        .await;
    let span_id = detail["spans"][0]["span"]["span_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let span = server
        .api
        .get(&format!("{project}/traces/{trace_id}/spans/{span_id}"))
        .await;
    assert!(span.to_string().contains("for cetra."), "{span}");

    let default = server.api.get(&format!("{project}/traces")).await;
    assert!(
        default["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["environment"] != "cetra-playground"),
        "{default}"
    );

    // The mock served exactly the one run, with no error.
    let log = mock.finish();
    assert!(log.errors.is_empty(), "{:?}", log.errors);
    assert_eq!(log.served.len(), 1, "{:?}", log.served);

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
    provider.abort();
}

/// Track AS M4 end to end (D-AS-31): an assistant turn over ingested data. The org setting's
/// test takes the first mock turn; the question then takes a `search_traces` call, a
/// `draft_filter` call and a text answer. The stream, the stored draft, the Apply query and the
/// turn trace are checked.
#[tokio::test(flavor = "multi_thread")]
async fn assistant_turn_end_to_end() {
    // The error trace starts a minute ago, so the search window around it holds it.
    let now = now_ns();
    let start_ns = now - 60 * NANOS_PER_SECOND;
    let mock = cetra_capture::Mock::new(vec![
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": [{"id": "call_1", "name": "ping", "arguments": {"ok": true}}]},
            "finish": "tool_calls",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": [{"id": "call_2", "name": "search_traces", "arguments": {
                "from": rfc3339(start_ns - 3_600 * NANOS_PER_SECOND),
                "to": rfc3339(now + 3_600 * NANOS_PER_SECOND),
                "filter": "status = 'error'"
            }}]},
            "finish": "tool_calls",
            "usage": {"input": 20, "output": 6},
            "ttft_ms": 0,
            "chunk_delay_ms": 0
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": [{"id": "call_3", "name": "draft_filter", "arguments": {
                "view": "traces",
                "filter": "status = 'error'",
                "range": "24h"
            }}]},
            "finish": "tool_calls",
            "usage": {"input": 30, "output": 6},
            "ttft_ms": 0,
            "chunk_delay_ms": 0
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": "There are errors; open the draft to see them."},
            "finish": "stop",
            "usage": {"input": 40, "output": 9},
            "ttft_ms": 0,
            "chunk_delay_ms": 0
        }))
        .unwrap(),
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_addr = listener.local_addr().unwrap();
    let router = mock.router();
    let provider = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    // The mock listens on 127.0.0.1, a private address the turn may reach only when allowed.
    let server = start(
        dir.path(),
        "\n[security]\nallow_private_network_egress = true\n",
    )
    .await;
    let project = format!("/api/v1/projects/{}", server.proto_id);

    // An error trace, one minute old, ingested with the project's key.
    let trace = format!("{:032x}", 41);
    post_otlp_http(
        &server.client,
        format!("{}/v1/traces", server.http),
        INIT_KEY,
        "application/x-protobuf",
        timed_error_request(&trace, start_ns).encode_to_vec(),
    )
    .await;
    let (_, ingested) = rows_until(
        &server.api,
        &format!("{project}/traces"),
        1,
        Duration::from_secs(10),
    )
    .await;
    assert!(ingested, "the error trace was not listed");

    // The connection to the mock, then the org setting, whose test takes the first turn.
    let connection = server
        .api
        .post(
            "/api/v1/orgs/acme/llm-connections",
            &json!({
                "name": "Mock",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": format!("http://{provider_addr}/v1"),
                "default_model": "gpt-test",
                "max_concurrency": 4,
                "requests_per_minute": 600
            }),
        )
        .await;
    server
        .api
        .put(
            "/api/v1/orgs/acme/assistant",
            &json!({"enabled": true, "connection_id": connection["id"], "model": "gpt-test"}),
        )
        .await;
    let usable = server.api.get(&format!("{project}/assistant")).await;
    assert_eq!(usable["usable"], true, "{usable}");

    // A conversation, then the question on the traces page.
    let created = server
        .client
        .post(format!("{}{project}/assistant/conversations", server.http))
        .header("cookie", &server.api.cookie)
        .header("origin", ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let conversation: Value = created.json().await.unwrap();
    let conversation_id = conversation["id"].as_str().unwrap().to_owned();

    let mut response = server
        .client
        .post(format!(
            "{}{project}/assistant/conversations/{conversation_id}/turns",
            server.http
        ))
        .header("cookie", &server.api.cookie)
        .header("origin", ORIGIN)
        .json(&json!({"message": "Which traces failed?", "context": {"page": "traces"}}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let events = read_until(&mut response, "event: done", Duration::from_secs(30)).await;
    drop(response);

    // The events come in this order. Each needle is searched after the previous one.
    let mut from_at = 0;
    for needle in [
        "event: start",
        "event: tool_call",
        "\"name\":\"search_traces\"",
        "event: tool_result",
        "\"ok\":true",
        "event: draft",
        "\"kind\":\"filter\"",
        "event: text",
        "event: done",
    ] {
        let found = events[from_at..]
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle} after byte {from_at}: {events}"));
        from_at += found + needle.len();
    }

    // The stored conversation: the tool message holds the filter draft, and the answer follows.
    let detail = server
        .api
        .get(&format!(
            "{project}/assistant/conversations/{conversation_id}"
        ))
        .await;
    let messages = detail["messages"].as_array().unwrap();
    let stored_draft = messages.iter().any(|message| {
        message["role"] == "tool"
            && message["parts"].as_array().unwrap().iter().any(|part| {
                part["type"] == "draft"
                    && part["kind"] == "filter"
                    && part["draft"]["filter"] == "status = 'error'"
            })
    });
    assert!(stored_draft, "{detail}");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant", "{detail}");
    assert!(
        last["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["type"] == "text"),
        "{detail}"
    );

    // Apply: the streamed draft's filter over a window that holds the error trace, as the traces
    // page would list it. The list route takes from and to, not the draft's range (the web client
    // converts it), so the window is the one the draft's range stands for.
    let draft_block = events
        .split("\n\n")
        .find(|block| block.lines().any(|line| line == "event: draft"))
        .unwrap_or_else(|| panic!("no draft event: {events}"));
    let draft_data = draft_block
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .unwrap_or_else(|| panic!("the draft event has no data: {draft_block}"));
    let streamed: Value = serde_json::from_str(draft_data.trim()).unwrap();
    let draft_filter = streamed["draft"]["filter"].as_str().unwrap();
    assert_eq!(draft_filter, "status = 'error'", "{streamed}");
    let mut apply = reqwest::Url::parse("http://localhost/").unwrap();
    apply
        .query_pairs_mut()
        .append_pair("filter", draft_filter)
        .append_pair("from", &rfc3339(now - 24 * 3_600 * NANOS_PER_SECOND))
        .append_pair("to", &rfc3339(now + 3_600 * NANOS_PER_SECOND));
    let applied = server
        .api
        .get(&format!("{project}/traces?{}", apply.query().unwrap()))
        .await;
    assert!(
        applied["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["trace_id"] == trace.as_str()),
        "{applied}"
    );

    // The turn's trace is in the reserved environment: listed with evaluation traces only.
    let (turn_rows, has_turn) = rows_until(
        &server.api,
        &format!(
            "{project}/traces?include_eval=true&filter=environment%20%3D%20%27cetra-assistant%27"
        ),
        1,
        Duration::from_secs(10),
    )
    .await;
    assert!(has_turn, "{turn_rows}");
    assert_eq!(
        turn_rows["data"][0]["environment"], "cetra-assistant",
        "{turn_rows}"
    );

    // The mock served the four turns, with no error.
    let log = mock.finish();
    assert!(log.errors.is_empty(), "{:?}", log.errors);
    assert_eq!(log.served.len(), 4, "{:?}", log.served);

    drop(server.api);
    drop(server.client);
    server.running.shutdown().await.unwrap();
    provider.abort();
}
