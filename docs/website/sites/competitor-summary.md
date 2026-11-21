# Competitor summary: how the category pitches itself

## The common pitch
- **Headline words:** "agents", "observability & evals", "ship/deliver", "reliable/quality", "at scale", "open-source". Braintrust "Ship quality agents at scale" and Laminar "Ship reliable agents" are interchangeable; Helicone and MLflow are just as generic. Few headlines say what is different.
- **CTAs:** "Start free / Try free" ("no credit card") plus "Book a demo / Contact sales". Only Phoenix and LangWatch put "Self-host" in the hero buttons. Only Phoenix and MLflow show a run command in the hero. Only MLflow links a live demo.
- **Proof:**
  - logo walls (best: Braintrust, Langfuse, Phoenix);
  - GitHub stars in the nav (Langfuse 37.4k, MLflow 28K, Phoenix 12.3k);
  - adoption numbers (Langfuse 90B observations/month, Helicone 11.6B requests, MLflow 30M downloads/month);
  - SOC 2 / HIPAA badges and video testimonials.

  It is all adoption proof, never engine proof.
- **Product:** a realistic UI within 1–2 viewports, often as coded mocks (Braintrust, Laminar, LangWatch). Shared themes: an in-app AI investigator, MCP/CLI/skills for coding agents, coding-agent cost tracking, an AI gateway.

## Gaps nobody fills
1. **No one proves operational simplicity.**
   - Langfuse diagrams Web + Worker + Postgres + Redis + ClickHouse + S3 and calls Compose "not for production".
   - Helicone admits "more complex self-hosting" (Workers + ClickHouse + Kafka).
   - Laminar, LangWatch and Opik hide their stacks behind one line.
   - Phoenix and MLflow are light but never say so.
   - No site states RAM, disk or one-VM capacity.
2. **No reproducible performance numbers.** Braintrust benchmarks anonymous "Competitor A/B", and Laminar's "20x cheaper storage" is against an unnamed "Competition".
3. **Comparisons are self-serving or stale.** Helicone gives every rival ✗ and calls their UIs "Confusing". Only Langfuse's dated, worked-example "vs" page is credible.
4. **No live demo except MLflow.**
5. **OSS stories diluted by upsell funnels** (Phoenix → AX, Opik → Comet), or no OSS at all (Braintrust).

## White space for cetra
- **"The whole platform in one binary."** No Postgres, ClickHouse, Redis, Kafka or S3; production on one machine. Show it as their six-box diagram next to our one box.
- **"Fast at volume, and here is the script."** Named, dated, reproducible benchmarks: ingest rate, p95 query, RAM and disk per million spans on a stated VM. Langfuse's comparison page reports that LangSmith moved its cloud to a proprietary Rust + DataFusion engine. That design is proven; we make it open and single-box.
- **An engine proof strip** (binary size, startup, ingest rate, idle RAM) until we have logos.
- **Try in seconds:** a hero install command plus a public read-only demo, then honest per-competitor "vs" and "Switch from X" pages.
- **Show parity** (OTLP, Python and TS SDKs, evals) so "simple" never reads as "toy".
