//! Every local file call of the storage engine (track R, D-R-5).
//!
//! Each function is the `std::fs` call it is named after. The generic bounds
//! match `std::fs`, so call sites compile unchanged. Under `cfg(test)`, a call
//! on a path under a root registered in lossy mode (`lossy::Lossy`) is
//! recorded, so a test can drop what a power loss may drop. SQLite does its
//! own I/O and is not seen here.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) mod lossy;

/// `std::fs::File`.
#[derive(Debug)]
pub(crate) struct File {
    inner: std::fs::File,
    /// Set when the file is under a lossy root.
    #[cfg(test)]
    tracked: Option<lossy::Tracked>,
}

/// `std::fs::OpenOptions`, with only the flags this crate sets.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read(&mut self, on: bool) -> &mut Self {
        self.read = on;
        self
    }

    pub(crate) fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }

    pub(crate) fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self
    }

    pub(crate) fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }

    pub(crate) fn create_new(&mut self, on: bool) -> &mut Self {
        self.create_new = on;
        self
    }

    pub(crate) fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        #[cfg(test)]
        {
            if let Some(root) = lossy::route(path.as_ref()) {
                return root.open(path.as_ref(), self);
            }
        }
        self.to_std(path.as_ref()).map(File::from_std)
    }

    /// The `std::fs` options with the same flags. `truncate` is never set: it
    /// is passed as `false` explicitly whenever `create` is, which keeps clippy's
    /// `suspicious_open_options` quiet.
    fn to_std(&self, path: &Path) -> io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new);
        if self.create {
            options.truncate(false);
        }
        options.open(path)
    }
}

impl File {
    fn from_std(inner: std::fs::File) -> File {
        File {
            inner,
            #[cfg(test)]
            tracked: None,
        }
    }

    /// Read-only.
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions {
            read: true,
            ..OpenOptions::default()
        }
        .open(path)
    }

    pub(crate) fn sync_all(&self) -> io::Result<()> {
        #[cfg(test)]
        {
            if let Some(tracked) = &self.tracked {
                return tracked.sync(&self.inner, std::fs::File::sync_all);
            }
        }
        self.inner.sync_all()
    }

    pub(crate) fn sync_data(&self) -> io::Result<()> {
        #[cfg(test)]
        {
            if let Some(tracked) = &self.tracked {
                return tracked.sync(&self.inner, std::fs::File::sync_data);
            }
        }
        self.inner.sync_data()
    }

    pub(crate) fn set_len(&self, len: u64) -> io::Result<()> {
        #[cfg(test)]
        {
            if let Some(tracked) = &self.tracked {
                return tracked.set_len(&self.inner, len);
            }
        }
        self.inner.set_len(len)
    }

    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.inner.metadata()
    }

    pub(crate) fn try_lock(&self) -> Result<(), std::fs::TryLockError> {
        self.inner.try_lock()
    }

    /// Hands the file to code that needs a `std::fs::File`. The remote
    /// multipart upload streams the spill file through `tokio::fs::File`, and
    /// writes through the result are not recorded by the lossy shim.
    pub(crate) fn into_std(self) -> std::fs::File {
        self.inner
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        {
            if let Some(tracked) = &self.tracked {
                return tracked.write(&mut self.inner, buf);
            }
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[inline]
pub(crate) fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(path.as_ref()) {
            return root.create_dir(path.as_ref());
        }
    }
    std::fs::create_dir(path)
}

#[inline]
pub(crate) fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(path.as_ref()) {
            return root.create_dir_all(path.as_ref());
        }
    }
    std::fs::create_dir_all(path)
}

#[inline]
pub(crate) fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(path.as_ref()) {
            return root.remove_file(path.as_ref());
        }
    }
    std::fs::remove_file(path)
}

/// Only the test-mode `remove_dir_all` recursion calls this: production calls
/// std's `remove_dir_all` directly.
#[cfg(test)]
#[inline]
pub(crate) fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    if let Some(root) = lossy::route(path.as_ref()) {
        return root.remove_dir(path.as_ref());
    }
    std::fs::remove_dir(path)
}

