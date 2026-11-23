# CI compiler cache

The Rust lint, test and e2e jobs, and the `check-packaging` job of `workflows/packaging.yml`
(main pushes and the weekly run), use kache 2.0.0 with the Garage bucket `kache`
served at `https://s3.chapu.is:8443`. The S3 endpoint and bucket are configured in
`actions/rust/action.yml`; the region is `us-east-1`.

## Credentials

On the Garage host, create an access key scoped to the `kache` bucket with read
and write permission:

```sh
garage key new --name cetra-ci
garage bucket allow --read --write kache --key cetra-ci
```

Store the key's **Access Key ID** and **Secret Access Key** in the GitHub
environment `s3-cache` using these secret names:

- `S3_CACHE_ACCESS_KEY_ID`
- `S3_CACHE_SECRET_ACCESS_KEY`

Add them under the GitHub repository's **Settings → Environments → s3-cache →
Environment secrets**. The Rust compilation jobs select this environment to
receive its secrets. Alternatively, from an authenticated GitHub CLI, run these
commands and enter each value at its hidden prompt:

```sh
gh secret set S3_CACHE_ACCESS_KEY_ID --repo cetra-ai/cetra --env s3-cache
gh secret set S3_CACHE_SECRET_ACCESS_KEY --repo cetra-ai/cetra --env s3-cache
```

## Trusted runs

| Run | Cache access |
| --- | --- |
| Push to protected `main` | Read and save under `kache-v2/` |
| PR authored by `sebastiencs`, from this repository, triggered and re-run by `sebastiencs` | Read main/PR caches and save under `kache-pr-v2/` |
| Other PRs, forks, Dependabot, or manual runs | None |

`CI_CACHE_ALLOWED` checks GitHub's event metadata before the Rust jobs receive
the S3 credentials. The composite action skips kache when caching is disabled
or credentials are missing. Whenever caching is enabled, kache saves its
results. Jobs with `cache: "false"` (rust-policy) do not install kache or access
S3.

Kache 2.0 enforces a separate CI write policy: only protected-branch pushes may
publish to the main prefix. `save-cache: true` alone does not enable PR uploads.
`KACHE_PULL_REQUEST_PREFIX=kache-pr-v2` enables its supported PR write path:
owner PRs read `kache-v2/` first, then `kache-pr-v2/`, and write only to
`kache-pr-v2/`. Protected `main` pushes ignore the PR prefix. Manual runs are
read-only in kache and therefore do not enable caching here.

Only the writer credential pair is used. This policy assumes that repository
writers and the code executed in trusted runs are trusted; repository writers
can modify workflows to access repository secrets. Keep the `pull_request`
trigger, which withholds secrets from fork PRs; do not switch to
`pull_request_target` to expose the cache.

The bucket stays private; neither public access nor CORS configuration is
needed. GitHub's workflow token keeps `contents: read`, and kache writes its
report to the job summary without posting PR comments.

## Linker

The Rust jobs that link (`rust-lint`, `rust-test`, `e2e`) pass `mold: "true"` to `actions/rust/action.yml`, which installs mold 3.0.0 on Linux x86_64 after
checking its sha256. Release and packaging jobs do not install it and use the default linker.
The Rust jobs link with mold rather than the default `rust-lld`, selected workflow-wide by
`RUSTFLAGS: -Clinker-features=-lld -Clink-arg=-fuse-ld=mold` in `ci.yml`. mold
links the workspace test binaries and the `cetra` binary several times faster.
The flag lives in CI only: local builds are unaffected, and any `RUSTFLAGS`
change changes every kache cache key, so one cold run repopulates the store.

## First runs

After the secrets are configured, a qualifying owner PR or a run on `main` can
populate the cache. The first run against the empty bucket compiles everything;
run it again and compare the complete `rust-lint`, `rust-test`, `e2e` and `packaging` job
durations, including kache setup and its post step.

The kache setup step should report the configured S3 remote. Its job summary
shows cache hits and misses. Each caching job has its own prefetch manifest
(`manifest-key: github.job`) within the appropriate main or PR prefix. Selective
warming is enabled; the entire remote is not downloaded before each build.

