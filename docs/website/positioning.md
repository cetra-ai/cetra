# Positioning draft (hypotheses to test)

Status: **draft, 2026-11-13.** Everything here is a hypothesis built from public evidence, not from talking to customers. Validate it before the homepage copy is final (§8).

Evidence base (190 items, all with links):
- `evidence/github.md`: 78 items from competitors' public GitHub issues and discussions (Langfuse, Phoenix, Opik, Helicone, Laminar, LangWatch, MLflow);
- `evidence/community.md`: 112 items from Hacker News threads (2023–2026), GitHub discussions and a few marked vendor pages.

Reddit could not be reached, and most web results were vendor listicles, which were dropped. References like "community A" or "github D" below point to the theme sections of those two files.

Method: April Dunford's positioning steps: alternatives → unique attributes → value → best-fit customers → category.

## 1. What the evidence says, in one page

| Finding | Strength | Evidence |
|---|---|---|
| **Choosing** a tool comes down to three things: open source and self-hostable (data control), how fast you get a first trace ("works out of the box"), and framework neutrality / OTel. | Strong, about 12–15 mentions each | community §3, ranks 1–3 |
| **Staying, leaving or refusing to upgrade** is driven by how heavy the tool is to run: idle CPU, ClickHouse disk bloat, broken migrations, stacks that fail on the first run, do-it-yourself disaster recovery. | Strong in volume, quiet in votes (many reports, few 👍 each) | github A–C, community A |
| Some users explicitly preferred a Postgres-only Langfuse v2 and refused to move to v3. One HN commenter asked for "a single-binary tool … with a small sqlite database". The Langfuse maintainers turned down a lightweight mode. | Medium, but sharp | community A; github §3 |
| Traces carry sensitive customer data. HIPAA and GDPR push teams to self-host. The US acquisition of Langfuse triggered "moving away immediately" comments. | Strong | community C |
| **Slowness as data grows** hits every product, including the one-container Phoenix on Postgres/SQLite. It is a late-stage pain (tens of GB, tens of millions of observations), rarely a reason to pick a tool. | Strong in volume, late in the journey | github D, community B |
| People want to **query their own traces** (SQL, export, direct database access) and reach them from coding agents (MCP). | Medium, rising | github H–I, community H |
| **Evals are an afterthought** for most teams ("vibes"). Prompt management is often seen as an optional extra. | Medium-strong | community G |
| **Coding-agent tracing** (Claude Code, Codex export OTel) is a fresh, unsolved need. People improvise with mitmproxy, JSONL scraping and SQLite dashboards. | Medium, rising in 2026 | community H |
| The category is consolidating (Sentry, Datadog, PostHog, ClickHouse, Cloudflare). Buyers worry whether small vendors will last. | Medium | community I |

**Counter-evidence to keep in mind** (community §4, github §3):
- Many teams are happy on SaaS.
- Some are happy running ClickHouse.
- Phoenix and OpenObserve are already called "simple and light".
- Langfuse argues that a single container "suffers from poor performance at scale" and now markets its v4 as much faster.
- Almost nobody picks a tool for speed.

## 2. Competitive alternatives (what customers use if we don't exist)

1. **Self-hosted Langfuse**: the default open-source choice, with real goodwill, on a 6-service stack (Web, Worker, Postgres, Redis, ClickHouse, S3).
2. **SaaS**: LangSmith, Braintrust, Langfuse Cloud, Datadog LLM Observability.
3. **Phoenix**: one container, easy start, but slows down at volume and has a thin product beyond tracing.
4. **General observability**: plain OTel into Grafana LGTM, Sentry or Datadog.
5. **Do it yourself**: logs, mitmproxy, a SQLite dashboard, or nothing ("vibes").

## 3. Unique attributes (true only if we build and measure them)

| Attribute | Versus | Status |
|---|---|---|
| One process with no external services. Nothing to install, upgrade or back up besides one binary and a data folder. | Langfuse/Laminar/Opik/Helicone stacks; Helicone's all-in-one container still runs many services inside | Architecture decided (D1, D3) |
| Stays fast as data grows, on one machine (columnar Parquet + DataFusion, not a row database) | Phoenix on Postgres/SQLite slowing at tens of GB; Langfuse needing a ClickHouse cluster | **Must be proven** with the benchmark (D23 targets) |
| Takes any OTel GenAI flavour (GenAI semconv, OpenInference, OpenLLMetry, Vercel AI SDK) without "unknown" spans | Phoenix's OpenInference-only labelling; Langfuse OTel gaps | Spec 02, fixtures |
| SQL over all of your traces, plus an MCP server for agents | Langfuse (no SQL), Phoenix (MCP only) | MVP scope |
| Your data in open Parquet files, readable without us | All of them (ClickHouse, Postgres, proprietary engines) | Architecture decided; worth saying |
| Built-in retention and GDPR deletion | Manual deletes and no TTL reported in github E | MVP scope |
| Independent and open | Acquisitions and upsell funnels | Open core, MIT core (D172) |

