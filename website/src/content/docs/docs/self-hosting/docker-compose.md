---
title: "docker-compose"
description: "Run cetra with Docker Compose, on a local volume or with a local Garage object store."
---

The files are in [`deploy/compose/`](https://github.com/cetra-ai/cetra/tree/main/deploy/compose/) of the repository, at the release's tag:

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
proxy that terminates TLS ([Configuration](/docs/self-hosting/configuration/#tls)).

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
set the bucket in `cetra.env` as in [Object storage](/docs/self-hosting/s3/).

## Operate

| Task | Command |
|---|---|
| Status and health | `docker compose ps` |
| Logs | `docker compose logs -f cetra` |
| Stop (drains and flushes; up to 60 s) | `docker compose stop cetra` |
| Admin commands | `docker compose exec cetra cetra admin list-users` |
| Upgrade | set `CETRA_IMAGE` in `.env`, then `docker compose up -d` ([Upgrade](/docs/self-hosting/upgrade/)) |
| Back up | [Backup and restore](/docs/self-hosting/backup/#docker-compose) |

Health checks and the Prometheus metrics port are described in [Monitoring](/docs/self-hosting/monitoring/).

The volumes are named `<project>_cetra-data` (the project is `cetra` unless you pass `-p`).
`docker compose down` keeps them; `docker compose down -v` deletes them and all data.
