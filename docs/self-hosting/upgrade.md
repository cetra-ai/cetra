# Upgrade

llmobs follows [Semantic Versioning](https://semver.org/). Before 1.0, a minor release (0.x → 0.y)
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
sudo systemctl stop llmobs
curl -fsSL https://github.com/sebastiencs/llmobs/releases/latest/download/install.sh | LLMOBS_INSTALL_DIR=/usr/local/bin sudo -E sh
sudo systemctl start llmobs
```

## docker-compose

Set the new version in `.env` (`LLMOBS_IMAGE=ghcr.io/sebastiencs/llmobs:<version>`), then:

```bash
docker compose pull llmobs
docker compose up -d
```

Also replace the compose files with those of the new release when its notes say so.

## Kubernetes

```bash
helm upgrade llmobs oci://ghcr.io/sebastiencs/charts/llmobs --version <version> -n llmobs --reuse-values
```

The StatefulSet replaces its one pod: the old pod stops (up to 60 s) before the new one starts on
the same volume.
