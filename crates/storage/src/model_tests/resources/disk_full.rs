//! A full disk and full inodes under the WAL and `put_media` (R5.3, D1, D2, D8, D9 of
//! docs/plans/r-hardening/05-m5-resources.md). Each case runs on a small filesystem, in its own
//! namespace child or on the operator's mount (`small_fs::on_small_fs`).

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use bytesize::ByteSize;
use sha2::{Digest, Sha256};

use super::super::check::check_full;
use super::super::harness::{Harness, HarnessOptions};
use super::super::ops::new_model;
use super::super::shape::{HOUR_NS, P, day, midnight};
use super::fill::{Text, append_filler, filler_batch, filler_rows};
use super::small_fs::{
    Ballast, InodeBallast, SMALL_FS_BYTES, SmallFs, free_bytes, on_small_fs, temp_files,
};
use super::{after_ops, model_steps, seed_ops, settle_and_check};
use crate::AppendReceipt;
use crate::MediaBlob;
use crate::config::StorageConfig;
use crate::error::{AppendError, MediaError, ObjectError, OpenError};
use crate::objects::Objects;

const MIB: u64 = 1 << 20;
/// One microsecond, the clock step before each filler batch.
const MICRO_NS: i64 = 1_000;

const D1: &str =
    "model_tests::resources::disk_full::appends_stop_at_min_free_before_the_disk_fills";
const D2: &str =
    "model_tests::resources::disk_full::a_full_disk_fails_the_wal_and_a_restart_recovers";
const D8: &str =
    "model_tests::resources::disk_full::put_media_on_a_full_disk_fails_and_leaves_nothing";
const D9: &str = "model_tests::resources::disk_full::running_out_of_inodes_is_an_error_not_a_panic";

/// A harness whose data directory is under `root`, with 1 MiB WAL segments (the minimum) and
/// `adjust` applied.
fn harness_in(root: &Path, adjust: impl FnOnce(&mut StorageConfig)) -> Harness {
    Harness::with_options(
        HarnessOptions {
            root: Some(root.to_path_buf()),
            ..HarnessOptions::default()
        },
        |config| {
            config.wal.segment_bytes = ByteSize::mib(1);
            adjust(config);
        },
    )
}

/// The model's first clock value, as the smoke test of R5.1 uses it.
fn clock_start() -> i64 {
    midnight(day(0)) + 12 * HOUR_NS
}

/// Rows of the `Ok` outcomes.
fn accepted_rows(outcomes: &[Result<AppendReceipt, AppendError>]) -> u64 {
    outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .map(|receipt| u64::try_from(receipt.rows).expect("a row count fits in u64"))
        .sum()
}

