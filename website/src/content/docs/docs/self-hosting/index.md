---
title: "Self-hosting"
description: "Run cetra on your own machine or cluster: the ways to install it, its ports and where to configure it."
---

cetra is one binary, `cetra`, that serves the web UI, the API, OTLP ingestion and MCP. It keeps
its data in one directory (the WAL, `catalog.db`, `meta.db`, `secret.key`, and the Parquet files
and media unless they go to an S3-compatible bucket). One process owns that directory: run exactly
one instance per data directory.

| Method | Best for | Guide |
|---|---|---|
| Binary (`install.sh`) | a laptop, a VM, systemd | [Install](/docs/self-hosting/install/) |
| Docker image | anything that runs containers | [Install](/docs/self-hosting/install/#docker) |
| docker-compose | one server, optionally with a local S3 (Garage) | [Docker Compose](/docs/self-hosting/docker-compose/) |
| Helm chart | Kubernetes | [Kubernetes](/docs/self-hosting/kubernetes/) |

Ports:

| Port | Serves |
|---|---|
| 8080 | web UI, API, OTLP/HTTP (`/v1/traces`; `/v1/logs` and `/v1/metrics` are accepted and dropped), MCP (`/mcp`), `/healthz`, `/readyz` |
| 4317 | OTLP/gRPC |
| 4318 | OTLP/HTTP and `/v1/agent-context` (the standard OTLP port) |

Then:

- [Configuration](/docs/self-hosting/configuration/): settings, secrets, TLS.
- [Object storage](/docs/self-hosting/s3/): Parquet files and media in an S3-compatible bucket.
- [Backup and restore](/docs/self-hosting/backup/): back up and restore, and restore mode for restore drills.
- [Monitoring](/docs/self-hosting/monitoring/): health checks, Prometheus metrics and alert examples.
- [Telemetry](/docs/self-hosting/telemetry/): the anonymous daily heartbeat, field by field, and how to turn it off.
- [Upgrade](/docs/self-hosting/upgrade/): upgrade to a new release.

Licences: the core is MIT; the `ee` directories are under the Elastic License 2.0 and their
features stay locked without a licence key (`LICENSE` in each archive and in
`/usr/share/doc/cetra/` of the image). Third-party licences are in `THIRD-PARTY-NOTICES.md`
next to them.
