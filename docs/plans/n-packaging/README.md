# N — Packaging and release

Implementation plan for track N: how cetra is built, packaged, published and documented for
self-hosting. It covers:

- release archives of the `cetra` binary (UI embedded) for Linux x86_64 and aarch64 and macOS arm64;
- a multi-arch Docker image on GHCR;
- docker-compose files, with an optional local S3 (Garage);
- a minimal Helm chart;
- an install script;
- the release workflow;
- licence notices;
- the self-hosting documentation, including the snapshot backup procedure (D37).

The plan has 11 sub-phases, N1 to N11. One agent does each sub-phase in one go, in order, using
only this README and its own sub-phase file. Each sub-phase ends with its checks green and one
commit.

**Why a plan and no design doc.** Packaging uses well-known tools: cargo, Docker, Compose, Helm and
GitHub Actions. § Design below is the whole design. The decisions are `D-N-1`–`D-N-16` in
`docs/mvp-decisions.md` § Track N — Packaging. Limits, quotas and the usage API are track P's
(D179), not N's.

**Prerequisites** (checked when this plan was written, 2026-11-14):

- G is done: `cetra serve` serves the embedded UI, `/healthz` and `/readyz`, and
  `just check-release` exists (D-G-18, D-G-37).
- **N3 to N11 need track P's P1** (`docs/plans/p-entitlements/01-m1-open-core-license.md` § P1)
  on `main`. P1 adds the root `LICENSE` and `crates/server/src/ee/LICENSE`, which every archive
  and the image ship. Check before N3: `test -f LICENSE && test -f crates/server/src/ee/LICENSE`.
  If either file is missing, stop and report.
- N11 has one paragraph that depends on track M's M7 (`cetra admin reapply-deletions`). N11
  says what to write in each case.
- The tools in § Local tools are installed. If one is missing, stop and report. Do not install
  system packages yourself.

## Read first (every sub-phase)

- `AGENTS.md`, all of it, especially § CI (actions pinned by SHA with the tag in a comment, `run:`
  steps call `just` recipes, new checks go in the `justfile` first, `just check-workflows`).
- This README, all of it. Then read your own sub-phase file. Do not read the other sub-phase files
  unless yours says so.
- `docs/spec/12-operations.md` § Configuration, § Packaging, § Process lifecycle, § Backups and
  § Upgrades.
- `docs/mvp-decisions.md`:
  - D37, D46, D81, D172;
  - D-G-18, D-G-21, D-G-35, D-G-37;
  - every `D-N-n`.

## Scope

**In:**

| Area | What | Phase |
|---|---|---|
| CLI | `cetra health`: the container healthcheck | N1 |
| Licences | `THIRD-PARTY-NOTICES.md` (Rust crates through cargo-about, web packages through pnpm) | N2 |
| Archives | `just dist <target>`: static musl Linux and macOS arm64 archives, built with `cargo auditable`; binary smoke test | N3 |
| Image | `deploy/docker/Dockerfile` (distroless, uid 65532, built from the archives) and its smoke test | N4 |
| Compose | `deploy/compose/` (local volume; S3 overlay with Garage) and its smoke tests | N5 |
| Helm | `deploy/helm/cetra/` (one StatefulSet), lint, kubeconform, a kind install test | N6 |
| Install | `deploy/install.sh` (`curl \| sh`) and its smoke test | N7 |
| Versions | `CHANGELOG.md`, `just release-prep`, `just release-verify`, release notes, image tags | N8 |
| CI | the `packaging` job in `ci.yml` (cheap checks, part of `ci-ok`) and `packaging.yml` (the full `just check-packaging` on `main` and weekly) | N9 |
| Release | `.github/workflows/release.yml`: tag-triggered build, smoke, publish, attest | N10 |
| Docs | `docs/self-hosting/` (install, compose, Kubernetes, configuration, S3, backup, upgrade) | N11 |

**Out:**

