# Testing llmobs locally

How to build llmobs from this repository, run it on your machine with demo data, and try every
area by hand. No LLM API key, SMTP server or cloud account is needed. Checked on Linux x86_64 on
2026-11-15 (`main` at f60b816).

## Prerequisites

The tools from `AGENTS.md` § Commands: `rustup`, `just`, Node (`.node-version`) with `pnpm`, and
`python3` (3.10 or later, standard library only, for the demo scripts). Ports 8080, 4317, 4318
and 8090 must be free.

## From zero to a populated UI

```bash
just build-release
```

Builds the web UI, then the release binary with the UI embedded (`target/release/llmobs`, about
10 minutes from a cold cache). `./target/release/llmobs version` prints `ui: embedded`. If rustc
crashes with `SIGSEGV` during the final link (thin LTO), run it again with a bigger stack:
`RUST_MIN_STACK=33554432 cargo build --release -p llmobs-cli`.

In a first terminal:

```bash
just demo-serve
```

Starts `llmobs serve` on `target/demo/data` with a known first admin (no setup page), org `demo`,
project `demo`, the ingest key `sk-llmobs-demo0000000000000000000000000000`, and private network
egress allowed (so the judge and webhooks can reach the mock on 127.0.0.1). The `LLMOBS_INIT_*`
variables apply only to an empty data directory; later starts reuse the data. Logs are JSON when
the output is not a terminal.

In a second terminal:

```bash
just demo-llm
```

Starts `tools/demo/mock_llm.py` on 127.0.0.1:8090: an OpenAI-compatible Chat Completions server
that plays the LLM judge (any API key, model `mock-judge`), and a webhook receiver that prints
every `POST /hook/...` body.

In a third terminal:

```bash
just demo-seed
```

Sends 300 traces (about 600 spans) over OTLP/HTTP JSON, made from the fixtures in `fixtures/otlp/`
(OpenInference, OpenLLMetry, Vercel AI SDK, GenAI semconv, Langfuse, Laminar, Google ADK, our own
SDKs) with fresh IDs, start times over the last 7 days (a fifth in the last hour, half in the last
day), 12 end users, about 50 sessions, 4 services, `production`/`staging`, tags and ~7 % errors.
It then writes ~350 scores (`helpfulness`, `thumbs_up`, `tone` on traces, `resolved` on sessions)
and signs in as the admin to create the `helpfulness` and `quality` score definitions used by the
Annotate tab. `just demo-seed 1000` sends more; run it again at any time for fresh traffic.
`python3 tools/demo/seed.py --help` lists the options (`--days`, `--seed`, `--url`, `--key`).

Open <http://localhost:8080> and sign in as `admin@demo.test` / `demo-password`.

To start over: stop the server, `rm -rf target/demo`, and run `just demo-serve` again.

The project sidebar shows at 768 px and wider; in a narrower window use the command palette
(Ctrl+K / ⌘K) to move between pages.

## The tour

Each item was checked by hand on the demo data. Paths are under
`http://localhost:8080/demo/demo/` unless they start with `/`.

### Explore

- [ ] **Traces** (`traces`): a table of the last 24 hours with status, name, input/output
  previews, duration, tokens, cost, one column per score, models, session, user, tags and
  environment.
- [ ] **Trace view**: click a row. Span tree with a timeline on the left; on the right the span's
  Chat / Input-Output / Attributes / Scores / Annotate tabs. `Transcript` switches to a chat
  view. A trace younger than a few minutes shows `Live`.
- [ ] **Filters**: `Edit as text`, type `status = 'error' AND environment = 'production'`, Enter.
  The filter becomes chips and goes into the URL (`?filter=…`).
- [ ] **Saved views**: with a filter set, `Default view` → `Save as…` → name → `Save view`
  (Enter does not submit). The view's name replaces `Default view`.
- [ ] **Live mode**: turn on `Live`, then send a few traces from the last minute:
  `python3 tools/demo/seed.py --key sk-llmobs-demo0000000000000000000000000000 --traces 5 --days 0.001`.
  They appear at the top within a few seconds.
- [ ] **Spans**, **Sessions** (one row per session; open one for its turns as a conversation),
  **Users** (one row per end user with traces, sessions, tokens, cost and errors).
