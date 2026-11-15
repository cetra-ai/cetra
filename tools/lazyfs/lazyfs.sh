#!/usr/bin/env bash
# Cross-checks the storage crash tests on LazyFS (Jepsen, FUSE; track R R2.12).
#
#   lazyfs.sh build   clone and build LazyFS at the pinned commit
#   lazyfs.sh run     mount it, run the crash children and a short ingest, unmount
#   lazyfs.sh probe   mount it, drop unsynced data, and print what survived
#
# Needs FUSE 3 and `user_allow_other` in /etc/fuse.conf. See tools/lazyfs/README.md.
set -euo pipefail

# The LazyFS commit this check is pinned to. Bump it with `probe` (README).
readonly LAZYFS_SHA=3edad01aa936b81fed9bd64e47e725fc5ef6c977
readonly LAZYFS_URL=https://github.com/dsrhaslab/lazyfs
readonly LAZYFS_HOME="${LLMOBS_LAZYFS_HOME:-$HOME/.cache/llmobs/lazyfs}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
template="$root/tools/lazyfs/lazyfs.toml.in"

# Set by `mount`; read by `unmount` from the EXIT trap.
work=""

usage() {
  sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 2
}

build() {
  local missing=()
  command -v g++ >/dev/null || missing+=(g++)
  command -v cmake >/dev/null || missing+=(cmake)
  pkg-config --exists fuse3 || missing+=(libfuse3-dev)
  command -v fusermount3 >/dev/null || missing+=(fuse3)
  if ((${#missing[@]})); then
    echo "missing: ${missing[*]}" >&2
    echo "install: sudo apt-get install -y g++ cmake libfuse3-dev fuse3" >&2
    exit 1
  fi

  if [[ ! -d "$LAZYFS_HOME/.git" ]]; then
    git clone --quiet "$LAZYFS_URL" "$LAZYFS_HOME"
  fi
  if ! git -C "$LAZYFS_HOME" cat-file -e "$LAZYFS_SHA^{commit}" 2>/dev/null; then
    git -C "$LAZYFS_HOME" fetch --quiet origin
  fi
  git -C "$LAZYFS_HOME" checkout --quiet --detach "$LAZYFS_SHA"

  (cd "$LAZYFS_HOME/libs/libpcache" && ./build.sh)
  (cd "$LAZYFS_HOME/lazyfs" && ./build.sh)
  echo "built LazyFS at $LAZYFS_SHA in $LAZYFS_HOME"
}

check_fuse_conf() {
  if ! grep -qs '^user_allow_other' /etc/fuse.conf; then
    echo "add this line to /etc/fuse.conf (this script does not edit /etc):" >&2
    echo "  user_allow_other" >&2
    exit 1
  fi
}

# Writes the config for work directory $work from the template.
write_config() {
  sed "s|@WORK@|$work|g" "$template" >"$work/config.toml"
}

# Starts LazyFS on $work/mnt over $work/root, in the background, and waits for the mount.
mount_lazyfs() {
  check_fuse_conf
  [[ -x "$LAZYFS_HOME/lazyfs/build/lazyfs" ]] || {
    echo "LazyFS is not built: run lazyfs.sh build" >&2
    exit 1
  }
  work="$(mktemp -d)"
  mkdir -p "$work/mnt" "$work/root"
  write_config
  trap unmount EXIT
  (cd "$LAZYFS_HOME" && scripts/mount-lazyfs.sh -c "$work/config.toml" -m "$work/mnt" -r "$work/root") \
    >"$work/lazyfs.out" 2>&1 &
  local waited=0
  until mountpoint -q "$work/mnt"; do
    if ((waited >= 100)); then
      echo "LazyFS did not mount within 10s; see $work/lazyfs.out" >&2
      exit 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
}

# Unmounts $work/mnt. Runs from the EXIT trap, so it never fails the script.
unmount() {
  [[ -n "$work" ]] || return 0
  if mountpoint -q "$work/mnt"; then
    (cd "$LAZYFS_HOME" && scripts/umount-lazyfs.sh -m "$work/mnt") >/dev/null 2>&1 ||
      fusermount3 -uz "$work/mnt" >/dev/null 2>&1 || true
  fi
  wait 2>/dev/null || true
}

# Drops every unsynced write under the mount (a power loss), and waits for LazyFS to say so.
clear_cache() {
  printf 'lazyfs::clear-cache\n' >"$work/faults.fifo"
  exec 4<>"$work/done.fifo"
  if ! read -r -t 30 -u 4 _; then
    exec 4<&-
    echo "LazyFS did not acknowledge clear-cache within 30s" >&2
    exit 1
  fi
  exec 4<&-
}

run() {
  mount_lazyfs
  local tests_dir="$work/mnt/crash-children"
  mkdir -p "$tests_dir"
  export LLMOBS_LAZYFS_DIR="$tests_dir"
  export LLMOBS_LAZYFS_FIFO="$work/faults.fifo"
  export LLMOBS_LAZYFS_DONE="$work/done.fifo"

  echo "== crash children, one power loss per point"
  (cd "$root" && cargo test --workspace --all-features -- --ignored --exact \
    crash_tests::every_hook_survives_lazyfs)

  echo "== short ingest, a power loss per kill"
  (cd "$root" && LLMOBS_CRASH_ITERATIONS=10 \
    LLMOBS_CRASH_DIR_ROOT="$work/mnt" \
    LLMOBS_CRASH_CLEAR_CACHE="$work/faults.fifo:$work/done.fifo" \
    cargo test --workspace --all-features --test crash -- --exact crash_recovery)
  echo "LazyFS cross-check passed at $LAZYFS_SHA"
}

probe() {
  mount_lazyfs
  local dir="$work/mnt/probe"
  mkdir -p "$dir"

  # Written and synced: the file data is fsynced, so LazyFS must keep it.
  printf 'synced\n' | dd of="$dir/synced.txt" conv=fsync status=none
  # Written and not synced: file data only in LazyFS's cache.
  printf 'unsynced\n' >"$dir/unsynced.txt"
  # Created with synced data, but the directory entry is never fsynced.
  printf 'created\n' | dd of="$dir/created.txt" conv=fsync status=none
  # Synced data, then renamed with no directory fsync.
  printf 'renamed\n' | dd of="$dir/before-rename.txt" conv=fsync status=none
  mv "$dir/before-rename.txt" "$dir/after-rename.txt"
  # Synced data, then unlinked with no directory fsync.
  printf 'unlinked\n' | dd of="$dir/unlinked.txt" conv=fsync status=none
  rm "$dir/unlinked.txt"

  clear_cache

  # `present` for a name that is on disk after the clear. The unlink case is `present` when
  # the removal was lost: the file came back.
  echo "== after clear-cache"
  local name
  for name in synced.txt unsynced.txt created.txt after-rename.txt unlinked.txt; do
    if [[ -e "$dir/$name" ]]; then
      echo "present: $name: $(cat "$dir/$name")"
    else
      echo "absent:  $name"
    fi
  done
}

case "${1:-}" in
  build) build ;;
  run) run ;;
  probe) probe ;;
  *) usage ;;
esac