## 4. Value: what the attributes give the customer

1. **It runs in a minute, and it stays running.** No database cluster to install, upgrade or babysit, and no migrations that break overnight. *(From: one process. Proof: install command, a "their stack vs ours" diagram, upgrade = replace the binary.)*
2. **Your customers' data never leaves your infrastructure.** Self-hosting is the default, not an enterprise deal. *(From: self-hosted single binary, open files, GDPR deletion. Proof: no telemetry by default; the deletion and retention features listed.)*
3. **It doesn't slow down as your traffic grows.** *(From: the storage engine. Proof: the benchmark page; the live demo with millions of traces.)*
4. **Your traces are yours to query: by you in SQL, and by your coding agents through MCP.** *(From: SQL console, MCP, Parquet.)*
5. **The cost is a machine, not a meter.** No per-trace bill that grows with agent fan-out. *(From: self-hosted. Proof: a sizing calculator.)*

## 5. Best-fit customers (rank these by interviewing)

- **H1: teams shipping LLM features that handle sensitive data and can't or won't send it to a SaaS.** EU companies, health, finance, legal, B2B with strict customers. They need self-hosting but have no ClickHouse skills or platform team. *Pains: community C, github A–C. Why now: the Langfuse acquisition shook EU users.*
- **H2: people already self-hosting a multi-service tool who are tired of running it.** Langfuse v2 holdouts, teams stuck on migrations, small instances paying for idle ClickHouse. *Pains: github A–C. Most direct to reach (they are searching "langfuse self-host" problems). A Langfuse importer would make switching easy (product suggestion, §9).*
- **H3: individual developers and coding-agent users** who want to see what Claude Code, Codex or their own agents did, locally, on a laptop, for free. *Pains: community H. Small budgets, but they create the bottom-up adoption D1 bets on, and they bring it to work later.*
- **Not a fit (for now):** teams happy on SaaS, teams already running ClickHouse with ops capacity, enterprises that need SAML (not in v1), buyers who want a broad all-in-one platform with prompt management first.

**Decision (user, 2026-11-13): sell to both, with revenue as the goal.** The two groups play different roles:
- **Developers (H3, and the engineers inside H1/H2) are the way in.** They find the project, install it and bring it to work. They rarely pay.
- **Companies (H1, H2) are who pays.** They pay for what a team or company needs around the free product (§5a).

So the homepage speaks to the developer first, because they arrive first and install. Every page also gives a buyer a clear path: "For teams", security and compliance, pricing, "Talk to us".

### 5a. What there is to sell (decided: D172–D178, D187)

- **Licence (D172): open core.** The core is MIT. Enterprise code lives in `ee/` under a source-available licence, unlocked by an offline-verified licence key. A self-hosted instance without a key runs the **full core with no volume, seat or retention limits**.
- **Cloud plans (D177), billed in units = spans + scores (D173):**
  - **Free:** $0, 100k units/month, hard cap.
  - **Starter:** $30/month, 200k units included, then overage.
  - **Pro:** $150/month, 5 years data access, retention management, priority support.
  - **Teams add-on:** $300/month, enterprise SSO and fine-grained roles.
  - **Enterprise:** custom.

  Seats (D176): viewers are free. Paid features (D178): unlimited rules, SSO, audit log and SCIM.
- **Self-hosted licence key:** SSO, audit log, SCIM and fine-grained roles for companies running cetra themselves.
- **Demo (D187):** a read-only demo org on the cloud. The website's "Try demo" opens `/demo`.

What this means for the message:
- **"No meter" is a self-host promise only.** The cloud is metered, so the site says "self-host free with no limits" and "cloud with transparent, predictable pricing" (overage rates and a spend cap, D174). It never says "no meter" in general.
- **Two revenue paths, one product.**
  - Cloud plans: the fastest money, sold self-serve.
  - Self-hosted licence keys: the companies of H1/H2 that must keep data in-house.

  The same one-binary story serves both: the cloud runs the same binary, and you can move between the two.
- **Earliest money:** 3–5 paying **design partners** from H1 before the public launch. They validate the positioning and the price, and give the first logos and quotes for the site.

### 5b. What this changes on the site
- The headline must work for both readers. Candidate A does: "run it yourself in a minute" speaks to the developer, "your data never leaves your server" to the buyer.
- Add audience routing after the product sections (Stripe pattern): "For developers" (install, docs, GitHub) and "For teams" (security, compliance, pricing, talk to us).
- The pricing page shows the free self-host prominently ("everything one team needs, free") and the paid company tier next to it, with "Talk to us".
- Add a security and compliance page (data flow, no telemetry, GDPR deletion, retention, deployment options). H1 buyers read it first.
- H3-only material (the coding-agent story) lives in the README, the Show HN, a docs recipe and one homepage section. It doesn't drive the whole site.

## 6. Market category

