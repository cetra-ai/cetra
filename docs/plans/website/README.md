# Marketing site (cetra.sh)

The design is `docs/website/00-design.md`. It has fifteen milestones, M1–M15, each with its own plan written just before it starts. M6 (benchmarks) waits for the benchmark and is not planned yet.

| Milestone | Plan | Status |
|---|---|---|
| M1 Foundation | [`01-m1-foundation.md`](01-m1-foundation.md) | done (F1–F4) |
| M2 Homepage | [`02-m2-homepage.md`](02-m2-homepage.md) | done (H1–H3) |
| M3 Pricing, self-hosting, security | [`03-m3-pricing-selfhost-security.md`](03-m3-pricing-selfhost-security.md) | done (P1–P3) |
| M4 Integrations | [`04-m4-integrations.md`](04-m4-integrations.md) | done (I1–I2) |
| M5 Compare | [`05-m5-compare.md`](05-m5-compare.md) | done (C1–C2) |
| M6 Benchmarks | not planned: waits for the benchmark | — |
| M7 Launch | [`07-m7-launch.md`](07-m7-launch.md) | done (L1–L2) |
| M8 Main sync | [`08-m8-main-sync.md`](08-m8-main-sync.md) | done (S1–S7) |
| M9 Pricing feature table | [`09-m9-pricing-matrix.md`](09-m9-pricing-matrix.md) | done (X1–X2) |
| M10 Focused screenshots | [`10-m10-focused-shots.md`](10-m10-focused-shots.md) | done (Z1–Z2) |
| M11 Showcase trace | [`11-m11-showcase-trace.md`](11-m11-showcase-trace.md) | done (T1–T2) |
| M12 Hero scenarios | [`12-m12-hero-scenarios.md`](12-m12-hero-scenarios.md) | done (E1–E3) |
| M13 Voice story, trace list, issues, alerts | not planned yet | — |
| M14 Prompts, dashboards, queues, assistant | not planned yet | — |
| M15 Story screenshots and switcher | not planned yet | — |

## Branches

- The site is now one branch, `claude/website`, rebased onto main on 2026-11-22, with one PR. The stack #174–#180 is closed.
- Nothing is merged without the user's yes.

## Conventions for every phase

- Run everything from the worktree root. Node is not on the non-interactive PATH: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first.
- `pnpm install --frozen-lockfile` after pulling a phase that changed `website/package.json` (or `pnpm install` in the phase that changes it, which updates `pnpm-lock.yaml`).
- While iterating, run only what you touched: `pnpm --dir website exec node --test <files>`, `pnpm --dir website exec astro check`, `pnpm --dir website exec biome check <paths>`. Run `just check-docs` once, before the commit. Never run `just check` (the whole repository) and never run Rust builds: this track touches no Rust.
- Never kill a process you did not start.
- Commit with the phase's exact subject, no attribution line, one commit per phase.
- After writing files, run `pnpm --dir website exec biome check --write <the files you changed>`. Biome's formatting and import order win over code quoted in a plan; behaviour stays as quoted. Accepted Biome warnings: `noUnusedImports` and `noUnusedVariables` on frontmatter names used in Astro templates, and `noImportantStyles` in `base.css`. Errors are not accepted.
- Build with `pnpm --dir website exec astro build` (not `pnpm build`, which also regenerates the API docs).
- To look at pages, build first, then run `node /tmp/claude-1000/-home-sebastien-travaux-llm-observer--claude-worktrees-website-research-c9eb27/36c8a93a-58fa-4df8-91cd-fccb6be3c70f/scratchpad/tools/pageshots.mjs <worktree> <route> …`. It starts `astro preview` on port 4329, takes full-page screenshots at 1440 and 390 px (scrolling first so lazy images load), prints each PNG path with `ok` or `HORIZONTAL OVERFLOW`, and stops the server. Read the PNGs.
- **Never guess.** If the code differs from what a phase quotes, or a step is ambiguous, stop and end your turn with `QUESTION:` and the exact questions.
- Copy: second person, short sentences, no exclamation marks, no emoji. Avoid the words banned in `website/STYLE.md` (seamless, blazing, effortless, powerful, simply). Write "cetra" in lower case. No performance numbers anywhere (design §1, §9.2).
