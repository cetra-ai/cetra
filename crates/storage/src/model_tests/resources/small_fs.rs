//! The small filesystem of the resource tests (R5.1): a tmpfs in a private user and mount
//! namespace, or a directory the operator mounted (`LLMOBS_TEST_SMALL_FS`). The case code gets
//! only the root path, and never knows which of the two it runs on.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::super::harness::{Harness, HarnessOptions};
use super::super::ops::new_model;
use super::super::shape::{HOUR_NS, P, day, midnight};
use super::{model_steps, seed_ops, settle_and_check};

/// Bytes of the namespace tmpfs.
pub(super) const SMALL_FS_BYTES: u64 = 64 << 20;

const MIB: u64 = 1 << 20;
/// `LLMOBS_TEST_SMALL_FS` must have at least this much free: ballast writes would be too slow.
const MIN_FREE: u64 = 48 * MIB;
const MAX_FREE: u64 = 1 << 30;
/// Set in the namespace child, which runs the case on its mount.
const CHILD_ENV: &str = "LLMOBS_SMALL_FS_CHILD";
/// `size=…,mode=0700`, and `nr_inodes=…` when set, for the child's mount.
const OPTIONS_ENV: &str = "LLMOBS_SMALL_FS_OPTIONS";
const MOUNTED_ENV: &str = "LLMOBS_TEST_SMALL_FS";
const CHILD_TIMEOUT: Duration = Duration::from_secs(300);
/// The inode ballast gives up past this many files, so a bad `nr_inodes` cannot run for hours.
const MAX_INODE_BALLAST: u64 = 1 << 22;

/// Options of the namespace tmpfs.
#[derive(Debug, Clone, Copy)]
pub(super) struct SmallFs {
    pub(super) bytes: u64,
    /// `nr_inodes=`; `None` keeps tmpfs's default.
    pub(super) inodes: Option<u64>,
}

/// Runs `case` with the root of a small, empty filesystem.
///
/// - `LLMOBS_SMALL_FS_CHILD` set: this process is the namespace child; `case` runs on that mount.
/// - `LLMOBS_TEST_SMALL_FS` set: `case` runs in a new directory under it.
/// - Neither: re-executes the test `name` inside `unshare -rm` over a fresh tmpfs and asserts that
///   it exited successfully.
pub(super) fn on_small_fs(name: &str, fs: SmallFs, case: impl FnOnce(&Path)) {
    if let Some(mount) = std::env::var_os(CHILD_ENV) {
        case(Path::new(&mount));
        return;
    }
    if let Some(root) = std::env::var_os(MOUNTED_ENV) {
        let dir = tempfile::TempDir::new_in(&root)
            .unwrap_or_else(|error| panic!("{MOUNTED_ENV}: temp dir under it: {error}"));
        let free = free_bytes(dir.path());
        assert!(
            (MIN_FREE..=MAX_FREE).contains(&free),
            "{MOUNTED_ENV} must have 48 MiB to 1 GiB free, found {free} bytes"
        );
        case(dir.path());
        return;
    }
    run_in_namespace(name, fs);
}