- Online backups (`cetra admin backup`), automatic pre-migration copies
  (`meta.db.pre-<version>`), and `cetra admin gc-orphans`. These are track N's M2, a later plan
  (§ After M1).
- Publishing the Python and TypeScript SDKs to PyPI and npm (§ Open items).
- Windows and Intel macOS binaries, Homebrew, Docker Hub (D-N-1, D-N-2).
- The cloud's cell provisioning. That is track Q, in the private repository `cetra-control`,
  which deploys N's image (§ Requests for Q).
- Self-monitoring (`/metrics`), which is D38's and comes just before release.
- Native TLS. The docs say to use a reverse proxy; spec 12's optional `tls.cert`/`tls.key` is not
  built.

## Design

### Artifacts

| Artifact | Where | Built by |
|---|---|---|
| `cetra-<version>-<target>.tar.gz` for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` | GitHub release | `just dist <target>` (N3) |
| `SHA256SUMS`, `cetra-<version>-<target>.spdx.json` (SBOM per binary), `install.sh`, the chart `cetra-<version>.tgz` | GitHub release | N3, N7, N10 |
| image `ghcr.io/<owner>/cetra:<version>` (`linux/amd64`, `linux/arm64`) | GHCR | `deploy/docker/Dockerfile` (N4, N10) |
| chart `oci://ghcr.io/<owner>/charts/cetra:<version>` | GHCR | `helm package` and `helm push` (N10) |
| build provenance attestations for the archives, the image and the chart | GitHub attestations (Sigstore) | `actions/attest-build-provenance` (N10) |

`<owner>` is the GitHub repository owner in lower case (`cetra-ai`). The files in the
repository write `ghcr.io/cetra-ai/...` literally. The D46 rename changed that name with the
others.

Each archive contains one directory, `cetra-<version>-<target>/`, with four files: `cetra`,
`LICENSE` (the root MIT licence, naming the `ee` directories), `LICENSE-ELASTIC` (a copy of
`crates/server/src/ee/LICENSE`) and `THIRD-PARTY-NOTICES.md` (D-N-8, D-N-16).

### Build (D-N-1, D-N-16)

- The Linux binaries are `*-unknown-linux-musl`: static, with no glibc version requirement. Each
  is built natively on its own architecture: `ubuntu-24.04` and `ubuntu-24.04-arm` runners in CI,
  which are free for public repositories. The C dependencies (bundled SQLite, zstd, lz4, mimalloc,
  ring) are compiled with `musl-gcc` (Ubuntu package `musl-tools`). There is no cross-compiling
  and no QEMU.
- The macOS binary (`aarch64-apple-darwin`) is built on a `macos-15` runner, in the release
  workflow only. D81 keeps PR CI on Linux.
- Builds run `cargo auditable build --release --locked`. That embeds the dependency list in the
  binary, so syft and vulnerability scanners can read it from the binary and the image. The
  release profile is the workspace's (`lto = "thin"`, `codegen-units = 1`, `debug =
  "line-tables-only"`).
- The web UI is built first (`pnpm --dir web run build`). `dist.sh` then checks that `cetra
  version` prints `ui: embedded` and the expected version, and that a Linux binary is static.

### Image (D-N-3)

- `gcr.io/distroless/static-debian13:nonroot`, pinned by digest. It has CA certificates, tzdata
  and a passwd entry, and no shell. It runs as uid/gid 65532. Spec 12 said 10001; D-N-3 amends it.
- Nothing is compiled in the Dockerfile: it copies the binary from a release archive, staged by
  `image-context.sh` under `target/dist/image/linux-<arch>/`. The image and the archives therefore
  hold the same binary, and a multi-arch build needs no emulation.
- `/data` is owned by 65532, so a new named volume is writable. Also: `ENV CETRA_DATA_DIR=/data`,
  `VOLUME /data`, `EXPOSE 8080 4317 4318`, `ENTRYPOINT ["/usr/local/bin/cetra"]`,
  `CMD ["serve"]`.
- `HEALTHCHECK` runs `cetra health`, which asks `http://127.0.0.1:8080/readyz` (N1, D-N-4).
  None of the competitors ships a healthcheck, because distroless has no curl.
