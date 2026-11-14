# Backup and restore

A backup is a copy of the data directory, and in S3 mode also of the bucket, taken while llmobs is
stopped (a cold snapshot, D37). Stopping takes up to a minute: llmobs drains requests and flushes
buffered spans to Parquet. While it is stopped, OTLP exporters retry and buffer; spans they drop
after their own retry limit are lost. Online backups without stopping the server are planned
(`llmobs admin backup`).

What a backup must contain:

- the whole data directory: `meta.db` and `catalog.db` (always together, from the same stop),
  the WAL (`wal/`), `secret.key`, and without S3 the Parquet files and media (`objects/`);
- in S3 mode, every object under the bucket prefix, copied during the same stop;
- the encryption key, if you set `LLMOBS_ENCRYPTION_KEY` instead of using `secret.key`: store it
  with the backup. A backup can only be read with the key that was current when it was taken.

`tmp/` (query spill files) and `LOCK` can be left out.

## Binary (systemd)

```bash
sudo systemctl stop llmobs
sudo tar --numeric-owner -C /var/lib -czf /backups/llmobs-$(date +%F).tar.gz llmobs
sudo systemctl start llmobs
```

## docker-compose

```bash
cd deploy/compose
docker compose stop llmobs
docker run --rm -v llmobs_llmobs-data:/data:ro -v "$PWD/backups:/backups" busybox:1.37 \
  tar --numeric-owner -C /data -czf "/backups/llmobs-$(date +%F).tar.gz" .
docker compose start llmobs
```

With `compose.s3.yaml`, also stop Garage and copy its two volumes in the same way
(`llmobs_garage-meta`, `llmobs_garage-data`) before starting both again.

## Kubernetes

```bash
kubectl -n llmobs scale statefulset llmobs --replicas 0
kubectl -n llmobs wait --for=delete pod/llmobs-0 --timeout 120s
# Snapshot the PVC data-llmobs-0 (a CSI VolumeSnapshot), or copy it with a pod that mounts it.
kubectl -n llmobs scale statefulset llmobs --replicas 1
```

The next `helm upgrade` sets the replica count back to 1 as well.

## S3 mode

Copy the bucket prefix while llmobs is stopped, after copying the data directory as above. With
the AWS CLI (any S3-compatible provider, `--endpoint-url` for non-AWS):

```bash
aws s3 sync s3://llmobs/prod s3://llmobs-backups/2026-11-14/prod --endpoint-url https://fsn1.your-objectstorage.com
```

or with rclone (`rclone sync src:llmobs/prod dst:llmobs-backups/2026-11-14/prod`). Copies inside
one provider are server-side and fast; copying into one fixed destination with versioning on is
an incremental alternative to dated copies.

## Restore

1. Stop llmobs.
2. Keep the current data directory (move it aside, or keep a copy of its `meta.db`): you need it
   for step 6.
3. Replace the data directory with the backup (`tar --numeric-owner -xzf` as root keeps the
   files' owner; in the image they belong to uid 65532).
4. In S3 mode, make the bucket prefix identical to the backup copy, deleting newer objects:
   `aws s3 sync s3://llmobs-backups/2026-11-14/prod s3://llmobs/prod --delete` (with
   `--endpoint-url` for non-AWS providers).
5. Start llmobs. A backup from an older version is migrated on start.
6. Deletions requested after the backup was taken are not in it. Re-apply them from the data
   directory you kept: `llmobs admin reapply-deletions --from <kept data directory>`.

## Retention and privacy

Data deleted in llmobs (GDPR erasure, retention) stays in older backups until they expire. Keep
backups for 30 days or less, and re-apply deletions after every restore (step 6).