/// `std::fs::remove_dir_all`. Under `cfg(test)`, a tree under a lossy root is
/// removed entry by entry (`remove_file` for files, `remove_dir` bottom-up for
/// directories), so the shim records every step. Elsewhere it is std's call.
#[cfg(not(test))]
#[inline]
pub(crate) fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

#[cfg(test)]
pub(crate) fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref();
    if lossy::route(path).is_none() {
        return std::fs::remove_dir_all(path);
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return remove_file(path);
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if entry.file_type()?.is_dir() {
            remove_dir_all(&child)?;
        } else {
            remove_file(&child)?;
        }
    }
    remove_dir(path)
}

#[inline]
pub(crate) fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(from.as_ref()) {
            return root.rename(from.as_ref(), to.as_ref());
        }
    }
    std::fs::rename(from, to)
}

/// Creates or truncates `path`, then writes all of `bytes`.
#[inline]
pub(crate) fn write<P: AsRef<Path>, B: AsRef<[u8]>>(path: P, bytes: B) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(path.as_ref()) {
            return root.write(path.as_ref(), bytes.as_ref());
        }
    }
    std::fs::write(path, bytes)
}

/// Hard-links `original` at `link`. Only for a backup directory outside the data
/// directory (`snapshot::copy_objects`), so lossy mode does not record it.
#[inline]
pub(crate) fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    std::fs::hard_link(original, link)
}

/// Copies `from` to `to`, like `std::fs::copy`. Only for a backup directory outside
/// the data directory (`snapshot::copy_objects`), so lossy mode does not record it.
#[inline]
pub(crate) fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    std::fs::copy(from, to)
}

#[inline]
pub(crate) fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

#[inline]
pub(crate) fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<std::fs::ReadDir> {
    std::fs::read_dir(path)
}

#[inline]
pub(crate) fn metadata<P: AsRef<Path>>(path: P) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

#[inline]
pub(crate) fn try_exists<P: AsRef<Path>>(path: P) -> io::Result<bool> {
    path.as_ref().try_exists()
}

#[inline]
pub(crate) fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Opens the directory and `sync_all`s it, which makes its entries durable.
#[inline]
pub(crate) fn sync_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(root) = lossy::route(path.as_ref()) {
            return root.sync_dir(path.as_ref());
        }
    }
    std::fs::File::open(path)?.sync_all()
}

/// The directory that holds the entry `path`, to fsync for that entry. A
/// relative one-component path has the parent `""`, which `open` rejects; its
/// entry is in the current directory, so that is returned instead. `None` when
/// `path` has no parent.
///
/// `..` maps to `.` as well, because its parent is `""`. That is not the real
/// parent of `..`, which is the current directory's parent. The crate never
/// syncs `..` on its own, and an fsync of `.` is harmless either way.
#[inline]
pub(crate) fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().map(|parent| {
        if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_one_component_path_syncs_the_current_directory() {
        assert_eq!(parent_dir(Path::new("data")), Some(Path::new(".")));
        assert_eq!(
            parent_dir(Path::new("data/objects")),
            Some(Path::new("data"))
        );
        assert_eq!(parent_dir(Path::new("/data")), Some(Path::new("/")));
        assert_eq!(parent_dir(Path::new("/")), None);
        // The mapped parent opens, where the empty one does not.
        sync_dir(parent_dir(Path::new("data")).unwrap()).unwrap();
    }
}

/// Sizes of the file system holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Space {
    /// Bytes available to unprivileged users: `f_bavail * f_frsize`.
    pub(crate) free: u64,
    /// Bytes in the file system: `f_blocks * f_frsize`.
    pub(crate) total: u64,
}

/// `statvfs` of the file system holding `path`.
#[inline]
pub(crate) fn space<P: AsRef<Path>>(path: P) -> io::Result<Space> {
    let stat = rustix::fs::statvfs(path.as_ref())?;
    Ok(Space {
        free: stat.f_bavail.saturating_mul(stat.f_frsize),
        total: stat.f_blocks.saturating_mul(stat.f_frsize),
    })
}
