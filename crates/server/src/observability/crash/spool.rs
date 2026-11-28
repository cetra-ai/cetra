//! The crash-report spool (D-TL-12): `<data_dir>/telemetry/spool/`, one JSON file per report, written by the panic hook and the crash layer on their own thread. Every function returns or swallows its error: none logs (a log would re-enter the crash layer) and none panics (a panic in a panic hook aborts). Writes are synchronous: a stuck disk delays the writing thread, a panicking one included. Only `telemetry/` and `spool/` are ever created, never the data directory. Backups never copy this directory (`backup::write` copies named files only).

use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use super::report::{CrashReport, encode};

/// At most this many reports wait in the spool; the oldest go first.
pub(crate) const MAX_SPOOL_FILES: usize = 50;
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Makes file names unique within the process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Serializes prune-and-write, so concurrent writers keep the cap. Never held across a call
/// that could report (the spool calls nothing that logs or panics).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Why the spool could not do something. Carries an I/O kind, never a path or content.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SpoolError {
    /// A file system call failed.
    #[error("crash spool i/o failed: {0:?}")]
    Io(io::ErrorKind),
    /// The report does not fit [`super::report::MAX_SPOOLED_BYTES`] even without its backtrace.
    #[error("crash report too large to spool")]
    TooLarge,
}

/// One spooled report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpoolFile {
    /// Its path.
    pub(crate) path: PathBuf,
    /// When it was written, from its name (local only: never sent).
    pub(crate) written_at_ns: i64,
}

/// The spool directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// A spool in `dir`. Does no I/O: the directory is created by the first write.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<data_dir>/telemetry/spool`.
    pub(crate) fn for_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("telemetry").join("spool"))
    }

    /// Writes `report` as a new file: encode, create `telemetry/spool/` if missing (never the
    /// data directory), drop the oldest files down to 49, write a temporary file, rename it.
    /// No fsync: a process exit keeps the page cache, and only a power loss could lose a
    /// report. Returns the new file's path.
    pub(crate) fn write(
        &self,
        report: &CrashReport,
        written_at_ns: i64,
    ) -> Result<PathBuf, SpoolError> {
        let body = encode(report).ok_or(SpoolError::TooLarge)?;
        let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        create_dirs(&self.dir).map_err(|error| SpoolError::Io(error.kind()))?;
        self.prune(MAX_SPOOL_FILES - 1);
        let name = file_name(written_at_ns, SEQUENCE.fetch_add(1, Ordering::Relaxed));
        let temporary = self.dir.join(format!(".{name}.tmp"));
        let target = self.dir.join(&name);
        let result =
            write_new(&temporary, &body).and_then(|()| std::fs::rename(&temporary, &target));
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(SpoolError::Io(error.kind()));
        }
        Ok(target)
    }

    /// The spooled reports, oldest first. A missing directory is empty; names that are not
    /// [`file_name`]s are ignored.
    pub(crate) fn list(&self) -> Result<Vec<SpoolFile>, SpoolError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SpoolError::Io(error.kind())),
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(written_at_ns) = name.to_str().and_then(parse_file_name) {
                files.push(SpoolFile {
                    path: entry.path(),
                    written_at_ns,
                });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Deletes the oldest reports until at most `keep` remain. Errors are ignored: the sender
    /// may have deleted a file first.
    fn prune(&self, keep: usize) {
        let Ok(files) = self.list() else {
            return;
        };
        let excess = files.len().saturating_sub(keep);
        for file in files.iter().take(excess) {
            let _ = std::fs::remove_file(&file.path);
        }
    }

    /// Deletes temporary files a crash left mid-write (`.crash-….tmp`). Errors are ignored.
    pub(crate) fn remove_temporaries(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(".crash-") && name.ends_with(".tmp"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Deletes the spool directory (crash reports are off), then its parent when that is an
    /// empty `telemetry` directory. A missing directory is fine.
    pub(crate) fn clear(&self) -> Result<(), SpoolError> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(SpoolError::Io(error.kind())),
        }
        if let Some(parent) = self.dir.parent()
            && parent.file_name() == Some(OsStr::new("telemetry"))
        {
            // Removes only an empty directory.
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }
}

/// `crash-<20-digit ns>-<10-digit sequence>.json`: names sort by time. A negative time is 0.
pub(crate) fn file_name(written_at_ns: i64, seq: u64) -> String {
    format!(
        "crash-{:020}-{:010}.json",
        written_at_ns.max(0),
        seq % 10_000_000_000
    )
}

/// The time in a [`file_name`], or `None` for any other name.
pub(crate) fn parse_file_name(name: &str) -> Option<i64> {
    let stem = name.strip_prefix("crash-")?.strip_suffix(".json")?;
    let (time, seq) = stem.split_once('-')?;
    let digits = |text: &str, len: usize| {
        text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit())
    };
    if !digits(time, 20) || !digits(seq, 10) {
        return None;
    }
    time.parse::<i64>().ok()
}