- One variant only: no root and no debug image.

### docker-compose (D-N-9)

- `deploy/compose/compose.yaml` runs `cetra` with a named volume.
- Compose settings (image, bind address, ports, Garage secrets) live in `.env`. Server settings
  live in `cetra.env`, passed with `env_file` (`required: false`). These are two files because:
  - the config loader rejects unknown `CETRA_*` variables (`CETRA_IMAGE` would fail);
  - it treats an empty variable as a set value.
- Ports bind to `127.0.0.1` by default.
- The image tag defaults to the release version, never `latest`. `release-prep` rewrites it.
- `compose.s3.yaml` is an overlay. It adds Garage v2.5.0 (one node, SQLite metadata) and
  `garage-init`, a one-shot `curlimages/curl` container. `garage-init` calls Garage's admin API
  v2 to:
  - apply a layout;
  - import the access key from `.env`;
  - create the bucket `cetra`;
  - give the key read and write on it.

  The overlay points cetra at `s3://cetra` on `http://garage:3900`. MinIO no longer publishes
  its image (`crates/storage/tests/s3.rs`), and Garage is what the user runs for the CI cache.

### Helm chart (D-N-10)

- `deploy/helm/cetra/`.
- One StatefulSet with `replicas: 1` fixed (no value), plus:
  - `podManagementPolicy: OrderedReady`;
  - a `volumeClaimTemplates` entry `data` mounted at `/data`, retained when the release is deleted;
  - `terminationGracePeriodSeconds: 60`;
  - security contexts for uid/gid 65532, a read-only root filesystem, no capabilities, seccomp
    `RuntimeDefault` and no service account token;
  - probes: startup and liveness on `/healthz` (the startup probe allows 10 minutes for WAL
    replay), readiness on `/readyz`;
  - `enableServiceLinks: false`, because Kubernetes' service-link variables for a Service named
    `cetra` (`CETRA_PORT=tcp://…`) are unknown `CETRA_*` keys the config loader rejects.
- A Service (8080, 4317, 4318) and the headless Service the StatefulSet requires.
- An optional Ingress for port 8080.
- Configuration:
  - `config` is rendered with `toToml` into a ConfigMap mounted at `/etc/cetra/cetra.toml`
    (`CETRA_CONFIG`);
  - `env` lists extra variables;
  - `secret.existingSecret` and `secret.values` are passed with `envFrom`;
  - `s3.*` becomes the `CETRA_OBJECT_STORE` and `CETRA_STORAGE__S3__*` variables and
    `AWS_REGION`.
- The chart never generates a secret. Phoenix regenerates its secret on every upgrade; we avoid
  that. Without `CETRA_ENCRYPTION_KEY`, the server writes `/data/secret.key` on the volume.
- Chart version = app version = workspace version (D-N-5).

### Install script (D-N-15)

- `deploy/install.sh` is POSIX `sh` and is published as a release asset:
  `https://github.com/<owner>/cetra/releases/latest/download/install.sh`.
- It maps `uname` to a target, finds the version (`CETRA_VERSION`, else the redirect of
  `/releases/latest`), downloads the archive and `SHA256SUMS`, checks the checksum, and installs
  to `CETRA_INSTALL_DIR` (default `~/.local/bin`).
- `CETRA_DOWNLOAD_BASE` overrides the download URL. Tests use it to serve `target/dist` on
  loopback.

### Versions, changelog, publication (D-N-5, D-N-6)

- SemVer, 0.x for now. One version for everything: the workspace `[workspace.package] version`
  (all crates inherit it), the chart's `version` and `appVersion`, the compose default image and
  the `.env.example` comment. `tools/packaging/versions-check.sh` checks that they agree, on every
  PR (N9).
- `CHANGELOG.md` is hand-written in the Keep a Changelog format, with an `## [Unreleased]` section
  that each user-visible change adds to. Commit subjects stay as they are (not Conventional
  Commits).
