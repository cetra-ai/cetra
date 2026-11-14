# Backup and restore

llmobs has two ways to back up. Both restore the same way.

- **Online**, with `llmobs admin backup` while the server runs (recommended). The server keeps
  ingesting and answering requests. See [Online backup](#online-backup).
- **Cold**, by stopping the server and copying its data. Stopping takes up to a minute: llmobs drains
  requests and flushes buffered spans to Parquet. While it is stopped, OTLP exporters retry and buffer;
  spans they drop after their own retry limit are lost. Kept for installations that prefer it. See
  [Cold backup](#cold-backup).

What a backup contains:

- the whole data directory: `meta.db` and `catalog.db` (always together, from the same stop),
  the WAL (`wal/`), `secret.key`, and without S3 the Parquet files and media (`objects/`);
- in S3 mode, every object under the bucket prefix, copied during the same stop;
- the encryption key, if you set `LLMOBS_ENCRYPTION_KEY` instead of using `secret.key`: store it
  with the backup. A backup can only be read with the key that was current when it was taken.

`tmp/` (query spill files) and `LOCK` can be left out.

An online backup holds `meta.db`, `catalog.db`, `secret.key`, `manifest.json` and, without S3, the
objects it lists. It has no WAL: it is taken right after a flush, so the WAL holds nothing it needs.

## Online backup

Run this on the server's host, or in its container:

```bash
llmobs admin backup --to <dir> [--meta-only]
```

The command asks the running server, through the control socket `<data_dir>/admin.sock`, to write
the backup into `<dir>`, then prints the result:

```text
backup written to /var/backups/llmobs/2026-11-14
mode: full
objects: 812 (3.0 GiB; 812 linked, 0 copied)
took: 4.2 s
```

- Set an absolute `LLMOBS_DATA_DIR` (or `data_dir`) for both the server and the command. A relative
  one resolves against each process's working directory, and the two differ: the command then looks
  for the socket in the wrong place and reports that no server answers.
- `<dir>` is a path on the **server's** filesystem, and the server writes it as its own user (uid
  65532 in the image). The target's parent directory must be writable by that user.
- `<dir>` must be absent or empty, and outside the data directory (symlinks are resolved first).
- One backup runs at a time. A second request fails with `busy`.
- If a backup fails, or the server stops while it runs, `<dir>` is left without `manifest.json`. It is
  incomplete: do not restore from it.
- `--meta-only` is for S3 mode only. In local mode the command refuses it (`meta_only_needs_s3`),
  because such a backup could not be restored.

The command fails with `backup failed (<code>): <message>` when the server refuses or cannot finish.
The codes are `bad_request`, `target_not_empty`, `target_inside_data_dir`, `meta_only_needs_s3`,
`busy`, `flush_failed`, `meta_copy_failed`, `snapshot_failed`, `objects_failed`, `forbidden` (the
caller is neither the data directory's owner nor root) and `io`. When no server answers, the command
says so and names the socket.

What the server does:

1. It flushes both tables, so every span and score acknowledged before the backup is in Parquet.
2. It copies `meta.db`, then `catalog.db`, with `VACUUM INTO`.
3. In local mode, it hard-links the objects the catalog lists into `objects/` (or copies them).
4. It copies `secret.key`, when the key is a file.
5. It writes `manifest.json` last.

Ingestion continues during the backup. Spans acknowledged after the flush are not in it. Compaction,
deletion and retention keep running. Writes wait while `catalog.db` is copied, which takes longer on
a large catalog, and object deletions wait until the backup ends.

### Where to put `<dir>`

Keep `<dir>` outside the data volume. In a container, that means a second volume. Because it is then
on another filesystem, the objects are copied, not hard-linked, on every run.

**Binary (systemd).** The `llmobs` user owns the data directory and the socket. Run the command as
that user, with the data directory set, and write to a directory it owns:

```bash
sudo install -d -o llmobs -g llmobs -m 0700 /var/backups/llmobs
sudo -u llmobs env LLMOBS_DATA_DIR=/var/lib/llmobs \
  /usr/local/bin/llmobs admin backup --to /var/backups/llmobs/$(date +%F)
```

**docker-compose.** Add a file `compose.override.yaml` next to `compose.yaml`. Compose reads it
automatically:

```yaml
services:
  llmobs:
    volumes:
      - backups:/backups

volumes:
  backups:
```

A new named volume belongs to root, so give it to the server's user (uid 65532) once:

```bash
cd deploy/compose
docker compose up -d
docker run --rm -v llmobs_backups:/backups busybox:1.37 chown 65532:65532 /backups
docker compose exec llmobs llmobs admin backup --to /backups/$(date +%F)
```

**Kubernetes.** The Helm chart has no value for a second volume yet. Add a PersistentVolumeClaim
`llmobs-backups`, writable by uid 65532, and mount it at `/backups` in the `llmobs` container of the
StatefulSet. A `kubectl` patch is overwritten by the next `helm upgrade`: keep the change in a
post-renderer or in your own copy of the chart. Then run:

```bash
kubectl -n llmobs exec llmobs-0 -- llmobs admin backup --to /backups/$(date +%F)
```

**S3 mode.** Use the same commands. The backup is metadata only, so `--meta-only` is implied.

### Hard links and copies

In local mode, objects are hard-linked when `<dir>` is on the same filesystem as the objects. A backup
then costs only the new files. On another filesystem they are copied. Hard links on one filesystem
protect against operator error, not against losing the disk. Ship `<dir>` to another place with
restic, rsync or `tar`.

### Retention in S3 mode

An S3-mode backup is restorable against the live bucket for `storage.gc.grace` (default `5m`, at most
`30d`). Set it longer than the backups you want to keep. For a week of nightly backups:

```bash
LLMOBS_STORAGE__GC__GRACE=8d
```

Two things to know:

- `storage.gc.grace` keeps erased data in the obsolete files for up to its value: a deletion's data
  stays on disk until GC removes those files. Keep backups, and this window, at 30 days or less.
- Attachments (media) follow their own, shorter rule: a media blob is deleted an hour after no span
  references it. A restore can then lose attachments, but not traces. `gc-orphans` reports them as
  missing media.

For restores older than `gc.grace`, keep bucket versioning, or a copy of the bucket (see
[Cold backup](#s3-mode)).

### The manifest

`manifest.json` says what the backup holds. The main fields:

| Field | Meaning |
|---|---|
| `format` | the layout version (`1`) |
| `llmobs_version` | the version that wrote the backup |
| `created_at` | when the backup was taken (RFC 3339) |
| `mode` | `full` (local mode, objects included) or `meta-only` (S3 mode) |
| `object_store` | `local` or `s3`; no bucket name is written |
| `encryption_key` | `file`: `secret.key` is in the backup. `environment`: keep `LLMOBS_ENCRYPTION_KEY` with it |
| `flushed_seq` | the flushed sequence of spans and scores: the backup holds every row up to it |
| `gc_grace_secs` | the `gc.grace` in force when the backup was taken |
| `restorable_until` | for `meta-only`, `created_at` plus `gc.grace`; `null` for `full` |
| `files` | `meta.db`, `catalog.db` and `secret.key`, each with its size and SHA-256 |
| `objects` | the count and total bytes of the objects the backup lists |

## Cold backup

A cold backup is a copy of the data directory, and in S3 mode also of the bucket, taken while llmobs
is stopped. The steps below are the ones of earlier versions.

What a cold backup must contain is the list at the top of this page, with `wal/` included.

### Binary (systemd)

```bash
sudo systemctl stop llmobs
sudo tar --numeric-owner -C /var/lib -czf /backups/llmobs-$(date +%F).tar.gz llmobs
sudo systemctl start llmobs
```

### docker-compose

```bash
cd deploy/compose
docker compose stop llmobs
docker run --rm -v llmobs_llmobs-data:/data:ro -v "$PWD/backups:/backups" busybox:1.37 \
  tar --numeric-owner -C /data -czf "/backups/llmobs-$(date +%F).tar.gz" .
docker compose start llmobs
```

With `compose.s3.yaml`, also stop Garage and copy its two volumes in the same way
(`llmobs_garage-meta`, `llmobs_garage-data`) before starting both again.

### Kubernetes

```bash
kubectl -n llmobs scale statefulset llmobs --replicas 0
kubectl -n llmobs wait --for=delete pod/llmobs-0 --timeout 120s
# Snapshot the PVC data-llmobs-0 (a CSI VolumeSnapshot), or copy it with a pod that mounts it.
kubectl -n llmobs scale statefulset llmobs --replicas 1
```

The next `helm upgrade` sets the replica count back to 1 as well.

### S3 mode

Copy the bucket prefix while llmobs is stopped, after copying the data directory as above. With
the AWS CLI (any S3-compatible provider, `--endpoint-url` for non-AWS):

```bash
aws s3 sync s3://llmobs/prod s3://llmobs-backups/2026-11-14/prod --endpoint-url https://fsn1.your-objectstorage.com
```

or with rclone (`rclone sync src:llmobs/prod dst:llmobs-backups/2026-11-14/prod`). Copies inside
one provider are server-side and fast; copying into one fixed destination with versioning on is
an incremental alternative to dated copies.

## Restore

Restore with the server stopped. The steps are the same for an online and a cold backup, except
where the step says so. The order matters: the server must not start before the deletions are
re-applied.

1. Stop llmobs.
2. Keep the current data directory aside (move it, or keep a copy of its `meta.db`): you need it
   for step 8.
3. Create an empty data directory and copy the backup into it:
   - **cold backup:** the whole directory, as before;
   - **online backup:** `meta.db`, `catalog.db`, `secret.key` (if the backup has it) and, for a full
     backup, `objects/`. There is no `wal/` to copy: the server creates it, and the WAL resumes after
     the catalog's flushed sequence.

   Copy with `cp -a`, which keeps the hard links' content. The data directory must belong to the
   server's user (uid 65532 in the image); in the image, `tar --numeric-owner` as root keeps the owner.
4. In S3 mode:
   - **cold backup:** make the bucket prefix identical to the backup copy, deleting newer objects:
     `aws s3 sync s3://llmobs-backups/2026-11-14/prod s3://llmobs/prod --delete` (with
     `--endpoint-url` for non-AWS providers).
   - **online backup:** skip this step. Step 5 rolls the live bucket back.
5. With the server still stopped, run `llmobs admin gc-orphans --dry-run` first, then
   `llmobs admin gc-orphans`. Use the server's configuration (the same `LLMOBS_DATA_DIR` and object
   store). In S3 mode the real run also needs `--rollback-bucket`.

   The command deletes the objects in `tables/` and `media/` that the restored catalog does not know
   and that are older than `--min-age` (1 hour by default). It prints the counts:

   ```text
   orphans deleted: 3 (12.5 MiB)
   recent unknown objects kept: 0
   missing objects: 0
   ```

   - **S3 mode is a rollback of the live bucket.** Every object written after the backup is deleted
     permanently. Read the dry run first.
   - **Missing media** (`missing media: N`) is a warning: attachments are lost, traces are intact.
   - **Missing data files** (`missing objects` above 0) fail the command with exit status 1. The
     backup is older than `gc.grace` (meta-only), or `objects/` was not copied. The restore is
     incomplete: use a newer backup, or copy the missing `objects/` and run the command again.
     `missing objects: 0` is required before you go on.
6. If the backup is from an older version, migrate it, still with the server stopped:
   `llmobs admin migrate`. It migrates `meta.db` and `catalog.db` to this version's schema and exits.
   The server keeps a copy of each file from before the migration (see [upgrade.md](upgrade.md)).
7. Re-apply the deletions requested after the backup was taken:
   `llmobs admin reapply-deletions --from <kept data directory>`. It needs both databases at this
   version's schema, which is why step 6 comes first. It copies every deletion that the restored
   `meta.db` lacks, with its scope, and the server registers them within seconds of starting, before
   it serves requests. It does not cancel evaluation jobs queued in the restored data: those fail to
   load their traces and are pruned after 7 days.
8. Start llmobs with `llmobs serve`.

## Retention and privacy

Data deleted in llmobs (GDPR erasure, retention) stays in older backups until they expire. Keep
backups for 30 days or less, and re-apply deletions after every restore (step 7). In S3 mode, the
window in which a meta-only backup stays restorable is `storage.gc.grace`. That setting also keeps
erased data in obsolete files for up to its value (see [Retention in S3 mode](#retention-in-s3-mode)).
