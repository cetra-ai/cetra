//! Fuzz entry points (track R M3, docs/plans/r-hardening/03-m3-fuzzing.md). Each one takes arbitrary bytes and must not panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

use std::sync::OnceLock;

use cetra_core::batch::spans_to_record_batch;
use cetra_core::ids::ProjectId;
use prost::Message;

use crate::{
    ExportTraceServiceRequest, IngestContext, IngestLimits, PriceTable, builtin_price_rows,
    decode_json, decode_protobuf, normalize,
};

/// The project the requests are normalized into.
fn context() -> IngestContext {
    IngestContext {
        // Harness setup, not input-dependent.
        project_id: "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse::<ProjectId>()
            .expect("the project id parses"),
        // 2026-01-01, the fixtures' usual ingest time.
        ingest_time: 1_767_225_600_000_000_000,
        retention_days: None,
        limits: IngestLimits::default(),
    }
}

/// The built-in price table, built once: compiling its patterns per input would dominate.
fn prices() -> &'static PriceTable {
    static PRICES: OnceLock<PriceTable> = OnceLock::new();
    PRICES.get_or_init(|| {
        // Harness setup, not input-dependent.
        PriceTable::new(builtin_price_rows()).expect("the built-in prices compile")
    })
}

/// Normalizes an accepted request, then converts its spans the way the server does before the
/// WAL append. A span `normalize` accepts must convert (track R M3).
fn normalize_and_convert(request: &ExportTraceServiceRequest) {
    let normalized = normalize(
        request,
        &context(),
        prices(),
        &crate::AgentContexts::default(),
    );
    assert!(
        spans_to_record_batch(&normalized.spans).is_ok(),
        "every span normalize accepts converts to a record batch"
    );
}

/// Decodes arbitrary bytes as an OTLP/HTTP protobuf export, then normalizes it (track R M3).
pub fn otlp_http_protobuf(data: &[u8]) {
    if let Ok(request) = decode_protobuf(data) {
        normalize_and_convert(&request);
    }
}

/// Decodes arbitrary bytes as an OTLP/HTTP JSON export, then normalizes it (track R M3).
pub fn otlp_http_json(data: &[u8]) {
    if let Ok(request) = decode_json(data) {
        normalize_and_convert(&request);
    }
}

/// Decodes arbitrary bytes as the protobuf an OTLP/gRPC export carries. The gRPC framing and
/// gzip are tonic's code, outside this crate, so only the decode and the size run here (track R
/// M3, `00-design.md` § M3).
pub fn otlp_grpc(data: &[u8]) {
    if let Ok(request) = decode_protobuf(data) {
        let _ = request.encoded_len();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
    /// `fuzz/regressions/<target>/`. The corpus must not be empty.
    fn replay(target: &str, entry: fn(&[u8])) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus").join(target));
        assert!(!corpus.is_empty(), "no corpus for {target}");
        let regressions = sorted_files(&root.join("regressions").join(target));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read a corpus file");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&data)));
            if outcome.is_err() {
                panic!("{target}: {} panicked", path.display());
            }
        }
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|entry| entry.expect("read a corpus entry").path())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        files
    }

    /// The `fixtures/otlp` input files, sorted by path.
    fn input_fixtures() -> Vec<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/otlp");
        let mut found = Vec::new();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read a fixture directory") {
                let path = entry.expect("read a fixture entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.file_name().is_some_and(|name| name == "input.json") {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn otlp_http_protobuf_corpus_replays() {
        replay("otlp_http_protobuf", otlp_http_protobuf);
    }

    #[test]
    fn otlp_http_json_corpus_replays() {
        replay("otlp_http_json", otlp_http_json);
    }

    #[test]
    fn otlp_grpc_corpus_replays() {
        replay("otlp_grpc", otlp_grpc);
    }

    #[test]
    fn a_fixture_normalizes_to_spans() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/otlp/genai/chat-cache-tokens/input.json");
        let bytes = std::fs::read(path).expect("read the fixture");
        let request = decode_json(&bytes).expect("the fixture decodes");
        let normalized = normalize(
            &request,
            &context(),
            prices(),
            &crate::AgentContexts::default(),
        );
        assert!(
            !normalized.spans.is_empty(),
            "the fixture has accepted spans"
        );
    }

    /// Writes the seed files of this crate's targets (track R M3, `just fuzz-seed`): every
    /// fixture as JSON, and its protobuf encoding for the two protobuf targets.
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let write = |target: &str, name: &str, bytes: &[u8]| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fuzz/corpus")
                .join(target);
            std::fs::create_dir_all(&dir).expect("create the corpus directory");
            std::fs::write(dir.join(format!("seed-{name}")), bytes).expect("write a seed");
        };
        for path in input_fixtures() {
            let case = path.parent().expect("a fixture directory");
            let group = case
                .parent()
                .and_then(Path::file_name)
                .expect("a group directory")
                .to_string_lossy()
                .into_owned();
            let name = format!(
                "{group}-{}",
                case.file_name()
                    .expect("a case directory")
                    .to_string_lossy()
            );
            let bytes = std::fs::read(&path).expect("read a fixture");
            write("otlp_http_json", &name, &bytes);
            if let Ok(request) = decode_json(&bytes) {
                let encoded = request.encode_to_vec();
                write("otlp_http_protobuf", &name, &encoded);
                write("otlp_grpc", &name, &encoded);
            }
        }
    }
}