- Releasing:
  1. `just release-prep X.Y.Z` sets every version, refreshes `Cargo.lock` and dates the changelog
     section.
  2. The commit `Release X.Y.Z` is merged to `main`.
  3. Pushing the annotated tag `vX.Y.Z` on that commit runs `release.yml`.
  4. `just release-verify` checks four things: tag = version, files agree, the changelog section
     exists, and the tag is on `origin/main`.
- Until the real name is chosen (D46), nothing is public.
  - A version with a pre-release part (`0.1.0-rc.1`) makes a **draft** pre-release.
  - Such a version gets only the exact image tag.
  - The GHCR packages stay private: the first push creates them private, and nobody changes that.
  - Only after the rename does a stable tag publish a release and add the image tags `X.Y`, `X`
    (when X > 0) and `latest`.

### Supply chain (D-N-7)

- `SHA256SUMS` over the archives.
- GitHub build provenance attestations (`actions/attest-build-provenance`, keyless, Sigstore) for
  the archives (through `subject-checksums`), the image and the chart. Users verify them with
  `gh attestation verify`.
- An SPDX JSON SBOM per binary, made by `anchore/sbom-action` (syft) from the binary's
  cargo-auditable data and attached to the release.
- The image gets BuildKit's own SBOM and `mode=max` provenance.
- No cosign signatures, no keys to manage.

### Licence notices (D-N-8)

- `THIRD-PARTY-NOTICES.md` is generated at build time and never committed. It has two parts:
  - the Rust crates of `cetra-cli` for the three targets, from `cargo about generate` with
    `tools/packaging/about.toml`;
  - the web packages, from `pnpm --dir web licenses list --prod --json` and
    `tools/packaging/js-notices.mjs`, which copies each package's licence file.
- `about.toml` accepts the same licences as `deny.toml`'s `[licenses]` allow-list (P1). Workspace
  crates are skipped (`private.ignore`); `LICENSE` covers them.
- The archives and the image's `/usr/share/doc/cetra/` carry `LICENSE`, `LICENSE-ELASTIC` and the
  notices. The `ee` code is compiled into every artifact and is dormant without a licence key
  (D172).

### Verification (D-N-13)

- `tools/packaging/smoke.sh <http_url> <otlp_http_url> <key>` is the one smoke test every launcher
  runs. It:
  1. waits up to 120 s for `/readyz`;
  2. checks that `/` is the built UI (it references `/assets/`);
  3. posts one OTLP/JSON span, with the current time, to `<otlp_http_url>/v1/traces` with
     `Authorization: Bearer <key>`;
  4. polls `GET /api/v1/projects/current/traces/<trace_id>` until the trace's name is
     `cetra-smoke`.

  The server is started with `CETRA_INIT_*` and the key
  `sk-cetra-SMOKE0123456789ABCDEFGHIJKLMNOPQ`.
- The launchers:
  - `binary-smoke.sh` runs a binary on free loopback ports;
  - `image-smoke.sh` runs the image with a fresh volume and checks uid 65532 and the Docker
    health state;
  - `compose-smoke.sh` runs a copy of `deploy/compose` in a temporary directory, so the checkout
    stays clean. With `s3`, it also checks that a flush wrote objects to the Garage bucket;
  - `helm-smoke.sh` creates a kind cluster, installs the chart and port-forwards;
  - `install-smoke.sh` serves `target/dist` and runs `install.sh`.
- PR CI: a `packaging` job on `ubuntu-24.04` (x86_64 only) runs `just check-packaging-lint`, with no
  Rust build:
  - `versions-check`, `helm-check`, the compose files and Biome on `tools/packaging`.

  It is in `ci-ok`. Kind is not in PR CI.
- `packaging.yml` runs the full `just check-packaging` on `ubuntu-24.04` (x86_64 only) on pushes to
  `main`, weekly (Mondays) and on manual dispatch. It is not in `ci-ok`:
  - `check-packaging-lint`;
  - `dist`;
  - the binary, install, image, compose and compose-S3 smokes.
