//! Tests of the helpers shared by the benches and the soak (track R M7,
//! docs/plans/r-hardening/07-m7-scale-soak.md). They never touch the network
//! or the process environment, and never need the downloaded corpus.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::print_stdout
)]

#[allow(dead_code)] // each test target uses part of the shared helpers
mod common;

use std::collections::HashMap;

use common::corpus::Corpus;
use common::machine::{physical_disk, strip_subvolume};
use common::payload::{
    AppKind, Content, LLM_MAX_BYTES, LLM_MIN_BYTES, Payloads, SMALL_MAX_BYTES, trace,
};
use common::rng::Rng;
use common::s3::S3Env;
use llmobs_core::ids::{ProjectId, TraceId};
use serde_json::Value;

#[test]
fn rng_sequence_is_unchanged() {
    // Pinned from the xorshift64* code of trace_bench before R7.1 moved it.
    let mut rng = Rng::new(1);
    let next: Vec<u64> = (0..3).map(|_| rng.next()).collect();
    assert_eq!(
        next,
        [
            5_180_492_295_206_395_165,
            12_380_297_144_915_551_517,
            13_389_498_078_930_870_103
        ]
    );
    let mut rng = Rng::new(1);
    let unit: Vec<f64> = (0..3).map(|_| rng.unit()).collect();
    assert_eq!(
        unit,
        [
            0.280_835_050_050_359_47,
            0.671_137_253_026_676_4,
            0.725_846_145_283_366_8
        ]
    );
}

#[test]
fn rng_streams_differ() {
    assert_ne!(
        Rng::stream(1, 0).next(),
        Rng::stream(1, 1).next(),
        "streams for different b must differ"
    );
    assert_ne!(Rng::stream(0, 0).next(), 0);
}

#[test]
fn strip_subvolume_handles_btrfs() {
    assert_eq!(
        strip_subvolume("/dev/mapper/luks-c34c[/home]"),
        "/dev/mapper/luks-c34c"
    );
    assert_eq!(strip_subvolume("/dev/nvme0n1p2"), "/dev/nvme0n1p2");
}

#[test]
fn physical_disk_walks_to_the_disk() {
    let input = "luks-c34c         0                crypt\n\
                 └─nvme1n1p3       0                part\n\
                   └─nvme1n1       0 CT4000P3PSSD8  disk\n";
    assert_eq!(
        physical_disk(input).as_deref(),
        Some("nvme1n1 rota=0 CT4000P3PSSD8")
    );
    let no_disk = "luks-c34c 0 crypt\n└─nvme1n1p3 0 part\n";
    assert_eq!(physical_disk(no_disk), None);
}

#[test]
fn physical_disk_reads_models_with_spaces() {
    let spaced = "nvme0n1 0 SAMSUNG MZVL21T0HCLR-00B00 disk";
    assert_eq!(
        physical_disk(spaced).as_deref(),
        Some("nvme0n1 rota=0 SAMSUNG MZVL21T0HCLR-00B00")
    );
    let blank = "sdb 1  disk";
    assert_eq!(physical_disk(blank).as_deref(), Some("sdb rota=1 unknown"));
}

fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<&'static str, &'static str> = pairs.iter().copied().collect();
    move |name: &str| map.get(name).map(|value| (*value).to_owned())
}

#[test]
fn s3_env_reads_region_and_style() {
    assert!(S3Env::from_lookup(env(&[])).is_none());

    let defaults = S3Env::from_lookup(env(&[
        ("LLMOBS_TEST_S3_ENDPOINT", "http://127.0.0.1:7070"),
        ("LLMOBS_TEST_S3_BUCKET", "llmobs-bench"),
    ]))
    .unwrap();
    assert_eq!(defaults.region, "us-east-1");
    assert!(defaults.path_style);

    let hetzner = S3Env::from_lookup(env(&[
        (
            "LLMOBS_TEST_S3_ENDPOINT",
            "https://fsn1.your-objectstorage.com",
        ),
        ("LLMOBS_TEST_S3_BUCKET", "acme-prod"),
        ("LLMOBS_TEST_S3_REGION", "fsn1"),
        ("LLMOBS_TEST_S3_PATH_STYLE", "false"),
    ]))
    .unwrap();
    assert_eq!(hetzner.region, "fsn1");
    assert!(!hetzner.path_style);

    let zero = S3Env::from_lookup(env(&[
        ("LLMOBS_TEST_S3_ENDPOINT", "http://127.0.0.1:7070"),
        ("LLMOBS_TEST_S3_BUCKET", "llmobs-bench"),
        ("LLMOBS_TEST_S3_PATH_STYLE", "0"),
    ]))
    .unwrap();
    assert!(!zero.path_style);
}

