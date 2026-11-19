# Community evidence: how practitioners choose, use and complain about LLM observability and eval tools

Collected 2026-11-13. Sources:
- **Hacker News comments**, fetched through the public Algolia API (search plus full threads).
- **GitHub issues and discussions** on langfuse, phoenix, lmnr, helicone and opik. These are first-hand reports from people who self-host.
- **A few web pages**. These are marked *(vendor)* or *(secondary)* when the author sells a competing product.

**Limitations:**
- Reddit could not be reached: the search tool refuses reddit.com and Reddit's JSON endpoints return 403 to scripts. Reddit sentiment is therefore missing; the only Reddit signal is what vendor comparison pages say about it, and that has been left out.
- Most web search results were vendor listicles and were left out.
- HN launch threads attract friendly comments from users (one commenter in the Langfuse 2024 launch thread suspected astroturfing). Founder and vendor comments are marked *(vendor)*.

Total: 112 evidence items (some appear under two themes).

## 1. Theme summary

| Theme | # items | Sources | Strength | One-line takeaway |
|---|---|---|---|---|
| A. Self-hosting pain: footprint, upgrades, ops | 18 | HN, GitHub issues/discussions | **Strong** | The multi-service stack (ClickHouse, Redis, S3, Kafka) brings disk/CPU blow-ups, risky migrations and DR work. People still running v2 on Postgres explicitly liked "only depended on Postgres". |
| B. Performance at volume | 13 | GitHub (Langfuse, Phoenix), HN | **Strong** (self-hosters at scale) | Dashboards time out, UIs become "nearly unusable" once there is tens of GB or tens of millions of observations. Bulk API reads are slow, so people go straight to the DB. |
| C. Data privacy, sovereignty, data ownership | 12 | HN, GitHub, web | **Strong** | Prompts and traces hold sensitive customer data. HIPAA/GDPR push self-hosting, and the US acquisition of Langfuse triggered "moving away immediately". |
| D. OTel, lock-in, framework coupling | 14 | HN | **Strong** | OTel is the accepted hedge against lock-in. LangSmith's tie to LangChain is the most-cited lock-in complaint. Phoenix gets marked down for non-OTel span semantics ("unknown" spans). |
| E. DX, ease of setup, UI quality | 14 | HN | **Strong** | Tools win on "set it up in minutes and stop looking", lose on bugs, docs and dependency conflicts. Nobody feels an LLM tool matches Honeycomb. |
| F. Cost and pricing | 9 | HN, web *(vendor)* | **Medium** | The fear is a Datadog-style bill that grows with trace volume: LLM logs are much bigger than APM logs, and per-span/per-trace pricing grows with agent fan-out. Solo devs want cheap tiers. |
| G. Evals: usefulness and feature sprawl | 14 | HN, hamel.dev | **Medium-strong** | Evals are "an afterthought" or "vibes" for most teams. Those who run them want something simple (dataset, run, pass rate) and often think they could vibe-code it. Prompt management is seen as optional. |
| H. Agents and coding-agent workflows | 10 | HN | **Medium** (rising in 2026) | Claude Code and Codex emit OTel. People hack mitmproxy, JSONL scraping and SQLite dashboards to see what agents did. Visibility across the whole org and cost runaways are the new pains. |
| I. Consolidation and vendor durability | 8 | HN, web | **Medium** | LLM observability is being absorbed by general vendors (Sentry, PostHog, Datadog, Logfire, ClickHouse, Cloudflare). Helicone went into maintenance mode. Buyers worry it is "a feature, not a product". |

## 2. Evidence by theme

### A. Self-hosting pain: footprint, upgrades, ops (18)

