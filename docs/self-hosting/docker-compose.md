# docker-compose

The files are in [`deploy/compose/`](../../deploy/compose/) of the repository, at the release's tag:

| File | Purpose |
|---|---|
| `compose.yaml` | llmobs alone, all data on the volume `llmobs-data` |
| `compose.s3.yaml` | overlay: Parquet files and media in a bucket on a local single-node [Garage](https://garagehq.deuxfleurs.fr/) |
| `llmobs.env.example` | server settings (copy to `llmobs.env`) |
| `.env.example` | compose settings: image, published ports, Garage secrets (copy to `.env`) |
| `garage.toml`, `garage-init.sh` | Garage's config and its one-shot setup (layout, key, bucket `llmobs`) |

Server settings and compose settings are separate files on purpose: compose reads `.env` to fill
in `compose.yaml`, and passes only the variables in `llmobs.env` to the server. An `LLMOBS_`
variable set to an empty string is a set value, so leave unused lines commented out.

## Local volume

```bash
cd deploy/compose
cp llmobs.env.example llmobs.env    # edit LLMOBS_PUBLIC_URL at least
docker compose up -d
docker compose logs llmobs | grep 'setup required'
```

Open the setup link to create the first admin. The ports are published on 127.0.0.1 only; set
`LLMOBS_BIND=0.0.0.0` in `.env` to publish them on every interface, preferably behind a reverse
proxy that terminates TLS ([configuration.md](configuration.md#tls)).

## With Garage (S3)

```bash
cd deploy/compose
cp llmobs.env.example llmobs.env
cp .env.example .env
# Fill the four GARAGE_* values in .env, each with the command written above it.
docker compose -f compose.yaml -f compose.s3.yaml up -d
```

`garage-init` runs once at each `up` and exits; llmobs starts after it succeeded. The llmobs volume
then holds only the WAL, `catalog.db`, `meta.db` and `secret.key`; Parquet files and media are in
the bucket `llmobs` on the volumes `garage-meta` and `garage-data`. Keep the same `.env` for the
life of the deployment: the access key is imported once.

To use another S3-compatible service instead of the local Garage, do not use `compose.s3.yaml`;
set the bucket in `llmobs.env` as in [s3.md](s3.md).

## Operate

| Task | Command |
|---|---|
| Status and health | `docker compose ps` |
| Logs | `docker compose logs -f llmobs` |
| Stop (drains and flushes; up to 60 s) | `docker compose stop llmobs` |
| Admin commands | `docker compose exec llmobs llmobs admin list-users` |
| Upgrade | set `LLMOBS_IMAGE` in `.env`, then `docker compose up -d` ([upgrade.md](upgrade.md)) |
| Back up | [backup.md](backup.md#docker-compose) |

The volumes are named `<project>_llmobs-data` (the project is `llmobs` unless you pass `-p`).
`docker compose down` keeps them; `docker compose down -v` deletes them and all data.
