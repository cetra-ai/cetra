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

use common::machine::{physical_disk, strip_subvolume};
use common::rng::Rng;
use common::s3::S3Env;

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
