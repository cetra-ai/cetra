# Self-hosting llmobs

llmobs is one binary, `llmobs`, that serves the web UI, the API, OTLP ingestion and MCP. It keeps
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
- [backup.md](backup.md): back up and restore, and restore mode for restore drills.
- [monitoring.md](monitoring.md): health checks, Prometheus metrics and alert examples.
- [upgrade.md](upgrade.md): upgrade to a new release.

Licences: the core is MIT; the `ee` directories are under the Elastic License 2.0 and their
features stay locked without a licence key (`LICENSE` in each archive and in
`/usr/share/doc/llmobs/` of the image). Third-party licences are in `THIRD-PARTY-NOTICES.md`
next to them.