Compete in the **existing category, "LLM observability" (and evals)**, with a qualifier rather than a new category name. Buyers already search for "self-hosted LLM observability", "Langfuse alternative" and "LLM tracing open source".

Qualifier options: "self-hosted", "single-binary", "runs on one machine". The qualifier carries the difference and the category carries the understanding.

## 7. Messaging

**Promise (one sentence):** LLM observability you can run yourself in a minute, and never have to babysit.

**Pillars**, in order (each needs its proof before launch):
1. **Nothing else to run.** One binary, no Postgres, ClickHouse, Redis or S3. Upgrades mean replacing a file. *Proof:* install command; stack diagram; quickstart timing.
2. **Your data stays with you.** *Proof:* self-host by default, no telemetry, retention and GDPR deletion, open Parquet files.
3. **Stays fast as you grow.** Written as an outcome ("millions of traces on one machine, no slowdown"). *Proof:* live demo plus benchmarks page with named competitors and a method.
4. **Works with what you already use.** OTel from any framework or coding agent; SQL and MCP for you and your agents. *Proof:* integrations list, each item backed by a captured fixture.

Evals and alerts are presented as "the rest of the loop, built in, when you're ready", not as the lead (community G).

**Objections and answers:**

| Objection | Answer | Proof needed |
|---|---|---|
| "One machine won't scale." This is Langfuse's own argument for moving to ClickHouse. | Show the numbers: what one machine handles, with a method. Point to S3 storage and snapshots. | Benchmark page |
| "Simple means a toy." | Feature parity on what matters: traces, sessions, evals, datasets, alerts, SQL, RBAC, OIDC. | Feature table on the "vs" pages |
| "Phoenix / OpenObserve are already light." | Light **and** fast at volume, with the full LLM loop. | Benchmark against Phoenix; feature table |
| "A new, small project might disappear." | Open source; your data sits in open Parquet files you can read without us; one binary keeps working. | MIT core (D172), docs on the data format |
| "We're fine on SaaS." | Not our first customer. A hosted offer may come later. | — |

**Headline candidates** (each tests a hypothesis):

| # | Headline | Sub | Tests |
|---|---|---|---|
| A | **LLM observability you run yourself. In a minute.** | One binary, no database cluster, nothing to babysit. Your data never leaves your server. | Simplicity + time to first trace (picking criteria 1 and 2) |
| B | **Self-host LLM observability without running a database cluster.** | Tracing, evals and alerts in one binary. No Postgres, ClickHouse or Redis. | H2 pain, said directly |
| C | **See what your agents did. On your own machine.** | Traces your team can read and your agents can query. Running in a minute, nothing else to set up. | H3 + agents angle; ownership |
| D | **Your LLM traces, on your servers, without the ops.** | One binary that stays fast as your data grows. | H1 compliance angle |
| E | **LLM observability in one binary.** | Traces your team can read and your agents can query. No Postgres, ClickHouse or Redis to run. | Most compact; may read as engineer-speak |

Current pick: **A**, with C as the strongest alternative for a developer-first launch (README, Show HN).

## 8. How to validate (cheap, in order)

1. **Five to ten interviews** (20 minutes each) with people running LLM apps in production, ideally recruited from H1/H2 (people who commented on self-host issues, EU AI teams, your network). No pitching. Ask:
   - What do you use to see what your LLM app does? How did you choose it?
   - What was the last annoying thing about it? Did you ever consider switching? Why didn't you?
   - Where does the trace data live? Who decided that? What would make you move it?
   - Do you run evals? How?
   - Have you tried tracing a coding agent (Claude Code, Codex)?

   Then show headlines A–E and ask which one makes them want to try it, and why.
2. **README tagline and first post.** Ship the GitHub README with the winning headline. Run the Show HN with the H3 story ("see what your coding agent did, locally, in one binary"), then measure installs → first trace, not stars.
3. **Search-intent pages.** "Self-hosted Langfuse alternative", "LLM observability without ClickHouse". Watch which page brings visitors who install.

## 9. Product notes that came out of this research (not website scope)

Out of scope for `docs/website/`. These are listed for the coordinating session to weigh.
- **Coding-agent tracing as an entry wedge.** Claude Code and Codex export OTel; a documented "point Claude Code at cetra" recipe (with a captured fixture) would serve H3 directly.
- **A Langfuse importer**, or at least a migration guide, would lower H2's switching cost.
- **No telemetry by default** should be a stated policy; H1 checks this first.
- **"Stays fast as data grows" is the claim the incumbent challenges.** The D23 benchmark is a sales asset, not just an engineering target.

## 10. Open decisions that block the final copy

1. ~~Licence (D9)~~: decided, open core (D172). The core is MIT, so "open" and "no lock-in" hold for the core.
2. ~~What is paid~~: decided (D172–D178, §5a). Revenue is the goal (decided 2026-11-13).
3. ~~Which segment to lead with~~: decided, both. Developers are the way in, companies pay (§5).
4. ~~Real name (D46)~~: decided, **cetra** (cetra.sh).
