# N10 — The release workflow

Read [README.md](README.md) first, especially § Design (all of it) and § Releasing. Read
`AGENTS.md` § CI again.

**Goal:** `.github/workflows/release.yml`. Pushing a tag `v<version>` on `main` does the following
(D-N-5, D-N-6, D-N-7, D-N-13, D-N-14):

1. Checks the tag (`just release-verify`).
2. Builds and smoke-tests every artifact on its own platform.
3. Publishes:
   - the archives, SBOMs, `SHA256SUMS`, the chart archive and `install.sh` on a GitHub release
     (a draft pre-release for a version with a pre-release part);
   - the multi-arch image and the chart on GHCR;
   - build provenance attestations for the archives, the image and the chart.

**Edit only:**
- `.github/workflows/release.yml` (new);
- `.github/README.md`.

**Commit:** `Add the release workflow`

**Tools:** actionlint and zizmor (`just check-workflows`). The workflow cannot run locally; the
user's dry run (README § Releasing) is its test. Do not push tags.

## How it works

Four jobs. The workflow has `permissions: {}`, and each job grants only what it needs.

| Job | Runner | Permissions | Does |
|---|---|---|---|
| `verify` | `ubuntu-24.04` | `contents: read` | full checkout, fetch `main`, `just release-verify "$GITHUB_REF_NAME"`; outputs `version` and `prerelease` (`true` when the version contains `-`) |
| `linux` (matrix `amd64` on `ubuntu-24.04`, `arm64` on `ubuntu-24.04-arm`) | native | `contents: read` | `just dist`, then the smokes `binary-smoke`, `install-smoke`, `image` + `image-smoke`, `compose-smoke` (local and s3), `helm-check`, `helm-smoke`; SBOM of the binary; uploads the archive and the SBOM |
| `macos` | `macos-15` (arm64) | `contents: read` | `just dist aarch64-apple-darwin`, `binary-smoke`, `install-smoke`; SBOM; upload |
| `publish` | `ubuntu-24.04`, environment `release` | `contents: write`, `packages: write`, `id-token: write`, `attestations: write` | the steps below |

The `publish` job:
1. Downloads every archive and SBOM.
2. Rebuilds both image contexts from the Linux archives (`just image-context`).
3. Writes `SHA256SUMS` and attests every archive through `subject-checksums`.
4. Logs in to GHCR with `GITHUB_TOKEN`.
5. Builds and pushes the multi-arch image with BuildKit's SBOM and `mode=max` provenance, and
   attests its digest. The tags come from `image-tags.sh`.
6. Packages the chart, pushes it to `oci://ghcr.io/<owner>/charts`, reads the digest from
   `helm push`'s output and attests it.
7. Creates the GitHub release with `gh release create`: notes from `just release-notes`;
   `--draft --prerelease` when `prerelease` is true.

Rules this workflow follows (zizmor checks most of them):

- **No caches.** A release build must not read a cache that a PR could have written.
  - `./.github/actions/rust` gets `cache: "false"`.
  - Node is set up with `pnpm/action-setup` and `actions/setup-node` directly, with
    `package-manager-cache: false`, instead of `./.github/actions/js` (which caches pnpm).
- **No template expansion of untrusted input in `run:`.** Values reach scripts through `env:`
  (`TARGET`, `VERSION`, `PRERELEASE`) and the runner's own variables (`GITHUB_REF_NAME`,
  `GITHUB_REPOSITORY_OWNER`, `GITHUB_ACTOR`). Only `with:` inputs use `${{ }}`.
- **Checkout.** Every checkout has `persist-credentials: false`. `verify` fetches `main` over
  HTTPS without credentials, which works because the repository is public.
- **Registry names in lower case.** They are made from `GITHUB_REPOSITORY_OWNER`, lower-cased
  with `${GITHUB_REPOSITORY_OWNER,,}`.
- **No reruns of the build in `publish`.** The image is assembled from the same archives the
  `linux` jobs smoke-tested. The multi-arch build needs no QEMU because the Dockerfile has no
  `RUN`.

Actions used, each pinned by SHA (looked up 2026-11-14, each release at least 7 days old):

