# Changelog

All notable changes to llmobs are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Added

- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).

### Fixed

- `llmobs login` exits at once when the sign-in is cancelled in the browser, instead of waiting five
  minutes for a callback that never comes.
- A password reset (forgot-password link or admin reset link) now revokes the user's personal
  access tokens as well as their web sessions, as a password change already did.
- A categorical webhook evaluator must list its categories; one saved without them is refused with
  400 (`output_score.categories`) instead of failing every run with `unknown label`.
- A webhook evaluator answering 402 now pauses its rule, as D160 says for every other evaluator
  (it was permanent, so each run failed until the error breaker tripped).
