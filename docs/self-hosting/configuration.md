# Configuration

llmobs reads, in order, each overriding the previous: built-in defaults, a TOML file
(`llmobs serve --config <file>` or `LLMOBS_CONFIG=<file>`), then environment variables. Every key
can be set as `LLMOBS_` followed by its path in upper case with `__` between levels:
`storage.flush.max_age` is `LLMOBS_STORAGE__FLUSH__MAX_AGE`. Common keys also have short names
(below). Unknown keys are an error, and so is an unknown `LLMOBS_` variable. Sizes are strings
with a binary unit (`"256MiB"`) and durations strings with a unit (`"120s"`). The full list with
defaults is in [spec 12 § Configuration](../spec/12-operations.md#configuration).

## The settings most installs set

| Key | Environment | Default | Notes |
|---|---|---|---|
| `data_dir` | `LLMOBS_DATA_DIR` | `./llmobs-data` (`/data` in the image) | one process per directory |
| `public_url` | `LLMOBS_PUBLIC_URL` | `http://localhost:8080` | the URL users open; used in emails and OAuth redirects |
| `http_addr` | `LLMOBS_HTTP_ADDR` | `0.0.0.0:8080` | UI, API, OTLP/HTTP, MCP |
| `grpc_addr` | `LLMOBS_GRPC_ADDR` | `0.0.0.0:4317` | empty disables OTLP/gRPC |
| `otlp_http_addr` | `LLMOBS_OTLP_HTTP_ADDR` | `0.0.0.0:4318` | empty disables it |
| `trusted_proxies` | `LLMOBS_TRUSTED_PROXIES` | none | comma-separated CIDRs whose `X-Forwarded-For` is trusted |
| `smtp_url`, `email_from` | `LLMOBS_SMTP_URL`, `LLMOBS_EMAIL_FROM` | unset (email off) | invites, password reset |
| `encryption_key` | `LLMOBS_ENCRYPTION_KEY` | `<data_dir>/secret.key`, generated | encrypts stored secrets |
| `license_key` | `LLMOBS_LICENSE_KEY` | unset | enterprise features |
| `storage.object_store` | `LLMOBS_OBJECT_STORE` | `file://<data_dir>/objects` | `s3://bucket[/prefix]`: [s3.md](s3.md) |
| `storage.gc.grace` | `LLMOBS_STORAGE__GC__GRACE` | `5m` | how long obsolete files stay; at most `30d`. In S3 mode, how long a meta-only backup stays restorable |
| `log.level` | `LLMOBS_LOG` | `info` | |
| `log.format` | `LLMOBS_LOG__FORMAT` | `pretty` on a terminal, `json` otherwise | |

First start without the setup page: set `LLMOBS_INIT_ADMIN_EMAIL`, `LLMOBS_INIT_ADMIN_PASSWORD`,
`LLMOBS_INIT_ORG` and `LLMOBS_INIT_PROJECT` together, and optionally `LLMOBS_INIT_SECRET_KEY`
(`sk-llmobs-` followed by 32 letters or digits) to know the project's ingest key in advance. They
apply only to an empty database.

## Secrets

Keep secrets in environment variables from a file only the service can read (`llmobs.env` with
compose, an `EnvironmentFile` with systemd, a Secret with Helm), not in `llmobs.toml` if that file
is shared. Logs never contain secret values.

The encryption key protects stored LLM connection keys and webhook secrets. Back it up with the
data: a backup without its key cannot be read ([backup.md](backup.md)). `llmobs admin
rotate-encryption-key` replaces it while the server is stopped.

## TLS

llmobs serves plain HTTP. Put a reverse proxy in front (Caddy, nginx, Traefik, a cloud load
balancer), set `public_url` to the `https://` URL, and set `trusted_proxies` to the proxy's
address so client IPs in rate limits and logs are right. For OTLP/gRPC through a proxy, the proxy
must speak HTTP/2 to llmobs on port 4317.
