# N11 — Self-hosting documentation

Read [README.md](README.md) first, especially § Design › Backups and upgrades.

**Goal:** the user documentation for installing, configuring, upgrading, backing up and restoring
cetra: install, configuration, upgrade, and the snapshot backup and restore procedure (D37,
D-N-11, D-N-12). It goes in `docs/self-hosting/`, a new directory that the future documentation
site takes over. Then the roadmap row for N says the track is implemented.

**Edit only:**
- `docs/self-hosting/README.md`, `install.md`, `docker-compose.md`, `kubernetes.md`,
  `configuration.md`, `s3.md`, `backup.md`, `upgrade.md` (new);
- `docs/roadmap.md`.

**Commit:** `Add the self-hosting documentation`

## Two conditional parts

Check each condition first and write the variant it selects.

1. **`reapply-deletions` (track M's M7).** Run `grep -qi reapply crates/cli/src/main.rs`.
   - It succeeds: write `backup.md` exactly as given.
   - It fails: in `backup.md` § Restore, replace step 6 with: "6. Deletions requested after the
     backup was taken are not in it. Once your version has `cetra admin reapply-deletions`,
     re-apply them with `cetra admin reapply-deletions --from <kept data directory>`; until then,
     repeat those deletions by hand." In § Retention and privacy, replace "and re-apply deletions
     after every restore (step 6)" with "and repeat deletions after every restore (step 6)".
2. **Licence key (track P).** Run `grep -q license_key crates/server/src/config.rs`.
   - It succeeds: write `configuration.md` exactly as given.
   - It fails: delete the table row whose first cell is `` `license_key` ``, and in
     `kubernetes.md` delete "the licence key, " from the sentence listing what goes in a Secret.

Record in your report which variant you wrote for each.

## Files

Create each file with exactly this content. The links between them are relative and must resolve:
the Verify loop checks them.

### `docs/self-hosting/README.md`

```markdown
# Self-hosting cetra

cetra is one binary, `cetra`, that serves the web UI, the API, OTLP ingestion and MCP. It keeps
its data in one directory (the WAL, `catalog.db`, `meta.db`, `secret.key`, and the Parquet files
and media unless they go to an S3-compatible bucket). One process owns that directory: run exactly
one instance per data directory.

| Method | Best for | Guide |
|---|---|---|
| Binary (`install.sh`) | a laptop, a VM, systemd | [install.md](install.md) |
| Docker image | anything that runs containers | [install.md](install.md#docker) |
| docker-compose | one server, optionally with a local S3 (Garage) | [docker-compose.md](docker-compose.md) |
| Helm chart | Kubernetes | [kubernetes.md](kubernetes.md) |

Ports:

| Port | Serves |
|---|---|
| 8080 | web UI, API, OTLP/HTTP (`/v1/traces`), MCP (`/mcp`), `/healthz`, `/readyz` |
| 4317 | OTLP/gRPC |
| 4318 | OTLP/HTTP only (the standard OTLP port) |

Then:

- [configuration.md](configuration.md): settings, secrets, TLS.
- [s3.md](s3.md): Parquet files and media in an S3-compatible bucket.
- [backup.md](backup.md): back up and restore.
- [upgrade.md](upgrade.md): upgrade to a new release.

Licences: the core is MIT; the `ee` directories are under the Elastic License 2.0 and their
features stay locked without a licence key (`LICENSE` in each archive and in
`/usr/share/doc/cetra/` of the image). Third-party licences are in `THIRD-PARTY-NOTICES.md`
next to them.
```

### `docs/self-hosting/install.md`

````markdown
# Install

Releases are on GitHub: <https://github.com/cetra-ai/cetra/releases>. Each release has:

| File | What it is |
|---|---|
| `cetra-<version>-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, static |
| `cetra-<version>-aarch64-unknown-linux-musl.tar.gz` | Linux aarch64 (arm64), static |
| `cetra-<version>-aarch64-apple-darwin.tar.gz` | macOS on Apple silicon |
| `SHA256SUMS` | SHA-256 of each archive |
| `cetra-<version>-<target>.spdx.json` | SBOM of each binary (SPDX) |
| `cetra-<version>.tgz` | the Helm chart (also on `oci://ghcr.io/cetra-ai/charts/cetra`) |
| `install.sh` | the install script below |

Each archive holds the `cetra` binary (web UI included), `LICENSE`, `LICENSE-ELASTIC` and
`THIRD-PARTY-NOTICES.md`. Other platforms, including Windows and Intel Macs: use the Docker image.

## Binary

```bash
curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh
```

The script picks the archive for your OS and CPU, checks it against `SHA256SUMS`, and copies
`cetra` to `~/.local/bin`. `CETRA_VERSION=0.1.0` installs a given version and
`CETRA_INSTALL_DIR=/usr/local/bin` another directory; for a root-owned directory pipe it to
`CETRA_INSTALL_DIR=/usr/local/bin sudo -E sh`.

Without the script:

```bash
version=0.1.0
target=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/cetra-ai/cetra/releases/download/v$version/cetra-$version-$target.tar.gz"
curl -fsSLO "https://github.com/cetra-ai/cetra/releases/download/v$version/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "cetra-$version-$target.tar.gz"
sudo install -m 0755 "cetra-$version-$target/cetra" /usr/local/bin/cetra
```

Run it:

```bash
cetra serve
```

It keeps its data in `./cetra-data` (set `CETRA_DATA_DIR` to choose) and listens on 8080, 4317
and 4318. On the first start it logs a setup link, `setup required: open
http://localhost:8080/setup?token=…`; open it to create the first admin, organization and project.
To skip the setup page, set `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD`,
`CETRA_INIT_ORG` and `CETRA_INIT_PROJECT` (and optionally `CETRA_INIT_SECRET_KEY`) for the first
start ([configuration.md](configuration.md)).

### systemd

```ini
# /etc/systemd/system/cetra.service
[Unit]
Description=cetra
After=network-online.target
Wants=network-online.target

[Service]
User=cetra
Group=cetra
ExecStart=/usr/local/bin/cetra serve
Environment=CETRA_DATA_DIR=/var/lib/cetra
Environment=CETRA_PUBLIC_URL=https://cetra.example.com
EnvironmentFile=-/etc/cetra/cetra.env
StateDirectory=cetra
# Shutdown drains requests (20 s) and flushes buffered spans (30 s).
TimeoutStopSec=60
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home /var/lib/cetra --shell /usr/sbin/nologin cetra
sudo systemctl daemon-reload
sudo systemctl enable --now cetra
```

## Docker

```bash
docker run -d --name cetra \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 \
  -v cetra-data:/data \
  -e CETRA_PUBLIC_URL=http://localhost:8080 \
  ghcr.io/cetra-ai/cetra:0.1.0
docker logs cetra 2>&1 | grep 'setup required'
```

The image (`linux/amd64`, `linux/arm64`) is distroless: no shell, the binary at
`/usr/local/bin/cetra`, running as uid/gid 65532, data in the volume at `/data`. Its health check
runs `cetra health`, which asks `/readyz`. Tags: `0.1.0` (exact), `0.1` (latest patch of 0.1) and
`latest`; pin an exact version in production. For more than a quick try, use
[docker-compose.md](docker-compose.md).

## Verify a download

Checksums are in `SHA256SUMS`. Each archive, the image and the chart also have a build provenance
attestation, signed by GitHub Actions through Sigstore, that ties them to the release workflow run
and commit. With the GitHub CLI:

```bash
gh attestation verify cetra-0.1.0-x86_64-unknown-linux-musl.tar.gz --repo cetra-ai/cetra
gh attestation verify oci://ghcr.io/cetra-ai/cetra:0.1.0 --repo cetra-ai/cetra
gh attestation verify oci://ghcr.io/cetra-ai/charts/cetra:0.1.0 --repo cetra-ai/cetra
```

The image also carries an SPDX SBOM and a provenance attestation from its build:
`docker buildx imagetools inspect ghcr.io/cetra-ai/cetra:0.1.0 --format '{{ json .SBOM }}'`.
````

### `docs/self-hosting/docker-compose.md`

````markdown
# docker-compose

The files are in [`deploy/compose/`](../../deploy/compose/) of the repository, at the release's tag:

| File | Purpose |
|---|---|
| `compose.yaml` | cetra alone, all data on the volume `cetra-data` |
| `compose.s3.yaml` | overlay: Parquet files and media in a bucket on a local single-node [Garage](https://garagehq.deuxfleurs.fr/) |
| `cetra.env.example` | server settings (copy to `cetra.env`) |
| `.env.example` | compose settings: image, published ports, Garage secrets (copy to `.env`) |
| `garage.toml`, `garage-init.sh` | Garage's config and its one-shot setup (layout, key, bucket `cetra`) |

Server settings and compose settings are separate files on purpose: compose reads `.env` to fill
in `compose.yaml`, and passes only the variables in `cetra.env` to the server. A`CETRA_`
variable set to an empty string is a set value, so leave unused lines commented out.

## Local volume

```bash
cd deploy/compose
cp cetra.env.example cetra.env    # edit CETRA_PUBLIC_URL at least
docker compose up -d
docker compose logs cetra | grep 'setup required'
```

Open the setup link to create the first admin. The ports are published on 127.0.0.1 only; set
`CETRA_BIND=0.0.0.0` in `.env` to publish them on every interface, preferably behind a reverse
proxy that terminates TLS ([configuration.md](configuration.md#tls)).

## With Garage (S3)

```bash
cd deploy/compose
cp cetra.env.example cetra.env
cp .env.example .env
# Fill the four GARAGE_* values in .env, each with the command written above it.
docker compose -f compose.yaml -f compose.s3.yaml up -d
```

`garage-init` runs once at each `up` and exits; cetra starts after it succeeded. The cetra volume
then holds only the WAL, `catalog.db`, `meta.db` and `secret.key`; Parquet files and media are in
the bucket `cetra` on the volumes `garage-meta` and `garage-data`. Keep the same `.env` for the
life of the deployment: the access key is imported once.

To use another S3-compatible service instead of the local Garage, do not use `compose.s3.yaml`;
set the bucket in `cetra.env` as in [s3.md](s3.md).

## Operate

| Task | Command |
|---|---|
| Status and health | `docker compose ps` |
| Logs | `docker compose logs -f cetra` |
| Stop (drains and flushes; up to 60 s) | `docker compose stop cetra` |
| Admin commands | `docker compose exec cetra cetra admin list-users` |
| Upgrade | set `CETRA_IMAGE` in `.env`, then `docker compose up -d` ([upgrade.md](upgrade.md)) |
| Back up | [backup.md](backup.md#docker-compose) |

The volumes are named `<project>_cetra-data` (the project is `cetra` unless you pass `-p`).
`docker compose down` keeps them; `docker compose down -v` deletes them and all data.
````

### `docs/self-hosting/kubernetes.md`

````markdown
# Kubernetes (Helm)

The chart runs cetra as a StatefulSet with exactly one pod and one PersistentVolumeClaim mounted at
`/data`. cetra is a single process that locks its data directory, so there is no replica setting
and no horizontal scaling. The volume must be `ReadWriteOnce` block or local storage: the lock
(`flock`) is not reliable on NFS-like volumes.

```bash
helm install cetra oci://ghcr.io/cetra-ai/charts/cetra --version 0.1.0 \
  --namespace cetra --create-namespace \
  --set publicUrl=https://cetra.example.com
kubectl -n cetra logs cetra-0 | grep 'setup required'
```

The chart version is the cetra version. The values are listed in the chart's
[README](../../deploy/helm/cetra/README.md); the main ones:

| Value | Default | Purpose |
|---|---|---|
| `publicUrl` | `http://localhost:8080` | `CETRA_PUBLIC_URL` |
| `persistence.size`, `persistence.storageClassName` | `20Gi`, cluster default | the data volume |
| `config` | `{}` | nested settings written to `cetra.toml` |
| `env` | `[]` | extra environment variables |
| `secret.existingSecret` | `""` | a Secret whose keys become environment variables |
| `secret.values` | `{}` | variables of a Secret the chart creates |
| `s3.*` | off | Parquet files and media in a bucket ([s3.md](s3.md)) |
| `ingress.*` | off | an Ingress for the HTTP port |

Secrets (SMTP URL, OAuth client secrets, the licence key, `AWS_*` credentials,
`CETRA_ENCRYPTION_KEY`) belong in a Secret you create, named by `secret.existingSecret`:

```bash
kubectl -n cetra create secret generic cetra-env \
  --from-literal=CETRA_SMTP_URL='smtps://user:password@smtp.example.com:465' \
  --from-literal=CETRA_EMAIL_FROM='cetra <noreply@cetra.sh>'
helm upgrade cetra oci://ghcr.io/cetra-ai/charts/cetra --version 0.1.0 -n cetra \
  --reuse-values --set secret.existingSecret=cetra-env
```

Without `CETRA_ENCRYPTION_KEY`, cetra generates `/data/secret.key` on its first start; it lives
on the volume and must be backed up with it ([backup.md](backup.md)).

OTLP: the Service exposes 8080 (HTTP, including `/v1/traces`), 4317 (gRPC) and 4318 (HTTP). The
Ingress routes 8080 only; expose gRPC through a Service of type `LoadBalancer` or a gRPC-aware
ingress if you need it from outside the cluster.

The pod runs as uid/gid 65532 with a read-only root filesystem and no capabilities. Probes:
startup and liveness on `/healthz` (the startup probe allows 10 minutes for WAL replay), readiness
on `/readyz`. `terminationGracePeriodSeconds` is 60 so a stop can drain and flush.

Upgrades: [upgrade.md](upgrade.md#kubernetes). Backups: [backup.md](backup.md#kubernetes).
````

### `docs/self-hosting/configuration.md`

```markdown
# Configuration

cetra reads, in order, each overriding the previous: built-in defaults, a TOML file
(`cetra serve --config <file>` or `CETRA_CONFIG=<file>`), then environment variables. Every key
can be set as `CETRA_` followed by its path in upper case with `__` between levels:
`storage.flush.max_age` is `CETRA_STORAGE__FLUSH__MAX_AGE`. Common keys also have short names
(below). Unknown keys are an error, and so is an unknown `CETRA_` variable. Sizes are strings
with a binary unit (`"256MiB"`) and durations strings with a unit (`"120s"`). The full list with
defaults is in [spec 12 § Configuration](../spec/12-operations.md#configuration).

## The settings most installs set

| Key | Environment | Default | Notes |
|---|---|---|---|
| `data_dir` | `CETRA_DATA_DIR` | `./cetra-data` (`/data` in the image) | one process per directory |
| `public_url` | `CETRA_PUBLIC_URL` | `http://localhost:8080` | the URL users open; used in emails and OAuth redirects |
| `http_addr` | `CETRA_HTTP_ADDR` | `0.0.0.0:8080` | UI, API, OTLP/HTTP, MCP |
| `grpc_addr` | `CETRA_GRPC_ADDR` | `0.0.0.0:4317` | empty disables OTLP/gRPC |
| `otlp_http_addr` | `CETRA_OTLP_HTTP_ADDR` | `0.0.0.0:4318` | empty disables it |
| `trusted_proxies` | `CETRA_TRUSTED_PROXIES` | none | comma-separated CIDRs whose `X-Forwarded-For` is trusted |
| `smtp_url`, `email_from` | `CETRA_SMTP_URL`, `CETRA_EMAIL_FROM` | unset (email off) | invites, password reset |
| `encryption_key` | `CETRA_ENCRYPTION_KEY` | `<data_dir>/secret.key`, generated | encrypts stored secrets |
| `license_key` | `CETRA_LICENSE_KEY` | unset | enterprise features |
| `storage.object_store` | `CETRA_OBJECT_STORE` | `file://<data_dir>/objects` | `s3://bucket[/prefix]`: [s3.md](s3.md) |
| `log.level` | `CETRA_LOG` | `info` | |
| `log.format` | `CETRA_LOG__FORMAT` | `pretty` on a terminal, `json` otherwise | |

First start without the setup page: set `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD`,
`CETRA_INIT_ORG` and `CETRA_INIT_PROJECT` together, and optionally `CETRA_INIT_SECRET_KEY`
(`sk-cetra-` followed by 32 letters or digits) to know the project's ingest key in advance. They
apply only to an empty database.

## Secrets

Keep secrets in environment variables from a file only the service can read (`cetra.env` with
compose, an `EnvironmentFile` with systemd, a Secret with Helm), not in `cetra.toml` if that file
is shared. Logs never contain secret values.

The encryption key protects stored LLM connection keys and webhook secrets. Back it up with the
data: a backup without its key cannot be read ([backup.md](backup.md)). `cetra admin
rotate-encryption-key` replaces it while the server is stopped.

## TLS

cetra serves plain HTTP. Put a reverse proxy in front (Caddy, nginx, Traefik, a cloud load
balancer), set `public_url` to the `https://` URL, and set `trusted_proxies` to the proxy's
address so client IPs in rate limits and logs are right. For OTLP/gRPC through a proxy, the proxy
must speak HTTP/2 to cetra on port 4317.
```

### `docs/self-hosting/s3.md`

```markdown
# S3-compatible object storage

By default cetra keeps everything in its data directory. With an object store, the Parquet files
and media (the bulk of the data) go to a bucket instead; the data directory keeps the WAL,
`catalog.db`, `meta.db` and `secret.key`, which stay on a local disk (SSD or NVMe) for speed.

| Setting | Environment | Example |
|---|---|---|
| `storage.object_store` | `CETRA_OBJECT_STORE` | `s3://cetra` or `s3://cetra/prod` (bucket and optional prefix) |
| `storage.s3.region` | `AWS_REGION` | `eu-central-1` |
| `storage.s3.endpoint` | `CETRA_STORAGE__S3__ENDPOINT` | unset for AWS; the provider's URL otherwise |
| `storage.s3.force_path_style` | `CETRA_STORAGE__S3__FORCE_PATH_STYLE` | `true` when the provider has no virtual-host buckets |
| `storage.s3.allow_http` | `CETRA_STORAGE__S3__ALLOW_HTTP` | `true` only for a plain-HTTP endpoint on a private network |
| credentials | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | keep them in a secret |

The bucket must exist, and the key needs to list, read, write and delete objects in it (under
the prefix). Use a bucket, or a prefix, per cetra instance. Examples:

| Provider | `CETRA_OBJECT_STORE` | `AWS_REGION` | `CETRA_STORAGE__S3__ENDPOINT` | Path style |
|---|---|---|---|---|
| AWS S3 | `s3://my-bucket` | the bucket's region | unset | `false` |
| Hetzner Object Storage | `s3://my-bucket` | `fsn1` (the location) | `https://fsn1.your-objectstorage.com` | `false` |
| Cloudflare R2 | `s3://my-bucket` | `auto` | `https://<account id>.r2.cloudflarestorage.com` | `true` |
| Garage | `s3://cetra` | the `s3_region` of `garage.toml` | `http://garage:3900` | `true` |

Moving an existing installation between local storage and a bucket is not supported: choose before
the first start.

For backups in S3 mode, see [backup.md](backup.md#s3-mode).
```

### `docs/self-hosting/backup.md`

````markdown
# Backup and restore

A backup is a copy of the data directory, and in S3 mode also of the bucket, taken while cetra is
stopped (a cold snapshot, D37). Stopping takes up to a minute: cetra drains requests and flushes
buffered spans to Parquet. While it is stopped, OTLP exporters retry and buffer; spans they drop
after their own retry limit are lost. Online backups without stopping the server are planned
(`cetra admin backup`).

What a backup must contain:

- the whole data directory: `meta.db` and `catalog.db` (always together, from the same stop),
  the WAL (`wal/`), `secret.key`, and without S3 the Parquet files and media (`objects/`);
- in S3 mode, every object under the bucket prefix, copied during the same stop;
- the encryption key, if you set `CETRA_ENCRYPTION_KEY` instead of using `secret.key`: store it
  with the backup. A backup can only be read with the key that was current when it was taken.

`tmp/` (query spill files) and `LOCK` can be left out.

## Binary (systemd)

```bash
sudo systemctl stop cetra
sudo tar --numeric-owner -C /var/lib -czf /backups/cetra-$(date +%F).tar.gz cetra
sudo systemctl start cetra
```

## docker-compose

```bash
cd deploy/compose
docker compose stop cetra
docker run --rm -v cetra_cetra-data:/data:ro -v "$PWD/backups:/backups" busybox:1.37 \
  tar --numeric-owner -C /data -czf "/backups/cetra-$(date +%F).tar.gz" .
docker compose start cetra
```

With `compose.s3.yaml`, also stop Garage and copy its two volumes in the same way
(`cetra_garage-meta`, `cetra_garage-data`) before starting both again.

## Kubernetes

```bash
kubectl -n cetra scale statefulset cetra --replicas 0
kubectl -n cetra wait --for=delete pod/cetra-0 --timeout 120s
# Snapshot the PVC data-cetra-0 (a CSI VolumeSnapshot), or copy it with a pod that mounts it.
kubectl -n cetra scale statefulset cetra --replicas 1
```

The next `helm upgrade` sets the replica count back to 1 as well.

## S3 mode

Copy the bucket prefix while cetra is stopped, after copying the data directory as above. With
the AWS CLI (any S3-compatible provider, `--endpoint-url` for non-AWS):

```bash
aws s3 sync s3://cetra/prod s3://cetra-backups/2026-11-14/prod --endpoint-url https://fsn1.your-objectstorage.com
```

or with rclone (`rclone sync src:cetra/prod dst:cetra-backups/2026-11-14/prod`). Copies inside
one provider are server-side and fast; copying into one fixed destination with versioning on is
an incremental alternative to dated copies.

## Restore

1. Stop cetra.
2. Keep the current data directory (move it aside, or keep a copy of its `meta.db`): you need it
   for step 6.
3. Replace the data directory with the backup (`tar --numeric-owner -xzf` as root keeps the
   files' owner; in the image they belong to uid 65532).
4. In S3 mode, make the bucket prefix identical to the backup copy, deleting newer objects:
   `aws s3 sync s3://cetra-backups/2026-11-14/prod s3://cetra/prod --delete` (with
   `--endpoint-url` for non-AWS providers).
5. Start cetra. A backup from an older version is migrated on start.
6. Deletions requested after the backup was taken are not in it. Re-apply them from the data
   directory you kept: `cetra admin reapply-deletions --from <kept data directory>`.

## Retention and privacy

Data deleted in cetra (GDPR erasure, retention) stays in older backups until they expire. Keep
backups for 30 days or less, and re-apply deletions after every restore (step 6).
````

### `docs/self-hosting/upgrade.md`

````markdown
# Upgrade

cetra follows [Semantic Versioning](https://semver.org/). Before 1.0, a minor release (0.x → 0.y)
may need action; the release notes ([CHANGELOG.md](../../CHANGELOG.md), and each GitHub release)
start with `### Upgrade notes` when it does.

What happens on upgrade:

- Database migrations (`meta.db`, `catalog.db`) run automatically when the new version starts.
  They only go forward.
- Parquet files are never rewritten for an upgrade; new columns are added, old files stay readable.
- The new version replays a WAL written by the previous one.
- Downgrading is not supported: after a new version has started on the data, the old version
  may refuse it. To go back, restore the backup taken before the upgrade.

Procedure:

1. Read the release notes of every version between yours and the new one.
2. Take a backup ([backup.md](backup.md)).
3. Replace the binary, image or chart version (below) and start it.
4. Check `/readyz` and the UI.

## Binary

```bash
sudo systemctl stop cetra
curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | CETRA_INSTALL_DIR=/usr/local/bin sudo -E sh
sudo systemctl start cetra
```

## docker-compose

Set the new version in `.env` (`CETRA_IMAGE=ghcr.io/cetra-ai/cetra:<version>`), then:

```bash
docker compose pull cetra
docker compose up -d
```

Also replace the compose files with those of the new release when its notes say so.

## Kubernetes

```bash
helm upgrade cetra oci://ghcr.io/cetra-ai/charts/cetra --version <version> -n cetra --reuse-values
```

The StatefulSet replaces its one pod: the old pod stops (up to 60 s) before the new one starts on
the same volume.
````

### `docs/roadmap.md`

In the Wave 2 table, replace the **N** row's `Status` cell with:

```markdown
**Implemented.** N1–N11: release archives (Linux x86_64/aarch64 static, macOS arm64), the image on GHCR, docker-compose (local, Garage), the Helm chart, `install.sh`, the release workflow with SBOMs and attestations, licence notices, `docs/self-hosting/`. Publishing waits for the name (D46, D-N-6). Online backups are N's M2.
```

Leave the other cells as the plan commit set them.

## Verify

```bash
for f in docs/self-hosting/*.md; do
  grep -o '](\(\.\./\)*[A-Za-z0-9_./-]*[)#]' "$f" | sed 's/^](//; s/[)#]$//' | grep -v '^$' | while read -r link; do
    test -e "docs/self-hosting/$link" || echo "$f: broken link $link"
  done
done
for anchor in 'install.md:## Docker' 'configuration.md:## TLS' 'backup.md:## S3 mode' 'backup.md:## docker-compose' 'backup.md:## Kubernetes' 'upgrade.md:## Kubernetes'; do
  grep -qx "${anchor#*:}" "docs/self-hosting/${anchor%%:*}" || echo "missing heading for $anchor"
done
grep -q '^| \*\*N\*\* Packaging and ops .*\*\*Implemented\.\*\* N1–N11' docs/roadmap.md || echo "roadmap row not updated"
git diff docs/roadmap.md
just check
just assert-clean
```

The loops and the roadmap grep must print nothing; the diff must change only the N row's Status
cell.

Also check these facts against the code; each is stated in the docs.
- `cetra admin list-users` exists (`crates/cli/src/main.rs`).
- The setup log line is `setup required: open <public_url>/setup?token=…`
  (`crates/server/src/serve.rs` test `an_empty_database_logs_the_setup_url_once`).
- The compose volume name is `cetra_cetra-data`: project `cetra`, from `name: cetra` in
  `compose.yaml`.

Report any mismatch instead of changing the code.

## Done when

- [ ] The eight docs exist, with the conditional variants recorded in your report; the roadmap row
      is updated.
- [ ] The link check prints nothing; `just check` and `just assert-clean` pass. One commit:
      `Add the self-hosting documentation`.
