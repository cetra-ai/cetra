//! Lossy mode of the `fs` seam (track R, M2, D-R-5). Test code only.
//!
//! A test registers a data directory as a root with [`Lossy::enable`]. From
//! then on every `crate::fs` call on a path under it is recorded in memory:
//! per file, the synced length and the unsynced writes; per directory, the
//! entry operations since its last sync. A lossy crash
//! ([`Lossy::crash_with`]) then rewrites the real directory to a state that a
//! power loss may leave: unsynced writes and entries are dropped, kept or torn
//! by a seeded choice. The model follows RocksDB's `FaultInjectionTestFS` and
//! the ALICE rules (`docs/research/storage/hardening.md`, track R, M2).
//!
//! Only in-crate tests see this module. `tests/crash.rs` is an integration test
//! and runs without `cfg(test)`; LazyFS covers it (R2.12).
//!
//! Each root is leaked when it is enabled, so its state can be locked for the
//! rest of the process by [`crash_before_abort`].

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{File, OpenOptions};

/// Every registered root. A root is leaked, so the registry holds `'static`
/// references.
static ROOTS: Mutex<Vec<&'static Root>> = Mutex::new(Vec::new());

type Ino = u64;
type DirId = u64;

/// `EIO`, the errno of a failed fsync.
const EIO: i32 = 5;

/// A registered data directory.
#[derive(Debug)]
pub(crate) struct Root {
    /// Canonical path of the data directory.
    path: PathBuf,
    /// Hard links of unlinked and replaced files live here until the reset.
    shadow: PathBuf,
    /// Live tracked files. A crash with any open is refused.
    open_files: AtomicUsize,
    state: Mutex<State>,
}

/// What the shim has seen under one root. Pub(crate) only because
/// [`crash_before_abort`] returns its guards.
#[derive(Debug, Default)]
pub(crate) struct State {
    next_id: u64,
    seq: u64,
    shadow_seq: u64,
    /// Root-relative file name to inode.
    names: BTreeMap<PathBuf, Ino>,
    inodes: BTreeMap<Ino, Inode>,
    /// Root-relative directory to its current incarnation.
    dirs: BTreeMap<PathBuf, DirId>,
    incarnations: BTreeMap<DirId, Incarnation>,
    /// Canonical prefixes whose next sync fails with `EIO`.
    fail_syncs: Vec<PathBuf>,
    /// Set by [`Lossy::crash_on_abort`]: the crash applied before an abort.
    seed: Option<u64>,
}

#[derive(Debug)]
struct Inode {
    synced_len: u64,
    len: u64,
    /// Writes past `synced_len`, in order, never overlapping.
    writes: Vec<Pending>,
    /// Current root-relative name, or `None` once the shim sees no name.
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    start: u64,
    end: u64,
    poisoned: bool,
}

/// One life of a directory: the entry operations since its last sync. A
/// directory removed and created again at the same path is a new incarnation.
#[derive(Debug)]
struct Incarnation {
    ops: Vec<Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    seq: u64,
    poisoned: bool,
    op: Op,
}

#[derive(Debug, Clone)]
enum Op {
    Create {
        path: PathBuf,
        ino: Ino,
    },
    Mkdir {
        path: PathBuf,
        child: DirId,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        ino: Ino,
        /// The file the rename replaced, and its shadow link.
        replaced: Option<(Ino, PathBuf)>,
    },
    Unlink {
        path: PathBuf,
        ino: Ino,
        shadow: PathBuf,
    },
    Rmdir {
        path: PathBuf,
        child: DirId,
    },
}

/// A registered lossy root. Dropping it unregisters the root.
#[derive(Debug)]
pub(crate) struct Lossy {
    root: &'static Root,
}

/// What a lossy crash does to the unsynced state (spec 03 § Durability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashMode {
    /// Every unsynced write and every pending entry operation is lost.
    DropAll,
    /// Per directory, a random prefix of its pending operations is kept. Per
    /// file, each unsynced write is kept or dropped at random; a dropped write
    /// followed by a kept one leaves zeros.
    KeepRandom,
    /// Every pending entry operation and every unsynced write is kept, except
    /// the last unsynced write of each file, which is cut to a random prefix
    /// (0 bytes included). This is the ext4 delayed-allocation state: names
    /// durable, data torn.
    TearLast,
}

/// What a crash did, for the test that asked for it.
#[derive(Debug)]
pub(crate) struct CrashReport {
    pub(crate) mode: CrashMode,
    pub(crate) dropped_writes: usize,
    pub(crate) torn_writes: usize,
    pub(crate) dropped_entries: usize,
}

impl CrashMode {
    /// `[DropAll, KeepRandom, TearLast][seed % 3]`.
    pub(crate) fn from_seed(seed: u64) -> Self {
        [Self::DropAll, Self::KeepRandom, Self::TearLast][usize::try_from(seed % 3).unwrap_or(0)]
    }
}

