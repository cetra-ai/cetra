//! The format version of the data directory (spec 03 § 12, D-R-26).

/// The format version this build writes and reads.
///
/// It changes when any persisted surface changes in a way that spec 03 § 12
/// calls a format change. Today a reader accepts exactly this version; the first
/// bump adds the readers of the previous one.
pub(crate) const FORMAT_VERSION: u16 = 1;

#[cfg(test)]
pub(crate) mod tests_support {
    use std::fs;
    use std::path::{Path, PathBuf};

    use sha2::{Digest, Sha256};

    /// Files that change while a directory is open and are not part of its
    /// content: the lock, and the WAL and shared-memory files of `catalog.db`.
    const IGNORED: [&str; 3] = ["LOCK", "catalog.db-wal", "catalog.db-shm"];

    /// The path relative to `dir` and the SHA-256 of every regular file under
    /// `dir`, sorted by path. Tests of refusals compare it before and after the
    /// failed open: equal digests mean no file's content changed.
    pub(crate) fn dir_digest(dir: &Path) -> Vec<(String, String)> {
        let mut files = Vec::new();
        collect(dir, &mut files);
        let mut digest: Vec<(String, String)> = files
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).expect("read a file of the data directory");
                let relative = path
                    .strip_prefix(dir)
                    .expect("walked paths are under dir")
                    .to_string_lossy()
                    .into_owned();
                (relative, hex::encode(Sha256::digest(&bytes)))
            })
            .collect();
        digest.sort();
        digest
    }

    fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("list the data directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                collect(&path, files);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !IGNORED.contains(&name) {
                files.push(path);
            }
        }
    }
}
