Status: plan, written 2026-11-20 after M1–M5a were implemented.

# CA · M5b — Docs pages and the end-to-end test

This is the last milestone of track CA. The design is `docs/plans/ca-coding-agents/00-design.md` § Milestones, and D-CA-1 to D-CA-13 apply. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**.

**Check rule** (user, 2026-11-20): phases are committed without compiling or testing. The full checks run once, after E3.

## Read first

- `AGENTS.md`, and `website/STYLE.md` (voice, banned terms, sample markers).
- `website/src/content/docs/docs/integrations/google-adk.mdx`: the page shape to follow. It has the front matter, `<Backing fixtures>`, the `{/* untested: … */}` markers, and the sections Point the exporter, What you get, Limits and Next.
- `website/src/content/docs/docs/integrations/index.mdx`.
- `docs/research/coding-agents.md` § 6, and the design § Setup command, § Cost and § Dashboard template.
- Spec 02 § Coding agents.
- `crates/cli/src/agents/` (what `cetra agents setup` writes and prints).
- `crates/cli/tests/e2e.rs`: `start`, `Server`, `Api`, `Project`, `fixture_request`, `post_otlp_http`, `poll_json`, and the test `live_streams_and_every_read_route_work_end_to_end` as the model.

## What it is, and what it is not

**When M5b is done:**
- Four integration pages and a "Coding agents" section in the integrations index exist.
- An end-to-end test runs a real `cetra serve`. It posts an agent context and a captured Claude Code export, then checks the stored session, person, repository, branch and a template widget's query.
- The roadmap row says the track is done.

**Not in M5b:** the docs-site screenshots (`just docs-screenshots`). The template needs coding-agent data in the web mocks, which do not have it.

---

## E1 — Integration pages

**Edit:**
- `website/src/content/docs/docs/integrations/claude-code.mdx` (new)
- `website/src/content/docs/docs/integrations/codex.mdx` (new)
- `website/src/content/docs/docs/integrations/gemini-cli.mdx` (new)
- `website/src/content/docs/docs/integrations/copilot.mdx` (new)
- `website/src/content/docs/docs/integrations/index.mdx`

**Steps.**
1. Each page follows `google-adk.mdx`'s structure and STYLE.md.
   - **Front matter:** a `title`, and a `description` under the length limit `check-terms.mjs` enforces.
   - **`<Backing>`:**
     - Claude Code: `fixtures={["claude_code/bash-tool"]}`.
     - Codex: `["codex/exec-command"]`.
     - Gemini CLI: `["gemini_cli/shell-tool"]`.
     - Copilot: `["genai/copilot-cli-tool"]`.
   - **`## Connect <agent>`:**
     - Create a project key with the `ingest` scope in Settings → API keys.
     - Run `export CETRA_INGEST_KEY=<SK_CETRA_KEY>`, then `cetra agents setup <agent> --url <CETRA_URL>`, in a `bash` fence. `check-samples` does not check bash fences.
     - Say what setup writes, from design § Setup command (the file and its keys), and that `--print` prints it instead.
     - Say that `cetra agents remove <agent>` undoes it.
     - Say to re-run setup after moving or upgrading the `cetra` binary: the hooks hold its path.
   - **`## What you get`:** the trace shape from spec 02 § Coding agents for that agent:
     - the kinds, the tool names, and the tokens;
     - the session, the person (D-CA-5), and `coding_agent`, `repository` and `branch`;
     - the "Coding agents" dashboard template (link `/docs/tracing/custom-dashboards/`).
   - **`## Content`:**
     - the opt-ins setup turns on, and `--no-content`;
     - Claude Code: prompts and tool output, but not the answer text, which Claude Code sends only as log events (D-CA-2);
     - Codex: no content on spans at all;
     - Gemini CLI: prompts and answers.
   - **`## Cost`:** for subscription plans, cost is what the tokens would cost at API prices (D-CA-7). Codex is priced per turn.
   - **`## Limits`:**
     - the versions tested: Claude Code 2.1.323, Codex 0.167.2 (`codex exec`), Gemini CLI 0.68.0; Copilot is documented, not captured;
     - spans are units, with numbers from the fixtures: Claude Code 6 spans for a one-tool prompt, Codex 2 kept of 19 sent;
     - Codex: approve the cetra hook in Codex once, or traces have no repository or branch;
     - Copilot CLI: exports only to `https://` endpoints, so a self-hosted cetra needs TLS in front;
     - Gemini CLI: also sends logs and metrics, which cetra drops; every model call is its own trace;
     - Windows: no hook.
   - **`## Next`:** links to Traces, Sessions and Custom dashboards.