impl Lossy {
    /// Creates `dir` (fsyncing its parent when it is new) and `shadow`, and
    /// registers `dir`. Panics if `shadow` is inside `dir`, or if `dir` is
    /// already registered.
    pub(crate) fn enable(dir: &Path, shadow: &Path) -> Lossy {
        assert!(
            !shadow.starts_with(dir),
            "lossy fs: the shadow directory must be outside the root"
        );
        if !dir.is_dir() {
            let parent = dir.parent().expect("lossy fs: the root has a parent");
            std::fs::create_dir_all(parent).expect("lossy fs: creating the root's parent");
            std::fs::create_dir(dir).expect("lossy fs: creating the root");
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .expect("lossy fs: syncing the root's parent");
        }
        std::fs::create_dir_all(shadow).expect("lossy fs: creating the shadow directory");
        let path = std::fs::canonicalize(dir).expect("lossy fs: canonicalizing the root");
        let shadow = std::fs::canonicalize(shadow).expect("lossy fs: canonicalizing the shadow");
        let mut roots = ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            roots.iter().all(|root| root.path != path),
            "lossy fs: {} is already registered",
            path.display()
        );
        let root: &'static Root = Box::leak(Box::new(Root {
            path,
            shadow,
            open_files: AtomicUsize::new(0),
            state: Mutex::new(State::new(0, 0)),
        }));
        roots.push(root);
        Lossy { root }
    }

    /// Makes the next hook abort of this process apply `crash(seed)` to this
    /// root first (see [`crash_before_abort`]).
    pub(crate) fn crash_on_abort(&self, seed: u64) {
        self.root.lock().seed = Some(seed);
    }

    /// `crash_with(CrashMode::from_seed(seed), seed)`.
    pub(crate) fn crash(&self, seed: u64) -> CrashReport {
        self.crash_with(CrashMode::from_seed(seed), seed)
    }

    /// Applies a crash state to the real directory, then forgets everything:
    /// what is left on disk counts as synced. Panics if a tracked file is
    /// still open, since the storage must be closed first.
    pub(crate) fn crash_with(&self, mode: CrashMode, seed: u64) -> CrashReport {
        let root = self.root;
        let mut st = root.lock();
        let open = root.open_files.load(Ordering::SeqCst);
        assert!(
            open == 0,
            "lossy fs: {open} file(s) still open under {}; close the storage before a crash",
            root.path.display()
        );
        let report = apply(root, &mut st, mode, seed);
        reset(root, &mut st);
        report
    }

    /// The next sync of a file or directory under `prefix` fails with `EIO`.
    pub(crate) fn fail_next_sync(&self, prefix: &Path) {
        let prefix = canonical(prefix).expect("lossy fs: the prefix resolves");
        self.root.lock().fail_syncs.push(prefix);
    }
}

impl Drop for Lossy {
    fn drop(&mut self) {
        let mut roots = ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
        roots.retain(|root| !std::ptr::eq(*root, self.root));
    }
}

/// Applies `crash_with(CrashMode::from_seed(seed), seed)` to every root that
/// has a seed from [`Lossy::crash_on_abort`], and returns the locks. The
/// caller aborts while holding them, so nothing changes the directories
/// afterwards. The state is not reset: the process is about to end. Each root
/// keeps its open files; the directory is changed under them, and they are
/// never used again.
pub(crate) fn crash_before_abort() -> Vec<MutexGuard<'static, State>> {
    let roots: Vec<&'static Root> = ROOTS.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let mut guards = Vec::new();
    for root in roots {
        let mut st = root.lock();
        if let Some(seed) = st.seed {
            let report = apply(root, &mut st, CrashMode::from_seed(seed), seed);
            eprintln!("lossy crash before abort: {report:?}");
        }
        guards.push(st);
    }
    guards
}

/// The root that holds `path`, when `path` resolves under a registered root.
/// Includes the root itself.
pub(crate) fn route(path: &Path) -> Option<&'static Root> {
    // The lock is released before canonicalizing: the registry is only read here.
    let roots: Vec<&'static Root> = ROOTS.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if roots.is_empty() {
        return None;
    }
    let canon = canonical(path)?;
    roots.into_iter().find(|root| canon.starts_with(&root.path))
}

/// `path` with its nearest existing ancestor canonicalized, so a path that
/// does not exist yet resolves the same way as the directory it will be in.
fn canonical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut tail: Vec<&OsStr> = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::canonicalize(cursor) {
            Ok(base) => {
                return Some(tail.iter().rev().fold(base, |acc, name| acc.join(name)));
            }
            Err(_) => {
                tail.push(cursor.file_name()?);
                cursor = cursor.parent()?;
            }
        }
    }
}

fn parent_of(rel: &Path) -> PathBuf {
    rel.parent().unwrap_or(Path::new("")).to_path_buf()
}

impl Root {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn abs(&self, rel: &Path) -> PathBuf {
        self.path.join(rel)
    }

    /// The root-relative path of `path`, which resolves under the root.
    fn rel(&self, path: &Path) -> PathBuf {
        let canon = canonical(path)
            .unwrap_or_else(|| panic!("lossy fs: cannot resolve {}", path.display()));
        canon
            .strip_prefix(&self.path)
            .unwrap_or_else(|_| panic!("lossy fs: {} is outside its root", path.display()))
            .to_path_buf()
    }

    pub(crate) fn open(&'static self, path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let rel = self.rel(path);
        let mut st = self.lock();
        let existed = st.resolve(self, &rel).is_some();
        let inner = opts.to_std(path)?;
        if !existed && !(opts.create || opts.create_new) {
            // A directory, or a path that is not a file: not recorded.
            return Ok(File::from_std(inner));
        }
        let ino = if existed {
            st.names[&rel]
        } else {
            let ino = st.new_id();
            st.inodes.insert(
                ino,
                Inode {
                    synced_len: 0,
                    len: 0,
                    writes: Vec::new(),
                    path: Some(rel.clone()),
                },
            );
            st.names.insert(rel.clone(), ino);
            st.push(
                &parent_of(&rel),
                Op::Create {
                    path: rel.clone(),
                    ino,
                },
            );
            ino
        };
        let tracked = Tracked::new(self, ino, opts.append, self.abs(&rel));
        Ok(File {
            inner,
            tracked: Some(tracked),
        })
    }

