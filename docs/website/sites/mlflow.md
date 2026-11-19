# MLflow (https://mlflow.org)

## Hero
- **Headline (verbatim):** "Deliver High-Quality AI, Fast"
- **Subhead (verbatim, short):** "Building AI products is all about iteration. MLflow lets you move 10x faster by simplifying how you debug, evaluate, and monitor your LLM applications, Agents, and Models."
- **Primary CTA:** "Try Demo" (white) → `demo.mlflow.org/#/experiments/1`. This is a **live public demo instance**, the only one among the competitors.
- **Secondary CTA:** "Get Started" (outlined) → `#get-started` on the same page.
- **Below the CTAs:** "New: let your coding agent set up MLflow tracing for you", with a copyable `curl -LsSf https://mlflow.org/wizard/setup.sh | sh`, and three proof pills (Linux Foundation, 28K+ Stars, 30M+ Downloads/mo).
- **Hero visual:** no product. A full-bleed vertical-stripe gradient (magenta → red → violet → blue) behind centred text.

## Structure
1. Nav (Components, Releases, Blog, Docs, Resources, GitHub 28K, Get Started, Try Demo).
2. Gradient hero.
3. Greyscale logo marquee (Microsoft, Meta, MosaicML, Zillow, Toyota, Booking.com, Wix).
4. "LLMs & Agents | Model Training" tabbed feature list (Observability, Evaluation, Prompts & Optimization, AI Gateway, Agent Server), each with Quickstart and Try Demo, next to a sticky trace screenshot. Scroll-reveal left the lower items faded in the capture.
5. "Most Adopted Open-Source AIOps Platform": Linux Foundation, repo stars, 30 Million+ downloads/month.
6. "Works With Any Framework": an 18-tile logo grid.
7. "Why Teams Choose MLflow": six tiles.
8. "Get Started in 3 Simple Steps": `uvx mlflow server` (~30 s), `mlflow.openai.autolog()` (~30 s), run code (~1 min).
9. FAQ, blog cards, community links, footer.

## Proof
Linux Foundation backing, 28K stars and 30M+ downloads/month, all in the hero. A big-brand marquee right below. Note "20K+ GitHub stars, 900+ contributors" in the Community tile contradicts the 28K above. No testimonials, no case studies, no performance numbers.

## Product shown how
A **live demo link is the primary CTA**. One real trace screenshot sits beside the feature list (about 1.3 viewports down). Four code blocks: the hero curl and the 3-step quick start with time estimates per step. No video.

## Visual style
DM Sans throughout (h1 52px, **weight 300**, -2% tracking): light and airy. Near-black slate (≈ #12161B) body, with a saturated magenta/red/blue striped gradient hero; the gradient echoes in the footer. Medium-low density, centred and symmetric. Scroll-fade motion on features.

## Pricing / self-host / comparison pages
No pricing (fully free, Apache 2.0; managed versions come from vendors, not linked here). Self-host is the default: `uvx mlflow server`, "Docker setup is also available". No footprint shown; competitors.md lists an SQL DB (SQLite/PG/MySQL/MSSQL) plus an artifact store, optionally Redis. The FAQ asks "How does MLflow compare to other LLMOps/MLOps tools?" (collapsed). No vs pages linked.

## What makes it sell
- Instant try: a live demo as the primary CTA, and a one-command server with time estimates.
- Institutional trust: Linux Foundation, 30M downloads, big brands.
- Clear "fully open source, forever free" story with no paid tier to upsell.

## Weaknesses we can exploit
- **The headline is generic** ("Deliver High-Quality AI, Fast"), and the page reads as MLOps-first (Model Training tab, Model Registry nav). LLM observability is one of many jobs.
- **No speed or scale claims at all;** it is a Python server on an SQL DB. "Fast at volume on one machine" is open ground.
- **Inconsistent numbers** (20K vs 28K stars) and almost no customer voice.

## Steal for cetra
- A public read-only live demo instance as a hero CTA.
- A 3-step quick start with a time estimate per step ("~30 seconds").

## Scores
- Beauty 3/5: striking gradient hero, plain template below.
- Selling 3/5: strong try-it paths, weak differentiation.
- Clarity (5-second test) 3/5: generic headline; "LLM" only in the subhead.
- Proof 4/5: Linux Foundation, 30M downloads, big brands.
- Product-showing 4/5: live demo primary CTA, but few screenshots on the page.

## Best screenshots
- `caps/mlflow-hero.jpg`: stripe-gradient hero with Try Demo, coding-agent curl and proof pills.
- `caps/mlflow-part02.jpg`: Linux Foundation and 30M downloads band, framework grid, "Why Teams Choose MLflow".
- `caps/mlflow-part03.jpg`: 3-step quick start with time estimates and the FAQ.
