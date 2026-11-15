# LazyFS cross-check

`just lazyfs` runs the storage crash tests on [LazyFS](https://github.com/dsrhaslab/lazyfs), a
FUSE filesystem with its own page cache (Jepsen, MIT). `lazyfs::clear-cache` drops every
unsynced write under the mount, which is a power loss on the real binary. The storage
model's lossy mode (`crates/storage/src/fs/lossy.rs`) is a model of the same thing in process.
LazyFS also covers `catalog.db`, which the model can't see (D-R-5).

## Pin

LazyFS has no releases. The check is pinned to commit
`3edad01aa936b81fed9bd64e47e725fc5ef6c977` (`main` on 2026-11-15), in `lazyfs.sh` as
`LAZYFS_SHA`. To bump it:

1. find the new commit with `git ls-remote https://github.com/dsrhaslab/lazyfs`;
2. change `LAZYFS_SHA` in `lazyfs.sh`, and diff `lazyfs.toml.in` against that commit's
   `lazyfs/config/default.toml`;
3. run `lazyfs.sh build` and `lazyfs.sh probe`, and record the probe output in
   `docs/research/storage/hardening.md`.

## Prerequisites

- Build dependencies: `sudo apt-get install -y g++ cmake fuse3 libfuse3-dev libspdlog-dev libfmt-dev`.
  `lazyfs.sh build` checks for them (`pkg-config` for fuse3, spdlog and fmt) and exits with the
  missing names.
- `user_allow_other` in `/etc/fuse.conf`. `lazyfs.sh run` prints the line to add and exits if
  it is missing. The script never edits `/etc`.
- `/dev/fuse` readable by your user.

## Commands

```bash
tools/lazyfs/lazyfs.sh build   # clone into ${LLMOBS_LAZYFS_HOME:-~/.cache/llmobs/lazyfs}, build
tools/lazyfs/lazyfs.sh probe   # mount, sync some files, drop unsynced data, print what is left
tools/lazyfs/lazyfs.sh run     # mount, run the crash children and a short ingest, unmount
just lazyfs                    # build, then run
```

`build` configures libpcache and LazyFS with `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, which the
pinned commit needs with CMake 4: its bundled toml11 declares an older `cmake_minimum_required`.
The build scripts call plain `cmake`, so `build` puts a `cmake` wrapper on their `PATH` that adds
the flag to each configure.

`run` makes a work directory with `mktemp -d` (its path is printed by the mount log on failure),
mounts LazyFS on `W/mnt` over `W/root`, and unmounts on exit.

- `every_hook_survives_lazyfs` (ignored by `cargo test`) runs every crash child of
  `crash_tests.rs` on the mount, one per point, and drops unsynced data after each child
  exits, before the group's checks.
- `crash_recovery` (`crates/storage/tests/crash.rs`) runs 10 kill-and-check iterations on the
  mount, with a power loss after each kill. Its `acked.log` is `sync_data`'d per line, so
  what the parent expects survives the clear.

Both tests read `LLMOBS_LAZYFS_DIR`, `LLMOBS_LAZYFS_FIFO` and `LLMOBS_LAZYFS_DONE`, or
`LLMOBS_CRASH_DIR_ROOT` and `LLMOBS_CRASH_CLEAR_CACHE=<fifo>:<done fifo>`. The script sets them.
Neither path may contain `:`.

## Reading a failure

- `build` fails: the pinned commit does not build. Stop; do not patch LazyFS.
- `run` fails to mount: `W/lazyfs.out` has LazyFS's own output, and `W/lazyfs.log` its log.
- A check fails on LazyFS and the same point passes under the shim (`fs::lossy`), this is a
  shim bug: fix `fs::lossy` and add a unit test for the state (R2.12 step 5).
- A check fails on both: the storage has the bug. Find the cause before changing anything.
- `clear-cache` is not acknowledged in 30 s: LazyFS is stuck or the FIFOs are wrong.

## Probe result and the GitHub runner check

`probe` writes these cases, in this order: a synced file, an unsynced file, a created file
with synced data and no directory fsync, a rename, and an unlink, each with no directory fsync.
Its output is recorded in `docs/research/storage/hardening.md` § LazyFS and the shim.

Whether `/dev/fuse` works on GitHub runners is checked on a throwaway branch,
`claude/r-lazyfs-probe`, with one workflow `lazyfs-probe.yml` (`workflow_dispatch`,
`ubuntu-24.04`). The workflow installs the packages above, adds `user_allow_other`, runs
`ls -l /dev/fuse`, and runs `tools/lazyfs/lazyfs.sh build && tools/lazyfs/lazyfs.sh probe`.
It is not in this repository (AGENTS.md: `.github/` is out of scope for R2.12). The branch is
deleted after the run.

- If the probe passes there, M3's nightly workflow runs `just lazyfs`.
- If not, the LazyFS check runs on the soak machine (M7) by hand before each release and
  nightly from a cron job there.

The run is manual. It is not part of `just check` or `just test`.