- The release workflow runs every smoke on every artifact before publishing:
  - each Linux architecture: binary, install, image, compose, compose-S3, helm-check and the
    kind install;
  - macOS: binary and install.

### Backups and upgrades (D-N-11, D-N-12)

- Backups in M1 are a documented **cold snapshot**:
  1. stop cetra (it drains and flushes, ≤ 60 s);
  2. copy the whole data directory (`meta.db` and `catalog.db` together, `wal/`, `secret.key`,
     and `objects/` in local mode);
  3. in S3 mode, also copy the bucket prefix during the same stop;
  4. start cetra.
- Restore:
  1. stop cetra;
  2. keep the current data directory;
  3. put the backup in place;
  4. in S3 mode, sync the bucket back with `--delete`;
  5. start cetra;
  6. `cetra admin reapply-deletions --from <kept dir>` (D-M-9), once track M's M7 is in; N11
     writes the docs for either case.
- The online `cetra admin backup` of spec 12 (S13) is N's M2.
- Upgrades:
  - migrations run forward-only at start;
  - no downgrade: restore the backup taken before the upgrade;
  - a cold backup before every upgrade;
  - release notes start with `### Upgrade notes` when action is needed.
- The automatic `.pre-<version>` copies of spec 12 also move to M2.

## Files (end state)

```
deploy/docker/Dockerfile                  the image                                    (N4)
.dockerignore                             build context = target/dist/image only      (N4)
deploy/compose/compose.yaml               cetra + volume                              (N5)
deploy/compose/compose.s3.yaml            + Garage, garage-init                        (N5)
deploy/compose/{garage.toml,garage-init.sh,cetra.env.example,.env.example}            (N5)
deploy/helm/cetra/                       Chart.yaml, values.yaml, README.md, templates/, ci/  (N6)
deploy/install.sh                         curl | sh installer                          (N7)
tools/packaging/about.toml, about.hbs     cargo-about config and template              (N2)
tools/packaging/js-notices.mjs            web licence notices                          (N2)
tools/packaging/notices.sh                THIRD-PARTY-NOTICES.md                       (N2)
tools/packaging/version.sh                prints the workspace version                 (N3)
tools/packaging/dist.sh                   release archive of one target                (N3)
tools/packaging/image-context.sh          image build context from an archive          (N3)
tools/packaging/smoke.sh                  the smoke test                               (N3)
tools/packaging/binary-smoke.sh           smoke a binary                               (N3)
tools/packaging/checksums.sh              SHA256SUMS                                   (N3)
tools/packaging/image-smoke.sh            smoke an image                               (N4)
tools/packaging/compose-smoke.sh          smoke deploy/compose                         (N5)
tools/packaging/helm-check.sh             helm lint + kubeconform                      (N6)
tools/packaging/helm-smoke.sh             kind install + smoke                         (N6)
tools/packaging/install-smoke.sh          smoke install.sh                             (N7)
tools/packaging/versions-check.sh         versioned files agree                        (N8)
tools/packaging/release-prep.sh           set a release version                        (N8)
tools/packaging/release-notes.sh          a version's changelog section                (N8)
tools/packaging/release-verify.sh         release workflow gate                        (N8)
tools/packaging/image-tags.sh             image tags of a version                      (N8)
CHANGELOG.md                                                                           (N8)
.github/actions/packaging/action.yml      musl-tools, cargo-auditable, cargo-about, helm, kubeconform (N9)
.github/workflows/ci.yml                  + packaging job                              (N9)
.github/workflows/release.yml             the release workflow                         (N10)
.github/workflows/packaging.yml           full packaging checks on main and weekly      (N9, D-N-13)
docs/self-hosting/*.md                    user docs                                    (N11)
crates/cli/src/health.rs                  cetra health                                (N1)
```

N edits only these files plus `justfile`, `AGENTS.md`, `.gitignore`, `biome.json`,
`crates/cli/src/main.rs`, `.github/CI.md` and `docs/roadmap.md`, as each sub-phase says.
Nothing else in `crates/`, `web/` or `sdks/` changes.