The remote bucket is not bounded by kache's local cache limit; see
[Cache cleanup](#cache-cleanup).

Port 8443 of `s3.chapu.is` goes straight to Garage through Caddy, not through
Cloudflare's proxy, so Cloudflare's 100 MB cap on a request body does not apply to
the large cached executables kache uploads. Caddy gets its certificate from Let's
Encrypt through the Cloudflare DNS challenge.

References:
[kache action](https://github.com/kunobi-ninja/kache-action/tree/ad7317540dcdd71c904f7a60a87b620a0c0e67ed),
[kache CI write policy](https://ninja.kunobi.com/docs/kache/remote-cache/ci#who-may-write).

## Cache cleanup

The bucket has a 100 GiB Garage quota. kache 2.0 cannot prune a remote, and
Garage records when an object was written, not when it was last read, so two
rules keep the bucket under the quota:

- **PR builds expire after 2 days**: a lifecycle rule on the bucket
  (`kache-pr-expiry`) expires `kache-pr-v2/` objects 2 days after they are
  written, and incomplete multipart uploads after 1 day. Garage applies it once
  a day. An owner PR idle for longer rebuilds its own crates once.
- **Main builds keep what a job still uses**: a nightly cron job on the Garage
  host (03:30, `~/kache-prune/kache_prune.py prune --delete`, log in
  `~/kache-prune/prune.log`) reads every kache manifest
  (`<prefix>/_manifests/**`, which lists the `cache_key` of each crate the
  latest build of a job used) and deletes, from `kache-v2/v3/packs/<crate>/`,
  each `<cache_key>.tar.zst` that no manifest names, that is older than 48
  hours and that is not among the 2 newest of its crate: old commits' builds of
  the workspace crates, and dependency builds no job uses any more. Write time
  alone is not a rule: Garage records when an object was written, not when it
  was read, and a dependency written two weeks ago may be hit on every run (a
  first version that kept the 16 newest objects per crate deleted DataFusion
  builds `rust-test` still used). If the manifests name fewer than 200 keys
  (missing, or a new kache manifest format), the script deletes nothing in
  `kache-v2/`. Crates named `llmobs_*` (before the D46 rename) are deleted;
  `_manifests/` and unknown keys are never deleted.

The script uses its own Garage key, `kache-prune` (read, write and owner on
`kache`, for the lifecycle rule), from `~/kache-prune/credentials.json` (mode
600). Without `--delete`, `prune` is a dry run; `inventory` shows the size by
prefix, by day written and by crate; `lifecycle --apply` sets the PR rule.

A full bucket makes every upload fail with `AccessDenied: Forbidden: Bucket size
quota is reached`. kache-action's post step only warns (`kache sync --push exited
with code 1`, `Failed uploads` in its job summary), so on main pushes
`rust-test (1/4)` runs `just cache-push` first, which fails the job. It
happened once: from 2026-11-21 to 2026-11-22 nothing was cached, and every run
recompiled the workspace crates. If it happens again, run `inventory` and a
dry-run `prune` on the Garage host, then `prune --delete`, or raise the quota
(`garage bucket set-quotas kache --max-size …`).

## Rust tests

`rust-test` runs `just test` in four parallel shards, `rust-test (1/4)` to
`rust-test (4/4)`, and `ci-ok` needs every shard. Each shard runs
`just test-shard <k> 4`:

- cargo-nextest 0.9.151 (installed by `actions/rust` with `nextest: "true"`) runs
  slice `k/4` of the unit and integration tests. A slice deals the tests
  round-robin across all test binaries, after filtering.
- The last shard also runs the storage crash harness (`just test-crash`: the
  ignored crash tests named by `crash_tests` in the `justfile`), all 13 in one
  parallel nextest run of about a minute. Its longest test takes about 45 s, so
  slicing it would only spread that minute over the shards.

The doctests (`just test-doc`, `cargo test --doc`; nextest does not run them) run
in `rust-lint`, which finishes well before the shards: on a shard their
compilation alone added 1.5 minutes.

`NEXTEST_PROFILE=ci` selects the `ci` profile of `.config/nextest.toml`: no
fail-fast, no retries (a flaky test fails its shard instead of passing on a
retry), and tests slower than 60 s are named in the log. The log gives each
test's time.

Each shard compiles the same test binaries; with the cache warm they come from
kache. Only shard 1 saves to the cache (`save-cache` input of `actions/rust`), so
the shards do not upload the same crates four times. Standard runners have 4
CPUs and larger runners are not available to the organisation, so the shards are
the only way to run more tests at once. Measured on PR #219 with the cache warm
(the test binaries from kache): each shard compiles for about 3 minutes and
tests for 2 to 3 (about 38 thread-minutes of tests in all; the slowest test
takes about 50 s), 6 to 8.5 minutes per shard, down from 21 minutes for one
job. A PR that changes `cetra-server` recompiles its test binary on every
shard, about 5 minutes. Add shards when a shard's test time grows past its
compile time.

## Web tests

`just check-web` runs as five parallel jobs. `web` runs `just check-web-static`
(the OpenAPI types check, Biome, `tsc`, the production build and its dist
check), and `web-test (1/4)` to `web-test (4/4)` run `just test-web-shard <k> 4`,
Vitest's `--shard=k/4` of the test files; `ci-ok` needs all five. Locally,
`just check-web` still runs everything in one go.

Vitest took 9 of the single job's 10 minutes, measured on PR #219: 363 files and
3,739 tests, each file rendering in its own jsdom environment on a 4-CPU runner.
The rest of the job takes about 40 seconds. A shard pays the install (about 30
seconds) and a quarter of the tests.

## Release workflow

`workflows/release.yml` runs on tags `v*` (D-N-14). Its `run:` steps call `just` recipes, except steps that only pass values between steps (reading the version, the image and chart names, `helm push`, `gh release create`), which stay inline. (docs/plans/n-packaging/README.md § Releasing, D-N-5–D-N-7). It uses no compiler cache. Its `publish` job runs in the GitHub environment `release` (create it under Settings → Environments, limited to tags `v*`; add a required reviewer to approve each publish) and is the only job with write permissions: `contents` (the GitHub release), `packages` (GHCR), `id-token` and `attestations` (build provenance). It needs no secret besides the workflow's own `GITHUB_TOKEN`.