- [HN](https://news.ycombinator.com/item?id=37765355) (2023-10). Got Helicone self-hosted after a lot of work, but it was 6–7 large services, and the commenter wished for a single Go binary with SQLite. — "room for a single-binary tool made in Go with a small sqlite database and in-memory caching"
- [HN](https://news.ycombinator.com/item?id=37151782) (2023-08). Was drawn to a new tool because it looked easier to self-host than Helicone, then found its self-host instructions assumed tooling that wasn't there. — "The self-hosted instructions don't really work"
- [HN](https://news.ycombinator.com/item?id=42444670) (2024-12). Liked that Langfuse v2 needed only Postgres and noted that the scalable v3 brings more dependencies. — "I also liked that it only depended on Postgres but the scalable version requires other dependencies."
- [HN](https://news.ycombinator.com/item?id=42449180) (2024-12). A team happy on Langfuse v2.7 chose not to upgrade to the ClickHouse-based v3. — "So we are still on V2.7 - works pretty good for us. Havent tried V3 yet"
- [HN](https://news.ycombinator.com/item?id=46656836) (2026-01). Remembers the v2→v3 ClickHouse migration as painful. — "the painful migration to Clickhouse from v2 to v3"
- [HN](https://news.ycombinator.com/item?id=49661637) (2026-09). Leaving Langfuse running on a laptop for a few months filled a terabyte of ClickHouse disk. — "I'm operating a terabyte-scale Clickhouse - but only because I left Langfuse running for a few months on a MacBook"
- [GitHub langfuse#6572](https://github.com/langfuse/langfuse/issues/6572) (2025-04). After v3, ClickHouse burns CPU even when idle. The team used to run v2 locally in Docker Desktop and now has to host it elsewhere. — "this issue makes that challenging and requires now to host it externally."
- [GitHub lmnr#2176](https://github.com/lmnr-ai/lmnr/issues/2176) (2026-08). After a month of light use, self-hosted Laminar's ClickHouse system logs reached 13.4 GiB against 15.6 KiB of real data, with about 330% sustained CPU. — "On a small instance these grow to dwarf the actual trace data"
- [GitHub helicone#3081](https://github.com/Helicone/helicone/issues/3081) (2024-12). A local run hit S3 403s, ClickHouse migration errors and an undocumented Kafka requirement. — "I did not see in the documentation where Kafka was a requirement."
- [GitHub langfuse#12172](https://github.com/langfuse/langfuse/issues/12172) (2026-02). Upgrading the Docker image left ClickHouse parts broken and the UI full of errors (33 comments). — "After upgrading the langfuse docker image, the user interface gives us lots of various errors"
- [GitHub langfuse#6198](https://github.com/langfuse/langfuse/issues/6198) (2025-03). The background migration of about 5M observations from Postgres to ClickHouse failed with a deadlock. — "Transaction failed due to a write conflict or a deadlock."
- [GitHub langfuse#11603](https://github.com/langfuse/langfuse/issues/11603) (2026-01). The event-storage layer writes huge numbers of small JSON files to S3 and exhausts inodes. — "S3 will upload a large number of small json files, resulting in 100% inode usage."
- [GitHub langfuse#6380](https://github.com/langfuse/langfuse/issues/6380) (2025-04). While ClickHouse was down, the web returned 500 and traces were lost instead of buffered. — "Trace ingestion fails — no trace data is stored in S3 or PostgreSQL"
- [GitHub discussion langfuse#19558](https://github.com/orgs/langfuse/discussions/19558) (2026-11). A self-hosted v3→v4 migration step fails on one large trace, even with batch size 1. — "Step 4 fails with `RangeError: Invalid string length`"
- [GitHub discussion langfuse#19984](https://github.com/orgs/langfuse/discussions/19984) (2026-11). A team running v4 on EKS has to design its own DR and replication plan for in-cluster ClickHouse. — "specifically focusing on data synchronization and replication for ClickHouse deployed inside Kubernetes"
- [HN](https://news.ycombinator.com/item?id=45402284) (2025-09). A small startup tried many tools; Phoenix was the only one that worked out of the box, and some others did not even start. — "Wasted lots of time with others, some solutions did not even boot."
- [HN](https://news.ycombinator.com/item?id=46657025) (2026-01). Installation problems pushed the commenter towards trying another provider. — "I also encountered some issues with installation that just made me want to roll the dice on another provider."
- [GitHub opik#2101](https://github.com/comet-ml/opik/issues/2101) (2025-05). Opik's docker-compose stack fails on small ARM hardware (Raspberry Pi CM4). The issue title says it: "Docker compose failing on RPi CM4" *(weak: an edge case)*.

### B. Performance at volume (13)

- [GitHub discussion langfuse#19002](https://github.com/orgs/langfuse/discussions/19002) (2026-10). With about 63M observations, dashboards time out beyond 30 days even after scaling ClickHouse to 16 vCPU × 3 replicas. — "Hitting the hardcoded ~35s dashboard query timeout for anything beyond ~30-day windows."
- [GitHub langfuse#10334](https://github.com/langfuse/langfuse/issues/10334) (2025-11). After an upgrade, the dashboards and trace pages return "overloaded" errors. — "Database resource limit exceeded. Please use more specific filters or shorter time range."
- [GitHub langfuse#10107](https://github.com/langfuse/langfuse/issues/10107) (2025-10). The latency-percentile panel never loads on large observation tables, even on a ClickHouse cluster. — "the 'Trace latency percentiles' panel on the homepage remains in a loading state indefinitely"
- [GitHub langfuse#7591](https://github.com/langfuse/langfuse/issues/7591) (2025-07). High ingestion volume on Kubernetes causes S3 socket exhaustion, failing liveness probes and pod restarts. — "High Data Volume Causes Liveness Probe Failures & Restarts"
- [GitHub langfuse#9732](https://github.com/langfuse/langfuse/issues/9732) (2025-10). A self-hoster gets timeouts when fetching traces. The issue title says it: "getting timeouts fetching traces"
- [GitHub phoenix#8420](https://github.com/Arize-ai/phoenix/issues/8420) (2025-07). After about 44 GB in production, Phoenix's Postgres-backed UI went from fast to nearly unusable. — "as the data piled up, browsing through traces and spans became slower and slower, eventually rendering the UI nearly unusable"
- [GitHub phoenix#6201](https://github.com/Arize-ai/phoenix/issues/6201) (2025-01). The traces list slows down as traces accumulate, despite weekly deletion and an Aurora r7g.xlarge. — "becomes increasingly slow relative to the number of traces"
- [GitHub phoenix#9461](https://github.com/Arize-ai/phoenix/issues/9461) (2025-09). Slow Postgres inserts let spans pile up in memory until Phoenix runs out of memory. — "due to slow rate of postgres insertion, span data can accumulate inside phoenix, resulting in out of memory"
- [GitHub phoenix#10699](https://github.com/Arize-ai/phoenix/issues/10699) (2025-12). Self-hosted Phoenix freezes under high span volume. — "the solution freezes and becomes slow all the time"
- [HN](https://news.ycombinator.com/item?id=42444558) (2024-12). Self-hosted Langfuse was useful for experiments, but the API was too slow for bulk reads, so the commenter queried the DB directly. — "The trace/generation retrieval API was brutally slow for bulk scanning operations"
- [HN](https://news.ycombinator.com/item?id=44317940) (2025-06). The team's only third-party tool, Langfuse, was "breaking down" for them, and they hoped v3 would help. — "Langfuse for observability and that too was breaking down for us."
- [HN](https://news.ycombinator.com/item?id=50270795) (2026-10). Saw a delay of minutes before agent traces became visible. A maintainer replied that only old SDKs cause it. — "it take some times for LangFuse to process spans and traces from the agents"
- [HN](https://news.ycombinator.com/item?id=46558282) (2026-01). LLM logs are far bigger than APM logs, which drives both the cost and the scale worry. — "the LLM logs are MUCH larger that my APM logs."

### C. Data privacy, sovereignty, data ownership (12)

- [HN](https://news.ycombinator.com/item?id=42442774) (2024-12). Chose Langfuse for its commitment to self-hosting, because traces contain sensitive customer data. — "it's not really practical to have traces with potentially sensitive customer data sitting around on some external company's server"
- [HN](https://news.ycombinator.com/item?id=42442608) (2024-12). Won't proxy LLM calls through a third party or keep prompts in a SaaS, and uses plain OTel instead. — "Sorry but I don’t feel comfortable either proxying my llm calls through a 3rd party"
- [HN](https://news.ycombinator.com/item?id=46659277) (2026-01). After ClickHouse (US) bought Langfuse, a commenter plans to leave Langfuse Cloud because of CLOUD Act/FISA exposure. — "Correct! Will be moving away immediately for this reason."
- [HN](https://news.ycombinator.com/item?id=46659028) (2026-01). Argues that a US parent makes the hosted Langfuse no longer GDPR-safe. — "Since clickhouse is headquartered in the US that means the langfuse cloud is no longer GDPR compliant."
- [HN](https://news.ycombinator.com/item?id=46659303) (2026-01). Disappointed that an EU privacy-branded vendor was sold. — "Very sad, for all their marketing around EU, GDPR, privacy and so on."
- [HN](https://news.ycombinator.com/item?id=41701809) (2024-09). Will self-host Langfuse for HIPAA projects and pay for cloud elsewhere: deployment choice depends on the project. — "I'll give self hosting LangFuse a try for HIPAA projects. And happy to pay for cloud for other projects."
- [HN](https://news.ycombinator.com/item?id=41646314) (2024-09). A consultant (quoted in the thread) prefers open-source tools so clients can bring them in-house for HIPAA. — "leaning towards open source platforms like Arize so clients have the option of pulling it inhouse"
- [HN](https://news.ycombinator.com/item?id=41641698) (2024-09) *(vendor founder)*. Built a "logs in your own DB" tool because they didn't want to log into five separate SaaS tools. — "just wanted the data in our own DB."
- [HN](https://news.ycombinator.com/item?id=40290784) (2024-05). First questions to a new LLM analytics tool: open source? self-hostable? Also objects to MITM proxying. — "Is this open source? Can I self host? Why would I choose your offering over LangFuse?"
- [HN](https://news.ycombinator.com/item?id=50148000) (2026-10). Collecting agent session logs across an organization means scrubbing credentials and handling security. — "You'll need to scrub credentials and handle security."
- [GitHub phoenix#14707](https://github.com/Arize-ai/phoenix/issues/14707) (2026-07). A self-hoster asks for encryption at rest of traces and datasets. The issue title says it: "Encryption at rest of traces and other business data"
- [mlflow.org](https://mlflow.org/articles/data-residency-llm-logs/) (2026) *(vendor)*. Residency covers every copy of prompts, traces, caches and eval datasets, not only where the model runs. Paraphrased; included to show the framing vendors now use.

### D. OTel, lock-in, framework coupling (14)

- [HN](https://news.ycombinator.com/item?id=46558636) (2026-01). Advice: integrate through OTel so switching costs stay low. — "Then I'd try to integrate via a standard connector, OTel for example. Then the cost of switching is much lower."
- [HN](https://news.ycombinator.com/item?id=50270795) (2026-10). Tracing alone is easy to move between tools; prompts and evals are where lock-in lives. — "If you only want observability then it is save and easy to switch to/from LangFuse."
- [HN](https://news.ycombinator.com/item?id=46663717) (2026-01). Uses Langfuse only as an OTel trace backend and finds prompt management too basic. — "We like Langfuse for observability via OpenTelemetry. Prompt management is too basic for our needs."
- [HN](https://news.ycombinator.com/item?id=45402284) (2025-09). Picked Phoenix for its OTel-first "mindset". — "Phoenix was the only solution that worked out of the box and seemed to have the right 'mindset', i.e. using Otel"
- [HN](https://news.ycombinator.com/item?id=45399902) (2025-09). Phoenix shows correctly labelled OTel spans as "unknown" because it expects OpenInference conventions. — "Spans labeled as 'unknown' when I definitely labeled them in the code is probably the most annoying part of Phoenix"
- [HN](https://news.ycombinator.com/item?id=42444670) (2024-12). Built their agent SDK on an OTel-like abstraction so the observability backend can be swapped. — "we’re building our Agents SDK in a way that the observability platform can be swapped"
- [HN](https://news.ycombinator.com/item?id=49396804) (2026-08). OTel is the only way to mix Sentry, Langfuse and Datadog, but it is unloved. — "it feels simultaeneously over-engineered and under-engineered."
- [HN](https://news.ycombinator.com/item?id=50156016) (2026-10). OTel "just works" and, unlike a proxy, failing open doesn't block work. — "OTEL is pretty widely supported and mostly \"just works\" at this stage of maturity."
- [HN](https://news.ycombinator.com/item?id=47301651) (2026-03). Monitors agents with the same OTel + LGTM stack as everything else; no LLM-specific tool. — "OTEL & LGTM, the same stack I use for monitoring everything"
- [HN](https://news.ycombinator.com/item?id=46239236) (2025-12). LangSmith is creeping into LangChain/LangGraph, which makes the commenter hesitant to invest. — "It’s giving AWS-like gentle but firm lock-in vibes"
- [HN](https://news.ycombinator.com/item?id=44840626) (2025-08). LangSmith more or less requires LangChain, which this commenter calls terrible, and they prefer Langfuse. — "I would much prefer Langfuse over it at this point."
- [HN](https://news.ycombinator.com/item?id=42452012) (2024-12). A team (quoted from its blog) rejected LangSmith because it seemed tied to LangChain. — "chose not to proceed with Langsmith as it seemed strongly coupled with Langchain."
- [HN](https://news.ycombinator.com/item?id=42499775) (2024-12). LangSmith shows only LLM inferences and works poorly with other frameworks. — "Langsmith only shows the llm inferences. Also it hardly works with other frameworks like crewai for instance."
- [HN](https://news.ycombinator.com/item?id=42453599) (2024-12). The first question in the Langfuse launch thread was about OTel compatibility (Langfuse added it later). — "Are the Traces OpenTelemetry Compatible?"

### E. DX, ease of setup, UI quality (14)

- [HN](https://news.ycombinator.com/item?id=42442995) (2024-12). Was triaging tools and stopped at Langfuse because it was easy to run locally. — "simple enough to get set up locally with my existing stack for me to stop looking"
- [HN](https://news.ycombinator.com/item?id=46651042) (2026-01). Praises Langfuse's UI, DX and docs over the LangChain family. — "Langfuse is by far the best of the langs (-chain, -graph, -smith, -flow) in terms of UI/DX/integration/docs/quality."
- [HN](https://news.ycombinator.com/item?id=45399697) (2025-09). Found Phoenix clunky and Langfuse's Python decorator easier. — "I’ve found phoenix to be a clunky experience and have been far happier with tools like langfuse."
- [HN](https://news.ycombinator.com/item?id=50270821) (2026-10). Likes Phoenix's very simple architecture and tracing, and would use separate tools for evals, datasets and prompts. — "Its architecture is extremely simple. Tracing is nice, I didn't have any complaints."
- [HN](https://news.ycombinator.com/item?id=50295165) (2026-10). Recommends OpenObserve as lighter to run (a direct competitor to the "light self-host" pitch). — "OpenObserve is much simpler and lighter to run with all the features, including prompt management and evals."
- [HN](https://news.ycombinator.com/item?id=50270798) (2026-10). Langfuse and Logfire both limit custom graphs. — "both very similar, and at the same time limited when you want to customize graphs"
- [HN](https://news.ycombinator.com/item?id=46660289) (2026-01). Moved from Langfuse to Sentry, rating the software as mediocre. — "I personally migrated away from it to use Sentry, their software was honestly not that great."
- [HN](https://news.ycombinator.com/item?id=46656961) (2026-01). Trialled Langfuse for a recommendation and was put off by bugs and AI-written docs. — "I struggled a lot with actually implementing langfuse due to numerous bugs/confusing AI driven documentation."
- [HN](https://news.ycombinator.com/item?id=46657295) (2026-01). No LLM tracing tool has felt as good as general-purpose observability. — "I haven't used any tracing/monitoring tools for LLMs that made me feel like, say, Honeycomb does."
- [HN](https://news.ycombinator.com/item?id=42446881) (2024-12). A long-time OSS Langfuse user values the absence of dark patterns. — "By far the best solution. No dark patterns found in other projects such as Portkey."
- [HN](https://news.ycombinator.com/item?id=42443522) (2024-12). Open source, an OpenAPI spec and an unopinionated design drove the choice. — "their product being OSS is one of the reasons we chose it."
- [HN](https://news.ycombinator.com/item?id=42444670) (2024-12). Chose Langfuse over Phoenix because of Python dependency conflicts. — "ended up choosing Langfuse due to some versioning conflicts on the python dependency."
- [HN](https://news.ycombinator.com/item?id=46805185) (2026-01). Wants Claude Code traces in Phoenix and finds the official route heavy. — "it seems kinda overweight."
- [HN](https://news.ycombinator.com/item?id=46656691) (2026-01). Docs are a sore point. — "maybe clickhouse can finally make sense of the langfuse documentation"

### F. Cost and pricing (9)

- [HN](https://news.ycombinator.com/item?id=46558282) (2026-01). Willing to pay, but fears a "Datadog billing problem" because LLM logs are huge. — "it starts reasonable and then grows to a non-trivial percent of saas budget"
- [HN](https://news.ycombinator.com/item?id=46398376) (2025-12). LangSmith's prompt workflow is a lot of work and gets expensive quickly. — "a lot of work and gets quite expensive quite fast"
- [HN](https://news.ycombinator.com/item?id=41452994) (2024-09). Valued Datadog at work but is price-sensitive as an independent developer. — "now that I am on my own, I am much more price sensitive."
- [HN](https://news.ycombinator.com/item?id=41456518) (2024-09). Dislikes the $0 / $50 / Custom pricing pattern and wants small or usage-based tiers. — "What about a $5 or $10 plan… or maybe a sliding scale that you pay for what you use?"
- [HN](https://news.ycombinator.com/item?id=39374071) (2024-02). Sees per-call pricing of $500 for 50k calls as far too high. — "Its priced way high, 500$ for 50k LLM calls? 50k is not much at all."
- [HN](https://news.ycombinator.com/item?id=46014587) (2025-11) *(vendor)*. Notes that many teams overspend on LLM calls themselves, which makes cost visibility per feature valuable. Paraphrased: the post claims that 40–70% of agent calls don't need flagship models.
- [HN](https://news.ycombinator.com/item?id=47395801) (2026-03) *(vendor)*. Langfuse and Helicone show cost after the fact but not per feature. — "Langfuse and Helicone are retrospective — they show traces but don't roll up to \"feature X costs $0.12/invocation.\""
- [openobserve.ai](https://openobserve.ai/blog/datadog-vs-openobserve-part-3-traces-apm/) (2026) *(vendor, secondary)*. Reports that Datadog auto-enabled LLM Observability at about $120/day after seeing LLM span attributes. Paraphrased; not independently verified.
- [zenml.io](https://www.zenml.io/blog/langsmith-pricing) (2026) *(vendor, secondary)*. Per-trace billing grows faster than users because one RAG or agent request produces many traces. Paraphrased.

### G. Evals: usefulness and feature sprawl (14)

- [HN](https://news.ycombinator.com/item?id=47320733) (2026-03). Practice varies a lot; for most teams evals are an afterthought. — "It's definitely an afterthought for most teams although we are starting to see increased interest."
- [HN](https://news.ycombinator.com/item?id=47331845) (2026-03). Most AI companies judge models on vibes. The recommendation is to collect real traces and hand-curate a golden dataset from them. — "The vast majority of AI companies I talk to seem to evaluate models mostly based on vibes."
- [HN](https://news.ycombinator.com/item?id=47330325) (2026-03). Uses promptfoo, ad-hoc scripts and Langfuse, is unhappy, but won't invest because things change too fast. — "Definitely not happy with it, but everything is moving too fast to feel like it's worth investing in."
- [HN](https://news.ycombinator.com/item?id=47409778) (2026-03). Only needed "upload dataset, run, pass rate" from Braintrust and would vibe-code it next time. — "Braintrust makes this pretty easy, but If I was to do it again I would vibecode the same functionality."
- [HN](https://news.ycombinator.com/item?id=47333821) (2026-03). Braintrust datasets were a big win for classification consistency. — "Essentially a game changer in understanding if your prompts are working."
- [HN](https://news.ycombinator.com/item?id=44456615) (2025-07). Doesn't want to build trace-sampling auto-evals in-house. Lists Braintrust (closed), promptfoo and Laminar (open). — "I'm not sure I really want to vibe code something that does auto evals on a sample of my OTEL traces"
- [HN](https://news.ycombinator.com/item?id=47330421) (2026-03). A PM-labelled question set bootstrapped an LLM judge, and they now regression-test new models in about 20 minutes. — "it's wildly better than one-off testing and the regressions it brings."
- [HN](https://news.ycombinator.com/item?id=46656920) (2026-01). Questions why these products include prompt management at all. — "the \"Prompt Management\" part of these products always seemed odd. Does anyone use it? Why?"
- [HN](https://news.ycombinator.com/item?id=46659272) (2026-01). Langfuse is just logging and tracing; the extras feel investor-driven and optional. — "probably to keep investors happy but those are entirely optional sidedishes."
- [HN](https://news.ycombinator.com/item?id=46668124) (2026-01). Chose PromptLayer for git-like prompt versioning and Langfuse for tracing: best-of-breed tools. — "Langfuse seemed way better at tracing though."
- [HN](https://news.ycombinator.com/item?id=42449180) (2024-12). Sees prompt evaluation as large enough to need its own SaaS, not part of an observability tool. — "it is a big enough problem to have dedicated saas, rather than piggy back on observability saas"
- [hamel.dev](https://hamel.dev/blog/posts/evals-skills/index.html) (2026-03). An eval practitioner warns that tools push generic metrics and recommends building custom review interfaces. — "Eval tools often get in the way."
- [HN](https://news.ycombinator.com/item?id=50156309) (2026-10) *(vendor)*. Agent traces are huge and their failures subtle, so at scale an LLM has to read the traces. — "Trace payloads are huge, and errors are much more subtle"
- [HN](https://news.ycombinator.com/item?id=47769845) (2026-04) *(vendor)*. Today's tools collect traces and leave the analysis to the user. — "langsmith/langfuse/braintrust collect traces, and then YOU need to look at them and analyze them"

### H. Agents and coding-agent workflows (10)

- [HN](https://news.ycombinator.com/item?id=50151130) (2026-10). No proxy needed: Codex and Claude Code export OTel, which goes to a collector. — "Both Codex[0] and Claude Code[1] output OTEL traces that you can ingest and process."
- [HN](https://news.ycombinator.com/item?id=50148000) (2026-10). On subscription plans, scraping `~/.claude/projects` JSONL beats a proxy; the real value is collecting across the whole org. — "the best value comes from collecting agent sessions across the whole organization."
- [HN](https://news.ycombinator.com/item?id=50150706) (2026-10). Logs Claude Code traffic with a bare mitmproxy. — "Right now I run my Claude-Code sessions through a very simple mitmproxy, logging everything that goes in & out."
- [HN](https://news.ycombinator.com/item?id=49872081) (2026-09). People lose track of agent trajectories because the CLI presents them badly. — "you don't know what it's doing because the CLI is no good at presenting complex information."
- [HN](https://news.ycombinator.com/item?id=47244932) (2026-03). Teams hack SQLite + WebSocket dashboards or watch raw stdout. Langfuse is the self-hosted fallback. — "Most sane teams are either hacking together thin SQLite + WebSockets dashboards, or just sucking it up and staring at raw stdout"
- [HN](https://news.ycombinator.com/item?id=46805185) (2026-01). Wants Claude Code (Max plan, no API key) traces in Phoenix. — "I am looking for ways to connect my Claude Code using Max plan (no API) to it"
- [HN](https://news.ycombinator.com/item?id=50154093) (2026-10) *(vendor)*. OTel shows cost runaways only after the fact; the commenter wants real-time limits. — "What happens when an agent goes down some random rabbit hole and burns through $100 before you even notice?"
- [HN](https://news.ycombinator.com/item?id=47335583) (2026-03). Tools record what an agent did, not why it deviated from its plan. — "most tools record what happened (tool X was called, output was Y), but not why the agent deviated from the plan."
- [HN](https://news.ycombinator.com/item?id=47371748) (2026-03) *(vendor)*. Teams on different frameworks (LangGraph, CrewAI, raw SDK) leave a split audit trail. — "your audit trail is completely fractured."
- [HN](https://news.ycombinator.com/item?id=46659013) (2026-01). Would rather drive tasks through Claude than learn and log into another platform. This supports MCP/agent access over a dashboard. — "I’d rather connect Claude or whatever to do my task than have to learn a new platform let alone login to it."

### I. Consolidation and vendor durability (8)

- [HN](https://news.ycombinator.com/item?id=46657913) (2026-01). With OpenAI tracing, Logfire and PostHog moving in, standalone LLM observability is turning into a feature. — "they become more a feature than a product of its own."
- [HN](https://news.ycombinator.com/item?id=46659128) (2026-01). Shopping for a general observability platform and sees every vendor adding LLM support. — "it seems all vendors are working to add LLM observability one way or the other"
- [HN](https://news.ycombinator.com/item?id=46659149) (2026-01). Data platforms are absorbing observability (Snowflake–Observe, ClickHouse–Langfuse). — "The line between your data lake/analytics vendor and observability vendor is getting blurry."
- [HN](https://news.ycombinator.com/item?id=48288627) (2026-05). Procurement pain favours one platform (Cloudflare, which now covers a Helicone-like gateway). — "many companies have a painful procurement process."
- [hackernoon](https://hackernoon.com/two-acquisitions-deep-two-still-standing-whats-really-happening-to-open-source-llm-observability) (2026-08) *(secondary)*. Helicone went into maintenance mode after being acquired by Mintlify, with no promise about open source or self-hosting. — "Helicone's services will remain live for the foreseeable future in maintenance mode."
- [HN](https://news.ycombinator.com/item?id=46657100) (2026-01). Acquisitions are acceptable only if the project stays open source. — "fine as long as all these open source projects stay open source then good for them"
- [HN](https://news.ycombinator.com/item?id=46657230) (2026-01). Doubts the funding runway of small LLM tool vendors and calls the acquisition a likely "fire sale". — "it's more than likely this was a fire sale."
- [HN](https://news.ycombinator.com/item?id=46665896) (2026-01). Thinks AI observability is still a niche. — "AI observability is an even smaller niche technology."

## 3. Decision criteria (ranked by how often they come up as the reason a tool was picked or dropped)

1. **Open source and self-hostable, mostly for data control.** About 15 mentions. Reasons given: sensitive customer data, HIPAA/GDPR, "pull it in-house", no dark patterns, and acquisition and sovereignty risk. Often the first question about any new tool ("open source? can I self-host?").
2. **Time to first trace / works out of the box.** About 12. "Simple enough … to stop looking", Phoenix "only one that worked out of the box", Python decorator vs. manual instrumentation, dependency conflicts, tools that "did not even boot", installation bugs.
3. **Framework neutrality and OTel.** About 12. Tied to LangChain = rejected (LangSmith). An OTel-native backend can be swapped. OTel compatibility is the first launch-thread question, and semantic mismatches ("unknown" spans) are penalized.
4. **Operational weight and stability when self-hosted.** About 10, mostly in GitHub issues from teams that already chose. It decides *staying vs. leaving or not upgrading*: v2-on-Postgres users refused v3, idle CPU forced Langfuse off laptops, and teams run into migration failures and DR planning.
5. **UI/DX quality and docs.** About 9. Langfuse wins on UI/DX. Phoenix is called "clunky", Langfuse "not that great" by a switcher to Sentry, and nobody's tool feels like Honeycomb.
6. **Cost predictability.** About 7. The fear of a Datadog-style bill outweighs the absolute price; small developers want cheap or usage-based tiers.
7. **Performance at volume.** About 6 explicit, all from teams already at tens of GB or tens of millions of observations. It is rarely a selection criterion up front, but a top reason for pain later.
8. **Breadth of features (evals, prompt management, datasets).** About 6. These are split: some want everything under one roof, many treat them as optional extras or pick best-of-breed tools (PromptLayer, promptfoo, Braintrust).
9. **Vendor durability.** About 5, a 2026 theme after the Langfuse and Helicone acquisitions.

## 4. Counter-evidence (what cuts against a "one binary, self-hosted, fast" pitch)

- **ClickHouse-based stacks are accepted and liked by some.** One user is "all in" on ClickHouse, self-hosts Langfuse on it and says it "mostly just works", including locally in docker compose ([HN](https://news.ycombinator.com/item?id=50437777), 2026-11). Others: "A full observability stack is just a docker compose away" ([HN](https://news.ycombinator.com/item?id=45398725)), and storing OTel in ClickHouse "works great" ([HN](https://news.ycombinator.com/item?id=45409109)). One ClickHouse fan says agent evals are where it shines ([HN](https://news.ycombinator.com/item?id=46658044)).
- **The incumbent's stated reason for leaving the single-container design is credible.** The Langfuse maintainer said single container + Postgres "suffers from poor performance at scale, especially for analytical queries" ([HN](https://news.ycombinator.com/item?id=42445667)). A one-binary pitch has to answer this directly with numbers. Langfuse now markets v4 as "up to 165× faster", so the incumbent is also competing on speed.
- **"Light and simple" is already claimed.** Phoenix is described as having an "extremely simple" architecture ([HN](https://news.ycombinator.com/item?id=50270821)). OpenObserve is recommended as "much simpler and lighter to run … including prompt management and evals" ([HN](https://news.ycombinator.com/item?id=50295165)). Simplicity alone is not a unique position.
- **Many people are happy with SaaS.** "You shouldn't have to build all this yourself if you're willing to spend some money" ([HN](https://news.ycombinator.com/item?id=46557487)). "LangSmith is good" ([HN](https://news.ycombinator.com/item?id=46559212)). "Braintrust is great!" ([HN](https://news.ycombinator.com/item?id=47313926)). Enterprises reportedly use LangSmith and W&B Weave most ([HN](https://news.ycombinator.com/item?id=46659409)). One HIPAA user self-hosts only for regulated projects and pays for cloud otherwise ([HN](https://news.ycombinator.com/item?id=41701809)). Self-hosting is a compliance decision for a subset, not a universal preference.
- **Few people say speed is why they choose a tool.** Performance complaints come from teams already at large scale (tens of millions of observations, 44 GB+). When people pick a tool, setup ease, OSS/self-host and framework fit dominate. A Rust/"extremely fast" pitch in a launch thread drew no discussion of speed ([HN](https://news.ycombinator.com/item?id=42445576)). A reported Langfuse ingestion delay was dismissed by a maintainer as an old-SDK artefact ([HN](https://news.ycombinator.com/item?id=50274564)).
- **General observability stacks and DIY compete.** Some monitor agents with plain OTel + LGTM ([HN](https://news.ycombinator.com/item?id=47301651)). One user moved from Langfuse to Sentry ([HN](https://news.ycombinator.com/item?id=46660289)). Others would vibe-code their eval runner ([HN](https://news.ycombinator.com/item?id=47409778)) or build custom review UIs (hamel.dev). Several say the category is becoming a feature of larger platforms.
- **Langfuse has real goodwill.** Many happy reports ("dead simple", "becoming the standard", "best of the langs"), though some may be solicited. Displacing it needs a sharp reason, not parity.
- **Evals adoption is low.** If most teams run on "vibes" and evals are "an afterthought", an evals-heavy pitch may land with only a minority today.
- **AI observability is called a niche** ([HN](https://news.ycombinator.com/item?id=46665896)), and the market is consolidating: a standalone tool must explain why it will still exist (open source and no vendor dependence can be the answer).