- [ ] **Dashboard**: traces and errors, error rate, latency percentiles, LLM latency and cost by
  model, tokens by type, TTFT, scores; `Group by` splits the series.

### Query

- [ ] **SQL console** (`sql`): the schema browser lists `traces`, `spans`, `sessions`, `scores`;
  `Run` the example query. Judge traces of online evaluation are rows of `traces` too (environment
  `llmobs-eval`).
- [ ] **API with a personal access token**: `/account` → Personal access tokens → `New token`, or
  `POST /api/v1/me/tokens`. Then for example
  `curl -H "Authorization: Bearer pat-llmobs-…" -H 'Content-Type: application/json' -H 'Accept: text/csv' -d '{"query":"SELECT trace_id, name FROM traces LIMIT 3"}' 'http://localhost:8080/api/v1/projects/demo%2Fdemo/sql'`
  returns CSV (JSON without the `Accept` header).
- [ ] **CLI**: `./target/release/llmobs login --url http://localhost:8080` opens the browser on an
  `Authorize` page and saves a token (in `~/.config/llmobs`; set `XDG_CONFIG_HOME` to keep it
  elsewhere). Or skip the login with `LLMOBS_URL`, `LLMOBS_TOKEN=pat-llmobs-…` and
  `LLMOBS_PROJECT=demo/demo`. Then:
  `llmobs projects list`,
  `llmobs traces list --limit 5`,
  `llmobs traces get <trace_id>` (span tree),
  `llmobs query --since 24h "SELECT user_id, count(*) n FROM traces GROUP BY user_id ORDER BY n DESC"`
  (`--format csv`, `-o file`).
- [ ] **MCP**: `POST /mcp` with `Authorization: Bearer pat-llmobs-…` and
  `Accept: application/json, text/event-stream`. `tools/list` returns `list_projects`,
  `get_schema`, `query_sql`, `list_saved_views`, `search_traces`, `get_trace`, `get_span`.
  `query_sql` takes `project`, `query`, `from`, `to` (required, at most 30 days apart). To use it
  from Claude Code: `claude mcp add --transport http llmobs http://localhost:8080/mcp --header "Authorization: Bearer pat-llmobs-…"`.

### Evaluate (with `just demo-llm` running)

- [ ] **LLM connection** (`/demo/settings/llm-connections`): `Add connection`, provider
  OpenAI-compatible, any API key, base URL `http://127.0.0.1:8090/v1`, default model `mock-judge`.
  The row shows status `OK`; `Test` calls the mock.
- [ ] **Evaluator** (`evaluators/new`): template `Relevance`, connection `Mock judge`, model
  `mock-judge`. Under **Test**, paste a trace ID that has an input and output (from the trace
  list) and `Run test`: a score in [0, 1] with the mock's explanation, and a link to the judge
  trace. `Create evaluator`. `Check` evaluators (regex, JSON schema, …) need no connection.
- [ ] **Online rule** (`online-rules/new`): name, evaluator `Relevance`, target Traces, sample
  100 %, `Create rule`. Run `just demo-seed 20`: within ~10 s of each trace ending the rule page
  shows Done jobs and a score distribution, and a `relevance` column appears on the trace list.
  Traces without an input or output fail with `missing variable input (trace.input)`; that is
  expected.
- [ ] **Backfill**: on the rule page, `Run on existing data` → `Estimate` (counts the matching
  traces) → `Run backfill`. ~550 jobs drain in a few minutes (the connection allows 300 calls a
  minute).
- [ ] **Annotation**: on a trace, `Annotate` tab: `helpfulness` (0–1) and `quality`
  (good/ok/bad) from the seeded score definitions (Settings → Score definitions). `Save`; the
  Scores tab shows it with source `human`. Sessions have an `Annotate` button too.

### Alerts (with `just demo-llm` running)

- [ ] **Channel** (Settings → Channels): `New channel` → Webhook, URL
  `http://127.0.0.1:8090/hook/alerts`, a signing secret of at least 16 characters. Row actions →
  `Send test`: the `demo-llm` terminal prints the payload.
