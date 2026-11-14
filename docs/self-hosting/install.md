# Install

Releases are on GitHub: <https://github.com/sebastiencs/llmobs/releases>. Each release has:

| File | What it is |
|---|---|
| `llmobs-<version>-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, static |
| `llmobs-<version>-aarch64-unknown-linux-musl.tar.gz` | Linux aarch64 (arm64), static |
| `llmobs-<version>-aarch64-apple-darwin.tar.gz` | macOS on Apple silicon |
| `SHA256SUMS` | SHA-256 of each archive |
| `llmobs-<version>-<target>.spdx.json` | SBOM of each binary (SPDX) |
| `llmobs-<version>.tgz` | the Helm chart (also on `oci://ghcr.io/sebastiencs/charts/llmobs`) |
| `install.sh` | the install script below |

Each archive holds the `llmobs` binary (web UI included), `LICENSE`, `LICENSE-ELASTIC` and
`THIRD-PARTY-NOTICES.md`. Other platforms, including Windows and Intel Macs: use the Docker image.

## Binary

```bash
curl -fsSL https://github.com/sebastiencs/llmobs/releases/latest/download/install.sh | sh
```

The script picks the archive for your OS and CPU, checks it against `SHA256SUMS`, and copies
`llmobs` to `~/.local/bin`. `LLMOBS_VERSION=0.1.0` installs a given version and
`LLMOBS_INSTALL_DIR=/usr/local/bin` another directory; for a root-owned directory pipe it to
`LLMOBS_INSTALL_DIR=/usr/local/bin sudo -E sh`.

Without the script:

```bash
version=0.1.0
target=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/sebastiencs/llmobs/releases/download/v$version/llmobs-$version-$target.tar.gz"
curl -fsSLO "https://github.com/sebastiencs/llmobs/releases/download/v$version/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "llmobs-$version-$target.tar.gz"
sudo install -m 0755 "llmobs-$version-$target/llmobs" /usr/local/bin/llmobs
```

Run it:

```bash
llmobs serve
```

It keeps its data in `./llmobs-data` (set `LLMOBS_DATA_DIR` to choose) and listens on 8080, 4317
and 4318. On the first start it logs a setup link, `setup required: open
http://localhost:8080/setup?token=…`; open it to create the first admin, organization and project.
To skip the setup page, set `LLMOBS_INIT_ADMIN_EMAIL`, `LLMOBS_INIT_ADMIN_PASSWORD`,
`LLMOBS_INIT_ORG` and `LLMOBS_INIT_PROJECT` (and optionally `LLMOBS_INIT_SECRET_KEY`) for the first
start ([configuration.md](configuration.md)).

### systemd

```ini
# /etc/systemd/system/llmobs.service
[Unit]
Description=llmobs
After=network-online.target
Wants=network-online.target

[Service]
User=llmobs
Group=llmobs
ExecStart=/usr/local/bin/llmobs serve
Environment=LLMOBS_DATA_DIR=/var/lib/llmobs
Environment=LLMOBS_PUBLIC_URL=https://llmobs.example.com
EnvironmentFile=-/etc/llmobs/llmobs.env
StateDirectory=llmobs
# Shutdown drains requests (20 s) and flushes buffered spans (30 s).
TimeoutStopSec=60
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home /var/lib/llmobs --shell /usr/sbin/nologin llmobs
sudo systemctl daemon-reload
sudo systemctl enable --now llmobs
```

## Docker

```bash
docker run -d --name llmobs \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 \
  -v llmobs-data:/data \
  -e LLMOBS_PUBLIC_URL=http://localhost:8080 \
  ghcr.io/sebastiencs/llmobs:0.1.0
docker logs llmobs 2>&1 | grep 'setup required'
```

The image (`linux/amd64`, `linux/arm64`) is distroless: no shell, the binary at
`/usr/local/bin/llmobs`, running as uid/gid 65532, data in the volume at `/data`. Its health check
runs `llmobs health`, which asks `/readyz`. Tags: `0.1.0` (exact), `0.1` (latest patch of 0.1) and
`latest`; pin an exact version in production. For more than a quick try, use
[docker-compose.md](docker-compose.md).

## Verify a download

Checksums are in `SHA256SUMS`. Each archive, the image and the chart also have a build provenance
attestation, signed by GitHub Actions through Sigstore, that ties them to the release workflow run
and commit. With the GitHub CLI:

```bash
gh attestation verify llmobs-0.1.0-x86_64-unknown-linux-musl.tar.gz --repo sebastiencs/llmobs
gh attestation verify oci://ghcr.io/sebastiencs/llmobs:0.1.0 --repo sebastiencs/llmobs
gh attestation verify oci://ghcr.io/sebastiencs/charts/llmobs:0.1.0 --repo sebastiencs/llmobs
```

The image also carries an SPDX SBOM and a provenance attestation from its build:
`docker buildx imagetools inspect ghcr.io/sebastiencs/llmobs:0.1.0 --format '{{ json .SBOM }}'`.