| Action | Version | SHA |
|---|---|---|
| `actions/checkout` | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| `taiki-e/install-action` | v2.90.0 | `b80e2bb2635f28eb98b9fce1e2f0887e1535c6b2` |
| `pnpm/action-setup` | v6.1.1 | `73e46992e05b19abd31f2f3c20e0e7370d762115` |
| `actions/setup-node` | v7.1.0 | `949feb2413d6458794dcd2491c4babbbce0c15c1` |
| `helm/kind-action` | v1.15.2 | `ea822c7bf1874f08e5a491df991c228edffc3376` |
| `anchore/sbom-action` | v0.24.3 | `66cbf4bc1f1c0d2edc94016e65bc221b6bb0ad6c` |
| `actions/upload-artifact` | v7.0.2 | `cf430e030ddbb5b0abf93d22962f4752f3646cd9` |
| `actions/download-artifact` | v8.0.2 | `9000827ccba6bdab643e8b6fd33ac0654aef8333` |
| `azure/setup-helm` | v5.0.1 | `9bc31f4ebc9c6b171d7bfbaa5d006ae7abdb4310` |
| `actions/attest-build-provenance` | v4.2.2 | `4d101475d8b20a2381f78447822ac1eab6504dd8` |
| `docker/setup-buildx-action` | v4.4.1 | `f87e5991a6d7451dcb8d9637bfbc97413f497069` |
| `docker/login-action` | v4.8.0 | `8aa5831fb8d48d8f76594c7ddde61ae58019f550` |
| `docker/build-push-action` | v7.4.0 | `c3c9e263c25d99ce0380d002d59b67737d91b0dc` |

Dependabot's `github-actions` entry already scans `/` and `/.github/actions/*`, so it updates
these too.

## Files

### `.github/workflows/release.yml` (new)