2. **`claude-code.mdx`** also has `## Claude Agent SDK` (D-CA-13). The SDK runs Claude Code with the same settings. It passes `TRACEPARENT`, so a turn nests under the application's span in one trace. Set the variables in the SDK's `env` option. Use a `python` fence marked `{/* untested: needs the Claude Agent SDK and a model */}`, showing `ClaudeAgentOptions(env={…})` with `CLAUDE_CODE_ENABLE_TELEMETRY`, `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA`, `OTEL_TRACES_EXPORTER`, `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`.
3. **`index.mdx`:**
   - Add four table rows: Claude Code, Codex, Gemini CLI and GitHub Copilot. Python and TypeScript are "—", How is `cetra agents setup <agent>`, and Tested against is "captured" (Copilot: "hand-written").
   - Add a `## Coding agents` `<CardGrid>` with four `LinkCard`s, placed before `## Providers`.
4. Give exact product names; no performance claims (D-W-14).

**Commit:** `Document the coding-agent integrations`

---

## E2 — End-to-end test

**Edit:** `crates/cli/tests/e2e.rs`.

**Steps.** Add `#[tokio::test(flavor = "multi_thread")] async fn coding_agents_end_to_end()`, following `live_streams_and_every_read_route_work_end_to_end`:
1. Start a server with `start(dir, "")` and get its `Project` with the ingest key, the way the other tests do.
2. `POST {url}/v1/agent-context` with the ingest key and the body `{"agent":"claude-code","session_id":"e2e-session","repository":"https://github.com/acme/widgets.git","branch":"feature/e2e","user":"Dev@Example.com"}`. Expect 204.
3. Load `fixtures/otlp/claude_code/bash-tool/input.json` as text and replace every `capture-volatile-2` (the session) with `e2e-session`. Decode it, give it a fresh trace id and fresh times the way `fixture_request` and `timed_request` do, and post it with `post_otlp_http`. Expect 200.
4. **Agent SDK case.** Build a second request: one application span `app-request` (resource `service.name = e2e-app`) with a new trace id. Take the same Claude Code spans with that trace id, and set `claude_code.interaction`'s `parentSpanId` to the app span. Post it.
5. `poll_json` the traces list for the project, filtered by `session_id = 'e2e-session'`, until both traces are listed. Use the list route and query parameters the existing tests use. For each:
   - `user_id == "dev@example.com"`;
   - `metadata.repository == "github.com/acme/widgets"`;
   - `metadata.branch == "feature/e2e"`;
   - `metadata.coding_agent == "claude-code"`.

   For the Agent SDK trace, also check that its root span is `app-request`, through the trace's spans route.
6. Post the dashboard query (`POST /api/v1/projects/{p}/dashboards/query`, the body shape of the existing dashboard e2e calls, or spec 05 § Dashboard queries) for the template's "Top repositories by cost" widget:
   - source `traces`, measure `sum cost_total`;
   - filter `metadata['coding_agent'] IS NOT NULL AND metadata['repository'] IS NOT NULL`;
   - breakdown `metadata['repository']`, limit 10, other false;
   - shape `total`, over a range covering the posted times.

   Expect one group, `github.com/acme/widgets`.
7. Also post the "Top sessions by cost" breakdown (`session_id`) and expect the group `e2e-session`.

**Commit:** `Test coding-agent tracing end to end`

---

## E3 — Roadmap and the design's status

**Edit:**
- `docs/roadmap.md` (the CA row only)
- `docs/plans/ca-coding-agents/00-design.md` (the status line only)

**Steps.**
1. The roadmap CA row's Status becomes: "**Implemented** (M1–M5b): captures, normalization, session context, `cetra agents`, the Coding agents template, docs pages and an end-to-end test. PR #185."
2. The design's first line becomes: "Status: implemented on 2026-11-20 (M1–M5b), with a Sonnet review and fixes per milestone."

**Commit:** `Mark track CA implemented`

---

## Plan-end checks (after E3)

1. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
2. `cargo test -p cetra-cli --test e2e coding_agents_end_to_end`.
3. `just check-docs`.
4. `just check`.

Fix what fails within this plan's files.