    pub(crate) fn create_dir(&'static self, path: &Path) -> io::Result<()> {
        let rel = self.rel(path);
        let mut st = self.lock();
        std::fs::create_dir(path)?;
        let child = st.new_id();
        st.incarnations
            .insert(child, Incarnation { ops: Vec::new() });
        st.dirs.insert(rel.clone(), child);
        st.push(&parent_of(&rel), Op::Mkdir { path: rel, child });
        Ok(())
    }

    /// Creates the missing directories of `path`, top down, through
    /// [`Self::create_dir`].
    pub(crate) fn create_dir_all(&'static self, path: &Path) -> io::Result<()> {
        let mut missing: Vec<&Path> = Vec::new();
        let mut cursor = path;
        while !cursor.is_dir() {
            missing.push(cursor);
            cursor = cursor
                .parent()
                .expect("lossy fs: create_dir_all above the root");
        }
        for dir in missing.into_iter().rev() {
            self.create_dir(dir)?;
        }
        Ok(())
    }

    pub(crate) fn remove_file(&'static self, path: &Path) -> io::Result<()> {
        let rel = self.rel(path);
        let mut st = self.lock();
        let Some(ino) = st.resolve(self, &rel) else {
            return std::fs::remove_file(path);
        };
        let shadow = st.shadow_path(self, ino);
        std::fs::hard_link(path, &shadow)?;
        std::fs::remove_file(path)?;
        st.names.remove(&rel);
        if let Some(inode) = st.inodes.get_mut(&ino) {
            inode.path = None;
        }
        st.push(
            &parent_of(&rel),
            Op::Unlink {
                path: rel,
                ino,
                shadow,
            },
        );
        Ok(())
    }

    pub(crate) fn remove_dir(&'static self, path: &Path) -> io::Result<()> {
        let rel = self.rel(path);
        let mut st = self.lock();
        std::fs::remove_dir(path)?;
        let child = st.dir_id(&rel);
        st.dirs.remove(&rel);
        st.push(&parent_of(&rel), Op::Rmdir { path: rel, child });
        Ok(())
    }

    pub(crate) fn rename(&'static self, from: &Path, to: &Path) -> io::Result<()> {
        let from_rel = self.rel(from);
        let to_rel = self.rel(to);
        let parent = parent_of(&to_rel);
        assert_eq!(
            parent_of(&from_rel),
            parent,
            "lossy fs: cross-directory rename; extend fs::lossy"
        );
        assert!(
            !from.is_dir(),
            "lossy fs: renaming a directory; extend fs::lossy"
        );
        let mut st = self.lock();
        let Some(ino) = st.resolve(self, &from_rel) else {
            return std::fs::rename(from, to);
        };
        let replaced = match st.resolve(self, &to_rel) {
            Some(old) => {
                let shadow = st.shadow_path(self, old);
                std::fs::hard_link(to, &shadow)?;
                Some((old, shadow))
            }
            None => None,
        };
        std::fs::rename(from, to)?;
        st.names.remove(&from_rel);
        st.names.insert(to_rel.clone(), ino);
        if let Some(inode) = st.inodes.get_mut(&ino) {
            inode.path = Some(to_rel.clone());
        }
        if let Some((old, _)) = &replaced
            && let Some(inode) = st.inodes.get_mut(old)
        {
            inode.path = None;
        }
        st.push(
            &parent,
            Op::Rename {
                from: from_rel,
                to: to_rel,
                ino,
                replaced,
            },
        );
        Ok(())
    }

    /// `fs::write`: create or truncate, then write everything.
    pub(crate) fn write(&'static self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.open(
            path,
            &OpenOptions {
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )?;
        file.set_len(0)?;
        file.write_all(bytes)
    }

    pub(crate) fn sync_dir(&'static self, path: &Path) -> io::Result<()> {
        let rel = self.rel(path);
        let mut st = self.lock();
        let id = st.dir_id(&rel);
        if st.take_fail(&self.abs(&rel)) {
            if let Some(inc) = st.incarnations.get_mut(&id) {
                for entry in &mut inc.ops {
                    entry.poisoned = true;
                }
            }
            return Err(io::Error::from_raw_os_error(EIO));
        }
        std::fs::File::open(path)?.sync_all()?;
        if let Some(inc) = st.incarnations.get_mut(&id)
            && !inc.ops.iter().any(|entry| entry.poisoned)
        {
            inc.ops.clear();
        }
        Ok(())
    }
}