## Sub-phases

| # | File | Adds | Needs |
|---|---|---|---|
| N1 | [`01-health.md`](01-health.md) | `cetra health` | — |
| N2 | [`02-notices.md`](02-notices.md) | third-party licence notices | — |
| N3 | [`03-dist.md`](03-dist.md) | release archives, smoke test, checksums | N1, N2, P's P1 |
| N4 | [`04-image.md`](04-image.md) | Docker image | N3 |
| N5 | [`05-compose.md`](05-compose.md) | docker-compose (local, Garage) | N4 |
| N6 | [`06-helm.md`](06-helm.md) | Helm chart | N4 |
| N7 | [`07-install.md`](07-install.md) | `install.sh` | N3 |
| N8 | [`08-versions.md`](08-versions.md) | changelog, release-prep, release-verify | N5, N6 |
| N9 | [`09-ci.md`](09-ci.md) | CI `packaging` job, `just check-packaging` | N1–N8 |
| N10 | [`10-release-workflow.md`](10-release-workflow.md) | `release.yml` | N9 |
| N11 | [`11-docs.md`](11-docs.md) | `docs/self-hosting/`, roadmap | N1–N10 |

## Conventions

- **Scripts:**
  - Packaging scripts are bash: `#!/usr/bin/env bash` then `set -euo pipefail`.
  - `deploy/install.sh` and `deploy/compose/garage-init.sh` are POSIX `sh`: `#!/bin/sh` and
    `set -eu`.
  - Every script is committed executable. Create it, then `chmod +x <file>`; git records the mode.
    Check with `git ls-files -s <file>`, which must print mode `100755`.
- **Locating the repository:** each script finds the root with
  `root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"`.
- **Recipes:** each script has one `just` recipe, which is what CI calls. Recipe comments end with
  the decision ID. New recipes go at the end of the `justfile`, under the comment line
  `# Packaging and release (track N, docs/plans/n-packaging/)` that N2 adds.
- **Outputs:** everything a script builds goes under `target/dist/`, which git ignores (`/target`).
  `just assert-clean` must pass after every recipe.
- **Exact contents:** copy each file's content exactly as the sub-phase gives it. Do not reformat
  it, and do not add files the sub-phase does not list.
- **Privacy (AGENTS.md):** smoke tests use fixed fake credentials. Logs never print keys.
- **Versions** (looked up 2026-11-14, each at least 7 days old):

  | Tool | Version |
  |---|---|
  | cargo-auditable | 0.7.7 |
  | cargo-about | 0.9.3 |
  | helm | v4.3.1 |
  | kubeconform | v0.8.0 |
  | kind | v0.33.0 |
  | Garage image | `dxflrs/garage:v2.5.0` |
  | curl image | `curlimages/curl:8.23.0` |
  | busybox (docs only) | `1.37` |
  | distroless | `gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931` |

  The actions and their SHAs are in N9 and N10. Never change a version or SHA given here.

## Local tools

This machine and the runners of N's CI jobs (`ubuntu-24.04`) are Ubuntu 24.04. The user installs
these before N2 (N2 needs cargo-about; N3 needs the rest):

```bash
sudo apt-get install -y musl-tools
cargo install cargo-auditable@0.7.7 --locked
cargo install cargo-about@0.9.3 --locked --features cli
```

Also needed:

- Docker Engine 25 or later with the Compose v2 plugin 2.24 or later (`docker version`,
  `docker compose version`). The plugin version matters because compose uses `env_file.required`
  and `healthcheck.start_interval`.
- helm v4.3.1, kubeconform v0.8.0, kind v0.33.0 and kubectl, on `PATH`.
- python3 and curl (both on Ubuntu 24.04 and on the runners): the smoke scripts use them.

Node is at `$HOME/.local/share/nvm/v26.13.0/bin` and is not on the non-interactive `PATH`. Run
`export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"` before any `just` recipe that uses pnpm.

