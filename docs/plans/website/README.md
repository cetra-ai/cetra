# Marketing site (cetra.sh)

The design is `docs/website/00-design.md`. It has seven milestones, M1–M7, each with its own plan written just before it starts. M6 (benchmarks) waits for the benchmark and is not planned yet.

| Milestone | Plan | Status |
|---|---|---|
| M1 Foundation | [`01-m1-foundation.md`](01-m1-foundation.md) | done (F1–F4) |
| M2 Homepage | `02-m2-homepage.md` | — |
| M3 Pricing, self-hosting, security | `03-m3-pricing-selfhost-security.md` | — |
| M4 Integrations | `04-m4-integrations.md` | — |
| M5 Compare | `05-m5-compare.md` | — |
| M6 Benchmarks | not planned (waits for the benchmark, design §9.2) | — |
| M7 Launch | `07-m7-launch.md` | — |

## Branches

- Each milestone is implemented on its own branch, `claude/website-m<N>`, cut from the previous milestone's branch (M1 from `claude/website-design`). Each branch has a PR whose base is the previous branch.
- Nothing is merged without the user's yes.

## Conventions for every phase

- Run everything from the worktree root. Node is not on the non-interactive PATH: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first.
- `pnpm install --frozen-lockfile` after pulling a phase that changed `website/package.json` (or `pnpm install` in the phase that changes it, which updates `pnpm-lock.yaml`).
- While iterating, run only what you touched: `pnpm --dir website exec node --test <files>`, `pnpm --dir website exec astro check`, `pnpm --dir website exec biome check <paths>`. Run `just check-docs` once, before the commit. Never run `just check` (the whole repository) and never run Rust builds: this track touches no Rust.
- Never kill a process you did not start.
- Commit with the phase's exact subject, no attribution line, one commit per phase.
- **Never guess.** If the code differs from what a phase quotes, or a step is ambiguous, stop and end your turn with `QUESTION:` and the exact questions.
- Copy: second person, short sentences, no exclamation marks, no emoji. Avoid the words banned in `website/STYLE.md` (seamless, blazing, effortless, powerful, simply). Write "cetra" in lower case. No performance numbers anywhere (design §1, §9.2).