/// Creates the spool directory `dir` and its parent (`telemetry/`), each with mode 0700,
/// when missing. The grandparent (the data directory) must exist: an error event before
/// `serve::bind` created it (a startup failure) must not create it with our mode.
fn create_dirs(dir: &Path) -> io::Result<()> {
    let parent = dir.parent().ok_or(io::ErrorKind::NotFound)?;
    let grandparent = parent.parent().ok_or(io::ErrorKind::NotFound)?;
    if !grandparent.is_dir() {
        return Err(io::ErrorKind::NotFound.into());
    }
    for path in [parent, dir] {
        match std::fs::DirBuilder::new().mode(DIR_MODE).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Creates `path` (it must not exist) with `body`.
fn write_new(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)?;
    file.write_all(body)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    use super::*;
    use crate::observability::crash::report::{EventSite, event_report};
    use crate::observability::facts::Deployment;

    fn report() -> CrashReport {
        let site = EventSite {
            file: Some("crates/x.rs"),
            line: Some(3),
            target: "cetra_x",
            module_path: None,
            fields: Vec::new(),
        };
        event_report(
            &site,
            Vec::new(),
            Deployment::Binary,
            Duration::from_secs(10),
        )
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_spool_is_under_the_data_dir() {
        let root = Path::new("/var/lib/cetra");
        assert_eq!(Spool::for_data_dir(root).dir, root.join("telemetry/spool"));
    }

    #[test]
    fn a_missing_data_dir_is_not_created() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(&root.path().join("absent"));
        assert!(matches!(
            spool.write(&report(), 1),
            Err(SpoolError::Io(io::ErrorKind::NotFound))
        ));
        assert!(!root.path().join("absent").exists());
    }

    #[test]
    fn only_telemetry_and_spool_are_created() {
        let root = tempfile::tempdir().unwrap();
        let before = mode(root.path());
        Spool::for_data_dir(root.path())
            .write(&report(), 1)
            .unwrap();
        assert_eq!(mode(&root.path().join("telemetry")), 0o700);
        assert_eq!(mode(&root.path().join("telemetry/spool")), 0o700);
        assert_eq!(mode(root.path()), before);
    }

    #[test]
    fn write_creates_the_directory_and_one_file() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        let original = report();
        let path = spool.write(&original, 42).unwrap();
        let files = spool.list().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, path);
        assert_eq!(files[0].written_at_ns, 42);
        let back: CrashReport = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, original);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.path().join("telemetry/spool")), 0o700);
    }

    #[test]
    fn file_names_sort_by_time_and_parse_back() {
        assert!(file_name(5, 1) < file_name(10, 0));
        assert_eq!(parse_file_name(&file_name(123, 7)), Some(123));
        assert_eq!(parse_file_name("crash-1-2.json"), None);
        assert_eq!(
            parse_file_name(".crash-00000000000000000001-0000000001.json.tmp"),
            None
        );
        assert_eq!(parse_file_name("other.json"), None);
        assert_eq!(
            parse_file_name("crash-00000000000000000001-0000000001.txt"),
            None
        );
    }

    #[test]
    fn a_negative_time_is_written_as_zero() {
        assert!(file_name(-5, 0).starts_with("crash-00000000000000000000-"));
    }

    #[test]
    fn the_spool_keeps_at_most_fifty_files() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        let original = report();
        for time in 1..=55 {
            spool.write(&original, time).unwrap();
        }
        let files = spool.list().unwrap();
        assert_eq!(files.len(), MAX_SPOOL_FILES);
        assert_eq!(files[0].written_at_ns, 6);
        assert_eq!(files[MAX_SPOOL_FILES - 1].written_at_ns, 55);
    }

    #[test]
    fn no_temporary_file_is_left() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        for time in 1..=3 {
            spool.write(&report(), time).unwrap();
        }
        let dir = root.path().join("telemetry/spool");
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(!name.to_string_lossy().starts_with('.'));
        }
    }

    #[test]
    fn an_unwritable_spool_is_an_error_not_a_panic() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("spool"), b"not a directory").unwrap();
        let spool = Spool::new(root.path().join("spool"));
        assert!(matches!(spool.write(&report(), 1), Err(SpoolError::Io(_))));
        assert_eq!(
            std::fs::read(root.path().join("spool")).unwrap(),
            b"not a directory"
        );
    }

    #[test]
    fn a_report_that_never_fits_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let mut original = report();
        original.fields = vec!["x".repeat(256); 300];
        let spool = Spool::for_data_dir(root.path());
        assert!(matches!(
            spool.write(&original, 1),
            Err(SpoolError::TooLarge)
        ));
        assert!(!root.path().join("telemetry").exists());
    }

    #[test]
    fn remove_temporaries_deletes_only_temporaries() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        spool.write(&report(), 1).unwrap();
        let dir = root.path().join("telemetry/spool");
        std::fs::write(dir.join(".crash-x.tmp"), b"half").unwrap();
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        spool.remove_temporaries();
        assert!(!dir.join(".crash-x.tmp").exists());
        assert!(dir.join("notes.txt").exists());
        assert_eq!(spool.list().unwrap().len(), 1);
    }

    #[test]
    fn clear_removes_the_spool_and_an_empty_telemetry_dir() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        spool.write(&report(), 1).unwrap();
        spool.clear().unwrap();
        assert!(!root.path().join("telemetry/spool").exists());
        assert!(!root.path().join("telemetry").exists());
        spool.clear().unwrap();
    }

    #[test]
    fn clear_keeps_a_telemetry_dir_with_other_files() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        spool.write(&report(), 1).unwrap();
        std::fs::write(root.path().join("telemetry/keep.txt"), b"keep").unwrap();
        spool.clear().unwrap();
        assert!(!root.path().join("telemetry/spool").exists());
        assert!(root.path().join("telemetry/keep.txt").exists());
    }

    #[test]
    fn list_of_a_missing_dir_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        assert_eq!(spool.list().unwrap(), Vec::new());
    }

    #[test]
    fn concurrent_writes_keep_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let spool = Spool::for_data_dir(root.path());
        let original = report();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let spool = spool.clone();
                let original = original.clone();
                std::thread::spawn(move || {
                    for time in 0..10 {
                        spool.write(&original, time).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let files = spool.list().unwrap();
        assert!(files.len() <= MAX_SPOOL_FILES);
        for file in files {
            let _: CrashReport =
                serde_json::from_slice(&std::fs::read(&file.path).unwrap()).unwrap();
        }
    }
}