/// A blob of `len` bytes, its SHA-256 and its MIME type.
fn media_blob(len: usize) -> (MediaBlob, Bytes) {
    let bytes = Bytes::from(vec![0x5a_u8; len]);
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    let blob = MediaBlob {
        sha256,
        mime_type: "application/octet-stream".to_owned(),
        bytes: bytes.clone(),
    };
    (blob, bytes)
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn appends_stop_at_min_free_before_the_disk_fills() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D1, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::mib(16);
            config.flush.max_bytes = ByteSize::mib(256);
        });
        let mut model = new_model(&h);
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        let ballast = Ballast::leave_free(root, 32 * MIB);
        h.storage().disk().refresh().expect("measure the free disk");
        let outcomes = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::from_millis(100),
            |outcome| outcome.is_err(),
            400,
        );

        let last = outcomes.last().expect("at least one filler append");
        assert!(
            matches!(last, Err(AppendError::DiskFull)),
            "the first refusal under min_free is DiskFull, got {last:?}"
        );
        assert!(
            outcomes
                .iter()
                .all(|outcome| !matches!(outcome, Err(AppendError::WalFailed))),
            "min_free refuses before the WAL meets ENOSPC: no WalFailed may appear"
        );
        assert!(
            !h.storage().health().spans_wal_failed,
            "the spans WAL is not failed after a refusal"
        );
        assert!(
            free_bytes(root) > 0,
            "the disk is not full to the last byte"
        );
        let mut accepted = accepted_rows(&outcomes);

        ballast.remove();
        h.storage().disk().refresh().expect("measure the free disk");
        // Batch numbers go on from the first call: a repeated number is a re-send of the same
        // spans, which the read dedupes.
        let first_number = u16::try_from(outcomes.len()).expect("fewer than 65536 batches");
        let more = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(first_number + batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |_| true,
            1,
        );
        assert!(
            more.iter().all(Result::is_ok),
            "with space freed, the next filler append is Ok: {more:?}"
        );
        accepted += accepted_rows(&more);

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the filler rows read back are the rows of every Ok append"
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn a_full_disk_fails_the_wal_and_a_restart_recovers() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D2, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::b(0);
            config.flush.max_bytes = ByteSize::mib(256);
        });
        let mut model = new_model(&h);
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        let mut ballast = Ballast::leave_free(root, 8 * MIB);
        h.storage().disk().refresh().expect("measure the free disk");
        let outcomes = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |outcome| outcome.is_err(),
            400,
        );
        let last = outcomes.last().expect("at least one filler append");
        assert!(
            matches!(last, Err(AppendError::WalFailed)),
            "with min_free 0 the WAL meets ENOSPC itself: WalFailed, got {last:?}"
        );
        assert!(
            h.storage().health().spans_wal_failed,
            "health reports the spans WAL failed"
        );
        let acknowledged = accepted_rows(&outcomes);

        // The failure is final until a restart, even with space back.
        ballast.resize_to_leave(32 * MIB);
        h.storage().disk().refresh().expect("measure the free disk");
        let later = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |_| false,
            1,
        );
        assert!(
            matches!(later.as_slice(), [Err(AppendError::WalFailed)]),
            "an append after space is freed is still WalFailed: {later:?}"
        );

        // A reopen while the disk is nearly full. Opening writes nothing that needs space, so Ok is
        // expected; any other OpenError but Config and Locked is accepted, and recorded.
        ballast.resize_to_leave(64 * 1024);
        h.storage().disk().refresh().expect("measure the free disk");
        let reopened = h.crash_then_try_open();
        match &reopened {
            Ok(()) => println!("reopen on a nearly full disk: Ok"),
            Err(error) => {
                println!("reopen on a nearly full disk: {error:?}");
                assert!(
                    !matches!(error, OpenError::Config(_) | OpenError::Locked { .. }),
                    "a full disk is not a config error and not a lock: {error:?}"
                );
            }
        }
        ballast.remove();
        if reopened.is_err() {
            h.open_again();
        }
        h.storage().disk().refresh().expect("measure the free disk");
        assert!(
            !h.storage().health().spans_wal_failed,
            "a reopened life starts with a healthy WAL"
        );

        check_full(&h, &mut model, 500);
        let rows = filler_rows(&h);
        assert!(
            (acknowledged..=acknowledged + 64).contains(&rows),
            "replay returns the acknowledged filler rows ({acknowledged}), plus at most the one \
             failed batch of 64 rows; found {rows}"
        );

        h.restart();
        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn put_media_on_a_full_disk_fails_and_leaves_nothing() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D8, fs, |root| {
        // Refusal under min_free: the blob is not written.
        {
            let mut h = harness_in(root, |config| {
                config.min_free_bytes = ByteSize::mib(2);
            });
            let mut model = new_model(&h);
            let mut clock = clock_start();
            model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

            let ballast = Ballast::leave_free(root, MIB);
            h.storage().disk().refresh().expect("measure the free disk");
            let (blob, _) = media_blob(512 * 1024);
            let refused = h.block_on(h.storage().put_media(P::A.id(), blob));
            assert!(
                matches!(refused, Err(MediaError::DiskFull)),
                "put_media under min_free is DiskFull, got {refused:?}"
            );
            ballast.remove();
        }

        // ENOSPC from the write itself.
        let mut h = harness_in(root, |_| {});
        let mut model = new_model(&h);
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        let ballast = Ballast::leave_free(root, MIB);
        h.storage().disk().refresh().expect("measure the free disk");
        let (blob, payload) = media_blob(3 * 1024 * 1024);
        let sha256 = blob.sha256;
        let sha_hex = hex::encode(sha256);
        let path = Objects::media_path(P::A.id(), &sha_hex);
        let failed = h.block_on(h.storage().put_media(P::A.id(), blob));
        match &failed {
            Err(MediaError::Object(ObjectError::Io { source, .. })) => assert_eq!(
                source.kind(),
                ErrorKind::StorageFull,
                "the write meets ENOSPC"
            ),
            other => panic!("a 3 MiB put on 1 MiB free is an object I/O error, got {other:?}"),
        }
        let read = h.block_on(h.storage().get_media(P::A.id(), &sha256, None));
        assert!(
            matches!(read, Err(MediaError::NotFound)),
            "a failed put leaves no media row"
        );
        let staged = h
            .block_on(h.storage().staged_paths())
            .expect("list the staged objects");
        assert!(
            staged.contains(&path),
            "the failed put staged {path}, which GC must collect"
        );
        h.block_on(h.storage().collect_staged(Duration::ZERO))
            .expect("collect the staged objects");
        let staged = h
            .block_on(h.storage().staged_paths())
            .expect("list the staged objects");
        assert!(
            !staged.contains(&path),
            "GC removed the staged row of the failed put"
        );

        // Retry with space back: the same put is Ok and reads back the same bytes.
        ballast.remove();
        h.storage().disk().refresh().expect("measure the free disk");
        let (retry, _) = media_blob(3 * 1024 * 1024);
        h.block_on(h.storage().put_media(P::A.id(), retry))
            .expect("the same put with space free is Ok");
        let read = h
            .block_on(h.storage().get_media(P::A.id(), &sha256, None))
            .expect("the blob reads back");
        assert_eq!(read.bytes, payload, "the blob reads back the same bytes");

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);

        // Expected to fail until R5.5: the failed object write leaves its temp file behind.
        let leftovers = temp_files(&h.dir().join("objects"));
        assert!(
            leftovers.is_empty(),
            "a failed put left {} temp file(s) under objects/ (expected until R5.5): {leftovers:?}",
            leftovers.len()
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn running_out_of_inodes_is_an_error_not_a_panic() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: Some(4096),
    };
    on_small_fs(D9, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::b(0);
            config.flush.max_bytes = ByteSize::mib(1);
        });
        let mut model = new_model(&h);
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        let inodes = InodeBallast::leave_free(root, 4);
        let outcomes = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(batch, 32, 4 * 1024, Text::Words, 0, clock)
            },
            Duration::from_millis(20),
            |outcome| outcome.is_err(),
            400,
        );
        let first_error = outcomes.iter().find_map(|outcome| outcome.as_ref().err());
        println!("the first append error with no inodes left: {first_error:?}");
        assert!(
            matches!(first_error, None | Some(AppendError::WalFailed)),
            "running out of inodes ends in WalFailed or in no error, got {first_error:?}"
        );
        assert!(free_bytes(root) > 0, "bytes are free: only inodes ran out");
        assert!(
            temp_files(&h.dir().join("objects")).is_empty(),
            "a flush that could not create its object left no temp file"
        );
        let acknowledged = accepted_rows(&outcomes);

        inodes.remove();
        h.crash_then_try_open()
            .expect("the reopen after the inodes are freed is Ok");
        h.storage().disk().refresh().expect("measure the free disk");
        check_full(&h, &mut model, 500);
        let rows = filler_rows(&h);
        assert!(
            (acknowledged..=acknowledged + 32).contains(&rows),
            "replay returns the acknowledged filler rows ({acknowledged}), plus at most the one \
             failed batch of 32 rows; found {rows}"
        );

        h.restart();
        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
    });
}
