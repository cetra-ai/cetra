# Relax by Makepad (https://makepad.github.io/relax/)

## Hero
- **Banner above the nav:** "Experimental · Relax is for development iteration. Use Rust itself (`cargo build --release`) for any release build."
- **Eyebrow (mono, green dot):** "A LIVE COMPILER AND JIT RUNTIME FOR RUST, WRITTEN IN RUST"
- **Headline (verbatim):** "Rust without the wait." Set in a huge 138px weight-800 system font with very tight tracking. "wait." carries a teal→blue→violet gradient and an underline.
- **Subhead (verbatim):** a two-line contrast, the first line in grey and the second in white: "rustc builds your program, then you run it. / Relax runs your program while it builds it." A paragraph explains the mechanism: no rustc, LLVM or linker; compiles each function on first call; optimises hot ones; patches edits into the running process.
- **Primary CTA:** "GitHub ↗" (teal gradient button) → github.com/makepad/relax. **Secondary:** "Try it" (outlined) → `#try` (a `git clone` + `cargo build` section).
- **Hero visual:** a small card, "THE SAME PROGRAM, TWO TOOLCHAINS", with two schematic bars (rustc + Cargo "build, then run" vs Relax "run while it builds") and the caption "Schematic, not to scale. The measured race is below." Beneath sits a four-stat strip: **11 s** (large 3D game, clean checkout → first frame, M4 MacBook), **1.15 s** (second run), **2.2 s** (save → 92 tests re-run), **0** (crates.io dependencies in 438k lines).

## Structure
1. Experimental banner, nav with section anchors ("vs rustc", "Tradeoffs", "Benefits", "Try it", "What it took", "In Makepad", "Where LLVM wins"), a 中文 toggle, a theme toggle and GitHub.
2. Hero with the headline, contrast subhead, schematic card and stat strip.
3. "Relax vs rustc" scoreboard: an animated race on one shared time axis (Relax 11 s vs rustc ~3 min, with a Replay button), then cards for the second run, cold small app, tests on save and hot patching, and rows comparing type checking, optimised code and what each toolchain needs.
4. "What a JIT costs you.": seven honest cost cards (slower optimised code with exact geomeans, more memory, checking not faster yet, some edits need a restart, known gaps, not a release build).
5. "What a JIT gives you.": nine benefit cards, each with real CLI commands (hot patching, tests on save, pause on panic, breakpoints, profiling, flight recorder, leaks), then the interactive "Saving is the build" demo: an editable code pane next to a running program that keeps its state on save.
6. "Build it, run a package.": exact commands with timings and the platforms tested.
7. "A new Rust toolchain, from source bytes to machine code.": a pipeline table (what rustc does vs what Relax does per stage) with big stats (438k lines, 0 dependencies, 134k lines of core, alloc and std compiled by Relax).
8. "Relax inside makepad run.": how it powers their framework, with seven capability cards.
9. "The loop that stays hot.": side-by-side rustc vs Relax timings for agent workloads, plus an "In practice" stat row (Nine agent lanes, 300+ builds an hour, Dozens of edits).
10. "Relax vs rustc, on real workloads.": a per-workload results chart (21 workloads, ratio bars around 1.00×) with x64/arm64 and stock/native `--release` toggles, the headline 1.12× geomean, "3 of 21" workloads where Relax is faster, and a method footnote: machines, rustc and LLVM versions, median of 3 rounds, measured on a stated date.
11. "Where LLVM still wins.": geomean details, which `--release` was compared, and "A function's life" (interpreter → O1 → O3).
12. Closing repeat of "Rust without the wait." (cut off in the capture at 14,000px of 16,900px).

## Proof
- **Engine proof only, and lots of it:** every stat has a unit, a scenario and the hardware (M4 MacBook; x64 and arm64 machines for the workload chart).
- A race animation labelled as time-compressed, and a schematic labelled as not to scale.
- Self-reported losses: 1.12× slower on x64 and 1.24× on arm64 (geomean of 21 workloads), more memory, known gaps.
- Dogfooding numbers from their own agents (lanes, builds per hour).
- No logos, testimonials or star counts. It is an experimental project.

## Product shown how
- In the hero: numbers and a schematic, not UI. Relax is a compiler, so the "product" is time.
- An animated, replayable race in the second section.
- An interactive edit-and-patch demo ("Saving is the build"), simulated in the page.
- 211 code blocks: the real commands to run, inspect and profile.

## Visual style
- **Type:** system-ui at every level (h1 138px / 800 / −7.6px tracking); monospace for eyebrows, labels and code. No custom typeface.
- **Colour:** near-black #06070B, slate cards with 1px borders, white and grey text; a teal→blue→violet gradient (around #5EE6C8 → #7AA2FF → #A78BFA) for the hero word and primary button; orange-brown for the rustc lanes against teal for Relax; amber experimental banner. A light theme toggle exists.
- **Density:** high. About 16,900px, almost every section built from the same bordered dark cards.
- **Motion:** one canvas, an animated race with replay, and animated ratio bars.
- **Signature devices:** two-tone headings (white claim, grey continuation), rustc-vs-Relax paired lanes in two fixed colours, mono uppercase eyebrows, inline "(tradeoffs)" links next to every bad number.

## Pricing / comparison pages
- No pricing (an open-source experiment).
- The whole page is a comparison against the incumbent (rustc + Cargo). "Tradeoffs" and "Where LLVM wins" are in the main nav.
- Docs entry point: GitHub README and the "Try it" anchor.

## What makes it sell
- A six-word headline that names the pain and the difference, followed by a two-line before/after contrast.
- A stat strip under the hero where every number carries its scenario and hardware.
- The race on a shared time axis turns "11 s vs ~3 min" into something you watch, and says it is time-compressed.
- The costs come before the benefits. Admitting 1.12×/1.24× slower code makes every other claim believable.
- A per-workload chart with geomean, machine specs and an architecture toggle reads like a paper's results table, down to the dated method footnote.
- An interactive demo of the one trick (edit → running program changes, state kept).

## Weaknesses
- No visual identity: system font, uniform dark cards and a gradient word look like a well-built template rather than a brand.
- Too long and repetitive: the speed story is told three times (scoreboard, loop, real workloads).
- The try path is `git clone` + `cargo build`, and there is no social proof.

## Steal for cetra
- Headline + two-line contrast: "their stack does X, cetra does Y."
- A stat strip under the hero with scenario and hardware on every number, then a race on one time axis against the multi-service stack.
- A "Tradeoffs" section and a "Where Langfuse / Phoenix still win" section linked from the main nav.
- A per-workload benchmark chart with geomean, machine specs and a method link.

## Scores
- Beauty 3/5: striking giant headline, but system font and uniform cards.
- Selling 4/5: relentless measured claims; weak try path, no social proof.
- Clarity (5-second test) 5/5: headline plus contrast line land instantly.
- Proof 5/5: hardware-stated numbers, published losses, per-workload chart.
- Product-showing 4/5: replayable race and interactive patch demo.

## Best screenshots
- caps/relax-hero.jpg: giant headline with gradient word, two-line contrast, schematic card, experimental banner.
- caps/relax-part01.jpg: stat strip and the "Relax vs rustc" scoreboard race on one time axis.
- caps/relax-part02.jpg: "What a JIT costs you" before "What a JIT gives you", and the interactive "Saving is the build" demo.
- caps/relax-part05.jpg: per-workload ratio chart and "Where LLVM still wins".
