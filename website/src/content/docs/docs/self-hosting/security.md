---
title: "Security"
description: "A checklist for running cetra in production: TLS, proxies, secrets, outbound requests and backups."
---

This page is a checklist to go through before you expose a self-hosted cetra to other people.

## Network

- [ ] **Terminate TLS in front of cetra.** cetra serves plain HTTP. Put a reverse proxy or a load balancer in front, set `public_url` to the `https://` URL, and publish only the proxy's port. See [TLS](/docs/self-hosting/configuration/#tls).
- [ ] **Set `trusted_proxies`** to the proxy's address, so rate limits and logs use the client's address and not the proxy's. Leave it empty if nothing sits in front of cetra, since cetra then ignores `X-Forwarded-For`.
- [ ] **Keep `metrics_addr` private.** The Prometheus endpoint has no authentication. Bind it to a loopback or cluster-internal address, never to a public interface. See [Monitoring](/docs/self-hosting/monitoring/).
- [ ] **Publish only the ports you use.** Port 8080 serves the UI and API, 4317 and 4318 serve OTLP. Set `grpc_addr` or `otlp_http_addr` to an empty string to turn one off. The compose files publish on `127.0.0.1` unless you change `CETRA_BIND`.
- [ ] **Decide who can sign up.** `auth.signup` defaults to `invite_only`. Use `open` only on a private network, and `auth.signup_domains` to limit the email domains.

## Secrets

- [ ] **Keep secrets in environment variables** from a file only the service can read (`cetra.env` with compose, an `EnvironmentFile` with systemd, a Secret with Helm), not in a shared `cetra.toml`. See [Secrets](/docs/self-hosting/configuration/#secrets).
- [ ] **Back up `secret.key` with the data.** It encrypts stored LLM connection keys and webhook secrets, and a backup without it cannot be read. If you set `encryption_key` yourself, keep that value with your other secrets. See [Backup and restore](/docs/self-hosting/backup/).
- [ ] **Do not share `sk-cetra-` keys.** An API key can write traces to its project. Create one key per application and rotate one that leaks.

## Outbound requests

- [ ] **Leave `security.allow_private_network_egress` off** unless you need it. LLM judges and webhooks then cannot reach loopback, private, link-local or cloud-metadata addresses, which stops a user from pointing them at your internal services. Turn it on only for a local Ollama or vLLM, and then only on a network where that is acceptable. See the [configuration reference](/docs/self-hosting/configuration-reference/#security).
- [ ] **Use `restore_mode` for restore drills**, so a restored copy sends no alerts, webhooks or emails. See [Backup and restore](/docs/self-hosting/backup/#restore-mode-restore-drills).

## Running the process

- [ ] **Run one instance per data directory,** as a user that is not root, with the data directory owned by that user. The Docker image runs as uid 65532 on a read-only root file system.
- [ ] **Pin the version** of the image or chart, and read the release notes before you [upgrade](/docs/self-hosting/upgrade/). Verify downloads as described in [Install](/docs/self-hosting/install/#verify-a-download).
- [ ] **Back up and rehearse the restore.** A backup you have not restored is not yet a backup. See [Backup and restore](/docs/self-hosting/backup/).
- [ ] **Keep payloads out of logs.** cetra's logs contain IDs and sizes, never prompts, completions or secret values. Keep it that way if you set `log.level` to a filter that includes other crates.