/// Re-executes `name` in a child that mounts a tmpfs over a directory of its own, and asserts
/// that the child passed.
fn run_in_namespace(name: &str, fs: SmallFs) {
    let mount = tempfile::tempdir().expect("a mount point for the small filesystem");
    let mut options = format!("size={},mode=0700", fs.bytes);
    if let Some(inodes) = fs.inodes {
        options.push_str(&format!(",nr_inodes={inodes}"));
    }
    let exe = std::env::current_exe().expect("the path of the test binary");
    let mut child = Command::new("unshare")
        .args([
            "-rm",
            "sh",
            "-c",
            r#"mount -t tmpfs -o "$LLMOBS_SMALL_FS_OPTIONS" tmpfs "$LLMOBS_SMALL_FS_CHILD" && exec "$0" "$@""#,
        ])
        .arg(&exe)
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, mount.path())
        .env(OPTIONS_ENV, options)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| {
            panic!(
                "unshare -rm failed to start ({error}): enable unprivileged user namespaces, or \
                 set {MOUNTED_ENV} to a small mounted filesystem"
            )
        });
    let stdout_pipe = child.stdout.take().expect("piped stdout of the child");
    let stderr_pipe = child.stderr.take().expect("piped stderr of the child");
    let stdout_reader = thread::spawn(move || read_all(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_all(stderr_pipe));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => panic!("wait for the namespace child of {name}: {error}"),
        }
        if started.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the namespace child of {name} ran past {CHILD_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_reader.join().expect("the stdout reader");
    let stderr = stderr_reader.join().expect("the stderr reader");
    print!("{stdout}");
    eprint!("{stderr}");

    // A name that matches nothing passes with 0 tests and exit code 0, so the output must show
    // that the test ran and passed.
    assert!(
        !stdout.contains("running 0 tests"),
        "no test is named {name}: a typo would pass unnoticed"
    );
    if !stdout.contains("running 1 test") {
        panic!(
            "unshare -rm failed before the test started ({status}): enable unprivileged user \
             namespaces, or set {MOUNTED_ENV} to a small mounted filesystem. The child's stderr \
             is above."
        );
    }
    assert!(
        status.success() && stdout.contains("test result: ok. 1 passed"),
        "the namespace child of {name} failed ({status}); its output is above"
    );
}

