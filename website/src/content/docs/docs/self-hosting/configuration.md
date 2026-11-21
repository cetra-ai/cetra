---
title: "Configuration"
description: "Set cetra's settings with a TOML file or environment variables, keep secrets safe and put TLS in front."
---

cetra reads, in order, each overriding the previous: built-in defaults, a TOML file
(`cetra serve --config <file>` or `CETRA_CONFIG=<file>`), then environment variables. Every key
can be set as `CETRA_` followed by its path in upper case with `__` between levels:
`storage.flush.max_age` is `CETRA_STORAGE__FLUSH__MAX_AGE`. Common keys also have short names
(below). Unknown keys are an error, and so is an unknown `CETRA_` variable. Sizes are strings
with a binary unit (`"256MiB"`) and durations strings with a unit (`"120s"`). The full list with
defaults is in the [configuration reference](/docs/self-hosting/configuration-reference/).

## The settings most installs set

| Key | Environment | Default | Notes |
|---|---|---|---|
| `data_dir` | `CETRA_DATA_DIR` | `./cetra-data` (`/data` in the image) | one process per directory |
| `public_url` | `CETRA_PUBLIC_URL` | `http://localhost:8080` | the URL users open; used in emails and OAuth redirects |
| `http_addr` | `CETRA_HTTP_ADDR` | `0.0.0.0:8080` | UI, API, OTLP/HTTP, MCP |
| `grpc_addr` | `CETRA_GRPC_ADDR` | `0.0.0.0:4317` | empty disables OTLP/gRPC |
| `otlp_http_addr` | `CETRA_OTLP_HTTP_ADDR` | `0.0.0.0:4318` | empty disables it |
| `trusted_proxies` | `CETRA_TRUSTED_PROXIES` | none | comma-separated CIDRs whose `X-Forwarded-For` is trusted |
| `metrics_addr` | `CETRA_METRICS_ADDR` | unset (off) | Prometheus `/metrics` on its own listener, with no authentication: bind it privately ([Monitoring](/docs/self-hosting/monitoring/)) |
| `restore_mode` | `CETRA_RESTORE_MODE` | `false` | no outbound calls, for restore drills ([Backup and restore](/docs/self-hosting/backup/#restore-mode-restore-drills)) |
| `smtp_url`, `email_from` | `CETRA_SMTP_URL`, `CETRA_EMAIL_FROM` | unset (email off) | invites, password reset |
| `encryption_key` | `CETRA_ENCRYPTION_KEY` | `<data_dir>/secret.key`, generated | encrypts stored secrets |
| `license_key` | `CETRA_LICENSE_KEY` | unset | enterprise features |
| `storage.object_store` | `CETRA_OBJECT_STORE` | `file://<data_dir>/objects` | `s3://bucket[/prefix]`: [Object storage](/docs/self-hosting/s3/) |
| `storage.gc.grace` | `CETRA_STORAGE__GC__GRACE` | `5m` | how long obsolete files stay; at most `30d`. In S3 mode, how long a meta-only backup stays restorable |
| `log.level` | `CETRA_LOG` | `info` | |
| `log.format` | `CETRA_LOG__FORMAT` | `pretty` on a terminal, `json` otherwise | |

First start without the setup page: set `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD`,
`CETRA_INIT_ORG` and `CETRA_INIT_PROJECT` together, and optionally `CETRA_INIT_SECRET_KEY`
(`sk-cetra-` followed by 32 letters or digits) to know the project's ingest key in advance. They
apply only to an empty database.

## Secrets

Keep secrets in environment variables from a file only the service can read (`cetra.env` with
compose, an `EnvironmentFile` with systemd, a Secret with Helm), not in `cetra.toml` if that file
is shared. Logs never contain secret values.

An OpenID Connect provider (`[[auth.oidc]]` in `cetra.toml`) takes its client secret from
`CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`, with the provider's `name` in upper case and `-`
written `_` (`my-idp` becomes `MY_IDP`). Leave
`client_secret` out of the file then.

The encryption key protects stored LLM connection keys and webhook secrets. Back it up with the
data: a backup without its key cannot be read ([Backup and restore](/docs/self-hosting/backup/)). `cetra admin
rotate-encryption-key` replaces it while the server is stopped.

## TLS

cetra serves plain HTTP. Put a reverse proxy in front (Caddy, nginx, Traefik, a cloud load
balancer), set `public_url` to the `https://` URL, and set `trusted_proxies` to the proxy's
address so client IPs in rate limits and logs are right. For OTLP/gRPC through a proxy, the proxy
must speak HTTP/2 to cetra on port 4317.
