# B10 — Documentation, spec and roadmap

Read [00-design.md](00-design.md) first, all of § Design.

**Goal:** the self-hosting docs describe online backups and the restore of one. Spec 12 § Backups
describes what was built, and the roadmap and changelog say M2 is done.

**Edit only:** `docs/self-hosting/backup.md`, `docs/self-hosting/upgrade.md`,
`docs/self-hosting/configuration.md`, `docs/spec/12-operations.md`, `docs/roadmap.md`, `CHANGELOG.md`.

**Commit:** `Document online backups`

**Needs:** B1–B9 merged.

## Changes

### `docs/self-hosting/backup.md`

Restructure it into:

1. **Intro.** Two ways to back up:
   - online, with `llmobs admin backup` while the server runs (recommended);
   - cold, by stopping the server and copying (the current content, kept for installations that
     prefer it).
2. **What a backup contains.** Keep the current list.
3. **`## Online backup`** (new), with:
   - the advice to set an absolute `LLMOBS_DATA_DIR`: a relative one resolves against the server's
     and the CLI's different working directories;
   - for containers: the target must be outside the data volume, so it is another mount, and the
     objects are then copied, not hard-linked, on every run. Hard links on one filesystem protect
     against operator error, not against losing the disk. Show the mounts: a second volume
     `backups:/backups` in a compose override, and an extra volume on the StatefulSet. These are
     examples in the docs; B10 does not change `deploy/`;
   - the command and an example for each install: the binary, `docker compose exec llmobs llmobs
     admin backup --to /backups/$(date +%F)` with a `/backups` volume owned by uid 65532, and
     Kubernetes with `kubectl exec`;
   - what it does, in five lines: flush, copy `meta.db` and `catalog.db`, link or copy the objects
     (local mode), `secret.key`, `manifest.json`;
   - the rules:
     - `<dir>` is on the server's filesystem and writable by it;
     - `<dir>` must be absent or empty and outside the data directory;
     - one backup at a time;
     - a directory without `manifest.json` is incomplete;
   - local mode: objects are hard-linked when `<dir>` is on the same filesystem as the objects, so a
     backup costs only the new files; on another filesystem they are copied. Ship `<dir>` with
     restic, rsync or `tar`;
   - S3 mode: the backup is metadata only (`--meta-only` is implied). It is restorable against the
     live bucket for `gc.grace` (default 5 minutes). Set `storage.gc.grace`, for example
     `LLMOBS_STORAGE__GC__GRACE=8d`, longer than the backups you want to restore. Also give the
     privacy note: erased data stays in obsolete files for up to that long. For longer-term restores,
     also keep bucket versioning or a copy of the bucket.
   - the manifest fields, shortly.
4. **`## Cold backup`.** Keep the current per-install sections (systemd, docker-compose, Kubernetes,
   S3 mode) under it.
5. **`## Restore`.** One list for both kinds of backup:
   1. Stop the server.
   2. Keep the current data directory aside.
   3. Create an empty data directory and copy in `meta.db`, `catalog.db`, `secret.key` and, for a
      full backup, `objects/`. For a cold backup, the whole directory as now.
   4. In S3 mode with a cold backup, sync the bucket back as now. With an online meta-only backup,
      do not.
   5. Run `llmobs admin gc-orphans --dry-run`, then `llmobs admin gc-orphans` (in S3 mode
      `llmobs admin gc-orphans --rollback-bucket`), with the server still
      stopped. **In S3 mode this rolls the live bucket back:** objects written after the backup are
      deleted permanently. Missing media is a warning (attachments lost). It deletes objects the restored
      catalog does not know and reports missing ones. `missing objects: 0` is required; more means
      the backup is too old for `gc.grace`, or `objects/` was not copied.
   6. If the backup is older than the binary, run `llmobs admin migrate` (X15).
   7. Run `llmobs admin reapply-deletions --from <kept directory>`, still with the server stopped.
   8. Start the server.

   Give each admin command in its container form too: `docker compose run --rm --no-deps llmobs
   admin …`, and on Kubernetes a one-off pod that mounts the data PVC with the chart's env,
   Secret and ConfigMap (the StatefulSet is scaled to 0, so there is no pod to exec into).
6. **Retention and privacy.** Keep it, and add the `gc.grace` sentence.

### `docs/self-hosting/upgrade.md`

In "What happens on upgrade", add: "Before migrating `meta.db` or `catalog.db`, the server keeps a copy
named `meta.db.pre-m<N>` and `catalog.db.pre-m<N>` in the data directory, where N is the highest
migration applied before the upgrade, and only the one from the last upgrade." In the procedure, step 2 becomes "Take a backup (`llmobs admin backup`, see
backup.md)".

### `docs/self-hosting/configuration.md`

Add a row to the main table:

```markdown
| `storage.gc.grace` | `LLMOBS_STORAGE__GC__GRACE` | `5m` | how long obsolete files stay; at most `30d`. In S3 mode, how long a meta-only backup stays restorable |
```

### `docs/spec/12-operations.md` § Backups

The plan commit left one paragraph starting "**Online backups (track N's M2, planned in
`docs/plans/n-packaging/m2/`, D-N-17–D-N-24):**". Change its lead to "**Online backups (track N's
M2, D-N-17–D-N-24):**" and check that its text matches what was built:
- the socket;
- the steps;
- the layout and manifest;
- S3 retention through `gc.grace`;
- the restore with `gc-orphans`.

Cite D-N-17–D-N-24. Keep the cold procedure paragraph.

### `docs/roadmap.md`

In the **N** row's Status cell, the plan commit wrote "M2 (online backups, `gc-orphans`,
pre-migration copies) planned." Change "planned." to "implemented." and leave the Plan cell as it
is.

### `CHANGELOG.md` § `[Unreleased]` › `### Added`

```markdown
- `llmobs admin backup` takes a consistent backup of a running server (local mode: hard-linked
  objects; S3 mode: metadata, restorable for `storage.gc.grace`, which may now be up to 30 days).
- `llmobs admin gc-orphans` cleans the object store after a restore and reports missing objects.
- The server keeps a copy of `meta.db` and `catalog.db` before migrating them.
```

## Verify

```bash
! grep -n "planned in \`docs/plans/n-packaging/m2/\`" docs/spec/12-operations.md
for f in docs/self-hosting/*.md; do
  grep -o '](\(\.\./\)*[A-Za-z0-9_./-]*[)#]' "$f" | sed 's/^](//; s/[)#]$//' | grep -v '^$' | while read -r link; do
    test -e "docs/self-hosting/$link" || echo "$f: broken link $link"
  done
done
just check
```

Also run the documented online backup and restore by hand once, with a release build, as § How to
verify in `00-design.md` describes, and report the outputs.

## Done when

- [ ] The docs, spec 12, roadmap and changelog are updated.
- [ ] The link check prints nothing, and the manual run passes.
- [ ] `just check` passes. One commit: `Document online backups`.
