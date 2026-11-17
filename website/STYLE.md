# Style guide

The voice and terminology of the cetra documentation. `scripts/check-terms.mjs` enforces the banned terms in CI.

## Voice

Second person, present tense, active voice. Plain words, short sentences, one idea per sentence. Say what to do, then why, only if the why matters to the reader.

- No marketing: no "seamless", "powerful", "blazing", "effortless", "simply", "just" (as a minimiser), "easy".
  - Good: "Set `CETRA_PORT` to change the port."
  - Bad: "Simply set the port with our powerful configuration system."
- No exclamation marks.
  - Good: "The trace appears in the UI."
  - Bad: "Your trace appears in the UI!"
- No emoji.
  - Good: "Warning: this deletes the project."
  - Bad: "⚠️ This deletes the project."
- No performance claims. Describe how cetra works and its limits, never "N spans/s" or "X times faster" (D-W-14).
  - Good: "cetra keeps recent spans in memory until they are written to Parquet."
  - Bad: "cetra ingests a million spans per second."
- British spelling of "licence" (noun), as in the rest of the repository; otherwise American spelling ("normalize", "color").
  - Good: "the licence key", "normalize".
  - Bad: "the license key", "normalise".
- Second person, active voice.
  - Good: "You send spans over OTLP."
  - Bad: "Spans are to be sent by the user over OTLP."

## Structure

- Each page starts with one sentence that says what the page lets the reader do.
- Task pages are numbered steps with one action each. Reference pages are tables.
- Headings are sentence case.
- Code blocks show complete, runnable code with the imports. Placeholders are `<ALL_CAPS>` in angle brackets, and keys are `sk-cetra-…` with a literal ellipsis.
- Every Python or TypeScript block is tested or marked as untested (see below).
- Internal links are absolute paths with a trailing slash (`/docs/tracing/sessions/`).
- A moved page gets a redirect once the site is public.

## Sample markers

Added in W3 (the markers do nothing until then):

- `{/* test */}`: a Python block run by the Python SDK's tests.
- `{/* test:otel */}`: a Python block run against a recording OTLP endpoint.
- `{/* example:<name> */}`: a TypeScript block that equals the body of a tested function.
- `{/* untested: <reason> */}`: a block that cannot be tested, with the reason.

`bash`, `toml`, `yaml`, `json` and `sh` blocks are not code samples.

## Terminology

The glossary page lists these; `check-terms.mjs` flags the wrong forms.

| Use | Not |
|---|---|
| cetra (lower case, even at the start of a sentence; `cetra` in code font for the binary) | Cetra, CETRA |
| trace, span | log, event (for a span), run (except an experiment run) |
| session | conversation, thread (as the product noun) |
| user (the end user of the reader's app) | customer, end-user |
| member (a person in an org) | user (for people who sign in to cetra) |
| organization, org | team, workspace, tenant |
| project | app, environment (for a project) |
| environment (the `environment` field) | stage |
| score | grade, metric (for a score) |
| evaluator; LLM judge | eval (as a noun), grader |
| online rule | monitor, automation |
| dataset, dataset item | test set, example |
| experiment | eval run |
| API key (`sk-cetra-…`), public key (`pk-cetra-…`), personal access token (`pat-cetra-…`) | token (alone), secret |
| self-hosted; cetra Cloud (once it exists) | on-prem, SaaS |
| the UI | the dashboard (except the Dashboard page), the app |
| OpenTelemetry (prose), OTel (only after the first use on a page), OTLP (the protocol) | otel, Open Telemetry |
| `llmobs` never appears (the old working name, D46) | |
