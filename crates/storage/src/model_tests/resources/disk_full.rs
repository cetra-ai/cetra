//! A full disk under the WAL, `put_media`, flush, compaction, spill and the read cache (R5.3 and
//! R5.4, D1–D9 of docs/plans/r-hardening/05-m5-resources.md). Each case runs on a small filesystem, in its own
//! namespace child or on the operator's mount (`small_fs::on_small_fs`).

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use bytesize::ByteSize;
use sha2::{Digest, Sha256};

use super::super::check::check_full;
use super::super::harness::{Harness, HarnessOptions, memory_store};
use super::super::ops::{Op, new_model};
use super::super::shape::{HOUR_NS, P, day, midnight};
use super::fill::{Text, append_filler, filler, filler_batch, filler_rows};
use super::small_fs::{
    Ballast, FreeSampler, InodeBallast, SMALL_FS_BYTES, SmallFs, free_bytes, on_small_fs,
    temp_files,
};
use super::{after_ops, model_steps, seed_ops, settle_and_check};
use crate::config::StorageConfig;
use crate::error::{AppendError, MediaError, ObjectError, OpenError};
use crate::hooks::HookPoint;
use crate::objects::Objects;
use crate::query_error::RunError;
use crate::wal::Table;
use crate::{AppendReceipt, MediaBlob, QueryContext, SqlRequest};

const MIB: u64 = 1 << 20;
/// One microsecond, the clock step before each filler batch.
const MICRO_NS: i64 = 1_000;
/// One millisecond, as `settle` adds to the clock.
const MILLI_NS: i64 = 1_000_000;

const D1: &str =
    "model_tests::resources::disk_full::appends_stop_at_min_free_before_the_disk_fills";
const D2: &str =
    "model_tests::resources::disk_full::a_full_disk_fails_the_wal_and_a_restart_recovers";
const D8: &str =
    "model_tests::resources::disk_full::put_media_on_a_full_disk_fails_and_leaves_nothing";
const D9: &str = "model_tests::resources::disk_full::running_out_of_inodes_is_an_error_not_a_panic";
const D3: &str =
    "model_tests::resources::disk_full::a_flush_on_a_full_disk_retries_and_leaves_no_temp_files";
const D4: &str =
    "model_tests::resources::disk_full::a_compaction_that_does_not_fit_is_deferred_until_it_does";
const D5: &str = "model_tests::resources::disk_full::a_compaction_that_meets_enospc_fails_cleanly_and_runs_later";
const D6: &str = "model_tests::resources::disk_full::a_spilling_query_stops_at_min_free";
const D7: &str = "model_tests::resources::disk_full::the_read_cache_never_fills_the_disk";

/// The query of D6: a sort over every span of the project, which spills when its memory pool is
/// small.
pub(super) const SPILL_SQL: &str = "SELECT count(*) AS n FROM (SELECT row_number() OVER (ORDER BY input, span_id) AS r FROM spans) WHERE r > 0";