fn read_all(mut pipe: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `f_bavail * f_frsize` of the filesystem holding `path`.
pub(super) fn free_bytes(path: &Path) -> u64 {
    let stat = rustix::fs::statvfs(path)
        .unwrap_or_else(|error| panic!("statvfs of the small filesystem: {error}"));
    stat.f_bavail * stat.f_frsize
}

/// `f_favail` of the filesystem holding `path`.
pub(super) fn free_inodes(path: &Path) -> u64 {
    rustix::fs::statvfs(path)
        .unwrap_or_else(|error| panic!("statvfs of the small filesystem: {error}"))
        .f_favail
}

const BALLAST: &str = "ballast";

/// A file of zeros that takes space until it is removed or shrunk.
pub(super) struct Ballast {
    root: PathBuf,
    path: PathBuf,
}

impl Ballast {
    /// Writes zeros to `root/ballast` until `free_bytes(root) <= leave`: 1 MiB writes, then one
    /// write of the remainder, then `sync_all`. Panics when the filesystem already has less than
    /// `leave` free.
    pub(super) fn leave_free(root: &Path, leave: u64) -> Self {
        let ballast = Self {
            root: root.to_path_buf(),
            path: root.join(BALLAST),
        };
        let free = free_bytes(root);
        assert!(
            free >= leave,
            "the small filesystem has {free} bytes free, fewer than the {leave} to leave"
        );
        ballast.grow_to_leave(leave);
        ballast
    }

    /// Grows or shrinks the ballast to leave `leave` bytes free.
    pub(super) fn resize_to_leave(&mut self, leave: u64) {
        let free = free_bytes(&self.root);
        if free > leave {
            self.grow_to_leave(leave);
        } else if free < leave {
            let shrink = leave - free;
            let file = OpenOptions::new()
                .write(true)
                .open(&self.path)
                .expect("open the ballast to shrink it");
            let len = file.metadata().expect("the length of the ballast").len();
            assert!(
                len >= shrink,
                "the ballast is {len} bytes, too small to free {shrink} more"
            );
            file.set_len(len - shrink).expect("shrink the ballast");
            file.sync_all().expect("sync the shrunk ballast");
        }
    }

    /// Removes the file. The space is free at once.
    pub(super) fn remove(self) {
        fs::remove_file(&self.path).expect("remove the ballast");
    }

    fn grow_to_leave(&self, leave: u64) {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .expect("create or open the ballast");
        let zeros = vec![0_u8; MIB as usize];
        loop {
            let free = free_bytes(&self.root);
            if free <= leave {
                break;
            }
            // Zeros, not a sparse `set_len`: a sparse file takes no space.
            let want = (free - leave).min(MIB);
            file.write_all(&zeros[..want as usize])
                .expect("write ballast zeros while the filesystem has room");
        }
        file.sync_all().expect("sync the ballast");
    }
}

/// Empty files under `root/inode-ballast/` until `free_inodes(root) <= leave`.
pub(super) struct InodeBallast {
    dir: PathBuf,
}

impl InodeBallast {
    pub(super) fn leave_free(root: &Path, leave: u64) -> Self {
        let dir = root.join("inode-ballast");
        fs::create_dir(&dir).expect("create the inode ballast directory");
        let mut count = 0_u64;
        while free_inodes(root) > leave {
            assert!(
                count < MAX_INODE_BALLAST,
                "the inode ballast passed {MAX_INODE_BALLAST} files without reaching {leave} free"
            );
            File::create(dir.join(count.to_string())).expect("create an inode ballast file");
            count += 1;
        }
        Self { dir }
    }

    /// Removes the directory and its files; the inodes are free at once.
    pub(super) fn remove(self) {
        fs::remove_dir_all(&self.dir).expect("remove the inode ballast");
    }
}

/// Samples `free_bytes(root)` every 5 ms on a thread; [`Self::stop`] returns the least value seen.
pub(super) struct FreeSampler {
    stop: Arc<AtomicBool>,
    least: Arc<AtomicU64>,
    thread: JoinHandle<()>,
}

impl FreeSampler {
    pub(super) fn start(root: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let least = Arc::new(AtomicU64::new(u64::MAX));
        let root = root.to_path_buf();
        let thread = {
            let stop = Arc::clone(&stop);
            let least = Arc::clone(&least);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    least.fetch_min(free_bytes(&root), Ordering::AcqRel);
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        Self {
            stop,
            least,
            thread,
        }
    }

    pub(super) fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("the free-space sampler panicked");
        self.least.load(Ordering::Acquire)
    }
}

/// The files under `dir` whose names contain `.tmp-`, recursively: the temp files of local
/// objects, `cache/` and `tmp/compaction/`. A missing directory has none.
pub(super) fn temp_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let entries = match fs::read_dir(&next) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => panic!("list {}: {error}", next.display()),
        };
        for entry in entries {
            let entry = entry.expect("a directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("the type of a directory entry");
            if file_type.is_dir() {
                pending.push(path);
            } else if entry.file_name().to_string_lossy().contains(".tmp-") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

const SMOKE: &str = "model_tests::resources::small_fs::small_fs_fills_and_frees";

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn small_fs_fills_and_frees() {
    let fs = SmallFs {
        bytes: SMALL_FS_BYTES,
        inodes: None,
    };
    on_small_fs(SMOKE, fs, |root| {
        let empty = free_bytes(root);
        assert!(
            (MIN_FREE..=MAX_FREE).contains(&empty),
            "an empty small filesystem has {empty} bytes free"
        );

        let ballast = Ballast::leave_free(root, 4 * MIB);
        let tight = free_bytes(root);
        assert!(
            tight <= 4 * MIB,
            "the ballast left {tight} bytes free, more than 4 MiB"
        );

        let probe = root.join("probe");
        let mut file = File::create(&probe).expect("create the probe file");
        let error = file
            .write_all(&vec![0_u8; 8 * MIB as usize])
            .expect_err("8 MiB cannot fit in 4 MiB free");
        assert_eq!(
            error.kind(),
            ErrorKind::StorageFull,
            "a full filesystem refuses the write with ENOSPC"
        );
        drop(file);
        fs::remove_file(&probe).expect("remove the probe file");

        ballast.remove();
        let freed = free_bytes(root);
        assert!(
            empty.abs_diff(freed) <= MIB,
            "after the ballast is removed, {freed} bytes are free, not within 1 MiB of {empty}"
        );

        let mut harness = Harness::with_options(
            HarnessOptions {
                root: Some(root.to_path_buf()),
                ..HarnessOptions::default()
            },
            |_| {},
        );
        let mut model = new_model(&harness, (P::A, P::B));
        let mut clock = midnight(day(0)) + 12 * HOUR_NS;
        model_steps(&mut harness, &mut model, &mut clock, &seed_ops(), 0);
        settle_and_check(&mut harness, &mut model, &mut clock);
    });
}
