# Bun (https://bun.sh)

## Hero
- **Headline:** "Bun is a fast JavaScript runtime & toolkit. All in one." ("fast" is in hot pink).
- **Subhead:** "Runtime, package manager, test runner and bundler in a single binary."
- **Primary CTA:** the install one-liner itself, not a button. It is a black code block with `curl -fsSL https://bun.sh/install | bash`, a copy button, macOS & Linux / Windows tabs and "View install script ↗". A link below reads "Then follow the quickstart →" (docs). The nav has a black "Install" button.
- **Secondary CTA:** inline links on "bun install" and "bun test" (to docs), and a top banner, "Bun has joined Anthropic…".
- **Hero visual:** a tabbed benchmark card (bun install / Express / Postgres / WebSockets) titled "Installing a Next.js app". It is a horizontal bar race (bun 0.21s with the mascot on its bar, against yarn 1.76s, pnpm 1.92s and npm 4.45s), with "replay in real time", the full methodology in small print and a "reproduce" link. Above the fold there is a "USED BY" logo strip: Anthropic, Cloudflare, Vercel, Cursor, Midjourney, X, Lovable, Tailwind, Replit, Prisma…

## Structure
1. Announcement bar, then the hero: the headline with the install one-liner on the left and the benchmark card on the right.
2. A "USED BY" logo strip (13 logos).
3. "Four tools, designed together": four cards (Runtime, Package manager, Test runner, Bundler), each with a starburst badge ("replaces Node.js / npm / Jest / Vite") and a command.
4. "One binary for the whole workflow": five numbered steps beside an autoplaying terminal ("Every step below is a real command with real output").
5. Latest release (v1.4.4): a big-number grid (286x, 3x, 7.8x, 3.2x) and a changelog bullet list.
6. "Bun in production": three video testimonial cards (Anthropic/Claude Code, Midjourney, Railway) and a large Cursor/frameworks card with a code sample.
7. "The fastest install in every situation you actually hit": a 6-scenario benchmark grid with RAM figures and "reproduce the benchmark".
8. "Everything built in. Nothing weighing it down.": peak-memory benchmarks against Node.js and Deno.
9. "The APIs you need, baked in": a tabbed code sample, then a standard-library index grid.
10. "Full speed, full-stack": three steps beside a real dev-server screenshot.
11. "Bun is a complete toolkit": a ✓/partial/✗ comparison table (Bun, Node.js, Deno; 23 rows, collapsible).
12. Black closing CTA, "Try it on the repo you have open right now.", with the install one-liner, Quickstart (pink) and "Other ways to install". Then the footer.

## Proof
- Benchmarks are the backbone, in three sections: the hero, install scenarios and memory. Each gives its methodology (hardware "EPYC 9R14", medians of 3) and a reproduce link.
- The logo strip sits directly under the fold.
- Named video testimonials from leaders at Anthropic, Midjourney, Railway and Cursor, each tied to a concrete use case ("Claude Code ships… as a Bun single-file executable").
- Release stats (286x faster than Prettier) show momentum.

## Product shown how
- A CLI product shown as real commands and output: an autoplaying terminal walkthrough (5 steps, hover to pause), about 80 code blocks, and one real screenshot of the HMR error overlay. The hero benchmark animates ("replay in real time"). There is 1 video element.

## Visual style
- **Type:** Archivo 800 for headings (68px, -1.36px tracking, very condensed and tight), system UI sans for body, monospace for labels and eyebrows ("A MINUTE WITH BUN").
- **Colours:** white #FFFFFF, near-black #0D0E0E/#222, and a single hot-pink accent around #F7248B (sampled). The four tool badges use pink, blue, green and yellow. The closing CTA block is black.
- **Mode and density:** light, crisp, grid lines and hairline borders, generous whitespace, information-dense cards.
- **Signature devices:** the mascot riding the winning bar, starburst "REPLACES X" stickers, mono eyebrows with pink square bullets, grey second line in headlines ("Adopt one, or all of them.").

## Pricing / comparison pages
- Free and open source, so there is no pricing page.
- Comparison lives on the homepage (the Bun/Node/Deno table and benchmarks).
- The captured "benchmarks" page (pages/bun-benchmarks) is actually the docs page "Benchmarking: How to benchmark Bun". It is a methodology guide (tools like mitata, oha, hyperfine; memory and heap profiling), not a marketing benchmark page. It still builds trust: "The source code for all of Bun's public benchmarks is in the /bench directory".
- Docs entry: "Docs" is first in the nav, with a ⌘/ search bar in the header and "Ask AI" in the docs.

## What makes it sell
- The hero answers "why switch?" with a number (0.21s vs 4.45s) before you read a sentence.
- Benchmarks are honest-looking: they state the scenario, hardware and medians, and offer "reproduce". They show multiple realistic scenarios, not one cherry-pick.
- The install command is the CTA, which removes friction for developers.
- Incremental-adoption framing ("Adopt one, or all of them"; "drop into existing Node.js projects") de-risks the switch.
- Testimonials name a person, a company and a concrete production use.
- A single accent colour plus heavy condensed type gives it a confident, engineered feel.

## Weaknesses
- The page is long (11,700px) with three separate benchmark sections, which feels repetitive.
- The comparison table is collapsed after 6 rows, which hides the strongest differentiation.

## Steal for cetra
- A hero benchmark card with tabs for different workloads (ingest spans/s, trace query p95, RAM at 1M spans, disk per 1M spans) against Langfuse and Phoenix stacks, with methodology and a "reproduce" link.
- The install one-liner as the primary CTA (`curl … | sh` / `docker run`), with OS tabs and "view install script".
- "One binary for the whole workflow": numbered steps beside an autoplaying terminal (install → run → send OTLP → open UI → run eval).
- A "replaces X" framing for components (replaces ClickHouse + Postgres + Redis + S3 + workers).

## Scores
- Beauty 5/5 — crisp type, one accent colour, beautifully designed charts.
- Selling 5/5 — numbers, a one-line install and named production users.
- Clarity 5/5 — headline plus benchmark answer what and why instantly.
- Proof 5/5 — reproducible benchmarks, logos, video testimonials.
- Product-showing 4/5 — terminal demos are great; little UI to show.

## Best screenshots
- caps/bun-hero.jpg — headline, install one-liner as CTA, tabbed benchmark race, logo strip.
- caps/bun-part01.jpg — "replaces X" tool cards and the autoplaying terminal walkthrough.
- caps/bun-part02.jpg — video testimonials and the 6-scenario install benchmark grid.
- caps/bun-part04.jpg — Bun/Node/Deno comparison table and the black "try it now" closing CTA.