```yaml
name: Release

# A tag v<version> on main publishes that version (D-N-5, D-N-6): archives, checksums, SBOMs and
# install.sh on a GitHub release, the multi-arch image and the chart on GHCR, and build
# provenance attestations (D-N-7). A version with a pre-release part (v0.1.0-rc.1) makes a draft
# pre-release and only the exact image tag. docs/plans/n-packaging/README.md § Releasing.
on:
  push:
    tags: ["v*"]

permissions: {}

concurrency:
  group: release-${{ github.ref }}
  cancel-in-progress: false

env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: "0"

jobs:
  verify:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    timeout-minutes: 10
    outputs:
      version: ${{ steps.version.outputs.version }}
      prerelease: ${{ steps.version.outputs.prerelease }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: taiki-e/install-action@b80e2bb2635f28eb98b9fce1e2f0887e1535c6b2 # v2.90.0
        with:
          tool: just@1.59.0
      - name: Fetch main
        run: git fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main
      - run: just release-verify "$GITHUB_REF_NAME"
      - id: version
        name: Read the version
        run: |
          set -euo pipefail
          version="$(tools/packaging/version.sh)"
          prerelease=false
          if [[ "$version" == *-* ]]; then
            prerelease=true
          fi
          {
            echo "version=$version"
            echo "prerelease=$prerelease"
          } >> "$GITHUB_OUTPUT"

  linux:
    needs: verify
    name: linux (${{ matrix.arch }})
    strategy:
      fail-fast: true
      matrix:
        include:
          - runner: ubuntu-24.04
            target: x86_64-unknown-linux-musl
            arch: amd64
          - runner: ubuntu-24.04-arm
            target: aarch64-unknown-linux-musl
            arch: arm64
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
    timeout-minutes: 90
    env:
      TARGET: ${{ matrix.target }}
      VERSION: ${{ needs.verify.outputs.version }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: ./.github/actions/rust # zizmor: ignore[self-repository] actionlint 1.7.12 rejects the $/ form
        with:
          cache: "false"
      # Not ./.github/actions/js: a release build uses no cache.
      - uses: pnpm/action-setup@73e46992e05b19abd31f2f3c20e0e7370d762115 # v6.1.1
      - uses: actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1 # v7.1.0
        with:
          node-version-file: .node-version
          package-manager-cache: false
      - uses: ./.github/actions/packaging # zizmor: ignore[self-repository] actionlint 1.7.12 rejects the $/ form
      - uses: helm/kind-action@ea822c7bf1874f08e5a491df991c228edffc3376 # v1.15.2
        with:
          version: v0.33.0
          install_only: true
      - run: just dist "$TARGET"
      - run: just binary-smoke "target/$TARGET/release/cetra"
      - run: just install-smoke
      - run: just image cetra:dev
      - run: just image-smoke cetra:dev
      - run: just compose-smoke cetra:dev
      - run: just compose-smoke cetra:dev s3
      - run: just helm-check
      - run: just helm-smoke cetra:dev
      - uses: anchore/sbom-action@66cbf4bc1f1c0d2edc94016e65bc221b6bb0ad6c # v0.24.3
        with:
          file: target/${{ matrix.target }}/release/cetra
          format: spdx-json
          output-file: target/dist/cetra-${{ needs.verify.outputs.version }}-${{ matrix.target }}.spdx.json
          upload-artifact: false
          upload-release-assets: false
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7.0.2
        with:
          name: dist-${{ matrix.target }}
          path: |
            target/dist/cetra-*.tar.gz
            target/dist/cetra-*.spdx.json
          if-no-files-found: error
          retention-days: 7
      - run: just assert-clean

  macos:
    needs: verify
    runs-on: macos-15
    permissions:
      contents: read
    timeout-minutes: 90
    env:
      TARGET: aarch64-apple-darwin
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: ./.github/actions/rust # zizmor: ignore[self-repository] actionlint 1.7.12 rejects the $/ form
        with:
          cache: "false"
      - uses: pnpm/action-setup@73e46992e05b19abd31f2f3c20e0e7370d762115 # v6.1.1
      - uses: actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1 # v7.1.0
        with:
          node-version-file: .node-version
          package-manager-cache: false
      - uses: ./.github/actions/packaging # zizmor: ignore[self-repository] actionlint 1.7.12 rejects the $/ form
      - run: just dist "$TARGET"
      - run: just binary-smoke "target/$TARGET/release/cetra"
      - run: just install-smoke
      - uses: anchore/sbom-action@66cbf4bc1f1c0d2edc94016e65bc221b6bb0ad6c # v0.24.3
        with:
          file: target/aarch64-apple-darwin/release/cetra
          format: spdx-json
          output-file: target/dist/cetra-${{ needs.verify.outputs.version }}-aarch64-apple-darwin.spdx.json
          upload-artifact: false
          upload-release-assets: false
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7.0.2
        with:
          name: dist-aarch64-apple-darwin
          path: |
            target/dist/cetra-*.tar.gz
            target/dist/cetra-*.spdx.json
          if-no-files-found: error
          retention-days: 7
      - run: just assert-clean

  publish:
    needs: [verify, linux, macos]
    runs-on: ubuntu-24.04
    environment:
      name: release
    permissions:
      contents: write
      packages: write
      id-token: write
      attestations: write
    timeout-minutes: 30
    env:
      VERSION: ${{ needs.verify.outputs.version }}
      PRERELEASE: ${{ needs.verify.outputs.prerelease }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: taiki-e/install-action@b80e2bb2635f28eb98b9fce1e2f0887e1535c6b2 # v2.90.0
        with:
          tool: just@1.59.0
      - uses: azure/setup-helm@9bc31f4ebc9c6b171d7bfbaa5d006ae7abdb4310 # v5.0.1
        with:
          version: v4.3.1
      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8.0.2
        with:
          pattern: dist-*
          path: target/dist
          merge-multiple: true
      - id: names
        name: Image and chart names
        run: |
          set -euo pipefail
          owner="${GITHUB_REPOSITORY_OWNER,,}"
          {
            echo "image=ghcr.io/$owner/cetra"
            echo "charts=ghcr.io/$owner/charts"
            echo "tags=$(tools/packaging/image-tags.sh "ghcr.io/$owner/cetra" "$VERSION")"
          } >> "$GITHUB_OUTPUT"
      - name: Stage the image contexts from the Linux archives
        run: |
          set -euo pipefail
          just image-context "target/dist/cetra-$VERSION-x86_64-unknown-linux-musl.tar.gz" amd64
          just image-context "target/dist/cetra-$VERSION-aarch64-unknown-linux-musl.tar.gz" arm64
      - run: just checksums target/dist
      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2
        with:
          subject-checksums: target/dist/SHA256SUMS

      - uses: docker/setup-buildx-action@f87e5991a6d7451dcb8d9637bfbc97413f497069 # v4.4.1
      - uses: docker/login-action@8aa5831fb8d48d8f76594c7ddde61ae58019f550 # v4.8.0
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - id: image
        uses: docker/build-push-action@c3c9e263c25d99ce0380d002d59b67737d91b0dc # v7.4.0
        with:
          context: .
          file: deploy/docker/Dockerfile
          platforms: linux/amd64,linux/arm64
          push: true
          tags: ${{ steps.names.outputs.tags }}
          sbom: true
          provenance: mode=max
      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2
        with:
          subject-name: ${{ steps.names.outputs.image }}
          subject-digest: ${{ steps.image.outputs.digest }}
          push-to-registry: true

      - id: chart
        name: Package and push the chart
        env:
          CHARTS: ${{ steps.names.outputs.charts }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          set -euo pipefail
          helm package deploy/helm/cetra --destination target/dist
          printf '%s' "$GH_TOKEN" | helm registry login ghcr.io --username "$GITHUB_ACTOR" --password-stdin
          helm push "target/dist/cetra-$VERSION.tgz" "oci://$CHARTS" 2>&1 | tee "$RUNNER_TEMP/helm-push.log"
          digest="$(grep -o 'sha256:[0-9a-f]\{64\}' "$RUNNER_TEMP/helm-push.log" | head -n1)"
          if [[ -z "$digest" ]]; then
            echo "no digest in the helm push output" >&2
            exit 1
          fi
          echo "digest=$digest" >> "$GITHUB_OUTPUT"
      - uses: actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2
        with:
          subject-name: ${{ steps.names.outputs.charts }}/cetra
          subject-digest: ${{ steps.chart.outputs.digest }}
          push-to-registry: true

      - name: Create the GitHub release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          set -euo pipefail
          just release-notes "$VERSION" > "$RUNNER_TEMP/notes.md"
          flags=()
          if [[ "$PRERELEASE" == "true" ]]; then
            flags=(--draft --prerelease)
          fi
          gh release create "$GITHUB_REF_NAME" "${flags[@]}" --verify-tag \
            --title "cetra $VERSION" \
            --notes-file "$RUNNER_TEMP/notes.md" \
            target/dist/cetra-*.tar.gz \
            target/dist/cetra-*.spdx.json \
            target/dist/SHA256SUMS \
            "target/dist/cetra-$VERSION.tgz" \
            deploy/install.sh
```