Each sub-phase lists the tools it needs. If one is missing, stop and report which one.

## How to verify

- Every sub-phase: `just check` passes, the sub-phase's own recipes pass, `just assert-clean`
  passes (no untracked or modified files after the run, other than your commit), and
  `just check-workflows` passes when `.github/` changed.
- After N9: `just check-packaging` passes locally on x86_64 Linux. It takes 20–30 minutes; most of
  that is the release build.
- After N10: the first dry run (§ Releasing) passes. The user runs it; GLM does not push tags.

## Releasing

**One-time setup (the user, before the first tag):**

1. Settings → Environments: create `release`, limited to tags `v*`. The workflow's `publish` job
   uses it. Adding the user as a required reviewer makes every publish wait for a click.
2. The repository stays public (it is today). The release workflow relies on that: the
   `ubuntu-24.04-arm` runner is free only for public repositories, and `verify` fetches `main`
   without credentials. D-N-6 keeps the *artifacts* private, not the source.
3. Settings → Actions → General: workflow permissions stay "Read repository contents". The
   workflow grants itself `contents: write`, `packages: write`, `id-token: write` and
   `attestations: write` in the `publish` job only.

**Dry run (until D46):**

1. `just release-prep 0.1.0-rc.1`, then review the diff. Commit `Release 0.1.0-rc.1` on a
   branch, open a PR and merge it.
2. Tag and push:

   ```bash
   git fetch origin && git tag -a v0.1.0-rc.1 -m 'cetra 0.1.0-rc.1' origin/main && git push origin v0.1.0-rc.1
   ```

3. Watch the Release workflow. When it is done, check:
   - the **draft** pre-release on GitHub, with 3 archives, 3 SBOMs, `SHA256SUMS`,
     `cetra-0.1.0-rc.1.tgz` and `install.sh`;
   - `ghcr.io/cetra-ai/cetra:0.1.0-rc.1` with both platforms
     (`docker buildx imagetools inspect`);
   - `oci://ghcr.io/cetra-ai/charts/cetra` version `0.1.0-rc.1`;
   - `gh attestation verify` on one archive, the image and the chart.
4. On GitHub → Packages, make sure `cetra` and `charts/cetra` are **private**. Leave them so
   until the rename.
5. Delete the draft release, or keep it as a record. Tags of drafts stay.

**First public release (after D46's rename):** re-enable cargo deny (D187: `deny` back in `check-rust` and CI's `rust-policy` job, the weekly schedule in `advisories.yml`, `just deny` green), then `just release-prep 0.1.0`, merge, tag `v0.1.0`.
Make the two GHCR packages public, then publish. That release is not a draft.

## Requests for Q

Track Q (the control plane, `cetra-control`) deploys N's image to cells; the first cell is one
Hetzner server with Hetzner Object Storage (D170). What N gives Q, and what Q needs to know:

1. **Image:** `ghcr.io/cetra-ai/cetra:<version>`, multi-arch.
   - Deploy by digest (`@sha256:…`), verified with
     `gh attestation verify oci://ghcr.io/cetra-ai/cetra@sha256:… --repo cetra-ai/cetra`.
   - While the package is private (until D46), pull with a token that has `read:packages`.
2. **Runtime contract:**
   - uid/gid 65532; the data volume at `/data`, owned by 65532;
   - ports 8080, 4317 and 4318;
   - `GET /healthz` (process up) and `GET /readyz` (meta.db and WALs);
   - `cetra health` inside the container;
   - SIGTERM drains (≤ 20 s) and flushes (≤ 30 s): allow 60 s;
   - one process per data directory (`flock` on `/data/LOCK`): never two containers on one
     volume;
   - read-only root filesystem works.
