# Upgrade

llmobs follows [Semantic Versioning](https://semver.org/). Before 1.0, a minor release (0.x → 0.y)
may need action; the release notes ([CHANGELOG.md](../../CHANGELOG.md), and each GitHub release)
start with `### Upgrade notes` when it does.

What happens on upgrade:

- Database migrations (`meta.db`, `catalog.db`) run automatically when the new version starts.
  They only go forward.
- Before migrating an existing `meta.db` or `catalog.db`, the server keeps a copy named
  `meta.db.pre-m<N>` and `catalog.db.pre-m<N>` in the data directory, where `<N>` is the highest
  migration applied before the upgrade (for example `meta.db.pre-m0012`). Only the copy from the last
  upgrade is kept.
  Delete the `*.pre-m*` files once the upgrade is verified. Until then they are copies of your data,
  like a backup, and count against your backup retention (GDPR).
- Parquet files are never rewritten for an upgrade; new columns are added, old files stay readable.
- The new version replays a WAL written by the previous one.
- Downgrading is not supported. An older version refuses data written by a newer one before it
  changes anything, with one of these messages: `meta.db has format version N…`,
  `meta.db has applied migration N, which this build does not have…`,
  `this data directory has format version N…`, `WAL segment … format N…`,
  `catalog.db has applied migration N…`. To go back, restore the pre-upgrade backup (or the
  `*.pre-m*` copies) and run the old binary. Deletions requested after the upgrade are not in that
  copy, and `reapply-deletions` reads a newer `meta.db` only when its format did not change: repeat
  those deletions by hand after a rollback.

Procedure:

1. Read the release notes of every version between yours and the new one.
2. Take a backup: `llmobs admin backup` ([backup.md](backup.md)).
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
