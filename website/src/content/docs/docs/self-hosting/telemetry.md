---
title: "Telemetry"
description: "What a self-hosted cetra sends to us, field by field, and how to turn it off."
---

This page lists what a self-hosted cetra sends to us and how to turn it off.

cetra sends one anonymous heartbeat a day to `telemetry.cetra.sh`. It tells us which versions run, on which platforms, at what scale, and which features are in use. It carries no names, emails, domains, URLs, host names, licence data or exact counts.

cetra can also send crash reports. They are off unless you turn them on: see [Crash reports](#crash-reports).

## Turn it off

Any one of these turns the heartbeat off:

| Setting | Example |
|---|---|
| `CETRA_TELEMETRY` set to `off`, `false` or `0` (any case) | `CETRA_TELEMETRY=off` |
| `DO_NOT_TRACK` set to anything but `0` or `false` | `DO_NOT_TRACK=1` |
| `telemetry.heartbeat = false` | `CETRA_TELEMETRY__HEARTBEAT=false` |

Any other value of `CETRA_TELEMETRY` stops cetra at startup with an error, so a typo never leaves the heartbeat on. An empty value is ignored. `CETRA_TELEMETRY` is not the environment form of a setting: `CETRA_TELEMETRY=on` is an error too.

`CETRA_TELEMETRY=off` and `DO_NOT_TRACK` turn crash reports off too.

With Docker Compose, add `CETRA_TELEMETRY=off` to `cetra.env`. With the Helm chart, add it to `env` in your values:

```yaml
env:
  - name: CETRA_TELEMETRY
    value: "off"
```

The heartbeat is also off, whatever the settings say:

- in [restore mode](/docs/self-hosting/backup/#restore-mode-restore-drills);
- in a CI environment: `CI` set to anything but `false` or `0`, or any of `GITHUB_ACTIONS`, `GITLAB_CI`, `BUILDKITE`, `CIRCLECI`, `JENKINS_URL`, `TF_BUILD`, `TRAVIS`, `TEAMCITY_VERSION`, `BITBUCKET_BUILD_NUMBER`, `CODEBUILD_BUILD_ID`, `DRONE`, `APPVEYOR` or `SEMAPHORE` set;
- in a debug build (built without `--release`).

`npx @cetra-ai/cli init` can start a local `cetra serve` for you. It is a server like any other, so it sends the heartbeat too; it inherits your shell's environment, so `export CETRA_TELEMETRY=off` before you run `init` turns it off there.

Turning the heartbeat off changes nothing else, and licences never turn it on or off. Licence keys are checked offline.

## The startup line

At every start, cetra logs two lines, at `info` with the target `cetra_server::observability::notice`: one says whether the heartbeat is on, the other whether crash reports are on. They show even when `log.level` is `warn`. When the heartbeat is on, its line gives the instance id and a link to this page. When something is off, the line's `reason` field says why: `CETRA_TELEMETRY`, `DO_NOT_TRACK`, `restore_mode`, `ci`, `debug_build` or `config`. Crash reports are never off for `ci` or `debug_build`.

`cetra debug-bundle` shows the instance id too, with the heartbeat the server would send now ([Reporting a bug](/docs/help/reporting-a-bug/)).

## When it is sent

The first heartbeat goes 10 to 20 minutes after the server starts, then one every 24 hours. cetra records in `meta.db` when the last heartbeat was accepted, so restarts send at most one a day.

The request is `POST https://telemetry.cetra.sh/v1/heartbeat` with a JSON body and the header `User-Agent: cetra/<version>`. It follows no redirect and gives up after 10 seconds. It goes through the proxy in `HTTPS_PROXY` when one is set, except for hosts in `NO_PROXY`. A heartbeat that fails is not retried before the next one, and failures are logged at `debug` only, so a server without internet access logs no warnings. To send it through a relay of your own, set [`telemetry.endpoint`](/docs/self-hosting/configuration-reference/#telemetry).

## What it sends

| Field | Values | Meaning |
|---|---|---|
| `schema` | `1` | The version of this format. |
| `instance_id` | a random UUID | The instance id (below). |
| `version` | `0.1.0` | The cetra version. |
| `target` | `x86_64-unknown-linux-musl` | The platform the binary was built for. |
| `profile` | `release`, `debug` | The build profile. |
| `deployment` | `docker`, `kubernetes`, `binary` | How cetra runs: a container, a Kubernetes pod, or the binary on a host. |
| `storage` | `file`, `s3` | Where Parquet files live. |
| `uptime` | `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+` | How long the server process has run, measured when the heartbeat is sent. |
| `install_age` | `<1d`, `1d-7d`, `7d-30d`, `30d-90d`, `90d-1y`, `1y+` | How long ago the first account was created. |
| `orgs`, `projects` | `0`, `1`, `2-5`, `6-20`, `21-100`, `101-1000`, `1000+` | Active organizations and projects. |
| `users` | the same buckets | Accounts that can sign in. |
| `spans_per_day`, `scores_per_day` | `0`, `<1k`, `<10k`, `<100k`, `<1M`, `<10M`, `<100M`, `100M+` | Spans and scores received yesterday (UTC), all organizations together. |
| `features.email` | `true`, `false` | Email (SMTP) is configured. |
| `features.github_sign_in`, `features.google_sign_in`, `features.oidc_sign_in` | `true`, `false` | Sign-in with GitHub, Google or an OIDC provider is configured. |
| `features.alert_rules`, `features.online_rules`, `features.datasets`, `features.prompts`, `features.annotation_queues` | `true`, `false` | At least one alert rule, online rule, dataset, prompt or annotation queue exists. |
| `features.self_monitoring` | `true`, `false` | cetra exports its own logs or traces to an OTLP endpoint of yours. |
| `features.crash_reports_on` | `true`, `false` | Crash reports are on. |
| `usage` | an object, one key per event below, each `0`, `1-9`, `10-99`, `100-999` or `1000+` | How often each feature was used since the last heartbeat, see [Feature usage](#feature-usage). |

An example:

```json
{
  "schema": 1,
  "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
  "version": "0.1.0",
  "target": "x86_64-unknown-linux-musl",
  "profile": "release",
  "deployment": "docker",
  "storage": "file",
  "uptime": "<1h",
  "install_age": "30d-90d",
  "orgs": "1",
  "projects": "2-5",
  "users": "6-20",
  "spans_per_day": "<100k",
  "scores_per_day": "<1k",
  "features": {
    "email": true,
    "github_sign_in": false,
    "google_sign_in": true,
    "oidc_sign_in": false,
    "alert_rules": true,
    "online_rules": true,
    "datasets": false,
    "prompts": true,
    "annotation_queues": false,
    "self_monitoring": false,
    "crash_reports_on": false
  },
  "usage": {
    "alert_rule.created": "0",
    "annotation_queue.created": "0",
    "annotation_queue.item_completed": "0",
    "api_key.created": "0",
    "assistant.panel_opened": "0",
    "assistant.turn_sent": "0",
    "backfill.started": "0",
    "channel.created": "0",
    "command_palette.opened": "0",
    "dashboard.created": "0",
    "dashboard.edited": "0",
    "dataset.created": "1-9",
    "dataset.exported": "0",
    "dataset.imported": "0",
    "dataset.items_added": "0",
    "deletion.requested": "0",
    "device.approved": "0",
    "evaluator.created": "0",
    "evaluator.run": "0",
    "experiment.created": "0",
    "invite.created": "0",
    "issue_source.created": "0",
    "list_layout.changed": "0",
    "live_tail.toggled": "0",
    "llm_connection.created": "0",
    "mcp.tool_called": "0",
    "online_rule.created": "0",
    "org.created": "0",
    "page.viewed": "100-999",
    "personal_token.created": "0",
    "playground.run": "0",
    "project.created": "0",
    "prompt.created": "0",
    "prompt.version_created": "0",
    "saved_view.created": "0",
    "scores.created": "0",
    "sql_console.run": "10-99",
    "theme.changed": "0",
    "trace.viewed": "1000+",
    "user.signed_in": "0"
  }
}
```

It never contains emails, email domains, names of organizations, projects or anything else, URLs, host names, IP addresses, organization or project ids, licence keys or licence state, or exact counts. Our relay sees the address the request comes from, as any server does, and does not store it.

## The instance id

The instance id is a random UUID that cetra creates in `meta.db` the first time it sends a heartbeat or a crash report. Nothing about your machine goes into it. A backup and its restore keep it, since they are the same instance; a copy of the data directory made by hand shares it with the original. The startup line shows it. To have us delete the heartbeats and crash reports of an instance, write to `hello@cetra.sh` with its id.

## Crash reports

Crash reports are off unless you turn them on with `telemetry.crash_reports = true` (or `CETRA_TELEMETRY__CRASH_REPORTS=true`). Then cetra sends us a report for every panic and every error-level log event, so we can fix bugs you have not reported. A report names the place in the code where the problem happened. It never contains the error message, a value, or anything from your data.

`CETRA_TELEMETRY=off`, `DO_NOT_TRACK` and restore mode turn crash reports off whatever the setting says. CI environments and debug builds do not, since crash reports only run when you ask for them.

### What a report contains

| Field | Example | Meaning |
|---|---|---|
| `schema` | `1` | The version of this format. |
| `instance_id` | a random UUID | The [instance id](#the-instance-id), added when the report is sent. |
| `version`, `target`, `profile`, `deployment` | `0.1.0`, `x86_64-unknown-linux-musl`, `release`, `docker` | As in the heartbeat. |
| `uptime` | `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+` | How long the server process had run. |
| `kind` | `panic`, `error_event` | A panic, or an error-level log event. |
| `location` | `crates/storage/src/wal.rs:412:9` | The file and line in cetra's code (a panic also has a column). A file of a dependency keeps only the part after Cargo's download directory (`arrow-55.0.0/src/array.rs`); any other file keeps only its name. |
| `event_target`, `module` | `cetra_storage::wal` | The log event's target and module. Absent for a panic. |
| `fields` | `["error", "project_id"]` | The names of the log event's fields, never their values. Empty for a panic. |
| `backtrace` | `["cetra_storage::wal::Wal::sync", …]` | The names of the functions on the stack, at most 48, without file or line. The frames of Rust's own panic machinery above cetra's code are left out. |
| `count` | `3` | How many times this happened. |
| `first_hour`, `last_hour` | `490896` | The hours (since 1970, UTC) of the first and last time, never the exact time. |

An example:

```json
{
  "schema": 1,
  "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
  "version": "0.1.0",
  "target": "x86_64-unknown-linux-musl",
  "profile": "release",
  "deployment": "docker",
  "uptime": "1h-1d",
  "kind": "error_event",
  "location": "crates/storage/src/wal.rs:412",
  "event_target": "cetra_storage::wal",
  "module": "cetra_storage::wal",
  "fields": ["error", "project_id"],
  "backtrace": ["cetra_storage::wal::Wal::sync", "cetra_storage::flush::run"],
  "count": 3,
  "first_hour": 490896,
  "last_hour": 490897
}
```

A report never contains the panic or error message, field values, error texts, thread names, host names, file paths outside cetra's code and its dependencies, environment variables, configuration, or exact times. Rust panic messages can quote data (an `unwrap` prints the error it found), so cetra drops the message whole rather than trying to clean it.

### How reports are sent

- The same place in the code is reported at most once an hour; later occurrences are counted and sent as one report after the hour. At most 20 reports go out an hour.
- cetra writes each report to `<data_dir>/telemetry/spool/` the moment it happens, so a panic that stops the server is sent at the next start. The spool holds at most 50 reports; the oldest go first.
- Every minute, cetra sends the spooled reports to `https://telemetry.cetra.sh/v1/crash` (or [`telemetry.endpoint`](/docs/self-hosting/configuration-reference/#telemetry)`/v1/crash`) and deletes each one the relay accepts or refuses. A report that cannot be sent (the relay is down, busy or rate-limiting) waits for the next try and is deleted unsent after 7 days. Requests follow no redirect and give up after 10 seconds; failures are logged at `debug` only.
- Writing a report takes a moment the first time in a process, while cetra reads its own function names. Error-level events of libraries that log through the `log` crate are not reported, since they carry no code location, and neither are errors of the HTTP stack that exports cetra's own logs to your OTLP endpoint (a down endpoint is not a cetra bug).
- Turning crash reports off deletes the spool at the next start. Backups do not include it.

## Feature usage

cetra counts how often each product event happens, whoever does it: the web UI, the CLI, an SDK, an agent over MCP, or the API. The heartbeat's `usage` field sends each count since the last heartbeat as a bucket: `0`, `1-9`, `10-99`, `100-999` or `1000+`. It never says who did what, in which organization or project, in what order, or when. cetra keeps the counts in `meta.db` while the heartbeat is on, saved once an hour, and clears what the relay accepted. When the heartbeat is off, nothing is saved or sent.

While the heartbeat is on, the web UI reports its own events (marked "UI" below) to your server, through `POST /api/v1/analytics/events` on its own origin, only so that they are counted: your server keeps no per-user record of them and sends nothing about them except the buckets. The UI loads no analytics script and sets no cookie. When the heartbeat is off and [product analytics](#product-analytics) are not set up, the UI reports nothing.

| Event | Counted when |
|---|---|
| `alert_rule.created` | an alert rule is created |
| `annotation_queue.created` | an annotation queue is created |
| `annotation_queue.item_completed` | a reviewer completes a queue item |
| `api_key.created` | a project API key is created |
| `assistant.panel_opened` | UI: the assistant panel opens |
| `assistant.turn_sent` | a message is sent to the assistant |
| `backfill.started` | an online-rule backfill starts |
| `channel.created` | a notification channel is created |
| `command_palette.opened` | UI: the command palette opens |
| `dashboard.created` | a dashboard is created |
| `dashboard.edited` | a dashboard or the overview is saved |
| `dataset.created` | a dataset is created |
| `dataset.exported` | a dataset export is downloaded |
| `dataset.imported` | a file is imported into a dataset |
| `dataset.items_added` | items are added to a dataset |
| `deletion.requested` | a GDPR deletion is requested |
| `device.approved` | a device sign-in (`cetra login`, `npx @cetra-ai/cli init`) is approved |
| `evaluator.created` | an evaluator is created |
| `evaluator.run` | an evaluator run starts |
| `experiment.created` | an experiment is created |
| `invite.created` | an organization invite is created |
| `issue_source.created` | an issue source is created |
| `list_layout.changed` | UI: the list layout changes |
| `live_tail.toggled` | UI: a list's Live switch is flipped |
| `llm_connection.created` | an LLM connection is created |
| `mcp.tool_called` | an MCP tool call ends (`query_sql` also counts as `sql_console.run`, `get_trace` as `trace.viewed`) |
| `online_rule.created` | an online rule is created |
| `org.created` | an organization is created |
| `page.viewed` | UI: a page is shown |
| `personal_token.created` | a personal access token is created |
| `playground.run` | a playground run starts |
| `project.created` | a project is created |
| `prompt.created` | a prompt is created |
| `prompt.version_created` | a prompt version is created |
| `saved_view.created` | a view or SQL query is saved |
| `scores.created` | scores are written to a project |
| `sql_console.run` | a SQL query succeeds, in the SQL console, the API or the MCP tool `query_sql` |
| `theme.changed` | UI: a theme is picked in the theme menu |
| `trace.viewed` | a trace's detail is loaded, by the UI (including when hovering a link preloads it), the API or the MCP tool `get_trace` |
| `user.signed_in` | someone signs in to the web UI |

New events are only ever added to this list; an event keeps its name.

## Product analytics

A self-hosted cetra sends us no per-user events: only the bucketed counts above, inside the heartbeat. Events are counted whether a person or a program did them, so an action made in the UI, the CLI, an SDK or over MCP counts once.

cetra can send per-user product events to a PostHog project, set with [`[analytics]`](/docs/self-hosting/configuration-reference/#analytics). It is off unless you set it, and restore mode turns it off. cetra's own cloud sets it to a PostHog project of ours in the EU. When it is on:

- The web UI tells the server which page it shows and the UI events above, through `POST /api/v1/analytics/events` on its own origin. The UI loads no analytics script, sets no cookie, and records no clicks or screens: only the events of the list above.
- The server sends each event to PostHog with the user's id (a random UUID), the organization's id, the project's id for project pages, and the event's own values (such as the page's route template, `/$org/$project/traces/`, never its address). It adds the user's role in the organization and the organization's plan.
- It never sends emails, names, organization or project names, addresses of pages, IP addresses, filters, queries, prompts or any trace data, and it asks PostHog not to geolocate events.
- A browser that sends `DNT: 1` (Do Not Track) or `Sec-GPC: 1` (Global Privacy Control) keeps its events out of PostHog; they still count toward the feature counts.
- Events made with an API key, which has no user, are only counted.