impl State {
    fn new(next_id: u64, shadow_seq: u64) -> State {
        let mut st = State {
            next_id,
            shadow_seq,
            ..State::default()
        };
        st.dir_id(Path::new(""));
        st
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The current incarnation of `dir`. A directory the shim has not seen is
    /// adopted as a new incarnation with no pending operations.
    fn dir_id(&mut self, dir: &Path) -> DirId {
        if let Some(id) = self.dirs.get(dir) {
            return *id;
        }
        let id = self.new_id();
        self.dirs.insert(dir.to_path_buf(), id);
        self.incarnations
            .insert(id, Incarnation { ops: Vec::new() });
        id
    }

    /// Records an entry operation in the current incarnation of `dir`.
    fn push(&mut self, dir: &Path, op: Op) {
        self.seq += 1;
        let entry = Entry {
            seq: self.seq,
            poisoned: false,
            op,
        };
        let id = self.dir_id(dir);
        self.incarnations
            .get_mut(&id)
            .expect("lossy fs: the incarnation of a directory exists")
            .ops
            .push(entry);
    }

    /// The inode of the file at `rel`. A file the shim has not seen is adopted
    /// as fully synced. `None` for a missing path or a non-file.
    fn resolve(&mut self, root: &Root, rel: &Path) -> Option<Ino> {
        if let Some(ino) = self.names.get(rel) {
            return Some(*ino);
        }
        let meta = std::fs::metadata(root.abs(rel)).ok()?;
        if !meta.is_file() {
            return None;
        }
        let ino = self.new_id();
        self.inodes.insert(
            ino,
            Inode {
                synced_len: meta.len(),
                len: meta.len(),
                writes: Vec::new(),
                path: Some(rel.to_path_buf()),
            },
        );
        self.names.insert(rel.to_path_buf(), ino);
        Some(ino)
    }

    /// A fresh shadow name for `ino`, in the shadow directory.
    fn shadow_path(&mut self, root: &Root, ino: Ino) -> PathBuf {
        self.shadow_seq += 1;
        root.shadow.join(format!("{ino}-{}", self.shadow_seq))
    }

    /// Consumes a failure armed for `path`, when one covers it.
    fn take_fail(&mut self, path: &Path) -> bool {
        match self
            .fail_syncs
            .iter()
            .position(|prefix| path.starts_with(prefix))
        {
            Some(index) => {
                self.fail_syncs.remove(index);
                true
            }
            None => false,
        }
    }
}

/// An open file under a root. Dropping it counts it as closed.
#[derive(Debug)]
pub(crate) struct Tracked {
    root: &'static Root,
    ino: Ino,
    append: bool,
    /// Canonical path, for messages and failure prefixes.
    path: PathBuf,
}

impl Tracked {
    fn new(root: &'static Root, ino: Ino, append: bool, path: PathBuf) -> Tracked {
        root.open_files.fetch_add(1, Ordering::SeqCst);
        Tracked {
            root,
            ino,
            append,
            path,
        }
    }

    /// Appends `buf` and records what was written. A write must start at the
    /// file's length: writes are appends.
    pub(crate) fn write(&self, inner: &mut std::fs::File, buf: &[u8]) -> io::Result<usize> {
        let mut st = self.root.lock();
        let len = st.inodes.get(&self.ino).map_or(0, |inode| inode.len);
        let start = if self.append {
            inner.metadata()?.len()
        } else {
            inner.stream_position()?
        };
        assert!(
            start == len,
            "lossy fs: non-append write at {}; extend fs::lossy",
            self.path.display()
        );
        let written = inner.write(buf)?;
        if written > 0 {
            let written = written as u64;
            if let Some(inode) = st.inodes.get_mut(&self.ino) {
                inode.writes.push(Pending {
                    start,
                    end: start + written,
                    poisoned: false,
                });
                inode.len += written;
            }
        }
        Ok(written)
    }