### `.github/README.md`

Append a new section at the end of the file:

```markdown
## Release workflow

`workflows/release.yml` runs on tags `v*` (D-N-14). Its `run:` steps call `just` recipes, except steps that only pass values between steps (reading the version, the image and chart names, `helm push`, `gh release create`), which stay inline. (docs/plans/n-packaging/README.md § Releasing, D-N-5–D-N-7). It uses no compiler cache. Its `publish` job runs in the GitHub environment `release` (create it under Settings → Environments, limited to tags `v*`; add a required reviewer to approve each publish) and is the only job with write permissions: `contents` (the GitHub release), `packages` (GHCR), `id-token` and `attestations` (build provenance). It needs no secret besides the workflow's own `GITHUB_TOKEN`.
```

## Verify

```bash
just check-workflows
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
just dist x86_64-unknown-linux-musl     # skip if target/dist already has this version's archive
v="$(tools/packaging/version.sh)"
just image-context "target/dist/cetra-$v-x86_64-unknown-linux-musl.tar.gz" amd64
just checksums target/dist
just release-notes 0.0.0 || echo "expected: no 0.0.0 section yet"
just check
just assert-clean
```

The `image-context` and `checksums` lines are the `publish` job's own commands with the version
filled in: they prove its archive paths. `helm-smoke` needs `kubectl`; `helm/kind-action`
installs it with kind (and GitHub's Ubuntu images have it).

`just check-workflows` must report no findings, as in N9.

Also read the workflow against these facts. They are the points a reviewer will check.

- The `linux` job's step order is `dist` → `binary-smoke` → `install-smoke` → `image` →
  `image-smoke` → `compose-smoke` (local, then `s3`) → `helm-check` → `helm-smoke` → SBOM →
  upload → `assert-clean`.
- The uploads contain only `cetra-*.tar.gz` and `cetra-*.spdx.json`. `publish` rebuilds the
  image contexts.
- `gh release create` attaches the three archives, the three SBOMs, `SHA256SUMS`,
  `cetra-<version>.tgz` and `deploy/install.sh`.

## Done when

- [ ] `release.yml` and the `.github/README.md` section are in.
- [ ] `just check-workflows`, `just check` and `just assert-clean` pass. One commit:
      `Add the release workflow`.
- [ ] Your report says the workflow is untested until the user's dry run (README § Releasing).
