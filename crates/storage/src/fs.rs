//! Every local file call of the storage engine (track R, D-R-5).
//!
//! Each function is the `std::fs` call it is named after. The generic bounds
//! match `std::fs`, so call sites compile unchanged. SQLite and DataFusion do
//! their own I/O and are not seen here.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// `std::fs::File`.
#[derive(Debug)]
pub(crate) struct File {
    inner: std::fs::File,
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
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new);
        if self.create {
            // Never truncate: the flag is set explicitly so that clippy's
            // `suspicious_open_options` is satisfied.
            options.truncate(false);
        }
        options.open(path).map(|inner| File { inner })
    }
}

impl File {
    /// Read-only.
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        std::fs::File::open(path).map(|inner| File { inner })
    }

    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }

    pub(crate) fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }

    pub(crate) fn set_len(&self, len: u64) -> io::Result<()> {
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
    std::fs::create_dir(path)
}

#[inline]
pub(crate) fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

#[inline]
pub(crate) fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_file(path)
}

#[inline]
pub(crate) fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// `std::fs::remove_dir_all` semantics: a missing path is `NotFound`, and a
/// symlink is removed, not followed. Each entry is removed by its own call
/// (`remove_file` for files, `remove_dir` bottom-up for directories), so the
/// lossy shim of R2.2 records every step.
pub(crate) fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref();
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
    std::fs::rename(from, to)
}

#[inline]
pub(crate) fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Creates or truncates `path`, then writes all of `bytes`.
#[inline]
pub(crate) fn write<P: AsRef<Path>, B: AsRef<[u8]>>(path: P, bytes: B) -> io::Result<()> {
    std::fs::write(path, bytes)
}

#[inline]
pub(crate) fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    std::fs::read_to_string(path)
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
    std::fs::File::open(path)?.sync_all()
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