3. **Configuration** is only `CETRA_*` and `AWS_*` variables, or a TOML file at `CETRA_CONFIG`.
   - For a cell: `CETRA_OBJECT_STORE=s3://<bucket>/<prefix>`,
     `CETRA_STORAGE__S3__ENDPOINT=https://<location>.your-objectstorage.com`,
     `AWS_REGION=<location>`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `CETRA_PUBLIC_URL`.
   - The encryption key comes from `CETRA_ENCRYPTION_KEY` (recommended for cells, kept in Q's
     secret store) or `/data/secret.key`.
   - Never set a`CETRA_*` variable to an empty string, and never let an unknown one in. On
     Kubernetes, a Service named `cetra` makes `CETRA_PORT` and similar service-link
     variables appear: set `enableServiceLinks: false`, as the chart does.
4. **Backups:** M1 gives only cold snapshots (docs/self-hosting/backup.md), i.e. a minute of
   downtime per cell. Q's design asks for two things: R9, `cetra admin backup [--meta-only] --to
   <dir>` run with `docker compose exec` while `cetra serve` runs, writing `meta.db` and
   `catalog.db` at the top of `<dir>`; and R10, a longer object retention for meta-only backups. Both are N's M2 (§ After M1,
   `docs/plans/n-packaging/m2/`), not this plan: R9 is `cetra admin backup` through the control
   socket `<data_dir>/admin.sock` (D-N-17), and R10 is `storage.gc.grace` allowed up to `30d`, which
   Q sets to `8d` (D-N-20). There is no `storage.gc.object_retention` key, and the config loader
   rejects unknown keys. Q's M1 phases 10–11 (backups, restore drill) wait for M2.

Q's R8 (the official image) is this plan's N4 and N10. Two differences from Q's interim image
(`cetra-control/deploy/image/Dockerfile`):

- **The user is uid 65532, not 10001.** Volumes written by the interim image need
  `chown -R 65532:65532` before they switch.
- **The image creates no `/backups` directory.** Q's compose file owns that mount; a host bind
  mount gets its owner from the host, not from the image.

The image's tags are release builds only, so they trust only the compiled-in licence keys, never
`dev-1` (D-P-6). Until D46 only `vX.Y.Z-rc.N` tags exist (private package).
5. **Upgrades:** forward-only migrations at start. Back up first; no downgrade. Q pins each cell
   to a digest and rolls cells one at a time.
6. **New releases:** Q learns of them from the GitHub release (stable tags only). Each release
   lists the image digest in its workflow run summary, and the attestation binds digest, tag
   and commit.

## Coordination

- **P:** N3 needs P1's `LICENSE` files. `about.toml` copies P1's `deny.toml` allow-list, so a
  licence that P adds there must also go in `tools/packaging/about.toml`. If a release build fails
  in `cargo about` on a new licence, add it to both.
- **M:** N11's restore section uses M7's `cetra admin reapply-deletions` (D-M-9). Spec 12 already
  says so.
- **Q:** § Requests for Q. Q's own repository holds its deploy tooling. N's chart and compose
  files are for self-hosters, and Q may use either.

## After M1 (N M2)

Planned in [`m2/`](m2/00-design.md): online `cetra admin backup` through a control socket,
`gc.grace` up to 30 days for S3-mode metadata backups, `cetra admin gc-orphans`, and pre-migration
copies of `meta.db` and `catalog.db` (D-N-17–D-N-24).

## N M3

Planned in [`m3/`](m3/00-design.md) (D180, D182):
- restore mode for Q's restore drills (R18): no outbound call and no object deletion;
- Prometheus `/metrics` on `metrics_addr` (R11);
- `/readyz` checking the object store, free disk and the storage failure flags;
- `/healthz` failing on sticky storage failures (D-N-25–D-N-36).

## Open items

- **SDK publishing** (PyPI, npm): not in N's scope as briefed. The SDK versions and release flow
  need a decision.
- **Copyright holder** in `LICENSE` (P's open item): confirm before the first public release.
- **Native TLS** (spec 12's `tls.cert`/`tls.key`): not built. The docs say to use a reverse proxy.
- **Upgrade testing:** releases are smoke-tested from a fresh install only. An upgrade test from
  the previous release can come once there are releases.