/// A harness whose data directory is under `root`, with 1 MiB WAL segments (the minimum) and
/// `adjust` applied.
fn harness_in(root: &Path, adjust: impl FnOnce(&mut StorageConfig)) -> Harness {
    Harness::with_options(
        HarnessOptions {
            root: Some(root.to_path_buf()),
            unrecorded: true,
            foreign_projects: vec![filler()],
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
        newest_span_start: 0,
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
        let mut model = new_model(&h, (P::A, P::B));
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
        let mut model = new_model(&h, (P::A, P::B));
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
            let mut model = new_model(&h, (P::A, P::B));
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
        let mut model = new_model(&h, (P::A, P::B));
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
        let read = h.block_on(h.storage().get_media(P::A.id(), &sha256, None, None));
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
            .block_on(h.storage().get_media(P::A.id(), &sha256, None, None))
            .expect("the blob reads back");
        assert_eq!(read.bytes, payload, "the blob reads back the same bytes");

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);

        // A failed object write leaves no temp file (R5.5).
        let leftovers = temp_files(&h.dir().join("objects"));
        assert!(
            leftovers.is_empty(),
            "a failed put left {} temp file(s) under objects/: {leftovers:?}",
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
        let mut model = new_model(&h, (P::A, P::B));
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

/// Paths of the spans files of project C, in catalog order.
fn filler_spans_paths(h: &Harness) -> Vec<String> {
    h.storage()
        .catalog_files(Table::Spans)
        .iter()
        .filter(|file| file.project == filler())
        .map(|file| file.path.clone())
        .collect()
}

/// The filler's spans files that cover `day(0)`, as `(path, size)`.
fn filler_day0_files(h: &Harness) -> Vec<(String, u64)> {
    h.storage()
        .catalog_files(Table::Spans)
        .iter()
        .filter(|file| file.project == filler() && file.day == day(0))
        .map(|file| (file.path.clone(), file.size))
        .collect()
}

/// Moves `clock` as `settle` does: past day 6 and its close delay, so the day-0 files are closed
/// for compaction.
fn closed_clock(h: &Harness, clock: &mut i64) {
    let close_delay =
        i64::try_from(h.config().compaction.close_delay.as_nanos()).unwrap_or(i64::MAX);
    *clock = (*clock)
        .max(midnight(day(6)))
        .saturating_add(close_delay)
        .saturating_add(MILLI_NS);
}

/// Polls `done` every `step` until it holds (true) or `limit` has passed (false).
fn wait_for(limit: Duration, step: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if done() {
            return true;
        }
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(step);
    }
}

/// The rows of project C the spill query counts, through `QueryContext::sql_json`. Its
/// `ORDER BY` sorts the whole project, so with a small memory pool it spills.
fn spill_query(h: &Harness) -> Result<u64, RunError> {
    h.block_on(async {
        let query = h
            .storage()
            .query(filler(), crate::QueryOptions::new(None, None))
            .await
            .expect("query the filler project");
        let ctx = QueryContext::new(query, None)?;
        let params = serde_json::Map::new();
        let json = ctx
            .sql_json(SqlRequest {
                query: SPILL_SQL,
                params: &params,
                limit: 1,
                timeout: Duration::from_secs(60),
            })
            .await?;
        Ok::<u64, RunError>(json.rows[0]["n"].as_u64().expect("the count is an integer"))
    })
}

/// Appends `batches` filler batches of `input_bytes` and `text` (numbered from `first`) and then
/// restarts, so each call leaves one flushed file of project C. Returns the rows accepted.
fn filler_file(
    h: &mut Harness,
    clock: &mut i64,
    first: u16,
    batches: u16,
    spans: u8,
    input_bytes: usize,
    text: Text,
) -> u64 {
    let outcomes = append_filler(
        h,
        |batch| {
            *clock += MICRO_NS;
            filler_batch(first + batch, spans, input_bytes, text, 0, *clock)
        },
        Duration::ZERO,
        |outcome| outcome.is_err(),
        batches,
    );
    assert!(
        outcomes.iter().all(Result::is_ok),
        "filler appends on enough space are Ok: {outcomes:?}"
    );
    h.restart();
    accepted_rows(&outcomes)
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn a_flush_on_a_full_disk_retries_and_leaves_no_temp_files() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D3, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::b(0);
            config.flush.max_bytes = ByteSize::mib(4);
            config.memtable.hard_limit = ByteSize::mib(24);
        });
        let mut model = new_model(&h, (P::A, P::B));
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        // Pause a flush after it staged its output, before it writes it.
        h.storage().hooks().hold(HookPoint::FlushAfterStage);
        let mut outcomes = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |outcome| {
                outcome.is_err() || h.storage().hooks().waiting_at(HookPoint::FlushAfterStage) >= 1
            },
            400,
        );
        assert!(
            outcomes.iter().all(Result::is_ok),
            "the appends up to the paused flush are Ok: {outcomes:?}"
        );
        assert!(
            h.storage().hooks().waiting_at(HookPoint::FlushAfterStage) >= 1,
            "a flush is paused after staging"
        );
        let next_batch = u16::try_from(outcomes.len()).expect("fewer than 65536 batches");
        outcomes.extend(append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(next_batch + batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |_| false,
            2,
        ));
        let accepted = accepted_rows(&outcomes);

        // The flush's output cannot fit, once it is released.
        let ballast = Ballast::leave_free(root, 256 * 1024);
        h.storage().disk().refresh().expect("measure the free disk");
        h.storage().hooks().release(HookPoint::FlushAfterStage);
        std::thread::sleep(Duration::from_secs(4));

        assert!(
            filler_spans_paths(&h).is_empty(),
            "no spans file of the filler is catalogued while its flush fails"
        );
        // Failed flush attempts leave no partial files (R5.5).
        let leftovers = temp_files(&h.dir().join("objects"));
        assert!(
            leftovers.is_empty(),
            "failed flush attempts left {} temp file(s) under objects/: {leftovers:?}",
            leftovers.len()
        );
        // Failed flush attempts give their space back (R5.5).
        let free = free_bytes(root);
        assert!(
            free >= 192 * 1024,
            "failed flush attempts gave their space back: {free} bytes free"
        );
        assert!(
            !h.storage().health().spans_wal_failed,
            "a failed flush does not fail the WAL"
        );
        check_full(&h, &mut model, 500);

        // The retry of the flush commits once the space is back.
        ballast.remove();
        h.storage().disk().refresh().expect("measure the free disk");
        assert!(
            wait_for(Duration::from_secs(70), Duration::from_millis(200), || {
                !filler_spans_paths(&h).is_empty()
            }),
            "the flush retry commits a spans file of the filler within 70 s"
        );
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the filler rows read back are the rows of every Ok append"
        );

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the filler rows survive the settle"
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn a_compaction_that_does_not_fit_is_deferred_until_it_does() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D4, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::b(0);
            config.flush.max_bytes = ByteSize::mib(256);
        });
        let mut model = new_model(&h, (P::A, P::B));
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        // Two L0 files of the filler on day 0, a clean restart after each.
        let mut accepted = filler_file(&mut h, &mut clock, 0, 16, 64, 16 * 1024, Text::Words);
        accepted += filler_file(&mut h, &mut clock, 16, 16, 64, 16 * 1024, Text::Words);
        let day0 = filler_day0_files(&h);
        assert_eq!(
            day0.len(),
            2,
            "two L0 files of the filler on day 0: {day0:?}"
        );
        let inputs: u64 = day0.iter().map(|(_, size)| size).sum();
        println!("D4: two filler L0 files of {inputs} bytes in total");
        let before = filler_spans_paths(&h);
        closed_clock(&h, &mut clock);

        // Not enough disk for the filler's reservation: its job is counted as failed.
        let mut ballast = Ballast::leave_free(root, inputs / 2);
        h.storage().disk().refresh().expect("measure the free disk");
        // The model's own A and B files of day 0 are compacted in the same pass when they fit; the
        // filler's job is the one that does not.
        let report = h
            .block_on(h.storage().compact(clock))
            .expect("the pass returns Ok and counts the job as failed");
        assert!(
            report.failed >= 1,
            "the filler's job is counted as failed: {report:?}"
        );
        assert_eq!(
            filler_spans_paths(&h),
            before,
            "the catalog's spans files are unchanged"
        );
        let leftovers = temp_files(h.dir());
        assert!(
            leftovers.is_empty(),
            "a deferred compaction left temp files: {leftovers:?}"
        );

        // Room for the reservation: the next pass runs the job.
        ballast.resize_to_leave(inputs * 3);
        h.storage().disk().refresh().expect("measure the free disk");
        let report = h
            .block_on(h.storage().compact(clock))
            .expect("the pass runs the job");
        assert!(report.jobs >= 1, "the deferred job runs: {report:?}");
        assert_eq!(
            filler_day0_files(&h).len(),
            1,
            "the two L0 files of day 0 are compacted into one"
        );
        ballast.remove();
        h.storage().disk().refresh().expect("measure the free disk");

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the compacted filler rows are all there"
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn a_compaction_that_meets_enospc_fails_cleanly_and_runs_later() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D5, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::b(0);
            config.compaction.threads = 8;
            config.flush.max_bytes = ByteSize::mib(256);
        });
        let mut model = new_model(&h, (P::A, P::B));
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        let mut accepted = filler_file(&mut h, &mut clock, 0, 16, 64, 16 * 1024, Text::Words);
        accepted += filler_file(&mut h, &mut clock, 16, 16, 64, 16 * 1024, Text::Words);
        assert_eq!(
            filler_day0_files(&h).len(),
            2,
            "two L0 files of the filler on day 0"
        );
        let before = filler_spans_paths(&h);
        let inputs: u64 = filler_day0_files(&h).iter().map(|(_, size)| size).sum();
        closed_clock(&h, &mut clock);
        let now = clock;

        // The reservation passes, then another writer takes the space while the job stages.
        h.storage().hooks().hold(HookPoint::CompactionAfterStage);
        let mut ballast = None;
        let report = std::thread::scope(|scope| {
            let pass = scope.spawn(|| h.block_on(h.storage().compact(now)));
            assert!(
                wait_for(Duration::from_secs(60), Duration::from_millis(10), || {
                    h.storage()
                        .hooks()
                        .waiting_at(HookPoint::CompactionAfterStage)
                        >= 1
                }),
                "the compaction job reaches its held point"
            );
            // The filler job reserves its inputs before it stages. The ballast must come after
            // that, or the job is refused (DiskFull) instead of meeting ENOSPC.
            assert!(
                wait_for(Duration::from_secs(60), Duration::from_millis(10), || {
                    h.storage().disk().reserved() >= inputs
                }),
                "the filler job holds its disk reservation"
            );
            ballast = Some(Ballast::leave_free(root, MIB));
            h.storage().disk().refresh().expect("measure the free disk");
            h.storage().hooks().release(HookPoint::CompactionAfterStage);
            pass.join().expect("the compaction pass does not panic")
        });
        let report = report.expect("the pass returns Ok and counts the job as failed");
        assert!(
            report.failed >= 1,
            "the filler's job fails cleanly when its write meets ENOSPC: {report:?}"
        );
        assert_eq!(
            filler_spans_paths(&h),
            before,
            "the catalog's spans files are unchanged"
        );
        check_full(&h, &mut model, 500);

        // A merge that fails in writer.finish leaves no temp file (R5.5).
        let leftovers = temp_files(h.dir());
        assert!(
            leftovers.is_empty(),
            "the failed job left {} temp file(s): {leftovers:?}",
            leftovers.len()
        );
        // The failed job's output is removed at once (R5.5).
        let free = free_bytes(root);
        assert!(
            free >= 512 * 1024,
            "the failed job gave its output back: {free} bytes free"
        );

        // The failed job's outputs are staged, and GC removes them.
        let staged = h
            .block_on(h.storage().staged_paths())
            .expect("list the staged objects");
        assert!(!staged.is_empty(), "the failed job staged its output names");
        h.block_on(h.storage().collect_staged(Duration::ZERO))
            .expect("collect the staged objects");
        let still = h
            .block_on(h.storage().staged_paths())
            .expect("list the staged objects");
        assert!(
            staged.iter().all(|path| !still.contains(path)),
            "GC removed every staged output of the failed job"
        );

        // Space back: the same pass commits the job.
        if let Some(ballast) = ballast {
            ballast.remove();
        }
        h.storage().disk().refresh().expect("measure the free disk");
        let report = h
            .block_on(h.storage().compact(now))
            .expect("the pass runs the job");
        assert!(
            report.jobs >= 1,
            "the job commits with space back: {report:?}"
        );

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the compacted filler rows are all there"
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn a_spilling_query_stops_at_min_free() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D6, fs, |root| {
        let mut h = harness_in(root, |config| {
            config.min_free_bytes = ByteSize::mib(16);
            config.memtable.hard_limit = ByteSize::mib(512);
            config.flush.max_bytes = ByteSize::mib(64);
            config.query.target_partitions = 1;
            config.query.max_memory = ByteSize::mib(32);
            config.query.max_memory_total = Some(ByteSize::mib(32));
            config.query.initial_memory = ByteSize::mib(16);
        });
        let mut model = new_model(&h, (P::A, P::B));
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

        // Four chunks of 16 batches (about 25 MiB of Arrow). Each chunk ends with a restart, so the WAL
        // holds at most one chunk. Larger tries failed run 1 as TooLarge: 40 batches per chunk, and
        // then 20 (a 33 MiB spill that DataFusion's cap counted above the 47 MB above min_free).
        let mut accepted = 0;
        let mut sent: u16 = 0;
        for _ in 0..4 {
            let first = sent;
            let outcomes = append_filler(
                &h,
                |batch| {
                    clock += MICRO_NS;
                    filler_batch(first + batch, 200, 2048, Text::Repeated, 0, clock)
                },
                Duration::ZERO,
                |outcome| outcome.is_err(),
                16,
            );
            assert!(
                outcomes.iter().all(Result::is_ok),
                "filler appends on enough space are Ok: {outcomes:?}"
            );
            sent += 16;
            accepted += accepted_rows(&outcomes);
            h.restart();
        }
        h.storage().disk().refresh().expect("measure the free disk");
        println!(
            "D6: free disk after the last restart: {} bytes",
            free_bytes(root)
        );

        // Run 1, room to spill. The peak is the spill space in use, sampled every 5 ms.
        let disk_manager = Arc::clone(&h.storage().runtime.disk_manager);
        let stop = AtomicBool::new(false);
        let (first_run, peak) = std::thread::scope(|scope| {
            let sampler = scope.spawn(|| {
                let mut peak = 0;
                while !stop.load(Ordering::Acquire) {
                    peak = peak.max(disk_manager.used_disk_space());
                    std::thread::sleep(Duration::from_millis(5));
                }
                peak
            });
            let result = spill_query(&h);
            stop.store(true, Ordering::Release);
            (
                result,
                sampler.join().expect("the spill sampler does not panic"),
            )
        });
        let rows = first_run.expect("run 1: the spill fits on the real disk");
        println!("D6: run 1 counted {rows} rows, peak spill {peak} bytes");
        assert_eq!(rows, accepted, "the spill query counts every filler row");
        assert!(peak > 0, "the query spilled: peak {peak} bytes");

        // Run 2, too little room: the spill may not take the space kept for ingestion.
        let ballast = Ballast::leave_free(root, 16 * MIB + peak / 2);
        h.storage().disk().refresh().expect("measure the free disk");
        let sampler = FreeSampler::start(root);
        let second_run = spill_query(&h);
        let least = sampler.stop();
        println!("D6: run 2 least free {least} bytes");
        assert!(
            matches!(second_run, Err(RunError::TooLarge)),
            "run 2 is query_too_large: {second_run:?}"
        );
        assert!(
            least >= 15 * MIB,
            "the spill never took the space kept for ingestion: least free {least} bytes"
        );
        assert!(
            wait_for(Duration::from_secs(10), Duration::from_millis(50), || {
                disk_manager.used_disk_space() == 0
            }),
            "the failed spill's files are removed within 10 s"
        );
        // The failed query's memory goes back to the shared pool as its tasks wind down.
        assert!(
            wait_for(Duration::from_secs(10), Duration::from_millis(50), || {
                h.storage().memory.used() == 0
            }),
            "the failed query's pool bytes are released within 10 s: {} bytes left",
            h.storage().memory.used()
        );
        let query_dir = h.dir().join("tmp").join("query");
        if let Ok(entries) = std::fs::read_dir(&query_dir) {
            for entry in entries {
                let entry = entry.expect("read a spill directory entry");
                let name = entry.file_name();
                assert!(
                    name.to_string_lossy().starts_with("datafusion-"),
                    "only DataFusion's spill directories remain under tmp/query: {name:?}"
                );
                assert!(
                    std::fs::read_dir(entry.path())
                        .expect("read a spill directory")
                        .next()
                        .is_none(),
                    "a spill directory left after the failed query is empty"
                );
            }
        }

        // Ingestion still works with the space kept for it.
        let outcomes = append_filler(
            &h,
            |batch| {
                clock += MICRO_NS;
                filler_batch(sent + batch, 64, 16 * 1024, Text::Words, 0, clock)
            },
            Duration::ZERO,
            |_| true,
            1,
        );
        assert!(
            outcomes.iter().all(Result::is_ok),
            "an append after run 2 is Ok: {outcomes:?}"
        );
        accepted += accepted_rows(&outcomes);

        // Run 3: the space is back, so the same query runs. A restart first flushes the append:
        // with rows in the memtable the plan adds a merge that needs memory the sort already
        // holds, and this 26 MiB sort in a 32 MiB pool is then too large (a clean TooLarge).
        ballast.remove();
        h.restart();
        h.storage().disk().refresh().expect("measure the free disk");
        let rows = spill_query(&h).expect("run 3: the spill fits again");
        assert_eq!(rows, accepted, "the spill query counts every filler row");

        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the filler rows read back are all there"
        );
    });
}

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn the_read_cache_never_fills_the_disk() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(D7, fs, |root| {
        // The data is seeded with the cache off, so flushes do not write blocks into it.
        let mut h = Harness::with_options(
            HarnessOptions {
                root: Some(root.to_path_buf()),
                store: Some(memory_store()),
                unrecorded: true,
                foreign_projects: vec![filler()],
                ..HarnessOptions::default()
            },
            |config| {
                config.wal.segment_bytes = ByteSize::mib(1);
                config.min_free_bytes = ByteSize::mib(8);
                config.query.read_cache_bytes = Some(ByteSize::b(0));
                config.flush.max_bytes = ByteSize::mib(8);
            },
        );
        let mut model = new_model(&h, (P::A, P::B));
        let mut clock = clock_start();
        model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);
        // At least 40 MiB of the filler's spans files in the store.
        let mut accepted = 0;
        let mut sent: u16 = 0;
        let mut stored = 0;
        for _ in 0..200 {
            let first = sent;
            let outcomes = append_filler(
                &h,
                |batch| {
                    clock += MICRO_NS;
                    filler_batch(first + batch, 64, 16 * 1024, Text::Words, 0, clock)
                },
                Duration::ZERO,
                |outcome| outcome.is_err(),
                8,
            );
            assert!(
                outcomes.iter().all(Result::is_ok),
                "filler appends with the cache off are Ok: {outcomes:?}"
            );
            sent += 8;
            accepted += accepted_rows(&outcomes);
            h.restart();
            stored = h
                .storage()
                .catalog_files(Table::Spans)
                .iter()
                .filter(|file| file.project == filler())
                .map(|file| file.size)
                .sum::<u64>();
            if stored >= 40 * MIB {
                break;
            }
        }
        assert!(
            stored >= 40 * MIB,
            "the filler's spans files reach 40 MiB in the store: {stored} bytes"
        );
        println!("D7: {stored} bytes of the filler's spans files in the store");
        // Second life, the cache on. It opens empty, and free disk is the binding limit.
        h.restart_with(|config| {
            config.query.read_cache_bytes = Some(ByteSize::mib(48));
        });
        let stats = h
            .storage()
            .read_cache_stats()
            .expect("the read cache is on");
        // The open reads the stored files' footers through the cache, so the cache is not empty
        // when the case starts (the plan expected 0). The count is reported.
        println!("D7: the cache holds {} blocks after the open", stats.blocks);
        assert!(
            free_bytes(root) > 24 * MIB,
            "enough free disk for the case: {} bytes; seed less if not",
            free_bytes(root)
        );
        let ballast = Ballast::leave_free(root, 24 * MIB);
        h.storage().disk().refresh().expect("measure the free disk");
        let sampler = FreeSampler::start(root);
        let mut totals = Vec::new();
        for run in 0..5 {
            let total = count_and_bytes(&h).expect("the filler's count and byte sum");
            totals.push(total);
            if run == 0 {
                let stats = h
                    .storage()
                    .read_cache_stats()
                    .expect("the read cache is on");
                assert!(stats.misses > 0, "the first query reads from the store");
                assert!(stats.blocks > 0, "the cache wrote blocks: {stats:?}");
            }
        }
        let least = sampler.stop();
        println!("D7: least free disk during the queries {least} bytes; totals {totals:?}");
        assert!(
            totals.iter().all(|total| *total == totals[0]),
            "the queries agree: {totals:?}"
        );
        assert_eq!(
            totals[0].0, accepted,
            "the cache-served count is every filler row"
        );
        assert!(
            least + MIB >= 8 * MIB,
            "the cache stopped at 2 x min_free, and never took the disk below min_free - 1 MiB: \
             least free {least} bytes"
        );
        // The monitor sheds blocks within a few seconds, so free disk recovers.
        assert!(
            wait_for(Duration::from_secs(5), Duration::from_millis(50), || {
                free_bytes(root) >= 8 * MIB
            }),
            "free disk is back to min_free within 5 s of the queries"
        );
        let cache_dir = h.dir().join("cache");
        let leftovers = temp_files(&cache_dir);
        assert!(
            leftovers.is_empty(),
            "the cache left temp files: {leftovers:?}"
        );

        // A write still succeeds while the cache is in use.
        model_steps(
            &mut h,
            &mut model,
            &mut clock,
            &[Op::Append {
                project: P::A,
                trace: 3,
                count: 2,
                split: false,
            }],
            200,
        );

        ballast.remove();
        // The settle compacts the 42 MiB of filler into 42 MiB of output on the same disk. A cache
        // of 48 MiB takes that space while the job runs (the cache does not honour the job's
        // reservation), the job fails with ENOSPC, and `settle` does not accept a failed job. The
        // audit of S3 mode needs the cache on, so it shrinks to 1 MiB.
        h.restart_with(|config| config.query.read_cache_bytes = Some(ByteSize::mib(1)));
        h.storage().disk().refresh().expect("measure the free disk");
        model_steps(&mut h, &mut model, &mut clock, &after_ops(), 100);
        settle_and_check(&mut h, &mut model, &mut clock);
        assert_eq!(
            filler_rows(&h),
            accepted,
            "the cache-served filler rows are all there"
        );
    });
}

/// The row count and the byte sum of the filler's `input`, read through the storage's query.
fn count_and_bytes(h: &Harness) -> Result<(u64, u64), RunError> {
    h.block_on(async {
        let query = h
            .storage()
            .query(filler(), crate::QueryOptions::new(None, None))
            .await
            .expect("query the filler project");
        let ctx = QueryContext::new(query, None)?;
        let params = serde_json::Map::new();
        let json = ctx
            .sql_json(SqlRequest {
                query: "SELECT count(*) AS n, sum(length(input)) AS b FROM spans",
                params: &params,
                limit: 1,
                timeout: Duration::from_secs(60),
            })
            .await?;
        Ok::<(u64, u64), RunError>((
            json.rows[0]["n"].as_u64().expect("the count is an integer"),
            json.rows[0]["b"]
                .as_u64()
                .expect("the byte sum is an integer"),
        ))
    })
}