- [ ] **Rule** (`alerts/new`): metric Error rate, window 5 minutes, above `1` %, minimum samples
  `1`, tick the channel, `Create rule`. Send traffic (`python3 tools/demo/seed.py --key
  sk-llmobs-demo0000000000000000000000000000 --traces 60 --days 0.002`); within about two minutes
  the rule turns `Firing` and the webhook prints the alert. Email channels need SMTP.

### Data management

- [ ] **Delete matching** (trace list, with a filter such as `user_id = 'alice@acme.test'`):
  the dialog counts the traces in the time range; after `Delete N traces`, reload the list (it
  does not refresh by itself). Settings → Data lists the request (`Hidden, erasing`, then
  `Erased`).
- [ ] **Delete a user's data** (Settings → Data): type an end-user ID (`oscar@wayne.test`),
  confirm by typing it again. Erased within about two minutes on the demo.
- [ ] **Retention** (Settings → General): days to keep, or forever; the online-evaluation trace
  completion timeout; `Delete project…`.
- [ ] **API keys** (Settings → API keys): the `init` key with its last use; `Create key`.

### Organization and instance

- [ ] **Members and invites** (`/demo/settings/members`): `Invite member` → email, role →
  `Send invitation`. Without SMTP the dialog shows the invite link to copy. Open it in a private
  window to sign up with a password and join the org.
- [ ] **Plan** (`/demo/settings/plan`): Self-hosted, usage this month (spans, scores, units,
  bytes, judge cost), features and limits.
- [ ] **Instance admin** (`/admin/users`, `/admin/settings`): users, `Add user`, reset links
  (instead of emails), instance settings.
- [ ] **Backup and restore**, while the server runs:
  `LLMOBS_DATA_DIR=target/demo/data ./target/release/llmobs admin backup --to $PWD/target/demo/backup-1`.
  To check it, copy `meta.db`, `catalog.db`, `secret.key` and `objects/` into an empty directory,
  then with `LLMOBS_DATA_DIR` set to it run `llmobs admin gc-orphans --dry-run` (`missing objects:
  0`), `llmobs admin reapply-deletions --from target/demo/data`, and
  `LLMOBS_HTTP_ADDR=127.0.0.1:8081 LLMOBS_GRPC_ADDR= LLMOBS_OTLP_HTTP_ADDR= llmobs serve`: the
  copy on port 8081 has the same traces. Full procedure: [self-hosting/backup.md](self-hosting/backup.md).
- [ ] **Restart**: stop `just demo-serve` (Ctrl+C) and start it again: the data is still there.

### Your own app

Point any OpenTelemetry exporter at the server:
`OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318`,
`OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer sk-llmobs-demo0000000000000000000000000000"`,
or use the Python and TypeScript SDKs in `sdks/` (spec 10). `just e2e` runs both SDK suites
against a real server.

## Enterprise features

Project roles, SSO enforcement and per-org single sign-on need a licence. Release builds trust
only the production signing key, so build a release binary that also trusts the development key
in `fixtures/license/dev-1.key`, like a debug build does:

```bash
just build-release-dev-license
```

`just demo-license` prints a licence with `project_roles`, `sso_enforcement` and `org_sso`
(`tools/demo/enterprise/license.json`). With only the licence, `just demo-serve` covers project
roles: `LLMOBS_LICENSE_KEY=$(just demo-license) just demo-serve`.

The SSO features need an OpenID Connect provider, and an org's domain is proven by a DNS TXT
record. `tools/demo/enterprise/compose.yaml` runs them with Docker on the host network:

- **keycloak** on <http://localhost:8180> (console `admin` / `admin`): realm `corp` with
  `alice@corp.test` and `dave@corp.test`, realm `acme` with `bob@acme.test` and `carol@acme.test`.
  Usernames are the part before the `@` and passwords `<name>-password` (`alice-password`).
  Client `llmobs`, secret `corp-demo-secret` or `acme-demo-secret`.
- **dns**: dnsmasq on 127.0.0.1:53, serving the TXT records in `dns/records.conf` and forwarding
  everything else to the host's resolver (`127.0.0.53`, systemd-resolved).