    /// `sync_all` or `sync_data`. A failed sync poisons the writes pending at
    /// that moment. A later successful sync covers every write made since,
    /// as on Linux, but not the poisoned ones: they stay pending, and a crash
    /// leaves them as zeros inside the synced length.
    pub(crate) fn sync(
        &self,
        inner: &std::fs::File,
        real: fn(&std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut st = self.root.lock();
        if st.take_fail(&self.path) {
            if let Some(inode) = st.inodes.get_mut(&self.ino) {
                for pending in &mut inode.writes {
                    pending.poisoned = true;
                }
            }
            return Err(io::Error::from_raw_os_error(EIO));
        }
        real(inner)?;
        if let Some(inode) = st.inodes.get_mut(&self.ino) {
            // The synced length is the whole length, not the end of the last
            // poisoned range: a write after that range was synced and must
            // survive a crash.
            inode.synced_len = inode.len;
            inode.writes.retain(|pending| pending.poisoned);
        }
        Ok(())
    }

    /// `set_len`. Shrinking below the synced length makes the shrink durable
    /// at once, as the storage always syncs after such a call.
    pub(crate) fn set_len(&self, inner: &std::fs::File, len: u64) -> io::Result<()> {
        let mut st = self.root.lock();
        let inode = st
            .inodes
            .get_mut(&self.ino)
            .expect("lossy fs: the inode of an open file exists");
        assert!(
            len <= inode.len,
            "lossy fs: set_len beyond the written length of {}; extend fs::lossy",
            self.path.display()
        );
        inner.set_len(len)?;
        if len < inode.synced_len {
            inode.synced_len = len;
            inode.len = len;
            inode.writes.clear();
        } else {
            inode.writes.retain(|pending| pending.start < len);
            for pending in &mut inode.writes {
                pending.end = pending.end.min(len);
            }
            inode.len = len;
        }
        Ok(())
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.root.open_files.fetch_sub(1, Ordering::SeqCst);
    }
}

/// SplitMix64: the seeded choices of a crash. Small on purpose, so the crate
/// needs no RNG dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`, for `n >= 1`.
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Chooses the crash state of `st` and writes it to the real directory. Does
/// not reset `st`.
fn apply(root: &Root, st: &mut State, mode: CrashMode, seed: u64) -> CrashReport {
    let mut rng = SplitMix64(seed);
    let mut report = CrashReport {
        mode,
        dropped_writes: 0,
        torn_writes: 0,
        dropped_entries: 0,
    };

    // 1. How many pending operations of each incarnation survive, as a prefix.
    let mut keep: BTreeMap<DirId, usize> = BTreeMap::new();
    for (id, inc) in &st.incarnations {
        let n = inc.ops.len();
        let first_poisoned = inc.ops.iter().position(|entry| entry.poisoned).unwrap_or(n);
        let kept = match mode {
            CrashMode::DropAll => 0,
            CrashMode::KeepRandom => usize::try_from(rng.below(n as u64 + 1)).unwrap_or(n),
            CrashMode::TearLast => n,
        };
        keep.insert(*id, kept.min(first_poisoned));
    }
    // Causal rules, to a fixpoint. A dropped `Mkdir` drops the whole child
    // incarnation. A kept `Rmdir` needs the child to be empty by then, so it
    // is kept only when every operation of the child is.
    loop {
        let mut changed = false;
        for id in st.incarnations.keys() {
            let k = keep[id];
            for (j, entry) in st.incarnations[id].ops.iter().enumerate() {
                match &entry.op {
                    Op::Mkdir { child, .. } if j >= k => {
                        if keep.get(child).copied().unwrap_or(0) != 0 {
                            keep.insert(*child, 0);
                            changed = true;
                        }
                    }
                    Op::Rmdir { child, .. } if j < k => {
                        let child_len = st.incarnations.get(child).map_or(0, |inc| inc.ops.len());
                        if keep.get(child).copied().unwrap_or(0) < child_len {
                            keep.insert(*id, j);
                            changed = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }

    // 2. Undo the dropped entry operations, newest first.
    let mut dropped: Vec<(u64, DirId, usize)> = Vec::new();
    for (id, inc) in &st.incarnations {
        for (j, entry) in inc.ops.iter().enumerate().skip(keep[id]) {
            dropped.push((entry.seq, *id, j));
        }
    }
    dropped.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    report.dropped_entries = dropped.len();
    for (_, id, j) in dropped {
        let op = st.incarnations[&id].ops[j].op.clone();
        undo(root, st, &op);
    }

    // 3. Cut every file that is still under a name to its crash state.
    let inos: Vec<Ino> = st.inodes.keys().copied().collect();
    for ino in inos {
        let Some(inode) = st.inodes.get(&ino) else {
            continue;
        };
        let Some(rel) = inode.path.clone() else {
            continue;
        };
        let synced = inode.synced_len;
        let writes = inode.writes.clone();
        let abs = root.abs(&rel);
        if std::fs::metadata(&abs).is_err() {
            continue;
        }
        let last_live = writes.iter().rposition(|pending| !pending.poisoned);
        let mut kept: Vec<(u64, u64)> = Vec::new();
        let mut dropped: Vec<(u64, u64)> = Vec::new();
        for (index, pending) in writes.iter().enumerate() {
            let survives = !pending.poisoned
                && match mode {
                    CrashMode::DropAll => false,
                    CrashMode::KeepRandom => rng.next_u64() & 1 == 1,
                    CrashMode::TearLast => true,
                };
            if !survives {
                report.dropped_writes += 1;
                dropped.push((pending.start, pending.end));
                continue;
            }
            let mut end = pending.end;
            if mode == CrashMode::TearLast && Some(index) == last_live {
                let full = pending.end - pending.start;
                let cut = rng.below(full + 1);
                end = pending.start + cut;
                if cut < full {
                    report.torn_writes += 1;
                }
            }
            kept.push((pending.start, end));
        }
        // Kept writes are appends, so the last one ends furthest.
        let new_len = kept.last().map_or(synced, |&(_, end)| end.max(synced));
        let zeros: Vec<(u64, u64)> = dropped
            .into_iter()
            .filter(|&(start, _)| start < new_len)
            .map(|(start, end)| (start, end.min(new_len)))
            .collect();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&abs)
            .unwrap_or_else(|error| {
                panic!("lossy fs: opening {} for the crash: {error}", abs.display())
            });
        file.set_len(new_len)
            .unwrap_or_else(|error| panic!("lossy fs: cutting {}: {error}", abs.display()));
        for (start, end) in zeros {
            let hole = vec![0_u8; usize::try_from(end - start).unwrap_or(0)];
            file.write_all_at(&hole, start)
                .unwrap_or_else(|error| panic!("lossy fs: zeroing {}: {error}", abs.display()));
        }
        if let Some(inode) = st.inodes.get_mut(&ino) {
            inode.synced_len = new_len;
            inode.len = new_len;
            inode.writes.clear();
        }
    }
    report
}

/// Undoes one dropped entry operation on the real directory, and on the names.
fn undo(root: &Root, st: &mut State, op: &Op) {
    match op {
        Op::Create { path, ino } => {
            undo_step(std::fs::remove_file(root.abs(path)), "removing", path);
            if st.names.get(path) == Some(ino) {
                st.names.remove(path);
            }
            if let Some(inode) = st.inodes.get_mut(ino) {
                inode.path = None;
            }
        }
        Op::Mkdir { path, .. } => {
            undo_step(std::fs::remove_dir_all(root.abs(path)), "removing", path);
            for inode in st.inodes.values_mut() {
                if inode
                    .path
                    .as_ref()
                    .is_some_and(|name| name.starts_with(path))
                {
                    inode.path = None;
                }
            }
            st.names.retain(|name, _| !name.starts_with(path));
            st.dirs.retain(|name, _| !name.starts_with(path));
        }
        Op::Rename {
            from,
            to,
            ino,
            replaced,
        } => {
            undo_step(
                std::fs::rename(root.abs(to), root.abs(from)),
                "renaming back",
                to,
            );
            st.names.remove(to);
            st.names.insert(from.clone(), *ino);
            if let Some(inode) = st.inodes.get_mut(ino) {
                inode.path = Some(from.clone());
            }
            if let Some((old, shadow)) = replaced {
                undo_step(std::fs::hard_link(shadow, root.abs(to)), "restoring", to);
                st.names.insert(to.clone(), *old);
                if let Some(inode) = st.inodes.get_mut(old) {
                    inode.path = Some(to.clone());
                }
            }
        }
        Op::Unlink { path, ino, shadow } => {
            undo_step(
                std::fs::hard_link(shadow, root.abs(path)),
                "restoring",
                path,
            );
            st.names.insert(path.clone(), *ino);
            if let Some(inode) = st.inodes.get_mut(ino) {
                inode.path = Some(path.clone());
            }
        }
        Op::Rmdir { path, child } => {
            undo_step(std::fs::create_dir(root.abs(path)), "recreating", path);
            st.dirs.insert(path.clone(), *child);
        }
    }
}

/// An undo step whose parent an earlier undo already removed is skipped.
fn undo_step(result: io::Result<()>, step: &str, path: &Path) {
    match result {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("lossy fs: {step} {} failed: {error}", path.display()),
    }
}

/// Empties the shadow directory and forgets the state. Ids keep counting.
fn reset(root: &Root, st: &mut State) {
    let (next_id, shadow_seq) = (st.next_id, st.shadow_seq);
    std::fs::remove_dir_all(&root.shadow).expect("lossy fs: clearing the shadow directory");
    std::fs::create_dir(&root.shadow).expect("lossy fs: recreating the shadow directory");
    *st = State::new(next_id, shadow_seq);
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    /// A root at `t/data` with its shadow at `t/shadow`.
    fn setup() -> (TempDir, PathBuf, Lossy) {
        let t = TempDir::new().unwrap();
        let data = t.path().join("data");
        let shadow = t.path().join("shadow");
        let lossy = Lossy::enable(&data, &shadow);
        (t, data, lossy)
    }

    /// Creates `path` (not yet synced) and writes `bytes`, then syncs the file.
    fn write_synced(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        crate::fs::sync_dir(path.parent().unwrap()).unwrap();
    }

    /// Every entry under `dir`, relative to `base`, with its bytes for files.
    fn tree(base: &Path, dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path.strip_prefix(base).unwrap().to_path_buf();
            if path.is_dir() {
                out.push((rel, None));
                out.extend(tree(base, &path));
            } else {
                out.push((rel, Some(std::fs::read(&path).unwrap())));
            }
        }
        out
    }

    #[test]
    fn drop_all_truncates_to_the_synced_length() {
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        crate::fs::sync_dir(&data).unwrap();
        file.write_all(b"abc").unwrap();
        file.sync_all().unwrap();
        file.write_all(b"def").unwrap();
        drop(file);

        let report = lossy.crash_with(CrashMode::DropAll, 0);

        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(report.mode, CrashMode::DropAll);
        assert_eq!(report.dropped_writes, 1);
        assert_eq!(report.torn_writes, 0);
        assert_eq!(report.dropped_entries, 0);
    }

    #[test]
    fn an_unsynced_create_is_lost_and_a_synced_one_stays() {
        let (_t, data, lossy) = setup();
        let d = data.join("d");
        crate::fs::create_dir(&d).unwrap();
        crate::fs::sync_dir(&data).unwrap();
        write_synced(&d.join("a"), b"");
        let b = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(d.join("b"))
            .unwrap();
        drop(b);

        let report = lossy.crash_with(CrashMode::DropAll, 0);

        assert!(d.join("a").exists());
        assert!(!d.join("b").exists());
        assert_eq!(report.dropped_entries, 1);
    }

    #[test]
    fn an_unsynced_unlink_is_undone_with_its_content() {
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        write_synced(&path, b"x");
        crate::fs::remove_file(&path).unwrap();

        lossy.crash_with(CrashMode::DropAll, 0);

        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn an_unsynced_rename_over_a_file_restores_both() {
        let (_t, data, lossy) = setup();
        let target = data.join("f");
        let temp = data.join(".f.tmp-1");
        write_synced(&target, b"old");
        write_synced(&temp, b"new");
        crate::fs::rename(&temp, &target).unwrap();

        lossy.crash_with(CrashMode::DropAll, 0);

        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read(&temp).unwrap(), b"new");
    }

    #[test]
    fn a_lost_directory_loses_its_synced_files() {
        let (_t, data, lossy) = setup();
        let d = data.join("d");
        crate::fs::create_dir(&d).unwrap();
        write_synced(&d.join("x"), b"x");

        lossy.crash_with(CrashMode::DropAll, 0);

        assert!(!d.exists());
    }

    #[test]
    fn tear_last_keeps_names_and_cuts_the_last_write() {
        for seed in 0..32 {
            let (_t, data, lossy) = setup();
            let path = data.join("f");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(b"aaaa").unwrap();
            file.write_all(b"bbbb").unwrap();
            drop(file);

            lossy.crash_with(CrashMode::TearLast, seed);

            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.starts_with(b"aaaa"), "seed {seed}");
            assert!(
                (4..=8).contains(&bytes.len()),
                "seed {seed}: {}",
                bytes.len()
            );
            assert!(b"bbbb".starts_with(&bytes[4..]), "seed {seed}");
        }
    }

    #[test]
    fn keep_random_leaves_zeros_for_a_dropped_write() {
        let mut found = false;
        for seed in 0..256 {
            let (_t, data, lossy) = setup();
            let path = data.join("f");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            crate::fs::sync_dir(&data).unwrap();
            file.write_all(b"aaaa").unwrap();
            file.write_all(b"bbbb").unwrap();
            drop(file);

            lossy.crash_with(CrashMode::KeepRandom, seed);

            let bytes = std::fs::read(&path).unwrap_or_default();
            if bytes == b"\0\0\0\0bbbb" {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "no seed in 0..256 drops the first write and keeps the second"
        );
    }

    #[test]
    fn a_failed_sync_poisons_the_writes() {
        for mode in [CrashMode::DropAll, CrashMode::TearLast] {
            let (_t, data, lossy) = setup();
            let path = data.join("f");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            crate::fs::sync_dir(&data).unwrap();
            file.write_all(b"abc").unwrap();
            lossy.fail_next_sync(&path);

            let error = file.sync_all().unwrap_err();
            assert_eq!(error.raw_os_error(), Some(5));
            file.sync_all().unwrap();
            drop(file);

            lossy.crash_with(mode, 0);

            // The poisoned bytes are lost; the length the later sync covered stays.
            assert_eq!(std::fs::read(&path).unwrap(), [0; 3], "{mode:?}");
        }
    }

    #[test]
    fn a_later_sync_covers_the_writes_after_a_failure() {
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        crate::fs::sync_dir(&data).unwrap();
        file.write_all(b"abc").unwrap();
        lossy.fail_next_sync(&path);
        assert_eq!(file.sync_all().unwrap_err().raw_os_error(), Some(5));
        file.write_all(b"def").unwrap();
        file.sync_all().unwrap();
        drop(file);

        lossy.crash_with(CrashMode::DropAll, 0);

        assert_eq!(std::fs::read(&path).unwrap(), b"\0\0\0def");
    }

    #[test]
    fn a_failed_sync_dir_poisons_its_entries() {
        for mode in [
            CrashMode::DropAll,
            CrashMode::KeepRandom,
            CrashMode::TearLast,
        ] {
            let (_t, data, lossy) = setup();
            let d = data.join("d");
            crate::fs::create_dir(&d).unwrap();
            lossy.fail_next_sync(&data);

            let error = crate::fs::sync_dir(&data).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(5));
            lossy.crash_with(mode, 0);

            assert!(!d.exists(), "{mode:?}");
        }
    }

    #[test]
    fn fs_write_over_a_synced_file_never_leaves_the_old_bytes() {
        for seed in 0..6 {
            let (_t, data, lossy) = setup();
            let path = data.join("f");
            write_synced(&path, b"old");
            crate::fs::write(&path, b"new").unwrap();

            lossy.crash(seed);

            let bytes = std::fs::read(&path).unwrap();
            assert!(b"new".starts_with(&bytes), "seed {seed}: {bytes:?}");
        }
    }

    #[test]
    fn create_dir_all_and_remove_dir_all_go_through_the_shim() {
        let (t, data, lossy) = setup();
        let deep = data.join("a").join("b").join("c");
        crate::fs::create_dir_all(&deep).unwrap();

        lossy.crash_with(CrashMode::DropAll, 0);
        assert!(!data.join("a").exists());

        crate::fs::create_dir_all(&deep).unwrap();
        crate::fs::sync_dir(&data).unwrap();
        crate::fs::sync_dir(data.join("a")).unwrap();
        crate::fs::sync_dir(data.join("a").join("b")).unwrap();
        write_synced(&deep.join("x"), b"x");
        let before = tree(&data, &data);

        crate::fs::remove_dir_all(data.join("a")).unwrap();
        assert!(!data.join("a").exists());
        lossy.crash_with(CrashMode::DropAll, 0);
        assert_eq!(tree(&data, &data), before);

        // Outside every root, the call is std's.
        let outside = t.path().join("outside");
        crate::fs::create_dir_all(outside.join("x")).unwrap();
        crate::fs::remove_dir_all(&outside).unwrap();
        assert!(!outside.exists());
    }

    #[test]
    fn the_same_seed_gives_the_same_state() {
        fn build(data: &Path) {
            crate::fs::create_dir(data.join("d")).unwrap();
            crate::fs::create_dir(data.join("e")).unwrap();
            crate::fs::sync_dir(data).unwrap();
            write_synced(&data.join("d").join("one"), b"11");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(data.join("e").join("two"))
                .unwrap();
            file.write_all(b"2222").unwrap();
            file.write_all(b"22").unwrap();
            drop(file);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(data.join("three"))
                .unwrap();
            file.write_all(b"333").unwrap();
            drop(file);
        }
        // 6, 7 and 8 pick each of the three modes once.
        for seed in [6, 7, 8] {
            let (_a, data_a, lossy_a) = setup();
            let (_b, data_b, lossy_b) = setup();
            build(&data_a);
            build(&data_b);

            lossy_a.crash(seed);
            lossy_b.crash(seed);

            assert_eq!(
                tree(&data_a, &data_a),
                tree(&data_b, &data_b),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn crash_before_abort_works_with_open_files() {
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        crate::fs::sync_dir(&data).unwrap();
        file.write_all(b"abc").unwrap();
        file.sync_all().unwrap();
        file.write_all(b"def").unwrap();
        lossy.crash_on_abort(0);

        let guards = crash_before_abort();
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        drop(guards);

        crate::fs::create_dir(data.join("after")).unwrap();
        assert!(data.join("after").is_dir());
        drop(file);
    }

    #[test]
    #[should_panic(expected = "still open")]
    fn crash_with_an_open_file_panics() {
        let (_t, data, lossy) = setup();
        let _file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(data.join("f"))
            .unwrap();
        lossy.crash_with(CrashMode::DropAll, 0);
    }

    #[test]
    #[should_panic(expected = "non-append")]
    fn non_append_writes_panic() {
        let (_t, data, _lossy) = setup();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(data.join("f"))
            .unwrap();
        file.write_all(b"abc").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"d").unwrap();
    }

    #[test]
    fn reopened_file_keeps_its_unsynced_state() {
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"abc").unwrap();
        drop(file);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.sync_all().unwrap();
        drop(file);
        crate::fs::sync_dir(&data).unwrap();

        lossy.crash_with(CrashMode::DropAll, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");

        write_synced(&data.join("g"), b"ab");
        crate::fs::rename(data.join("g"), data.join("h")).unwrap();
        let mut file = OpenOptions::new().write(true).open(data.join("h")).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"cd").unwrap();
        drop(file);

        lossy.crash_with(CrashMode::DropAll, 0);
        assert_eq!(std::fs::read(data.join("g")).unwrap(), b"ab");
        assert!(!data.join("h").exists());
    }

    #[test]
    fn set_len_cuts_a_straddling_write() {
        for seed in 0..8 {
            let (_t, data, lossy) = setup();
            let path = data.join("f");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            crate::fs::sync_dir(&data).unwrap();
            file.write_all(b"aaaa").unwrap();
            file.sync_all().unwrap();
            file.write_all(b"bbbbbb").unwrap();
            file.set_len(7).unwrap();
            drop(file);

            lossy.crash_with(CrashMode::TearLast, seed);
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.starts_with(b"aaaa"), "seed {seed}");
            assert!((4..=7).contains(&bytes.len()), "seed {seed}");
            assert!(b"bbb".starts_with(&bytes[4..]), "seed {seed}");
        }
        let (_t, data, lossy) = setup();
        let path = data.join("f");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        crate::fs::sync_dir(&data).unwrap();
        file.write_all(b"aaaa").unwrap();
        file.sync_all().unwrap();
        file.write_all(b"bbbbbb").unwrap();
        file.set_len(7).unwrap();
        drop(file);
        lossy.crash_with(CrashMode::DropAll, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"aaaa");
    }

    #[test]
    fn a_kept_rmdir_implies_its_children() {
        for seed in 0..64 {
            let (_t, data, lossy) = setup();
            let d = data.join("d");
            crate::fs::create_dir(&d).unwrap();
            crate::fs::sync_dir(&data).unwrap();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(d.join("x"))
                .unwrap();
            file.sync_all().unwrap();
            drop(file);
            crate::fs::remove_file(d.join("x")).unwrap();
            crate::fs::remove_dir(&d).unwrap();

            let report = lossy.crash_with(CrashMode::KeepRandom, seed);

            if !d.exists() {
                assert!(!d.join("x").exists(), "seed {seed}");
                assert_eq!(report.dropped_entries, 0, "seed {seed}");
            }
        }
    }

    #[test]
    fn a_recreated_directory_is_a_new_incarnation() {
        // Every state a crash can leave: `d` absent; `d` empty; `d` with `a`;
        // `d` with `b`. `a` and `b` never exist together.
        for seed in 0..64 {
            let (_t, data, lossy) = setup();
            let d = data.join("d");
            crate::fs::create_dir(&d).unwrap();
            crate::fs::sync_dir(&data).unwrap();
            write_synced(&d.join("a"), b"a");
            crate::fs::sync_dir(&d).unwrap();
            crate::fs::remove_file(d.join("a")).unwrap();
            crate::fs::remove_dir(&d).unwrap();
            crate::fs::create_dir(&d).unwrap();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(d.join("b"))
                .unwrap();
            drop(file);

            lossy.crash_with(CrashMode::KeepRandom, seed);

            let state = tree(&data, &data);
            let allowed: [Vec<(PathBuf, Option<Vec<u8>>)>; 4] = [
                vec![],
                vec![(PathBuf::from("d"), None)],
                vec![
                    (PathBuf::from("d"), None),
                    (PathBuf::from("d/a"), Some(b"a".to_vec())),
                ],
                vec![
                    (PathBuf::from("d"), None),
                    (PathBuf::from("d/b"), Some(vec![])),
                ],
            ];
            assert!(allowed.contains(&state), "seed {seed}: {state:?}");
            assert!(!(d.join("b").exists() && !d.exists()), "seed {seed}");
        }
    }
}