#[test]
fn describe_hides_remote_targets() {
    let local = S3Env::from_lookup(env(&[
        ("LLMOBS_TEST_S3_ENDPOINT", "http://127.0.0.1:7070"),
        ("LLMOBS_TEST_S3_BUCKET", "llmobs-bench"),
    ]))
    .unwrap();
    assert!(local.describe("trace-bench").contains("llmobs-bench"));
    assert!(local.describe("trace-bench").contains("127.0.0.1:7070"));

    let remote = S3Env::from_lookup(env(&[
        (
            "LLMOBS_TEST_S3_ENDPOINT",
            "https://fsn1.your-objectstorage.com",
        ),
        ("LLMOBS_TEST_S3_BUCKET", "acme-prod"),
    ]))
    .unwrap();
    let text = remote.describe("trace-bench");
    assert!(!text.contains("acme-prod"), "{text}");
    assert!(!text.contains("fsn1.your-objectstorage.com"), "{text}");
    assert!(text.contains("<bucket>") && text.contains("<endpoint>"));
}

// ---------------------------------------------------------------- payloads (R7.2)

const WORDS: [&str; 12] = [
    "the", "model", "answer", "context", "tool", "query", "result", "span", "trace", "index",
    "prompt", "system",
];

/// 200 synthetic texts of 50–3,000 bytes. The real corpus is never needed here.
fn synthetic_corpus() -> Corpus {
    let texts = (0..200)
        .map(|i| {
            let target = 50 + (i * 397) % 2950;
            let mut text = format!("message {i}: ");
            let mut w = i;
            while text.len() < target {
                text.push_str(WORDS[w % WORDS.len()]);
                text.push(' ');
                w += 7;
            }
            text
        })
        .collect();
    Corpus::from_texts(texts)
}

fn payloads() -> Payloads {
    Payloads::new(synthetic_corpus())
}

/// Every string field of a payload, joined: equal keys mean equal bytes.
fn key(content: &Content) -> String {
    let fields = [
        &content.input,
        &content.output,
        &content.input_preview,
        &content.output_preview,
        &content.system_instructions,
        &content.tool_definitions,
        &content.request_params,
    ];
    let mut out = String::new();
    for field in fields {
        out.push_str(field.as_deref().unwrap_or("\u{1}"));
        out.push('\u{2}');
    }
    for (k, v) in &content.attributes {
        out.push_str(k);
        out.push('\u{3}');
        out.push_str(v);
        out.push('\u{4}');
    }
    out
}

fn parse(text: &str) -> Vec<Value> {
    serde_json::from_str(text).expect("a canonical message array")
}

fn trace_ids(seed: u64) -> (ProjectId, TraceId) {
    let project = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(seed + 1).to_be_bytes());
    (project, TraceId::from_bytes(bytes).unwrap())
}

#[test]
fn payloads_are_deterministic() {
    let (a, b) = (payloads(), payloads());
    for (x, y) in a.llm_calls(42, 3).iter().zip(&b.llm_calls(42, 3)) {
        assert_eq!(key(x), key(y));
    }
    assert_eq!(key(&a.root(42)), key(&b.root(42)));
    assert_eq!(key(&a.step(42, 1)), key(&b.step(42, 1)));
}

#[test]
fn llm_calls_hit_the_size_band() {
    let payloads = payloads();
    for seed in 0..200 {
        let calls = payloads.llm_calls(seed, 3);
        let first = calls[0].bytes();
        assert!(
            (LLM_MIN_BYTES..=LLM_MAX_BYTES + 24 * 1024).contains(&first),
            "seed {seed}: call 0 is {first} bytes"
        );
        for pair in calls.windows(2) {
            assert!(
                pair[1].bytes() > pair[0].bytes(),
                "seed {seed}: later calls must be larger"
            );
        }
    }
}

#[test]
fn later_calls_extend_the_conversation() {
    let payloads = payloads();
    let seed = (0..)
        .find(|&s| payloads.app(s).kind == AppKind::Agent)
        .unwrap();
    let calls = payloads.llm_calls(seed, 2);
    let first = calls[0].input.as_deref().unwrap();
    let second = calls[1].input.as_deref().unwrap();
    let head = &first[..first.len() - 1];
    assert!(
        second.starts_with(head),
        "call 1 must begin with call 0's messages"
    );
}