- **llmobs**: the host's `target/release/llmobs` in an `ubuntu:26.04` container whose
  `/etc/resolv.conf` points at the dns service, so the host's DNS is untouched. It listens on
  <http://localhost:8081> (beside `just demo-serve`), with data in `target/demo-ee`, the same
  admin, org, project and key as the demo, and `llmobs.toml`: an `[[auth.oidc]]` provider
  `corp` (auto-provisioned as `member` of `demo`) and `enforce_sso_domains = ["corp.test"]`.
  The binary links against the host's glibc, so the image's must be the same or newer
  (`LLMOBS_IMAGE=…` to change it).

```bash
cd tools/demo/enterprise
echo "LLMOBS_LICENSE_KEY=$(just demo-license)" > .env
docker compose up -d
just demo-seed 100 http://localhost:8081
```

Keycloak takes about 30 seconds to start. Without Docker on your machine, copy
`tools/demo/enterprise`, `.env` and the binary (`strip` it first: 630 MB with debug info, 125 MB
without) to the same paths on a Linux host that has Docker, run `docker compose up -d` there, and
forward the two ports: `ssh -N -L 8081:127.0.0.1:8081 -L 8180:127.0.0.1:8180 <host>`. Keycloak
and llmobs must be at the same `localhost` URLs for the browser and for llmobs, since the issuer
is `http://localhost:8180/realms/…` (http issuers must be on a loopback host).

Checked with Keycloak 26.8.1 on Ubuntu 26.04 (`main` at 30bed18):

- [ ] **Licence**: the llmobs log shows `licence active` with `kid` `dev-1`; `/demo/settings/plan`
  lists Project roles, SSO enforcement and Single sign-on as Included.
- [ ] **Instance SSO**: on <http://localhost:8081/login>, `Continue with Corp Keycloak`, sign in as
  `alice`: alice lands in org `demo` as a Member.
- [ ] **SSO enforcement**: invite `dave@corp.test` (Members → Invite member), open the invite link
  and sign up with a password: refused with `sso_required`. An admin reset link for alice
  (`/admin/users`) is refused the same way when used.
- [ ] **Project roles** (`/demo/demo/settings/members`, as admin): set alice's project access to
  `Viewer`. Signed in as alice, `New evaluator` is disabled and writes answer 403.
- [ ] **Per-org SSO** (`/demo/settings/sso`, as admin): provider `Acme Keycloak`, issuer
  `http://localhost:8180/realms/acme`, client `llmobs`, secret `acme-demo-secret`, `Save`. Add
  domain `acme.test`; copy its token into `dns/records.conf`
  (`txt-record=_llmobs-verify.acme.test,"llmobs-verify=<token>"`), `docker compose restart dns`,
  then `Verify`: Verified. Turn on `Enabled`.
- [ ] **Email-first sign-in**: sign out. The login page now asks for the email only; `bob@acme.test`
  → `Continue` goes to the acme realm, and bob joins `demo` with the role for new members
  (Viewer).
- [ ] **Org enforcement**: as bob, create a personal access token; as admin, turn on `Enforced`.
  Bob's token now answers 401 while his SSO session keeps working, and a password signup of
  `carol@acme.test` from an invite is refused with `sso_required`. The admin's email is not in a covered
  domain, so the admin is unaffected; an instance admin of a covered domain would sign in at
  `/login?password=1` (break-glass).

Stop with `docker compose down`. The data in `target/demo-ee` belongs to root (the container's
user); remove it with `sudo rm -rf target/demo-ee` to start over.

## Known gaps

- **Teams add-on and org tokens**: the plans of the cloud control plane (`PUT
  /api/internal/v1/orgs/{org_id}/entitlements`) are not covered here.
- **Email** (invites, password resets, email alert channels, email verification) needs
  `LLMOBS_SMTP_URL`; without it, links show in the UI and `llmobs admin reset-password` prints a
  reset URL.
- **Datasets and experiments** (track J), **cloud auth** (track S) and **model price management**
  (track T) are not built yet; the `Experiments` toggle on lists has nothing to show.
- **Real LLM providers** work the same way: add a connection with a real key instead of the mock.
- **Narrow windows**: below 768 px the project sidebar cannot be opened; use the command palette.
