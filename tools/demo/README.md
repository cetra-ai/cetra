# The cetra demo org: runbook

The demo org is the org every signed-in user of a cloud cell reads as a viewer (spec 08 § Demo org, spec 12 § Demo org, D187). This file is what the cetra team follows, once per cell, to set it up and keep it running. `cetra-demo` (this crate) keeps it busy: it writes realistic traces built from the captured fixtures and serves the judge endpoint that the demo's evaluators call. It is not shipped with `cetra`.

Use team email addresses only. Every visitor sees the names and emails of the org's members.

## What the generator does

- `cetra-demo` sends traces to `POST {url}/v1/traces` with the demo project's `sk` key. At startup it writes the missing traces: from the newest trace of the last 30 days, or all 30 days when there is none, up to now, at the daily curve's rate. Then it sends live traces every 10 seconds.
- With `--judge-addr`, it also serves `POST /v1/chat/completions` in the OpenAI-compatible shape. It answers in the form the request's JSON schema asks for (numeric or boolean `value`, or a categorical `label`, with an `explanation` when the schema has one). The answers are drawn, not computed, so the demo's scores are plausible, not meaningful. The judge returns numeric values in [0, 1] only, so numeric demo evaluators must use a 0 to 1 range. The judge never logs request bodies, and it refuses requests without the judge key (401) or without a schema (400).

| Setting | Flag | Environment | Default |
|---|---|---|---|
| Cell base URL | `--url` | `CETRA_DEMO_URL` | required |
| Demo project key (`ingest`, `read`) | `--key` | `CETRA_DEMO_KEY` | required |
| Fixtures | `--fixtures` | | `fixtures/otlp` (the image uses its own copy) |
| Peak rate, traces a minute | `--rate` | | `60` |
| Backfill days | `--backfill-days` | | `30` |
| Judge listen address | `--judge-addr` | `CETRA_DEMO_JUDGE_ADDR` | unset: no judge |
| Judge bearer key | `--judge-key` | `CETRA_DEMO_JUDGE_KEY` | required with `--judge-addr` |

Keys are only read from flags or environment variables and are never printed.

## The image

```sh
just demo-image cetra-demo:dev   # static musl binary + fixtures in target/demo-image, then the image
```

This needs the `x86_64-unknown-linux-musl` Rust target and `musl-gcc` (Ubuntu: `musl-tools`) on the build host, and Docker. The image's entry point is `cetra-demo` with the image's fixtures; it runs as UID 65532, on the same distroless base as `deploy/docker/Dockerfile`. It exposes no port: the judge port is set with `CETRA_DEMO_JUDGE_ADDR`.

## Setting up the demo org (once per cell)

1. **Control plane.** Create the org through the control plane with a team account, so it is known like any other org. Ask the control plane operator for an override on that org (request R-demo-1 in `docs/plans/demo-org/README.md`): no data-access window, no units cap, an ingest rate at least the generator's peak, and an exemption from "one Free org per account".
2. **Cell config.** Set `demo.org` to the org's id in the cell's configuration and restart the cell (spec 12 § Configuration). The cell refuses to start if the org does not exist or is not active.
3. **Project and key.** In the demo org, create the project `demo`. Create an `sk` key on it with the `ingest` and `read` scopes. Put that key in the cell's environment file as `CETRA_DEMO_KEY` for the generator.
4. **LLM connection.** Add an LLM connection to the project:
   - provider: OpenAI-compatible;
   - base URL: `https://<judge-host>/v1`;
   - API key: the judge key (`CETRA_DEMO_JUDGE_KEY`);
   - model: `cetra-demo-judge`;
   - structured output: `native`.
   The cell keeps `security.allow_private_network_egress = false`, so the judge host must be public (step 9).
5. **Evaluators.** Create these in the demo project:
   - `Relevance`, from the built-in template: numeric 0 to 1 (judge);
   - `Toxicity`, from the built-in template: boolean (judge);
   - `Refusal`, a `check` of kind `contains` on `output`, looking for "I'm sorry": boolean. It costs no judge tokens.
6. **Online rules.** Create two rules, each with a sampling rate of 20%:
   - `Relevance (demo)`, running the `Relevance` evaluator;
   - `Toxicity (demo)`, running the `Toxicity` evaluator.
   The `Refusal` check is used by manual runs and backfills only.
7. **Score definitions.** Create one score definition per evaluator output with the same name as the evaluator: `Relevance` (numeric), `Toxicity` (boolean), `Refusal` (boolean).
8. **Alert rule.** Create one alert rule named `Toxicity above baseline` on the `Toxicity` score. The team picks the threshold in the Alerts page.
9. **Dashboards.** Create two dashboards, on the last 24 hours:
   - `Demo overview`: trace volume, latency, cost and tokens, by environment;
   - `Demo quality`: the `Relevance` and `Toxicity` scores over time.
   Their settings are the team's choice in the dashboard editor; they are shared with every visitor, so keep them free of team-only data.
10. **Generator and judge host.** Run `cetra-demo` next to the cell, as a third container in the cell's compose stack, with `CETRA_DEMO_URL` set to the cell, `CETRA_DEMO_KEY` set to the `sk` key, `CETRA_DEMO_JUDGE_ADDR` set to an address the reverse proxy reaches (for example `0.0.0.0:8099`), and `CETRA_DEMO_JUDGE_KEY` set to the judge key. Route `<judge-host>` through the cell's reverse proxy to that port, over HTTPS. Keep the judge key in the cell's env file, not in the compose file.
11. **Check.** Sign in as a user with no membership of the demo org. The org should appear last in the switcher with the Demo badge and the role viewer. In the demo project, the `Relevance` and `Toxicity` scores should start to appear within a few minutes of the first sampled traces.

Nothing in this list is seeded automatically. Changing an evaluator, a rule or a dashboard is done in the UI and is not tracked by this repository.

## Operating notes

- Restart the generator with a new key after the key is rotated. The generator does not reload configuration.
- A `402` from the cell means the org's units cap is reached: the generator waits 10 minutes and sends the same batch again. Check the override from step 1.
- The backfill runs at each startup, from the newest trace in the backfill window (or the whole window when there is none), so a restart after a partial backfill fills the rest. A gap in a running generator is not refilled; live traces resume from the restart.
- Logs print counts and ids only.