#[test]
fn inputs_are_canonical_messages() {
    let payloads = payloads();
    for seed in 0..50 {
        for call in payloads.llm_calls(seed, 3) {
            let input = parse(call.input.as_deref().unwrap());
            for message in &input {
                assert!(message["role"].is_string(), "seed {seed}: role");
                assert!(message["parts"].is_array(), "seed {seed}: parts");
            }
            let output = parse(call.output.as_deref().unwrap());
            assert_eq!(output.len(), 1, "seed {seed}: one output message");
            // Every tool call of the input has its response in the same input.
            let ids = |kind: &str| -> Vec<String> {
                input
                    .iter()
                    .flat_map(|m| m["parts"].as_array().cloned().unwrap_or_default())
                    .filter(|p| p["type"] == kind)
                    .filter_map(|p| p["id"].as_str().map(str::to_owned))
                    .collect()
            };
            let calls_ids = ids("tool_call");
            let responses = ids("tool_call_response");
            for id in &calls_ids {
                assert!(responses.contains(id), "seed {seed}: unmatched {id}");
            }
        }
    }
}

#[test]
fn previews_follow_rule_9() {
    let payloads = payloads();
    for seed in 0..50 {
        let call = &payloads.llm_calls(seed, 1)[0];
        let input = parse(call.input.as_deref().unwrap());
        let last_user = input
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .expect("a user message");
        let text = last_user["parts"][0]["content"].as_str().unwrap();
        let expected: String = text.chars().take(512).collect();
        let preview = call.input_preview.as_deref().unwrap();
        assert_eq!(preview, expected, "seed {seed}");
        assert!(preview.chars().count() <= 512);
    }
}

#[test]
fn small_spans_stay_small() {
    let payloads = payloads();
    for seed in 0..200 {
        for (name, content) in [
            ("root", payloads.root(seed)),
            ("tool", payloads.tool(seed, 0)),
            ("retriever", payloads.retriever(seed)),
            ("step", payloads.step(seed, 1)),
        ] {
            assert!(
                content.bytes() < SMALL_MAX_BYTES,
                "seed {seed}: {name} is {} bytes",
                content.bytes()
            );
        }
    }
}

#[test]
fn trace_summary_matches_its_spans() {
    let payloads = payloads();
    for seed in 0..100 {
        let (project, id) = trace_ids(seed);
        let generated = trace(&payloads, project, id, seed, 1_800_000_000_000_000_000);
        let spans: Vec<_> = generated.first.iter().chain(&generated.late).collect();
        let summary = &generated.summary;
        assert_eq!(spans.len() as i64, summary.span_count, "seed {seed}");
        let mut starts: Vec<i64> = spans.iter().map(|s| s.start_time).collect();
        starts.sort_unstable();
        starts.dedup();
        assert_eq!(starts.len(), spans.len(), "seed {seed}: shared start times");
        let tokens: i64 = spans.iter().filter_map(|s| s.total_tokens).sum();
        assert_eq!(summary.total_tokens, Some(tokens), "seed {seed}");
        let cost: f64 = spans.iter().filter_map(|s| s.cost_total).sum();
        assert_eq!(summary.cost_total, Some(cost), "seed {seed}");
        assert_eq!(
            spans.iter().filter(|s| s.parent_span_id.is_none()).count(),
            1,
            "seed {seed}: exactly one root"
        );
        let first = spans[0];
        for span in &spans {
            assert_eq!(span.session_id, first.session_id, "seed {seed}");
            assert_eq!(span.user_id, first.user_id, "seed {seed}");
            assert_eq!(span.environment, first.environment, "seed {seed}");
            assert_eq!(span.release, first.release, "seed {seed}");
            assert_eq!(span.service_name, first.service_name, "seed {seed}");
            assert_eq!(span.tags, first.tags, "seed {seed}");
        }
    }
}

#[test]
fn corpus_from_texts_pairs_turns() {
    let texts: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let corpus = Corpus::from_texts(texts);
    assert_eq!(corpus.turns(), 2);
    assert_eq!(corpus.turn(0), ("a", "b"));
    assert_eq!(corpus.turn(1), ("c", "d"));
}

/// Needs `just fetch-corpus` first. Prints counts only.
#[test]
#[ignore = "needs target/bench-corpus (just fetch-corpus); run by hand"]
fn real_corpus_loads() {
    let corpus = Corpus::from_env();
    println!(
        "corpus: {} texts, {} turns, {} bytes",
        corpus.texts(),
        corpus.turns(),
        corpus.bytes()
    );
    assert!(corpus.texts() >= 64_000, "texts {}", corpus.texts());
    assert!(corpus.turns() >= 40_000, "turns {}", corpus.turns());
}
