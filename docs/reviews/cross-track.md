# Cross-track consistency review

Date: 2026-11-14. Base: `main` at `0fb6b0a`, track R's branch `claude/r-hardening-design-43eb31` (design and M1 plan), and the control-plane docs (`cetra-control` `docs/00-design.md`, `01-m1-first-cell.md`, `decisions.md`).

This review looks only at the **seams between tracks**: contracts, decisions, hooks and behaviours that two or more tracks must agree on. Each plan already had its own review. Nothing in the plans, spec or decisions was changed here; each finding names the track that should make the fix.

## Summary

Six reviewers (shared contracts; decisions and spec; order and ownership; data lifecycle; cloud and licensing; process) reported 66 findings. An adversarial verifier read both sides of every finding. None was refuted: 59 were confirmed and 7 are plausible. After merging duplicates, **38 findings** remain:

| Severity | Count | Of which plausible |
|---|---|---|
| blocker | 1 | 0 |
| major | 16 | 1 |
| minor | 21 | 6 |

By owner: Q 6, P 5, I 2, N 2, M 4, R 5, coordinator 14.

The main themes:

1. **Q is out of sync with llm-observer.** Q's requests (R1–R18) are answered in P's and N's plans, but Q's docs were not updated (X1–X5, X28 below), and several requests have no owner on the llm-observer side (X25). The one blocker is here: Q's cell config uses a key that N decided not to add.
2. **Hooks P promised to other tracks no longer match the callers.** The single `QueryOptions` builder, the retention-cap hook and the rule-limit hook are each described differently by P, I and M (X7, X8, X10).
3. **Lifecycle edges between M, N, P and I.** Usage rows deleted on org drop that billing still needs (X9), a judge call paid for and then retried at the units cap (X12), restore ordering (X15), and an erasure promise that `gc.grace` now breaks (X26).
4. **Track R was designed without N's M2 and P's M4**, which edit the same storage files (X20).
5. **Process**: migration numbers, changelog lines, decision IDs and the roadmap keep drifting because nothing enforces them (§ Systemic process fixes).

### Top findings

1. X1 (blocker, Q): the cell config writes `storage.gc.object_retention`; N's D-N-20 uses `storage.gc.grace` and unknown keys are a startup error, so Q's phase 10 gate can never pass.
2. X25 (major, coordinator): Q's M2 depends on R5, R7, R13, R16, R17 and R18, which have no owning track, milestone or roadmap row.
3. X9 (major, P with M): an org drop deletes `usage_daily`, but Q's R5 billing and P's usage export need those rows for at least 40 days.
4. X12 (major, I): at the units cap the score write answers `402 quota_exceeded` after the judge call, and I's executor retries the job, paying for the judge again each time.
5. X7 (major, P): D-P-21's "only `open_query_on` builds `QueryOptions`" test would break I's and M's direct `storage.query` callers, which need `QueryError`; the plan window can leak through any site left out.

### What could not be checked

- Plans not yet written: P's M4/M5, I's M5/M6, Q's M2–M6 and R's M2–M8. Hooks they are supposed to provide were judged from the designs only.
- Q's 2 000-line M1 plan was read only where it touches cetra (config, image, env, backups, restore).
- The web error handling of `402 quota_exceeded` and `403 plan_limit` across C, H, K, L and I, and `fixtures/api` against `openapi.json`, were not compared in detail.
- What sqlx does on duplicate migration versions was not tested; collisions are inferred from merge history (renumber merges `68937f2`, `c5a09d9`, `355a602`, `60deeff`).
- Track R's branch was available locally and was read; it is not on `main` yet.

Each finding below lists the reviewer ids it merges, the verdict, and the references on both sides of the seam. Paths without a prefix are in this repository; `cetra-control docs/` is the private control-plane repository.

## Owner: Track Q

### X1. Q's cell config uses `storage.gc.object_retention`, which N's M2 replaced by `storage.gc.grace`

- **Severity:** blocker · **Verdict:** CONFIRMED · **Owner:** Q · **Tracks:** A1, N, Q
- **Reviewer ids:** SC-1, DS-1, OW-4, LC-7, CL-1
- **References:**
  - cetra-control `docs/`00-design.md line 40 (D-Q-12), line 149 (R10), line 189
  - cetra-control `docs/`01-m1-first-cell.md line 13 (Gates), lines 1592-1616 (Phase 10 Precondition and cetra.toml.tmpl)
  - docs/plans/n-packaging/m2/00-design.md § S3 object retention: raise `gc.grace`'s cap (D-N-20), lines ~234-242
  - docs/mvp-decisions.md line 561 (D-N-20)
  - docs/spec/12-operations.md § Configuration (line 5: unknown keys are an error) and § Backups 'Online backups' paragraph

**Problem.** Q's design and M1 plan still request R10 as a new key `storage.gc.object_retention`. N's M2 (user-chosen D-N-20) rejects that key and instead raises the cap of the existing `storage.gc.grace` from 24 h to 30 days. Spec 12 says unknown config keys are a startup error. Q's phase 10 precondition adds `object_retention = "8d"` to the cell toml and stops with "blocked on R9/R10" when `just smoke` reports an unknown key.

**Failure scenario.** After N's M2 merges, the R9 check passes (`admin backup --help`), but the cell toml with `[storage.gc] object_retention` fails config parsing, so the R10 gate can never pass. Phases 10 and 11 stay blocked although the feature exists as `grace = "8d"`. If the key were ignored instead, the cell would keep the 5 min default grace and the 'last 7 nightly backups restore' guarantee in D-Q-12 would be false.

**Fix.** In Q, mark R10 as answered by N's D-N-20 (`storage.gc.grace`, max 30d). The template becomes `[storage.gc] grace = "8d"`. The R10 gate becomes `just smoke` passing with that key, since values over 24h are refused before N's M2. Update D-Q-12 and decisions.md to match. Note in D-Q-12 and the runbook that media blobs are not covered: N's manifest says missing media can be reported, but traces are not lost. Also note D-N-20's privacy point: erased data stays up to `gc.grace`. That fits Q's 23-day total.

**Verifier.** Q still asks for the key. 00-design.md D-Q-12 (line 40) and R10 (line 149) name `storage.gc.object_retention`, and the 01-m1-first-cell.md Gates (line 13) and Phase 10 (lines 1592-1616) write `object_retention = "8d"` and stop on an unknown key. N rejects it. n-packaging/m2/00-design.md § S3 object retention (D-N-20, line 236) says "Spec 12's `gc.object_retention` is not added" and "Q sets `gc.grace = \"8d\"`", and mvp-decisions D-N-20 (line 561) and spec 12 § Online backups say the same. Spec 12 § Configuration line 5 says "unknown keys are an error", so the R10 gate can never pass.

### X2. Q's restore runbook and drill skip `gc-orphans` and the backup manifest

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** Q · **Tracks:** A1, N, Q
- **Reviewer ids:** SC-2, CL-5, OW-4
- **References:**
  - cetra-control `docs/`01-m1-first-cell.md Phase 11 (line 1812+), docs/runbooks/cell-restore.md 'same server' steps 1-7 and 'new server' steps 1-4
  - docs/plans/n-packaging/m2/00-design.md § Restore (D-N-21) steps 1-5, and the manifest `restorable_until` field (line ~220)
  - docs/mvp-decisions.md line 562 (D-N-21)
  - docs/plans/a1-storage/00-design.md line 488
  - docs/spec/12-operations.md § Backups, 'Online backups' paragraph

**Problem.** Q's restore runbook and drill skip N's mandatory `gc-orphans` step. They also start the server once (to migrate) before any orphan or missing-object check, while N runs `gc-orphans` before the first start. The snapshot age limit is hard-coded at 7 days instead of read from `manifest.json` `restorable_until`.

**Failure scenario.** The server is lost and restored from a 5-day-old meta-only backup. Every Parquet file written in the 5 days is now an orphan in the bucket and is never collected, so the bucket grows forever. If the backup was past `gc.grace`, the cell starts with catalog rows pointing at deleted objects and queries fail at read time instead of at restore time. The drill (a copy of the bucket) never exercises either path.

**Fix.** In Q's cell-restore.md (both the same-server and new-server paths), add a step after the restore and before any start. It runs `docker compose run --rm --no-deps cetra admin gc-orphans --dry-run`, then the real command, and requires zero missing data files, with a warning that this deletes objects written after the snapshot. Choose the snapshot by `manifest.json` `restorable_until`. Have restore-drill.sh run `gc-orphans` against the drill bucket. Add the `gc-orphans --help` check to the Phase 11 precondition.

**Verifier.** Q's cell-restore runbook (01-m1-first-cell.md Phase 11, around lines 1950-1960) goes: stop, move the data dir aside, restic-restore meta.db and catalog.db, start once to migrate, reapply-deletions, up. It never runs `gc-orphans` and never reads `manifest.json`. It hard-codes "no older than 7 days". N's § Restore (D-N-21) step 3 runs `cetra admin gc-orphans` before the first start, with the server stopped. That step deletes objects the restored catalog does not know ("In S3 mode this is a rollback of the live bucket") and exits 1 on missing data files. Spec 12 § Online backups and A1 00-design line 488 say the same. Q's step 4 also starts the server before any orphan check, the reverse of N's order.

### X3. Q's 30-day 'keep the old window' workaround assumes R12 is unbuilt; P built it

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** Q · **Tracks:** M, P, Q
- **Reviewer ids:** CL-3
- **References:**
  - cetra-control `docs/`00-design.md D-Q-23 (line 61, Defaults), D-Q-26 (line 69), R12 (line 151), M3 row (line 184), Cell config row (line 129)
  - docs/plans/p-entitlements/00-design.md D-P-22 (line 68), D-P-30 (line 88), § Milestones M2/M4 rows (lines 229, 231), § Open items (line 241)
  - crates/server/migrations/meta/0008_entitlements.sql (org_window_ends)
  - docs/mvp-decisions.md D-P-30

**Problem.** Q's D-Q-26 is written as a stopgap 'until P fixes this (R12)', but P has already taken R12 as D-P-30: the history is in M2, and its use in retention comes in P's M4, on which Q M3 already depends. Q's documents do not record this, and Q M3 still allows the workaround. If the workaround ships alongside D-P-30, a downgraded or unpaid org keeps its paid window visible for 30 days, and deletion lands at about 60 days. The defaults part is weaker than the reviewer claims. Using Free without `data_access_days` is a deliberate Q choice: it protects data beyond D-P-30's 30-day history if renewal fails for longer. It is not wrong in itself, but it contradicts P § Open items ('the instance defaults of a cloud cell are its Free plan'), and D-Q-23's stated reason (deleted 'within the hour') is no longer true with D-P-30. Lapsed orgs would see unlimited history, which needs an explicit decision.

**Failure scenario.** A Pro org (1 825 days) fails payment. Q follows D-Q-26 and keeps `data_access_days: 1825` in its tokens for 30 days: the org keeps reading 5 years of history on Free for a month. Then Q lowers to 30; P's history keeps the 1 825-day window for another 30 days before M deletes. The org also keeps the Free window of 30 days only if a token exists: a lapsed or token-less org on the cell reads unlimited history, not Free's 30 days.

**Fix.** Q: mark R12 as met by D-P-30 (P M4), drop the keep-previous-value rule from D-Q-26 and the 'or the D-Q-26 workaround' alternative from the M3 dependencies, and update the downgrade page text. Then either keep Free-without-window defaults with the reason restated (protection beyond 30 days of lapse, unlimited visibility accepted), or switch to the full Free plan. Tell P either way so P's § Open items line matches. P: in M4, state the combined delay (hidden at the shrink, deleted 30 days later) for Q to quote.

**Verifier.** P's D-P-30 (00-design.md:88, 'user; Q's R12') takes R12: org_window_ends exists in migration 0008_entitlements.sql (P M2). Retention will use the largest window of the last 30 days, the expired token's window included, once M4 passes it to M. Q still lists R12 as open (00-design.md:151) and keeps the D-Q-26 workaround (line 69, 'every token that lowers `data_access_days` keeps the previous value for 30 days'). Q's M3 depends on 'R12 (or the D-Q-26 workaround)' and on P's M4, which delivers R12. If both apply, data is deleted about 60 days after a downgrade, and the paid window stays visible for 30 days after the downgrade, against D175.

### X4. Q assumes N's image is a drop-in for the interim one (uid, `/backups`, distroless)

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** Q · **Tracks:** N, Q
- **Reviewer ids:** CL-6
- **References:**
  - cetra-control `docs/`00-design.md line 36 (D-Q-8), line 147 (R8)
  - cetra-control `docs/`01-m1-first-cell.md Phase 2 (line 185, Dockerfile 264-269), lines 1112-1113, 1858-1861, 1911, cell-restore step 2-3
  - cetra-control `docs/`runbooks/cell-upgrade.md § New cetra version
  - docs/plans/n-packaging/04-image.md (nonroot distroless, USER 65532, ENTRYPOINT /usr/local/bin/cetra)
  - deploy/docker/Dockerfile lines 13,16
  - docs/plans/n-packaging/m2/00-design.md § Trigger (line ~109: backup dir writable by uid 65532)
  - docs/spec/12-operations.md line 70 (distroless)
  - side-session-notes n-requests-for-q.md line 21

**Problem.** Q's M1 scripts and runbooks hard-code uid 10001 for `/srv/cetra/data` and `/srv/cetra/backups`, and use a Debian-based image with `rm`, `chown` and `curl`. N's image runs as 65532 and, being distroless, has none of those tools. Q's design cites spec 12 for uid 10001, but spec 12 says 65532. N told Q about the uid, but no Q document carries a switch step.

**Failure scenario.** After Q points `versions.env` at N's image, the host's backup dir is owned by 10001, so the server (65532) cannot create `/backups/<stamp>` and every nightly `cetra admin backup` fails; the restore drill's `docker run --entrypoint chown/rm` fails on the distroless image; the restore runbook creates a data dir owned by the wrong uid and the server cannot open it.

**Fix.** Q: add a 'switch to the official image' step to the upgrade runbook and phase scripts: chown `data` and `backups` to 65532:65532 once, replace `--entrypoint rm/chown` helper containers with a pinned utility image (or host `sudo` commands), parametrize the uid in `render.sh`, `deploy.sh`, `restore-drill.sh` and the runbooks, and fix the spec 12 citation. N: nothing, provided its README keeps the uid/`/backups` notes.

**Verifier.** Q's interim Dockerfile (01-m1 lines 264-269), the host setup at 1112-1113, the drill at 1858-1861 and 1911, and runbook step 2 all hard-code uid 10001 and use `--entrypoint rm/chown`. Phase 2 says this follows spec 12 § Packaging ("non-root uid 10001"), but spec 12, N's 04-image.md and deploy/docker/Dockerfile on main use distroless static with USER 65532:65532. N's note to Q (n-requests-for-q.md R8) flags the uid and the missing /backups, and Q's cell-upgrade.md has no step for switching images.

### X5. Q's request table is stale against what P adopted

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** Q · **Tracks:** P, Q
- **Reviewer ids:** CL-8
- **References:**
  - cetra-control `docs/`00-design.md line 61 (D-Q-23 pacing), lines 136-157 (Requests table)
  - docs/plans/p-entitlements/00-design.md D-P-26..D-P-31, D-P-33, D-P-39, D-P-40
  - docs/plans/p-entitlements/03-m3-units-and-limits.md lines ~534-544
  - crates/token/src/keys.rs (cloud-1, cloud-2)

**Problem.** R1, R2, R3, R4, R6, R12, R14 and R15 are all adopted by P (R1-R4 and R15 already on main), but Q's design still says none is planned and still builds around the limits R15 removes. P also fixed details Q's text does not state: `control_plane` tokens carry one `scope` string, instance admins are refused by the user endpoints, and unknown subjects return 404.

**Failure scenario.** The Q M2 plan is written from Q's design, not from P's: it paces pushes at 4 per minute for no reason, treats 'unknown subject' as success, or expects `users/disable` to protect the last owner, and the control plane's behaviour drifts from the cell's.

**Fix.** Q: replace the request table rows with pointers to D-P-26...D-P-43, keep only what is still open (R5, R7, R13, R16, R17, R18, R11 and the N items), and write the M2 plan against P's 02/03 plans. Remove the stale 'Q's R-number' ambiguity by citing D-P numbers.

**Verifier.** Q's 00-design.md line 136 still reads "None is in a llm-observer plan yet". P has since adopted R1-R4, R6, R12, R14 and R15 as D-P-26-D-P-40, and crates/token on main already holds cloud-1 and cloud-2. D-Q-23 still paces pushes at 4 per minute because of the shared per-IP limiter, which D-P-33 removes. P's 03-m3 plan (lines 542-544) fixes behaviour Q does not record: instance admin gets 403, unknown subject gets 404, and disable still returns 204 for a last owner.

### X6. Cloud cell defaults: an unset `features` grants every core feature

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** Q · **Tracks:** P, Q
- **Reviewer ids:** CL-9
- **References:**
  - cetra-control `docs/`00-design.md D-Q-22, D-Q-23 (Defaults), § Plan values (Features row: Free '—'), line 129 cell-config row
  - docs/spec/12-operations.md config table rows `entitlements.defaults.plan_name` / `entitlements.defaults.features` (lines 24-25)
  - docs/plans/p-entitlements/02-m2-org-entitlements.md § Config tests `cloud_defaults_load`, § Tests `configured_defaults_apply`
  - docs/plans/p-entitlements/00-design.md § Interfaces for track Q (items 1-5: no item on `[entitlements.defaults]`)
  - docs/plans/p-entitlements/03-m3-units-and-limits.md P8 (`control_plane_provider` default `oidc:cloud`)

**Problem.** Nothing contradicts today. But P's § Interfaces for track Q never describes the cell defaults Q must generate, and the unset-versus-empty rule for `features` appears only in spec 12. If Q's M2 generator leaves out an empty feature list (Free has none), every token-less or lapsed cloud org gets `retention_management`. If it leaves out `plan_name`, those orgs show 'Self-hosted'. The OIDC provider name `cloud` and `oidc:cloud` agree, but nothing records that they are coupled.

**Failure scenario.** Q's deploy script renders Free defaults without `features`. Every org without a valid token (new orgs before their first push, orgs whose token lapsed) gets `retention_management` for free and the plan name "Self-hosted" on the Plan page; the Pro gate in M's `may_manage_retention` is open to those orgs.

**Fix.** Q (M2): have the generator always emit `plan_name` and `features = []` for Free, plus every limit except `data_access_days`, and add a test on the rendered file. Note in D-Q-19 that the provider name `cloud` must match `entitlements.control_plane_provider`. P (optional): add an item to § Interfaces for track Q on `[entitlements.defaults]` with the unset-versus-empty rule.

**Verifier.** Both sides agree today: spec 12 says unset `entitlements.defaults.features` means every core feature (`retention_management`), and P M2's `cloud_defaults_load` test models `features = []`. On the Q side, the Plan values table marks Free features as '—' and D-Q-22/23 say the cell defaults are generated from the catalog (Free without `data_access_days`). That generator belongs to Q M2, which is not written yet, so the failure is a latent trap rather than a conflict. The provider names already match: Q's `[[auth.oidc]]` is named `cloud` and P's default `control_plane_provider` is `oidc:cloud`.


## Owner: Track P

### X7. P's one-`QueryOptions`-builder rule (D-P-21) does not fit I's and M's direct `storage.query` callers

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** P · **Tracks:** I, K, M, P
- **Reviewer ids:** SC-3, DS-3, OW-2, LC-3
- **References:**
  - docs/plans/p-entitlements/00-design.md line 67 (D-P-21), line 145 (§ Reads), lines 177-191 (§ Interfaces for M, K and I: I row, 'G, H, K, I reads' row, M scope-reads row, closing line), line 231 (M4 row)
  - crates/server/src/deletion/resolve.rs:32 and crates/server/src/eval/trace_load.rs:115 (main: `QueryOptions { around .. }` outside open_query_on)
  - docs/plans/i-online-eval/02-m2-evaluators.md line 730
  - docs/plans/i-online-eval/04-m4-triggers.md lines 151, 952
  - docs/plans/m-deletion/03-worker.md line 360
  - docs/plans/i-online-eval/03-m3-queue-rules.md lines 217-220 and § `eval/rule_limit.rs` (lines 966-981)
  - docs/mvp-decisions.md line 437 (D-I-34), line 435 (D-I-32)

**Problem.** (a) P's D-P-21 and M4 summary say `open_query_on` alone builds `QueryOptions`, enforced by a test. Main already builds it directly in deletion/resolve.rs and eval/trace_load.rs, and I's M4 and M's worker plans add more direct `Storage::query` sites that need `QueryError` classification. The test as described would fail, or would push those sites through an `ApiError` path that loses the busy/retry classification. P's own M scope-reads row contradicts the rule. (b) P's I row still describes an interim 409 and call sites that pass `limit: None`. I's M3 plan, as D-I-32 allows, uses `PlanLimit` directly and exposes `eval::rule_limit::limit(state, org) -> Option<u64>`. D-I-32 and I's design are not wrong. Only P's table and closing line are stale.

**Failure scenario.** P's M4 implementer follows the table, adds the 'no `QueryOptions {` outside `open_query_on`' test and it fails on about 6 I/M call sites. Making them go through `open_query_on` loses the `QueryError` classification (a busy read becomes a permanent failure or a retry storm). Or they add `visible_from` by hand at each site and the invariant test is dropped, so the plan data window leaks through whichever site is missed. Separately, the I hook edit in P's M4 targets a `check(.., limit: None)` call site that does not exist (`limit()` is the seam), and D-I-32 tells readers a 409 ships.

**Fix.** P, in D-P-21, the § Reads line and the M4 plan: have a single `QueryOptions` constructor (for example `entitlements::query_options(state, org, around, exempt)` or the same in routes/read.rs). `open_query_on`, I's trace_load, sweeper and backfill reads, and M's resolve and worker reads all call it. The invariant test forbids the literal `QueryOptions {` everywhere except that function. Rewrite P's I row as 'body of `eval::rule_limit::limit`: `entitlements::of(state, org).limits.max_active_online_rules`', like the K row, and delete the line about the '409 becomes 403'. I only needs the D-I-34 wording amended once P picks the name.

**Verifier.** (a) is real and already in the code on main. P's 00-design.md § Reads (line 145) and the M4 row (line 231) say `open_query_on` "alone builds `QueryOptions`", with "a test that nothing else builds `QueryOptions`". But main already builds it directly in crates/server/src/deletion/resolve.rs:32 and crates/server/src/eval/trace_load.rs:115. I's M4 plan (04-m4-triggers.md lines 151 and 952) and M's 03-worker.md line 360 add more such sites, because they need `QueryError`. P's own table row 'M | scope reads ... pass `visible_from: None`' already contradicts its 'alone' rule. (b) is only half right. D-I-32 is conditional ("If K or P has already added `plan_limit` when M3 starts, M3 uses it directly"). I's 00-design.md line 348 and 03-m3-queue-rules.md line 220 already use `PlanLimit` with no 409. So I's docs are not stale. P's I row (00-design line 185) and line 191 are stale: they still describe call sites that pass `limit: None` and an interim 409, but I's seam is `rule_limit::limit(state, org)`.

### X8. Retention-cap hook specified two ways: `retention_cap_days()` vs the 30-day window history (D-P-30)

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** P · **Tracks:** M, P, Q
- **Reviewer ids:** OW-6, LC-2
- **References:**
  - docs/plans/p-entitlements/00-design.md:68 D-P-22, :88 D-P-30, :181 and :187 hook rows
  - docs/mvp-decisions.md:513 D-P-22, :521 D-P-30
  - docs/plans/m-deletion/README.md:221
  - docs/plans/m-deletion/06-retention.md:94
  - crates/server/src/retention.rs:59-61
  - crates/server/src/entitlements/mod.rs:305-330 retention_window_at
  - cetra-control `docs/`00-design.md:151 R12, D-Q-26

**Problem.** M's hook, P's hook table, P's side note and M's README all say P replaces the `None` with the synchronous `of(..).retention_cap_days()`. D-P-30, added later for Q's R12, says retention must use `retention_window_at`, which is async, reads `meta.db`, and returns the largest window of the last 30 days, not the current one. The two are different values and signatures. The hook table was never updated, and D-P-22's ingest D85 age check (synchronous, in `IngestContext`) uses the same `retention_cap_days()`.

**Failure scenario.** P's M4 implementer follows the hook table and writes `let cap = entitlements::of(..).retention_cap_days()`. A downgrade from Pro (1,825 days) to Free (30) then makes the hourly retention run delete everything older than 60 days within the hour, the exact behaviour R12 was filed to prevent. The 30-day grace stored by M2 (`org_window_ends`) is never read. Q's M3, which depends on R12 or on its own D-Q-26 workaround, keeps the workaround forever.

**Fix.** P amends D-P-22 and the hook table rows for M (retention_once and the D85 ingest bound). retention_once awaits `state.entitlements.retention_window_at(&state.meta, org, now)` and derives the cap from it. P states whether +30 is still added on top of the window history, and which value the synchronous ingest check uses. M's README row, the 06-retention snippet and the comment in retention.rs are updated to match.

**Verifier.** P's hook table (00-design.md:181, :187), D-P-22 (00-design.md:68; mvp-decisions.md:513) and M's side (m-deletion/README.md:221, 06-retention.md:94, crates/server/src/retention.rs:59-61) all give retention the synchronous `of(org).retention_cap_days()` (current window + 30). D-P-30 (00-design.md:88; mvp-decisions.md:521) says M4 instead gives retention the async `EntitlementsState::retention_window_at(meta, org, now)`, which is already on main (entitlements/mod.rs:308). The two contracts differ in value and signature, and it is also unclear whether the +30 still applies on top of the 30-day history.

### X9. Org drop deletes `usage_daily`, but Q's R5 and P's usage export need the rows for 40 days

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** P (amends M's D-M-7) · **Tracks:** M, P, Q
- **Reviewer ids:** OW-5, SC-5, DS-4, LC-4, CL-2
- **References:**
  - docs/mvp-decisions.md:474 D-M-7
  - docs/plans/m-deletion/README.md § Cascades in meta.db, row usage_daily
  - docs/plans/m-deletion/03-worker.md:83 delete_org_usage
  - crates/server/src/deletion/mod.rs:568-583 (flush then delete_org_usage) and tests ~1280-1334
  - docs/plans/p-entitlements/03-m3-units-and-limits.md:514 ('Rows of dropped orgs stay')
  - docs/plans/p-entitlements/00-design.md:106 D-P-43 and :231 M4 row (no R5/R17)
  - docs/mvp-decisions.md:534 D-P-43
  - cetra-control `docs/`00-design.md:144 R5

**Problem.** Q's R5 asks that a dropped org's usage rows stay in the export for at least 40 days. D-P-43 gives R5 to P's M4, but P's M4 scope row leaves it out. M's org drop (in its decision, plan and merged code) deletes the rows in the same transaction as the org. P's M3 export plan assumes the opposite ('Rows of dropped orgs stay'). Two tracks state opposite behaviour, and nobody owns the change.

**Failure scenario.** A paid org is closed on the 3rd of a month. M's worker drops the org and deletes its `usage_daily` rows. Q's daily pull ran at 00:15, so up to a day of units is lost, and the whole month's overage for any day Q has not yet pulled or re-pulled (a failed pull, a catch-up after Q downtime, or Q's month-end true-up that re-reads the export) is unbillable. P's M4 implementer, following D-P-43, adds a deletions feed and finds nothing in its scope that changes M's cascade.

**Fix.** Amend D-M-7 and m-deletion/README.md § Cascades: an org drop keeps its usage_daily rows. P's M4, which owns R5 under D-P-43, removes `delete_org_usage` from the drop, changes M's tests, and adds a pruner for rows of orgs that no longer exist after 40 days (or more). Add R5, R17 and the 40-day rule to P's M4 milestone row and to § Interfaces for track Q (3). Fix the wording at 03-m3:514 so it matches.

**Verifier.** D-M-7 (mvp-decisions.md:474), m-deletion/README.md § Cascades (usage_daily, org column: 'deleted, after one Usage::flush'), and the code in crates/server/src/deletion/mod.rs:581 (`crate::usage::delete_org_usage(conn, org)` in the drop transaction) all delete the org's usage rows. Q's R5 (cetra-control/docs/00-design.md:144) asks for 'at least 40 days', and D-P-43 assigns R5 to P's M4, but P's M4 milestone row (00-design.md:231) never mentions R5, R17 or the 40 days. P's own M3 plan (03-m3-units-and-limits.md:514) also says 'Rows of dropped orgs stay (`usage_daily` has no foreign key)', which contradicts M's code.

### X10. I's active-rule-limit hook is described three ways

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** P (I fixes D-I-32) · **Tracks:** I, K, P
- **Reviewer ids:** OW-8
- **References:**
  - docs/mvp-decisions.md:435 D-I-32
  - docs/plans/i-online-eval/03-m3-queue-rules.md:217-220 error table, :966-980 eval/rule_limit.rs, :1647 test
  - docs/plans/p-entitlements/00-design.md:185 hook row for I and the sentence after the table
  - crates/server/src/api/common.rs:138
  - crates/server/src/alerts/quota.rs:13

**Problem.** P's hook table row for I is stale. It describes literal `limit: None` call sites and an interim 409 that P's M4 would replace, with spec 09 and I's error table edits. I's M3 plan, as D-I-32's conditional allows, answers 403 `plan_limit` from the start and puts the limit in `rule_limit::limit()`. The detail texts of the two tracks also differ.

**Failure scenario.** P's M4 implementer searches for call sites passing `limit: None` and an interim 409 mapping, finds neither, and either adds a second code path or rewrites I's tests that assert 403. A reader of D-I-32 implements the 409 that I's plan explicitly does not.

**Fix.** P rewrites its row for I to match K's: 'body of `eval::rule_limit::limit(state, org)`: `entitlements::of(state, org).limits.max_active_online_rules`; I's PlanLimit mapping and message stay'. P drops the 409, spec 09 and error-table wording and the sentence after the table. Optionally, I trims D-I-32's 409 and `limit: None` text, since K's PlanLimit has landed.

**Verifier.** D-I-32 already says 'If K or P has already added plan_limit when M3 starts, M3 uses it directly', and I's M3 plan (03-m3-queue-rules.md:217-220) uses 403 `plan_limit` ('so there is no interim 409'), so D-I-32 is not wrong. What is stale is P's hook row for I (00-design.md:185) and the line after the table: they still describe call sites passing `limit: None`, an interim 409 to replace, and spec 09 and I's error table changing. I's plan instead routes the limit through `eval::rule_limit::limit(state, org) -> Option<u64>` (03-m3:975). The detail strings differ: 'active online rule(s)' in I, 'active online evaluation rule(s)' in P. `ApiError::PlanLimit` is on main (api/common.rs:138).

### X11. Spec 09 is behind shipped and planned P surfaces

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** P · **Tracks:** N, P, coordinator
- **Reviewer ids:** SC-12
- **References:**
  - docs/spec/09-api.md prefix table lines 5-13 (no /api/internal row) and § Orgs, projects, members, keys line 67 (the /api/internal/v1/* endpoints are listed there)
  - docs/spec/09-api.md § CLI line 179 (admin row lists `check`; no `entitlements show`)
  - crates/cli/src/main.rs AdminCommand::Entitlements / EntitlementsCommand::Show (~lines 403-412)
  - docs/plans/p-entitlements/02-m2-org-entitlements.md P9 (line 58, 995)
  - docs/plans/n-packaging/m2/08-cli.md § docs/spec/09-api.md § CLI (lines 119-127)
  - docs/plans/i-online-eval/00-design.md line 89; docs/spec/12-operations.md line 5

**Problem.** Spec 09's CLI table lacks P's shipped `cetra admin entitlements show <org>`. Its prefix table omits `/api/internal/*`, although those endpoints and their token auth are documented in the endpoints section. `cetra admin check` is specified in specs 09 and 12 but no track owns building it. N's B8 edits a different row and does not conflict.

**Failure scenario.** Q's authors, who work from spec 09, cannot find the internal-endpoint contract in the prefix table. N's B8 and the P follow-up both edit the same table rows and conflict, or one silently drops the other's line. `cetra admin check` is documented in specs and, by D-N-4/N docs, may be relied on as a preflight that does not exist.

**Fix.** P: add `cetra admin entitlements show <org>` to spec 09's CLI table, and a `/api/internal/*` row to the prefix table (auth: signed org token or control_plane bearer; see § Orgs). Coordinator: assign `admin check` to a track, or mark it '(not built)' in specs 09 and 12.

**Verifier.** `cetra admin entitlements show` is on main and absent from spec 09's CLI table, and `admin check` is specified in specs 09 and 12 but built by no track. Two parts of the original claim are overstated. The /api/internal endpoints and their auth are already in spec 09's endpoint table (line 67); only the prefix table lacks the row. N's B8 replaces only the compact/wal-inspect/gc-orphans row (line 181), not the admin row (line 179), so it does not conflict with P's command.


## Owner: Track I

### X12. At the units cap I's executor pays for the judge call, then retries on 402 `quota_exceeded`

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** I · **Tracks:** I, P
- **Reviewer ids:** LC-1, SC-4, OW-3
- **References:**
  - docs/plans/p-entitlements/03-m3-units-and-limits.md § P2 'Where it runs' (Scores bullet) and § Coordination (I bullet)
  - docs/plans/p-entitlements/00-design.md § Interfaces for M, K and I, last I row ('read as is')
  - docs/plans/i-online-eval/03-m3-queue-rules.md § Job execution steps 10-16 and the 'Dispositions after the call' table
  - docs/mvp-decisions.md D-I-13, D-P-17

**Problem.** P's cap check sits inside `scores::write`, which I's executor calls only after the judge call and the judge-trace commit (steps 12-15). P's request that I treat 402 as permanent exists only as a coordination line in P's M3 plan. I's M3 and M4 plans do not mention `quota_exceeded`, and P's interface table says I's executor is 'read as is'. A 402 falls into I's generic write-failure path, which D-I-13 defines as: retry the writes 3 times 5 s apart, then requeue with backoff and call the provider again.

**Failure scenario.** A Free org (100k units) reaches its cap on day 20 with queued or incoming rule jobs. Each job calls the customer's own LLM and commits a judge trace (not billable, so never refused). The score write then returns 402. The job retries the write 3 times and requeues, up to 6 attempts. That is up to 6 paid judge calls per trace, each charged to `rule_spend`, with no score ever stored until the 1st of the month. The queue is flooded, and the rule budget can pause on `budget` even though nothing was produced.

**Fix.** In I's M3 (§ Job execution, step 15), map `ApiError::QuotaExceeded` to `Failed { Permanent, "monthly units cap reached" }` with `reached_evaluator = false`, so it neither trips the breaker nor retries. Also, to avoid paying for judge calls whose score cannot be stored, call `entitlements::units::check_cap` before step 10 and end the job as `Skipped { error: "quota" }`. Add an executor test for both. P M3 then replaces its Coordination sentence with a pointer to I's rule.

**Verifier.** P M3 § P2 'Where it runs' puts `check_cap` at the top of `scores::write` and says evaluator scores 'then fail; see § Coordination'. The Coordination line asks I to treat this as permanent. I's 03-m3 § Job execution step 15 maps every non-BadRequest error to 'the retryable path, error `could not store the result`', and D-I-13 says that path 'calls the provider again'. No I plan mentions quota_exceeded or 402, and P's interface table says I is 'read as is'.

### X13. I's M4 defines its migration number two incompatible ways

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** I · **Tracks:** I, P, coordinator
- **Reviewer ids:** SC-10
- **References:**
  - crates/server/migrations/meta/ (0001-0008)
  - docs/plans/i-online-eval/00-design.md line 150 and § Tables (0009_eval_queue, 0010_pending_traces, 0011_backfills)
  - docs/plans/i-online-eval/03-m3-queue-rules.md § The migration number (lines ~147-152)
  - docs/plans/i-online-eval/04-m4-triggers.md § The migration number (lines ~126-131)
  - docs/plans/p-entitlements/03-m3-units-and-limits.md line 425 ('The next free number `NNNN_usage_notices.sql`')

**Problem.** I's M4 plan defines its number both as 'next free' and as 'M3's number plus one'. These disagree if P's usage_notices migration merges between I's M3 and M4. I's design tables hard-code 0009-0011, but the plans already tell the implementer to correct them.

**Failure scenario.** P's P6 merges after I's M3: it takes 0010. I's M4 plan then says `0010`, 'MM = M3 + 1', so two files claim 0010. Or the order is reversed and I's design tables show wrong numbers.

**Fix.** I: drop 'which is M3's eval_queue number plus one' and the concrete `0010` from 04-m4. Keep 'next free on origin/main when you start; renumber at rebase if another migration landed'. A roadmap-wide line about renumbering and regenerating meta_schema.snap at merge is optional (coordinator).

**Verifier.** I's M3 and M4 plans already say to check the base and `git log origin/main` for the next free number, and to edit the design only when it differs. So most of the race is handled. The leftover inconsistency is M4's definition: 'the next free number…, which is M3's eval_queue number plus one'. If P's P6 lands between I's M3 and M4, these give different answers.


## Owner: Track N

### X14. N's pre-migration copy is keyed by `CARGO_PKG_VERSION`, but Q deploys commit-pinned builds

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** N · **Tracks:** N, Q
- **Reviewer ids:** SC-9
- **References:**
  - docs/plans/n-packaging/m2/00-design.md § Pre-migration copies (D-N-22), lines 275-287
  - docs/plans/n-packaging/m2/09-pre-migration.md § One rule, lines 26-40 ('target = dir / "{name}.pre-{CARGO_PKG_VERSION}"'; 'if target exists: keep it and skip the copy')
  - Cargo.toml line 6 `version = "0.0.0"`
  - cetra-control `docs/`01-m1-first-cell.md `docs/runbooks/cell-upgrade.md` steps 1 and 4 and Rollback (lines ~1556-1566); 00-design.md D-Q-8

**Problem.** Between releases, every commit has the same CARGO_PKG_VERSION (0.0.0 today). After the first migrating upgrade, N's rule keeps the existing `.pre-0.0.0` copy and skips new ones. Q's cell-upgrade rollback restores that copy, which can be from several upgrades earlier. Q's runbook does take a full backup first (step 3), so data is recoverable. But the documented rollback path silently restores stale meta.db/catalog.db.

**Failure scenario.** Q upgrades eu1 from commit A to B (copy `meta.db.pre-0.0.0` made, B migrates). A week later it upgrades to commit C, which adds a migration. The copy is skipped because `.pre-0.0.0` exists. C misbehaves. The runbook restores `.pre-0.0.0`, which is a week old, and the week's orgs, keys and deletions in `meta.db` are silently lost (and `catalog.db` is out of step with the objects).

**Fix.** N (B9/D-N-22): key the copy on the schema being left, e.g. `{name}.pre-m{applied}` (applied migration number), keeping 'never replace an existing copy of the same name'. A failed half-migration leaves `applied` unchanged, so the rule still holds. Q: make the rollback step name the copy by that key, or restore from the step-3 backup instead.

**Verifier.** N names the copy `<name>.pre-<CARGO_PKG_VERSION>` and keeps an existing one; the workspace is 0.0.0, and Q's runbook moves `CETRA_COMMIT` between main commits, then on rollback restores `meta.db.pre-<version>`/`catalog.db.pre-<version>`. So after the first upgrade, later upgrades skip the copy, and a rollback restores a stale copy. Q's step 3 takes a backup first, which limits the damage. The rollback text still points at the wrong file.

### X15. Restore starts the server before `reapply-deletions`, and reapply does not cover eval or `meta.db` cascades

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** N · **Tracks:** I, M, N
- **Reviewer ids:** LC-6
- **References:**
  - docs/plans/n-packaging/m2/10-docs.md § Restore steps 6-7 (lines 67-68)
  - docs/plans/n-packaging/m2/00-design.md § Restore (D-N-21) steps 4-5
  - docs/plans/m-deletion/07-reapply.md intro (start once only if older version) and final paragraph (~line 118: guard learns scope only at next start; M8 cascade skipped)
  - docs/plans/m-deletion/README.md § Evaluation traces (guard 24 h from created_at) and § Lifecycle cut-off
  - docs/spec/12-operations.md § Backups 'Restore' (line 117: start, then re-apply) vs 'GDPR and backups' (line 119: start only if older version)
  - docs/mvp-decisions.md D-I-12 (line 415), D-I-28 (line 431: backfill jobs live 7 days)

**Problem.** The reviewer's Monday→Thursday scenario mostly fails for rule jobs: queued rule jobs expire after 24 h (D-I-28). It still holds for backfill jobs (7 days), for jobs the pending_traces sweeper creates, and for a restore within 24 h of the backup. Spec 12 itself is inconsistent: its cold-restore paragraph starts the server and then reapplies, while its GDPR paragraph starts the server only for an older backup. N's new steps follow the first version.

**Failure scenario.** Backup on Monday. On Wednesday a user is erased. A disk failure on Thursday leads to a restore. The server starts and the queued rule jobs judge the user's restored traces. They commit judge traces (copies of the prompt and completion) and `reason` scores with today's ingest time. The operator then runs reapply. The user's traces are tombstoned, but the new judge traces and scores survive. Meanwhile the UI, API and alerts show the erased data.

**Fix.** N: in 10-docs.md and 00-design.md § Restore (and spec 12 § Backups 'Restore'), run `reapply-deletions` before the first start when the backup is at the binary's schema. When it is not, start with eval workers and alerts disabled, or provide a migrate-only command, until reapply is done. M: in M7, load reapplied rows into the guard at registration regardless of `created_at` age, re-run `eval_trace_ids` over the stored scope when registering, and apply M8's eval_jobs cascade to reapplied rows. Add a test for each.

**Verifier.** N's 10-docs.md § Restore steps 6-7 and 00-design.md § Restore steps 4-5 always start `cetra serve` before `reapply-deletions`. M7 (07-reapply.md intro) and spec 12 'GDPR and backups' say to start the server once only if the backup is from an older version, and the command 'works whether the server runs or not'. M7's last paragraph accepts that the guard learns reapplied scopes only at the next start and that M8's eval_jobs cascade does not run. D-I-28 keeps queued backfill jobs for 7 days, and the pending_traces sweeper creates new jobs, so restored jobs can judge erased traces in that window. Their judge traces get an ingest_time after the cut-off, outside the stored scope.


## Owner: Track M

### X16. D-M-17's `on_dropped` push hook for Q is vestigial next to Q's pull design

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** M · **Tracks:** M, P, Q
- **Reviewer ids:** DS-8
- **References:**
  - docs/mvp-decisions.md line 484 (D-M-17: "track Q adds the control-plane notification ... there")
  - docs/plans/m-deletion/README.md § Hooks for other tracks, Q bullet (line ~317): "add the control-plane notification to `deletion::on_dropped`"
  - crates/server/src/deletion/mod.rs:192-195 (doc comment "Track Q adds the control-plane notification here ... through its own outbox"; `on_dropped(_state: &AppState, ...)`)
  - docs/plans/p-entitlements/00-design.md line 222 ("Not P's ... `deletion::on_dropped` (track M's hook for Q, D-M-17)")
  - cetra-control `docs/`00-design.md § Interfaces table row "Org drops ... (via R5)" and R5 row (pull of `deletions` rows, "D-M-17's durable outbox")
  - docs/mvp-decisions.md line 534 (D-P-43: R5 to P's M4)

**Problem.** D-M-17 says Q adds its notification inside `on_dropped`. Q is a separate repository and instead pulls `GET /api/internal/v1/deletions` (R5), assigned to P's M4. Nobody owns the push hook, so `on_dropped` stays a function that only logs and ignores its state argument.

**Failure scenario.** An implementer of Q follows D-M-17 and looks for the hook to edit in llm-observer, or an agent adds an HTTP push to `on_dropped` that duplicates R5 and cannot retry.

**Fix.** Amend D-M-17, M's README Q bullet and the `on_dropped` doc comment: the control plane learns of drops by pulling `deletions` rows through R5 (P's M4), and `on_dropped` remains only the audit emitter (drop the unused `_state` parameter).

**Verifier.** Q is a separate repository and learns of drops only by pulling `deletions` rows through R5, which D-P-43 assigns to P's M4. D-M-17, M's README and the code comment still say Q adds a push inside `on_dropped`. No one will make that edit, and anyone who follows those docs would add a push that duplicates R5. It is wording drift with no behaviour risk, so minor.

### X17. EXPLAIN in the SQL console exposes an erased person's `user_id`

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** M · **Tracks:** A1, A2, H, M
- **Reviewer ids:** DS-12
- **References:**
  - docs/mvp-decisions.md line 280 D-A1-3 (predicate includes `user_id IN …`; accepted consequence: EXPLAIN shows IDs of a pending deletion)
  - docs/spec/03-storage.md line 377; docs/spec/05-query.md line 96 and D96 (plain EXPLAIN allowed)
  - crates/storage/src/deletion.rs lines 216-275 covered_predicate: utf8_term(schema, USER_ID, users) literal values in a physical expression; crates/storage/src/deletion_tests.rs line 343 shows EXPLAIN prints the tombstone filter
  - docs/mvp-decisions.md line 472 D-M-5 (user_id 1-255 bytes), line 475 D-M-8 (identifier 'never reach[es] logs'); no EXPLAIN mention anywhere in docs/plans/m-deletion/

**Problem.** Exposure is limited to callers with SQL access on the same project (pending deletions are per project, deletion.rs for_project), who could already read that user_id in the data before the erasure. What leaks is the fact that this person requested erasure, and the identifier stays readable until the rewrite applies the deletion.

**Failure scenario.** Within `max_delay` (24 h, or longer while the rewrite waits), any viewer, read-only API key or MCP agent runs `EXPLAIN SELECT 1 FROM spans` and reads the e-mail address of the person whose data was just erased.

**Fix.** Either redact the IN-list literals in the predicate's display (for example a wrapper expression whose fmt prints a count or hash), or accept it explicitly by amending D-A1-3/D-M-8 and spec 03 line 377 to name user ids. Add a test on EXPLAIN output either way.

**Verifier.** The code builds the user deletion's user_id values as literals into the physical filter that plain EXPLAIN prints, D-A1-3's acceptance predates user deletions, and M's privacy trade-off (D-M-8) and plans never mention EXPLAIN.

### X18. Pre-migration copies hold erased data, and reapply cannot read a newer-schema source

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** M · **Tracks:** M, N
- **Reviewer ids:** LC-9
- **References:**
  - docs/spec/12-operations.md § Upgrades (line 128 'Downgrades are not supported (documented: restore the pre-upgrade backup)'; line 125) and § Backups 'GDPR and backups' (line 119)
  - docs/plans/n-packaging/m2/00-design.md § Pre-migration copies (D-N-22) lines 275-286; 09-pre-migration.md § Changes; 10-docs.md § docs/self-hosting/upgrade.md
  - docs/plans/m-deletion/07-reapply.md § Admin function lines 78-107 (`SourceSchema`: 'run `cetra serve` on a copy of it first') and line 7
  - crates/server/src/meta/mod.rs open_existing / SchemaMismatch (lines ~77-86, 181)

**Problem.** (a) A rollback restores the `.pre-<version>` copies or a pre-upgrade backup and runs the old binary. `reapply-deletions --from <kept dir>` then fails with `SourceSchema`, because the source is newer. The suggested remedy (run serve on the source) cannot work, since the old binary cannot open a newer schema either. Erasures made between the upgrade and the rollback are lost with no supported way to reapply them. (b) `meta.db.pre-<version>` keeps pre-upgrade member emails, names and project rows until the next upgrade, which may be months away, and the GDPR backup-retention advice does not cover it. `catalog.db.pre-*` holds only file and catalog metadata, not payload, so the GDPR concern is mainly about `meta.db.pre-*`.

**Failure scenario.** Upgrade on Monday migrates `meta.db`. An erasure is requested on Tuesday. On Wednesday the operator rolls back by restoring `meta.db.pre-0.3` with the 0.2 binary and cannot run `reapply-deletions` from the migrated directory. The erased user's data reappears with no supported way to erase it again. The copies also outlive every documented retention.

**Fix.** M (07-reapply): when the source schema is newer, read `deletions` and its scope tables by column name, refusing only if the columns are missing, or document that a rollback must be followed by a forward upgrade before reapplying. Change the `SourceSchema` message so it gives no impossible remedy for a newer source. N (10-docs, upgrade.md and backup.md) and spec 12: describe the rollback-then-reapply limitation, tell operators to delete `*.pre-*` once the upgrade is verified, and say these copies count as backups for GDPR retention.

**Verifier.** Spec 12 § Upgrades documents a downgrade as restoring the pre-upgrade backup. M7's `reapply_deletions` opens `--from` with `MetaDb::open_existing`, which refuses any schema mismatch (crates/server/src/meta/mod.rs `SchemaMismatch`), and its `SourceSchema` remedy only helps with an older source. After a rollback the kept directory is newer, so no binary can reapply from it. Separately, D-N-22/B9 keep `meta.db.pre-<version>` until the next upgrade, and neither N's docs nor spec 12 § 'GDPR and backups' (backups ≤ 30 days) mention these copies.

### X19. Judge traces and evaluator scores outlive their target's retention and plan window

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** M · **Tracks:** I, M, P
- **Reviewer ids:** LC-11
- **References:**
  - docs/plans/m-deletion/README.md § Retention (D-M-10) lines 180-193 ('judge traces: they are spans of the project')
  - docs/spec/03-storage.md line 35 (scores day = created_at) and § 8 Retention (lines 428-435)
  - docs/plans/i-online-eval/00-design.md D-I-6/D-I-11 (manual and backfill runs)
  - docs/plans/p-entitlements/00-design.md D-P-21 (line 67)

**Problem.** Judge traces (copies of the target's prompt and completion) and scores are stored on the day they are written, not on the target's day. Retention and P's window are time-based, so an evaluation, or any late score, on an old trace keeps the copy for up to one more retention period after the target is dropped, and leaves it visible with a dead trace link after the target leaves the window. Only GDPR deletion follows the target, through `eval_trace_ids`. Nothing in spec 03 § 8, D-M-10 or the Data tab copy states this bound.

**Failure scenario.** A project has `retention_days = 7`. On day 6 a member runs 'Run evaluator' (or a backfill) on a 6-day-old trace. The judge trace (a copy of the prompt and completion) and the score `reason` are stored with today's date. The target is dropped on day 7, but the copies stay until day 13 and appear in the score list with a dead trace link. Under a Free window of 30 days the same copies remain visible after their target is hidden.

**Fix.** M: in D-M-10 and spec 03 § 8, state that evaluation output and late scores follow their own write day (at most retention plus the target's age at write time), and add it to the retention UI copy. Optionally, retention could also drop judge traces whose target day has been dropped. Coordinate with I and P if the behaviour changes.

**Verifier.** The facts are correct. Spec 03 l35 partitions scores by the UTC day of `created_at`, and retention drops whole days per anchor (§ 8). M's README § Retention says judge traces are covered only because 'they are spans of the project', and D-P-21 hides scores by `created_at`. So a judge trace or score written later for an old target outlives that target under both retention and the window. M chose this on purpose, and it equally affects SDK and annotation scores on old traces. Whether it is a defect is a matter of interpretation, and the docs never state the bound.


## Owner: Track R

### X20. Track R ignores N's M2 and P's M4 changes in `crates/storage`

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** R · **Tracks:** A1, M, N, P, R
- **Reviewer ids:** SC-11, DS-9, OW-9, LC-8, PR-5
- **References:**
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md § Audit so far item 2 ('Nothing lists the object tree'), § M2 (fs seam: 'Every local file operation of crates/storage'), § M4 Checks ('an orphan audit (list the bucket, compare it with catalog.db)'), § M1 Budget (CETRA_MODEL_CASES, D-R-14)
  - claude/r-hardening-design-43eb31:docs/roadmap.md row R (deps 'A1, A2, M')
  - docs/plans/n-packaging/m2/00-design.md lines 162-178 (Storage::copy_objects hard links), 320-321 (copy_objects, collect_orphans in storage), § Pre-migration copies; docs/roadmap.md row N ('M2 ... planned')
  - docs/plans/p-entitlements/00-design.md D-P-21 (line 67, QueryOptions::visible_from, media per-blob newest start_time) and M4 row (line 231)
  - crates/otel/tests/proptest.rs line 95, crates/eval/tests/hostile.rs line 100 (CETRA_PROPTEST_CASES)

**Problem.** R's statement 'nothing lists the object tree' describes current code. N's planned M2 adds that listing (`collect_orphans`) and new file operations (`copy_objects` hard links) in crates/storage. P's M4 adds `visible_from` to every provider. R's design and dependency column ignore both, so R's M2 fs seam, its M4 orphan audit and its M1 model risk duplicating or missing them. The case-count env var differs from the repo's existing CETRA_PROPTEST_CASES.

**Failure scenario.** R's M4 writes a second bucket lister that drifts from `collect_orphans`. N's `copy_objects` lands with raw `std::fs` calls and R's M2 either misses them (backup path untested for lost writes) or must refactor merged code. P's window filter is untested by R's model, so wrong results from `visible_from` pass R's 'never returns a wrong result' claim.

**Fix.** R: in M4, reuse N's `collect_orphans(dry_run)` for the orphan audit (or state the order if R lands first). Add N M2's storage file operations to M2's fs-seam scope and the durability table. Add `visible_from` to the M1 model's read set once P's M4 lands. Reuse CETRA_PROPTEST_CASES or document both. Coordinator: add N M2 and P M4 to R's dependency/ordering column. P's M4 plan states where the media record lives (if it goes in catalog.db, it is a format change).

**Verifier.** R's design never mentions N's M2 or P's M4. Its roadmap row depends only on A1, A2 and M. N's M2 adds `Storage::copy_objects` (hard links) and `collect_orphans` (lists the store) in crates/storage, and P's M4 adds `QueryOptions::visible_from` to every provider. Both overlap R's M2 fs-seam scope, R's M4 orphan audit, and R's model of every read. The env var split (CETRA_MODEL_CASES vs the existing CETRA_PROPTEST_CASES) is also real. Sub-point (d) is speculative: P's M4 plan is not written, and it does not say where the media record lives.

### X21. Track R's golden data directory per release has no owner in N's release process

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** R · **Tracks:** N, R, coordinator
- **Reviewer ids:** OW-11
- **References:**
  - branch claude/r-hardening-design-43eb31: docs/plans/r-hardening/00-design.md § M8 (lines 154-163), § Cross-cutting rules 'Scope of edits' (line 167), § M3 Layout/CI (lines 90-106)
  - branch claude/r-hardening-design-43eb31: docs/mvp-decisions.md D-R-7, D-R-12
  - docs/plans/n-packaging/README.md lines 63, 197, 319-321, 347
  - AGENTS.md § Dependencies, § Layout, § Commands, § CI

**Problem.** R's design does not give the per-release golden directory an owner in N's release tooling. This is deferred rather than wrong, because R's M8 plan is written only near the release. R's M3 also adds a `fuzz/` crate with its own pins and lockfile, `tools/lazyfs/`, new just recipes and nightly.yml, but its scope of edits leaves out AGENTS.md and dependabot.yml, so the repository rules and Dependabot and cargo-deny coverage of `fuzz/` are not updated.

**Failure scenario.** The first public release is cut with `release-prep` and the workflow, and no golden directory is produced, so the format-stability guarantee (D-R-12) starts one release late and no CI job notices. A reviewer applying AGENTS.md rejects `fuzz/Cargo.toml` pins, or Dependabot and `cargo deny` do not cover the fuzz lockfile and nobody has decided that.

**Fix.** R's M3 plan adds AGENTS.md (Layout, Commands, the `fuzz/` exception to the dependency rule) and .github/dependabot.yml to its scope of edits, or asks the coordinator to make those edits. R's M8 plan adds a release-verify check, or a release-prep step, for `fixtures/storage/golden/<version>`, as a change to N's scripts made through the coordinator.

**Verifier.** R's 00-design § M8 says each release commits `fixtures/storage/golden/<version>.tar.zst`, and N's release-prep and release-verify (n-packaging/README.md lines 63, 197, 319-321) do not handle it. However, R defers the M8 plan until the release date is close ('M8's plan is written once the release date is close'), so the plan can still add the step. R's scope of edits (line 167) leaves out AGENTS.md and .github/dependabot.yml. D-R-7 explains the fuzz pin exception only in mvp-decisions.md, and AGENTS.md § Layout, § Commands and § Dependencies are not updated.

### X22. R's model test does not cover the reads deletion, alerts and the plan window rely on

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** R · **Tracks:** K, M, P, R
- **Reviewer ids:** LC-10
- **References:**
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md § M1 (lines 56-66)
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md § R1.5 Ops (lines 585-606) and § What not to build in M1 (lines 736-742)
  - crates/storage/src/reads/deletions.rs (filter_trace_ids l70, user_deletion_scope l126, eval_trace_ids l193); crates/storage/src/reads/alerts.rs (alert_value l86)
  - docs/plans/p-entitlements/00-design.md D-P-21 (line 67) and § M4 (line 231)

**Problem.** R M1 claims to model 'every read the server makes', but it skips M's three deletion-scope reads and K's `alert_value`, and its `DeleteUser` op computes the scope in the oracle instead of through `user_deletion_scope`. A bug in the real scope read therefore passes the model. R also has no plan to cover P M4's `visible_from`. The straddling-trace behaviour of D-P-21 is P's own design, not something R contradicts.

**Failure scenario.** `user_deletion_scope` misses a trace whose spans carry no `user_id`, or `eval_trace_ids` misses a judge trace. R's model still passes, because the oracle computes the scope itself, so the erasure gap ships. P's `visible_from` later hides spans by `start_time` and traces by their own `start_time`. A trace that straddles the boundary shows up with some spans missing, and no model test notices.

**Fix.** R: either add `user_deletion_scope`, `filter_trace_ids` (unfiltered) and `eval_trace_ids` (model eval spans with `cetra.eval.target_trace_id`) plus `alert_value` to the checked reads, and resolve `DeleteUser` through the real read compared against the oracle, or reword 'every read' and list them under 'What not to build'. P M4's plan should say whether it extends R's model with a `visible_from` window.

**Verifier.** R's 00-design § M1 and 01-m1-model.md list the checked reads (trace/span/score/session/user lists, metrics, `deletion_status`). They do not include `filter_trace_ids`, `user_deletion_scope`, `eval_trace_ids` (crates/storage/src/reads/deletions.rs) or `alert_value` (reads/alerts.rs). `DeleteUser` is resolved inside the oracle ('A user deletion as the server resolves it (D-M-3)', lines 591/604), and nothing in R mentions `visible_from`, which P M4 adds to storage under D-P-21. The gap is real. It is mostly R's own coverage choice, and the 'What not to build' section openly excludes several reads, so the cross-track inconsistency is limited to the 'every read the server makes' wording and the oracle re-implementing M's rule.

### X23. Track R's `fuzz/` crate sits outside the workspace

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** R · **Tracks:** R, coordinator
- **Reviewer ids:** PR-6
- **References:**
  - branch claude/r-hardening-design-43eb31 docs/mvp-decisions.md D-R-7 ('The fuzz/ crate's dependencies are pinned in its own Cargo.toml, because the workspace rule ... applies to workspace members')
  - r-hardening/00-design.md § M3 Layout line 90 and § Cross-cutting rules line 167 (scope of edits excludes deny.toml and .github/dependabot.yml)
  - AGENTS.md § Dependencies
  - justfile line 36 (`cargo deny check advisories bans licenses sources`, root manifest only) and line 112
  - deny.toml lines 49-75 (deny-multiple-versions for datafusion, arrow-*, parquet, object_store)
  - .github/dependabot.yml cargo entry directory '/' only

**Problem.** The out-of-workspace fuzz/ crate (D-R-7) has its own Cargo.lock that no cargo-deny run, family ban, or Dependabot entry covers, and R's edit scope does not include deny.toml or dependabot.yml, so nobody is assigned to close that gap. Version requirements still come from the root workspace through the path dependencies, so family majors cannot diverge; patch versions and fuzz-only dependencies (libfuzzer-sys, arbitrary) can drift and go unaudited.

**Failure scenario.** A DataFusion major lands by hand in the root workspace. fuzz/Cargo.lock keeps the old arrow family, so the nightly job fuzzes a different Parquet decoder than the one shipped. Or a RustSec advisory in a fuzz-only transitive dependency is never reported, and the 'solid' claim the track exists to back is made on unaudited inputs.

**Fix.** Either make fuzz a root workspace member outside default-members with libfuzzer-sys/arbitrary in [workspace.dependencies], or keep it standalone and add `cargo deny --manifest-path fuzz/Cargo.toml check` to `just deny` plus a `/fuzz` cargo entry in dependabot.yml; widen R's scope of edits to include those files (or request it from the coordinator) and note the choice in AGENTS.md § Dependencies.

**Verifier.** Confirmed that just deny, deny.toml bans and Dependabot cover only the root manifest, so a standalone fuzz/Cargo.lock is never audited or updated. But the reviewer's failure scenario is overstated: the fuzz crate depends on cetra-storage by path and that crate inherits its version requirements from the root workspace, so a DataFusion major bump forces fuzz's lock to re-resolve to the new major. Only patch-level drift and missing advisory scans remain, in a crate that does not ship, hence minor.

### X24. Track R's conventions deviate from AGENTS.md registries

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Owner:** R · **Tracks:** R, coordinator
- **Reviewer ids:** PR-10
- **References:**
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md § Conventions for every sub-phase (Scope) and § R1.7 `justfile`
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:68, 85, 90, 106, 167
  - D-R-14 (branch diff of docs/mvp-decisions.md)
  - AGENTS.md § Tests, § Commands, § Layout
  - docs/plans/a1-storage/00-design.md:495 (precedent)

**Problem.** R's scope lists leave out AGENTS.md, so the new `just model` recipe, the CETRA_MODEL_CASES env var and, later, `fuzz/`, `tools/lazyfs/` and the nightly workflow will not appear in AGENTS.md's Commands, Tests, Layout and CI sections. The commit-subject and D-R-2 parts of the finding do not hold.

**Failure scenario.** After R1.7 merges, `just model` and CETRA_MODEL_CASES exist but the commands table and Tests section do not list them, so the next session does not know how to run the long model test. Sub-phase subjects with '(R1.1)' stay in history next to plain ones, breaking the plain-subject convention that D-N-5 sets for release tooling.

**Fix.** Add AGENTS.md to R's scope lists (r01 § Conventions and 00-design § Scope of edits). R1.7 adds `just model <cases>` to the Commands table and CETRA_MODEL_CASES to § Tests. M3 adds `fuzz/`, `tools/lazyfs/` to § Layout and the nightly workflow to § CI. Leave the commit subjects unchanged.

**Verifier.** Only part of this holds. The commit-subject claim is refuted: 416 of the 751 subjects on main use a `scope: ` prefix, and several end in a sub-phase id, e.g. `storage: ... (M6.9)`. R follows the existing practice. D-N-5's 'Commit subjects stay plain' is about release tooling, not about forbidding scopes. D-R-2 / R1.8 fixing 'anywhere in crates/storage/src/' stays inside R's own scope (00-design § Scope of edits: `crates/storage/`), so that is not a violation either. What holds: neither r01 § Conventions 'Scope' nor 00-design.md:167 lists AGENTS.md, yet R adds `just model`, `CETRA_MODEL_CASES` (D-R-14), `fuzz/`, `tools/lazyfs/` and `nightly.yml`. AGENTS.md has none of them, while it lists the sibling CETRA_CRASH_ITERATIONS / CETRA_TEST_S3_* and every just recipe. A1 set the precedent of updating AGENTS.md (a1-storage/00-design.md:495).


## Owner: Coordinator

### X25. Several Q requests (R5, R7, R11, R13, R16, R17, R18) have no owner or milestone in llm-observer

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** D, I, K, L, M, N, P, Q, coordinator
- **Reviewer ids:** OW-1, SC-6, DS-5, CL-4, PR-7
- **References:**
  - cetra-control `docs/`00-design.md § Requests to llm-observer (line 136 and rows R5, R7, R11, R13, R14, R16, R17, R18), § Milestones M2 and M6 rows
  - docs/mvp-decisions.md D-P-43 (line 534)
  - docs/plans/p-entitlements/00-design.md D-P-43 (line 106), D-P-39 (line 102), § Milestones M3/M4 rows (lines 230-231)
  - docs/plans/p-entitlements/03-m3-units-and-limits.md § Coordination (line 625)
  - docs/roadmap.md track table (P row line 56, Q row line 57; no auth-track row)
  - docs/plans/n-packaging/README.md line 77
  - side note q-requests.md

**Problem.** Q's M2 depends on R1–R5, R7, R13–R16 (plus R18, which Q's request table needs 'before the first customer' but which its M2 dependency list omits). On the llm-observer side, R1–R4 and R15 are in P's M2 and R14 in P's M3. R5 is assigned to P's M4 only in D-P-43's prose; the P M4 row omits it, and M4 waits on I's M3 and M's M6. R7, R13 and R16 go to an 'auth track' that does not exist in the roadmap and has no plan. R18 (restore mode) and R11 (/metrics) have no owner at all. Q's M2 plan cannot be scheduled against real milestones, and the cell OIDC switch (R7/R13/R16) and placement freeing (R5) are blocked indefinitely.

**Failure scenario.** Q starts M2 after P's M2 as its own table says. Its cell login (R7: login page goes straight to the only provider; R13: instance admins keep password login; R16: OIDC client secret from env) cannot be built. R5 (placement freeing) and R14 (account closure reaching the cells) need P's M3/M4, and P's M4 itself waits on I's M3. R18 is needed 'before the first customer' (restore drills run on copies holding live channel and LLM-connection secrets, so alerts and judges would fire from a drill) and nobody owns it. Q M2 and Q M6 stall, or Q ships workarounds (the 0640 config file, Q-side pacing) that become permanent.

**Fix.** The coordinator adds a roadmap row and a small plan for the auth track (R7, R13, R16), or folds them into a named P/D-follow-up milestone. It assigns R18 (restore mode) to N, either in N's M2 next to backups/restore or as N M3, with a D-N decision, and assigns R11 to a named milestone. P adds R5 and R17 to its M4 row, or moves R5 into M3, since M's deletions outbox is on main. Q adds R18 to its M2 'Depends on', replaces the 'None is in a llm-observer plan yet' line with the actual owners, and the roadmap Q row records these dependencies.

**Verifier.** The reviewer misstated one detail. Q's M2 row (cetra-control/docs/00-design.md § Milestones) does list 'R1–R5, R7, R13–R16' as dependencies, not just P's M2. Q's design (line 136) also says the coordinator assigns the remaining requests before Q's M2 plan is written. Still, the ownership gaps are real. D-P-43 (mvp-decisions.md line 534; p-entitlements/00-design.md line 106) sends R7, R13 and R16 to 'a small auth track' that has no row in docs/roadmap.md and no plan. It sends R5 and R17 to P's M4, but the P M4 milestone row (00-design.md line 231) lists neither. R14 is in P's M3 (D-P-39, P8). R18 is marked 'Q M2 (before the first customer)' in Q's table, yet it is missing from Q's M2 dependency list and appears in no llm-observer doc. R11 (/metrics, Q M6) is deferred by N's README line 77 ('comes just before release') with no owner. The Q row in the roadmap still says 'Depends on P' and 'Not planned'.

### X26. The 'erased within 24 h' promise ignores `gc.grace` (now up to 30 days; 8 days on Q cells)

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator (N edits spec 03) · **Tracks:** A1, M, N, Q
- **Reviewer ids:** LC-5, SC-7, DS-2
- **References:**
  - docs/spec/03-storage.md § 7 'Guarantee' (~line 416) and § 5 'GC of obsolete files' (line 274)
  - docs/plans/m-deletion/09-web-data-tab.md:138,142; 10-web-actions.md:68,83; 11-web-drop.md:52,61; 04-api.md:83
  - docs/plans/n-packaging/m2/00-design.md § 'S3 object retention: raise gc.grace's cap (D-N-20)' lines 234-243; D-N-20 row line 307
  - docs/plans/n-packaging/m2/01-gc-grace.md (edits spec 03 §5 only)
  - cetra-control `docs/`00-design.md D-Q-12 (line 40)
  - docs/spec/12-operations.md § Backups 'GDPR and backups' (line 119)

**Problem.** Even before N, the guarantee was really 24 h + `gc.grace` (default 5 min, cap 24 h). N M2 raises the cap to 30 days and Q uses 8 days, but only N's self-hosting docs and Q's decision say so. Spec 03 §7's Guarantee, M's API doc comment and M's UI copy still promise erasure within 24 h. This covers deletions, project/org drops and retention, since all of them obsolete files that GC removes.

**Failure scenario.** On a cloud cell with `gc.grace = 8d`, a customer deletes an end user. The Data tab shows 'Erased' and the dialog promised 24 hours. The user's spans remain readable in the bucket's obsolete Parquet files for 8 more days, plus 14 days of noncurrent versions. The same holds for project and org drops and for retention. A DSAR response based on 24 h is wrong.

**Fix.** The coordinator picks between (a) a short GC grace of its own for files obsoleted by the deletion rewriter, a drop or retention, with gc-orphans and reapply tolerating their absence, and (b) rewording: spec 03 §7 Guarantee, M's 04-api doc comment and the M9-M11 copy say 'hidden within seconds, removed from live data within 24 h (deletion.max_delay), and from obsolete files within storage.gc.grace'. With (b), N's B1 also edits spec 03 §7.

**Verifier.** Spec 03 §7 (line ~416) still says "the bytes are gone from Parquet files within `deletion.max_delay` (default 24 h)", and M's UI copy (09-web-data-tab.md:138/142, 10-web-actions.md:68/83, 11-web-drop.md:52/61) and 04-api.md:83 promise erasure within 24 h. GC removes obsolete files only after `obsoleted_at + gc.grace` (spec 03 §5). N M2 D-N-20 and 00-design.md lines 236-242 raise the `gc.grace` cap to 30d and admit that erased data stays in obsolete files for that long. N's B1 (01-gc-grace.md) edits only spec 03 §5 and spec 12, never §7's Guarantee. Q's D-Q-12 sets 8d and computes 23 days end to end.

### X27. `/readyz` checks only `meta.db` and the WALs; spec 12 and Q assume object store coverage

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, N, Q, R
- **Reviewer ids:** SC-8
- **References:**
  - crates/server/src/http/health.rs lines 1-44 (`readyz`, module comment)
  - crates/storage/src/lib.rs `StorageHealth` (~lines 131-140)
  - docs/spec/12-operations.md § Process lifecycle (line 82)
  - cetra-control `docs/`00-design.md D-Q-13 (line 41)
  - docs/plans/n-packaging/README.md § Requests for Q item 2 (line 468)
  - docs/mvp-decisions.md D-N-4, D-N-10 (container healthcheck and Helm readiness on /readyz)
  - claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md D-R-6 (lines 81-84), § Scope of edits (line 167)

**Problem.** Spec 12 promises that `/readyz` covers the object store and free disk. Code and N's own statement to Q say it covers only `meta.db` and the WALs. Q's only external health signal for a cell (D-Q-13) and N's container healthcheck (`cetra health`) rely on it. R's D-R-6 makes the storage 'report unhealthy' after a failed fsync, but `StorageHealth` has no such field, R's scope excludes `crates/server`, and no doc says who wires it into `/readyz`.

**Failure scenario.** The Hetzner bucket becomes unreachable, or the NVMe fills below `min_free_bytes`. Appends answer 503 and flushes fail, but `/readyz` stays 200. UptimeRobot never alerts, and the container healthcheck keeps the container 'healthy'. After R's M2, a catalog committer stopped by a failed fsync is likewise invisible to probes.

**Fix.** Coordinator: assign the spec-12 readiness checks (object store reachable cached 10 s, free disk against the existing M6.2 ingest-refusal threshold, and a storage-unhealthy flag in StorageHealth) to a track, or narrow spec 12 to the current behaviour. Q: correct D-Q-13 and add object-store and disk probes to its host-health script until /readyz covers them. R: list 'StorageHealth gains an unhealthy flag and /readyz reports it' as an explicit request in its M2 plan.

**Verifier.** crates/server/src/http/health.rs readyz checks only meta.ping and spans/scores WAL failure. Its module comment admits that object-store and free-disk checks 'arrive with the tracks that own them', and no track owns them. Spec 12 line 82 promises 'object store reachable (cached 10 s), free disk above threshold'. Q's D-Q-13 says /readyz 'already covers the WAL, SQLite, the object store and free disk' and is Q's only external health signal in M1. N's README line 468 correctly says '(meta.db and WALs)'. R's D-R-6 (00-design.md lines 81-84) has the store/committer 'report the storage unhealthy', but StorageHealth (crates/storage/src/lib.rs ~131-140) has no such field, and R's scope excludes the server (line 167: server changes requested from the coordinator), so nobody has requested the readiness wiring.

### X28. Teams SSO has no owner on the cloud after D-P-41, and Q's Teams token still carries `org_sso`

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator (Q and P edit) · **Tracks:** P, Q
- **Reviewer ids:** OW-7, CL-7
- **References:**
  - docs/mvp-decisions.md:532 D-P-41, :503 D-P-12, D-P-10
  - docs/plans/p-entitlements/00-design.md:53 D-P-12, :198-200 licence payload, :209 org token example with org_sso, :232 M5 row
  - cetra-control `docs/`00-design.md:174 Plan values, :184 M3, :195 Open items

**Problem.** After D-P-41, Q's design still treats Teams SSO as undecided. M3 gates Teams sales on P's M5, which now serves self-hosted only. Q's Teams token still carries `org_sso`, which a cloud cell must never act on, and no Q milestone holds the IdP federation work. On P's side, D-P-12 and the examples still tie `org_sso` to Teams and to org tokens, with no licence example for self-hosted M5.

**Failure scenario.** Q sells the Teams add-on at M3 as its plan says, relying on D-P-41's federation. No milestone builds it, so Teams customers get `project_roles` but no SSO. Separately, P's M5 is implemented gated on `org_sso`: a self-hosted customer with a licence cannot use it (the licence has no `org_sso`), and on a cloud cell it is dead code granted by a token that Q never wants to use.

**Fix.** Q records D-P-41 as a D-Q decision, removes the Open item, puts the federation work (IdP registration, discovery by email domain, enforcement on Q's login) into M3 or a new milestone, and changes M3's Teams gate to that work. Q's Teams catalog drops `org_sso`. P updates D-P-12 to '`org_sso` (ee; licence, self-hosted; M5)' and adds it to the licence example.

**Verifier.** D-P-41 (mvp-decisions.md:532) settles that the control plane federates Teams SSO and that P's M5 serves self-hosted only, and P's M5 row says this (00-design.md:232). Q's docs were not updated (last change in commit 7317ca9/9f38cd8). § Open items (00-design.md:195) still says 'Decide with the coordinator before M3'. The Teams catalog still grants `org_sso` (00-design.md:174). M3 says 'Teams is sold only once P's M5 (or the alternative in § Open items) delivers its SSO' (00-design.md:184), and no Q milestone holds the federation work. The self-hosted half is weaker: D-P-10 lets licence features through and the licence payload is only an example, so `org_sso` can be granted by a licence. Only the 'Teams' label in D-P-12 and the example are misleading there.

### X29. Custom model prices and `/models` are MVP scope but assigned to no track

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** B1, G, I, coordinator
- **Reviewer ids:** DS-6
- **References:**
  - docs/mvp-decisions.md line 12 (MVP scope: "LiteLLM import + per-project custom prices") and line 319 (D-G-10: "belong to a track still to be assigned")
  - docs/plans/g-spine/00-design.md lines 26, 75, 306 ("Coordinator: assign `model_prices` and `/models` (D-G-10) to a track")
  - docs/plans/i-online-eval/00-design.md line 87 ("custom model prices (`model_prices`, D-G-10) [unassigned]")
  - docs/spec/09-api.md lines 134-135; docs/spec/04-metadata-db.md line 202
  - docs/mvp-decisions.md line 418 (D-I-15 "needs a judge model with a price (else 400)"), line 407 (D-I-4); docs/spec/12-operations.md § Security checklist (local Ollama/vLLM)
  - crates/server/src/ingest/mod.rs:166 ("The track that adds custom prices removes the `cfg`")
  - docs/roadmap.md: no row mentions model_prices or /models

**Problem.** G postponed the price table, `/models` and custom prices to 'a track to be assigned', and I repeated that in its design. Eight waves of roadmap later, no row owns them, yet spec 09 and 04 and the MVP scope list them.

**Failure scenario.** A user adds a local Ollama or vLLM connection (explicitly supported, D-I-4) and wants a daily cost cap on the rule. Without custom prices the model has no price, so D-I-15 refuses to save a budgeted rule with 400 and there is no way to add the price. Cost columns are null for such models, and judge-cost accounting in `usage_daily` shows zero.

**Fix.** Add a roadmap row, for example inside I's M5/M6 or a small track, that owns `model_prices`, `/models`, the server `PriceTable` swap and its UI. Link it from D-G-10 and D-I-15. Or cut the scope item and the spec 09 rows by a decision.

**Verifier.** A grep across docs shows `model_prices` and `/models` only in G's and I's "[unassigned]" notes, in spec 04/09 and in B1's README. No roadmap row or plan owns them, yet the MVP scope and spec 09 list them. D-I-15 refuses a budgeted rule when the judge model has no price, and a local or custom model has no built-in price, so the user cannot set a budget for it.

### X30. The format-compatibility policy is stated three incompatible ways; D-R-12 is cross-cutting

- **Severity:** major · **Verdict:** PLAUSIBLE · **Owner:** coordinator · **Tracks:** N, Q, R, coordinator
- **Reviewer ids:** DS-7
- **References:**
  - AGENTS.md line 16 ("unreleased. There are no users, no stored data") and line 34 ("No ... versioned codecs ... or dual read/write paths")
  - docs/spec/12-operations.md § Upgrades (line ~127): "WAL format carries a version byte in each segment header; a new binary reads the previous WAL version"
  - branch claude/r-hardening-design-43eb31: docs/mvp-decisions.md D-R-12 and docs/plans/r-hardening/00-design.md § M8 lines 149-163 ("Before the first public release: no constraint (no users, `AGENTS.md`)"; "M8's plan is written once the release date is close")
  - docs/mvp-decisions.md D-N-6 (line 547), D-N-12 (line 553)
  - cetra-control `docs/`decisions.md D-Q-8 (interim image from a pinned public commit); 00-design.md § Milestones M1 (eu1 cell, ops org) and M2 (invite-only customers)

**Problem.** Three documents disagree on storage format compatibility. Spec 12 § Upgrades requires reading the previous WAL version now. AGENTS.md forbids versioned codecs. D-R-12 (R branch) has no constraint until the "first public release", which nothing defines. Q's cloud cells (eu1 in M1, invited customers in M2) may hold data that must survive while still before that trigger. Then a storage format change in A1, R or M ships without a migration, and M8's golden-directory tooling is not built yet.

**Failure scenario.** The first cloud cell takes customer data on a pre-public build. A later storage change (a P/N/M format tweak) needs an open-time migration that AGENTS.md says not to write, and R's golden-directory CI does not exist yet. The cell cannot open its own data after the next deploy.

**Fix.** Have the coordinator define the trigger in a decision as the first deployment holding data that must survive (Q's first cell holding customer data, or the first public tag, whichever comes first). Align spec 12 § Upgrades (drop or defer the WAL previous-version rule) and AGENTS.md with it, and have R reference it from D-R-12 and schedule M8 before that point.

**Verifier.** Spec 12 on main already requires a new binary to read the previous WAL version, which AGENTS.md's "no versioned codecs" rule forbids, and D-R-12 says there is no constraint before release. That three-way conflict is real. The trigger "first public release" is not defined against Q, whose cells hold ops data from M1 and invited customers' data from M2, built from pinned commits (D-Q-8), possibly before any stable tag (D-N-6). The reviewer overstates in two places: D-R-12 covers only storage-crate formats (WAL, Parquet, catalog.db, object paths, read cache), not every track, and it is a user decision that is consistent with AGENTS.md today.

### X31. No plan tells implementers to write CHANGELOG lines

- **Severity:** major · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, H, I, K, L, M, N, P, R, coordinator
- **Reviewer ids:** PR-2
- **References:**
  - AGENTS.md § Commits and reports (CHANGELOG line per user-visible change, Upgrade notes, D-N-5)
  - docs/mvp-decisions.md line 546 (D-N-5)
  - CHANGELOG.md [Unreleased]: one 'Added' bullet (track N)
  - grep -rli changelog docs/plans/*: 0 files in every plan directory except n-packaging (5); 0 in r-hardening/01-m1-model.md on claude/r-hardening-design-43eb31
  - tools/packaging/release-prep.sh lines 32-37 (only checks that [Unreleased] is non-empty)
  - tools/packaging/release-notes.sh lines 13-22

**Problem.** D-N-5 and AGENTS.md require a changelog line per user-visible change. Only track N's plans carry the step. Tracks G, H, I, K, L, M and P have shipped or planned user-visible features (LLM connections and the startup encryption key, alerts, deletion and retention, entitlements and the plan page, admin UI) with no entry, and the in-flight plans (I M3/M4, P M3 with the 402 cap and SDK pause, N M2's new admin commands, R) do not add one either. The release tooling is satisfied by the single track-N bullet.

**Failure scenario.** The first tagged release passes release-prep and release-notes with release notes that mention only packaging. Upgrade-relevant items (the startup encryption key and its backup, forward-only meta.db migrations, the 402 hard cap on Free) have no '### Upgrade notes' entry, contradicting the promise in spec 12 and D-N-12.

**Fix.** Add a standing line to the plan conventions (and to AGENTS.md § Commits): the closing sub-phase of every milestone adds its CHANGELOG lines and Upgrade notes, and its 'Done when' lists it. Do one backfill commit now for G, H, I M1-M2, K, L, M, P M1-M2. Optionally make release-prep warn when [Unreleased] has fewer bullets than merged tracks.

**Verifier.** Checked: CHANGELOG.md [Unreleased] holds only track N's packaging bullet. No plan directory other than n-packaging mentions the changelog, even though G, H, I, K, L, M and P shipped user-visible features. release-prep.sh only rejects an empty [Unreleased] section, so it is already satisfied. The rule from N's D-N-5 and AGENTS.md is not carried into any other track's plan steps.

### X32. Specs and plain decisions still describe things later decisions changed

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, I, N, P, coordinator
- **Reviewer ids:** DS-11
- **References:**
  - docs/spec/08-auth.md line 83 (end): "Any signed-in user may create an org and becomes its owner" vs D-P-31 (orgs.user_create); line 101: "The principal is a session user, a PAT user, or an API key" vs D-P-28 and crates/server/src/auth/principal.rs line 80 Principal::ControlPlane
  - docs/spec/12-operations.md line 81 "stop workers (leases expire and are re-queued)" vs D-I-12 (supersedes spec 06's lease); line 86 "D-P-1–D-P-25" (D-P goes past 43)
  - docs/spec/README.md S12 "Static assets embedded with memory-serve" vs D-G-35
  - docs/mvp-decisions.md D52 (5 crates), D139, D172 "(customer, features, limits, expiry)" vs D-P-7, D179 vs D-P-10, D-G-11 "track N" vs D179
  - docs/roadmap.md G row "D-G-1–D-G-37" (D-G-38, D-G-39 exist); A2 row "D91–D101" (D-A2-1..4 exist); Order paragraph "Next: H and I, then M" while H is Implemented and M Done except M8

**Problem.** As stated, minus D-G-3 (already carries an 'Amended by D-P-17 and D-P-19' note). D-P-7 itself says it amends D172 but D172 is not annotated. The D179 vs D-P-10 ordering difference is mostly wording (the org token carries plan plus overrides).

**Failure scenario.** An implementer of the next track reads spec 12's shutdown text and writes lease-expiry handling that I removed. A reader of spec 08 assumes only three principal kinds, so a new route is declared without thinking about `ControlPlane`. D172's 'limits' leads to a limits field in licence keys that D-P-7 dropped.

**Fix.** One cleanup PR: annotate D52, D139, D172, D179 and D-G-11 with 'amended by'; update spec 08 lines 83 and 101, spec 12 lines 81 and 86, README S12; update roadmap G and A2 decision ranges and the Order paragraph.

**Verifier.** Each cited stale line was checked on both sides and is real; however D-G-3 is already annotated ("Amended by D-P-17 and D-P-19: track P owns them"), so it should be dropped from the list.

### X33. `docs/roadmap.md` rows and Order paragraph disagree with the plans

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** H, K, N, P, Q, R, coordinator
- **Reviewer ids:** OW-10
- **References:**
  - docs/roadmap.md § Wave 2 intro and § Order (line after the Wave 2 table)
  - docs/roadmap.md rows H, K, M, N, P, Q
  - docs/roadmap.md § Decision IDs (lines 15-23)
  - cetra-control `docs/`00-design.md requests R9/R10 (lines 148-149), M1 row (line 182), line 189
  - docs/plans/n-packaging/m2/00-design.md lines 4-7

**Problem.** The Order paragraph is stale: it still schedules H, I and M as next and says P's design can start. Row Q says 'Not planned' and depends only on P, while Q has a design and an M1 plan whose go-live needs N's M2 (R9 and R10). N's M2 is planned but has no place in the Order paragraph. The roadmap also does not say that Q's decisions live outside mvp-decisions.md.

**Failure scenario.** A coordinator or implementer reading the roadmap on main schedules Q from scratch, or starts Q's M1 go-live without N's M2, and does not know that M8, P's M4, N's M2 and the auth track form the critical path for Q's M2. Sessions are started in an order that cannot work.

**Fix.** Rewrite § Order as a dependency chain: I M3 then M M8; I M3, K and M M6 then P M4; N M2 then Q M1 go-live; P M3 and M4 then Q's later milestones. Update row Q's Plan and Status columns and give it N M2 as a dependency, and state where D-Q decisions live. Add row R when its branch merges.

**Verifier.** On main, roadmap § Order still says 'P's design can start now' and 'Next: H and I, then M'. The rows say H is Implemented, K, L and G are Done, M is done except M8, and P's M1 and M2 are merged with M3 planned. Row Q says 'Not planned' and depends only on P, but cetra-control/docs/00-design.md has a design, a 2005-line M1 plan, and an M1 go-live that waits on R9 and R10, which N's M2 implements (n-packaging/m2/00-design.md line 5). § Decision IDs puts every track's decisions in mvp-decisions.md, but Q keeps its D-Q decisions in its private repo. I did not check the 'missing auth track (OW-1)' part.

### X34. Q's decisions and status are invisible in llm-observer; decision-ID registry drift

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, P, Q, R, coordinator
- **Reviewer ids:** DS-10, CL-10, PR-8
- **References:**
  - docs/roadmap.md Wave 2 Q row: "the org → cell mapping and request routing (D170)" | plan "—" | "Not planned" | decisions "—"
  - docs/mvp-decisions.md line 223 D170: "the org → cell mapping and routing"; 8 D-P decisions cite "Q's R<n>" (e.g. D-P-31 "Q's R4")
  - cetra-control `docs/`00-design.md line 34 D-Q-6 (customers use the cell hostname directly; "no request ro[uter]")
  - cetra-control `docs/`decisions.md header (never restates plain decisions) and D-Q-1..D-Q-27

**Problem.** Q has a design, a written M1 plan and 27 decisions in a private repository, but llm-observer's roadmap says 'Not planned' and mvp-decisions has no Track Q section. Many D-P decisions refer to R-numbers that readers of this repository cannot resolve. D-Q-6 (customers use the cell hostname, no routing) contradicts D170 and the roadmap row ('request routing'); Q's decisions.md says it never restates plain decisions.

**Failure scenario.** A reader of D170 or the roadmap plans for a routing layer that Q has decided not to build. An agent planning P or N cannot find why `audience` is the cell hostname or what R5 means.

**Fix.** Update the roadmap Q row (design and M1 plan written, location, decisions D-Q-1..27) and add a short Track Q section to mvp-decisions pointing at the private repo. Amend D170 ('routing' becomes 'a per-cell hostname, no router, D-Q-6').

**Verifier.** Q has a 37K design, a 77K M1 plan and 27 decisions, but the llm-observer roadmap still says plan '—', 'Not planned', decisions '—', and mvp-decisions has no Track Q section; D-Q-6 explicitly drops a request router while D170 and the roadmap row still say 'routing', and neither side amends the other.

### X35. `meta.db` migration numbers are hard-coded in plans and collide on parallel merges

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** H, I, J, K, M, N, P, coordinator
- **Reviewer ids:** PR-1
- **References:**
  - crates/server/migrations/meta/ (0001..0008 on main)
  - docs/plans/i-online-eval/00-design.md line 81 (§ Scope: 'migrations 0004, 0006 and 0009–0011') and lines 150, 200, 246, 254 (file names 0009_eval_queue, 0010_pending_traces, 0011_backfills)
  - docs/plans/i-online-eval/03-m3-queue-rules.md § The migration number (lines 147-156)
  - docs/plans/i-online-eval/04-m4-triggers.md lines 101, 263-265 (00MM_pending_traces.sql)
  - docs/plans/p-entitlements/03-m3-units-and-limits.md line 425 (NNNN_usage_notices.sql)
  - docs/plans/m-deletion/README.md line 247; docs/plans/k-alerts/README.md line 124; docs/plans/p-entitlements/02-m2-org-entitlements.md line 490
  - docs/spec/04-metadata-db.md line 504
  - crates/server/src/meta/mod.rs expected_version() lines 224-231; open_twice_is_idempotent lines 385-395
  - merge commits 68937f2, c5a09d9 (deletions renumbered to 0006 then 0007), 355a602, 60deeff

**Problem.** Migration numbers are fixed only by merge order. Every plan already states the same renumber-on-rebase rule. The real drift is the hard-coded literals in i-online-eval/00-design.md (§ Scope line 81 and the § Tables file names 0009/0010/0011), which go stale when P M3's usage_notices merges between I milestones. Nothing in the roadmap shows that I M3 and P M3 will both take the next number at the same time. Duplicate prefixes surface only through the meta_schema snapshot or the migration tests, not as a git conflict.

**Failure scenario.** I M3 is being implemented as 0009_eval_queue.sql while P M3's P6 is told to take 'the next free number', which on a base without I M3 is also 0009. Whichever merges second gets a clean git merge, a broken build or schema test, and a conflicting meta_schema.snap. I's design also pre-assigns 0010 and 0011, which become wrong the moment P M3 merges between I M3 and I M4. Docs that cite numbers (spec 04 line 504, I design file names) go stale each time.

**Fix.** Replace the 0009-0011 literals in the I design with NNNN/stems. Put the one renumber rule in AGENTS.md § Parallel work and have plans cite it. Optionally add a cheap check that prefixes are unique and contiguous, and a short 'pending meta.db migrations' note in docs/roadmap.md listing I M3, I M4, I M6 and P M3. Keep the contiguous numbering that expected_version() and open_twice_is_idempotent rely on.

**Verifier.** The facts hold: main has 0001-0008, the I design hard-codes 0009-0011, P M3 takes 'the next free number', and four merges renumbered migrations by hand. But every plan already carries the same procedure (next free number when you start, the second to merge renumbers on rebase, a merged migration is never renumbered). I M3 names NN, lists what depends on it, and says to fix the design's 0009 if NN differs. I M4 uses 00MM. Spec 04's 'migration 0004' is correct today. Collisions are expected and handled, so this is friction and drifting design literals, not a break.

### X36. AGENTS.md says `just check` equals CI, but `ci-ok` also needs e2e, workflows and packaging-lint

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, N, R, coordinator
- **Reviewer ids:** PR-3
- **References:**
  - AGENTS.md § CI ('A branch that passes `just check` locally and leaves a clean tree passes CI, except for the runtime matrix')
  - justfile line 75 (check: check-rust check-web check-py check-ts)
  - .github/workflows/ci.yml lines 189, 213, 227 (just check-workflows, just e2e, just check-packaging-lint) and ci-ok needs (lines 232-244: workflows, e2e, packaging)
  - docs/mvp-decisions.md D-N-13
  - git show claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md lines 106, 167 (.github/workflows/nightly.yml in M3)
  - justfile recipes missing from AGENTS.md § Commands: api-examples, fixtures-check, fixtures-bless, fixtures-fmt, build-release, js-install, fmt-check, release-notes, release-verify, prices-update

**Problem.** The CI sentence became false when the e2e, workflows and packaging jobs joined ci-ok (D-N-13 split cheap PR checks from the full `check-packaging`). Plans and AGENTS.md disagree on when to run which recipe: some plans end at `just check`, others add `just e2e`. Several recipes the plans rely on (api-examples, fixtures-check) are missing from the table the file calls the single reference.

**Failure scenario.** An implementer of a plan that edits `.github/` (R's M3 nightly.yml), `deploy/`, `tools/e2e` or SDK behaviour gets a green `just check` per the plan, pushes, and fails ci-ok on zizmor/actionlint, the real-server SDK suites or the chart/compose lint.

**Fix.** Correct the sentence and name the three extra jobs. Add a `just ci` recipe (check + check-workflows + e2e + check-packaging-lint) and a path rule ('touched .github/ -> check-workflows; touched deploy/, tools/packaging -> check-packaging-lint; touched ingest, SDKs, web wiring -> e2e'). Complete the commands table and Layout (deploy/, .github/). Plans cite AGENTS.md instead of repeating command lists.

**Verifier.** ci-ok requires the workflows, e2e and packaging jobs, and `just check` runs none of them, so the AGENTS.md sentence is false. The justfile has recipes the commands table does not list. R's design edits .github/workflows/nightly.yml in M3, which ci-ok's actionlint/zizmor job would check but `just check` would not. This is drift that misleads, so minor is right.

### X37. A user-specific nvm path is copied into ~50 plan files

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** B1, B2, C, G, H, I, K, L, M, N, P, R
- **Reviewer ids:** PR-4
- **References:**
  - grep: 'nvm/v26' 66 occurrences in 50 files under docs/
  - docs/plans/b1-normalization/README.md line 175 and docs/plans/b2-capture/README.md line 169 ('the PATH needs Node; see AGENTS.md § Commands')
  - AGENTS.md § Commands / Required tools: no mention of PATH or nvm
  - docs/plans/a2-fixes/README.md line 89 (version-agnostic wording)
  - .node-version = 26.13.0
  - r-hardening/01-m1-model.md line 11 on claude/r-hardening-design-43eb31

**Problem.** The Node-PATH workaround lives only in plans, as a hard-coded home-directory path and version. Two plan READMEs point at an AGENTS.md section that does not say it. Bumping .node-version silently invalidates 50 files, and an implementer who follows the pointer finds nothing.

**Failure scenario.** Node is bumped to 26.14 in .node-version. A track-I implementer pastes the plan's export, `just check-web` runs without Node or with the old one, and the failure looks like a project problem. A fresh implementer following b1/b2 'see AGENTS.md' has no instructions.

**Fix.** Add a version-agnostic line to AGENTS.md § Commands, like a2-fixes line 89: put the Node version from .node-version first on PATH before any just recipe that runs web or TS work. Keep the home-directory nvm path out of the checked-in AGENTS.md, since it is specific to one machine; it already lives in the user's memory notes. Optionally make js-install fail with a clear message when node is missing or does not match .node-version. Future plans cite AGENTS.md instead of copying the export, and the existing copies can be swept mechanically.

**Verifier.** Checked: 66 copies of the hard-coded nvm v26 path across 50 plan files. AGENTS.md has nothing about PATH or nvm, so the b1/b2 pointers lead nowhere. It is doc hygiene more than a track seam, and b1/b2 are finished tracks, so the impact is low.

### X38. Shared generated files and fixture IDs have no ownership or conflict rule

- **Severity:** minor · **Verdict:** CONFIRMED · **Owner:** coordinator · **Tracks:** G, I, K, M, P, coordinator
- **Reviewer ids:** PR-9
- **References:**
  - fixtures/api/README.md § Fixed identifiers (Deletions and Evaluator rows)
  - fixtures/api/static/deletions.json:42, org-deletions.json:56 (`…0900`)
  - fixtures/api/static/channels.json:36, alert-rule.json:14, evaluator.json:2 (`…0500`)
  - docs/plans/i-online-eval/03-m3-queue-rules.md § Fixed identifiers (fixtures and tests), Eval job (done) row
  - docs/plans/i-online-eval/00-design.md:107
  - AGENTS.md § Parallel work and ownership

**Problem.** The ID collisions span different entity types, so nothing breaks at runtime: UUIDs are unique per table. But the README table is incomplete: K's channel `…0500` has no row and duplicates the evaluator id. I M3 (not yet merged) plans a score with `…0900`, which the README already gives to deletions. No document says how to resolve conflicts in generated files.

**Failure scenario.** I M3 and P M3 both regenerate openapi.json and the api-examples manifest. The second rebase produces a large textual conflict in generated files, someone merges by hand and the next `just openapi` or api_examples::examples_are_up_to_date run rewrites it, which makes the diff noisy and can drop an operation. A reader cannot tell which 0500 a mock refers to.

**Fix.** Add per-type ID blocks to fixtures/api/README.md and add the K rows (give the channel and alert rule ids outside 05xx, or document the reuse). Move I M3's score id off `…0900` before M3 is implemented. Optionally add one line to AGENTS.md § Parallel work: on a conflict in a generated file, take main's version and regenerate (`just openapi`, api-examples, snapshot update); the branch that merges second does it.

**Verifier.** fixtures/api/README.md § Fixed identifiers gives `…0900` to Deletions (used in static/deletions.json:42 and org-deletions.json:56). docs/plans/i-online-eval/03-m3-queue-rules.md § Fixed identifiers (line ~164) gives the done eval job's score `…000000000900`. `…0500` is the Evaluator in the README and evaluator.json:2, and also the id of the K channel in channels.json:36, which alert-rule.json:14 references; the README has no channel or alert-rule rows. I found no rule anywhere in AGENTS.md, the roadmap or the plans for merging generated files (openapi.json, schema.gen.ts, manifest, snapshots) when they conflict. i-online-eval/00-design.md:107 only lists those files as shared.


## Systemic process fixes

These come from X25, X31, X34–X38 and the repeated collisions behind several findings above. Each is a coordinator change unless noted.

1. **Migration numbers (four renumber merges so far, three more queued).** Keep contiguous numbering (`expected_version()` and the idempotence test rely on it), but stop writing literal numbers in designs: plans say `NNNN_<stem>.sql`. Put the single rule in AGENTS.md § Parallel work: "take the next free number on `origin/main` when you start; whoever merges second renumbers on rebase and regenerates `meta_schema.snap`; a merged migration is never renumbered." Add a "pending `meta.db` migrations" list to `docs/roadmap.md` (today: I M3, I M4, I M6, P M3) and a cheap `just check` test that prefixes are unique and contiguous. Timestamped names were considered and rejected: they break the contiguous-version invariant for no gain on a single-writer schema.
2. **Cross-repository requests.** Keep one registry of Q's requests (R1–R18) in `docs/roadmap.md`, one row per request: owning track, milestone, decision (D-P-n, D-N-n) and status. Q's design points to it instead of keeping its own copy. A track that answers a request differently from how Q asked (R10 → D-N-20) updates the row and tells Q in the same change.
3. **Decision IDs.** Register Q's decision range (`D-Q-n`, kept in `cetra-control`) in `docs/roadmap.md` § Decision IDs, so that a Q decision that narrows a plain decision (D-Q-6 vs D170) is visible here. Rename Q's request ids (`R1`…) to `Q-R1`… so they cannot be confused with track R. Cross-cutting choices made inside a track (D-R-12 on format compatibility, D-N-20 on erasure timing) go to the coordinator as plain decisions.
4. **Changelog lines (D-N-5).** Add a "CHANGELOG" line to the plan template and to every sub-phase that changes user-visible behaviour, and a `just check` step that fails when `CHANGELOG.md` § `[Unreleased]` is unchanged on a branch that touches `crates/`, `web/` or `sdks/` without a `[no-changelog]` marker in the commit message.
5. **`just check` versus CI.** Fix AGENTS.md § CI: `ci-ok` also needs `just e2e`, `just check-workflows` and `just check-packaging-lint`. Plans that touch packaging or workflows list those recipes in their check step.
6. **Node PATH.** Remove the user-specific nvm path from the ~50 plan files and the reference to a non-existent AGENTS.md section. Put one line in AGENTS.md § Commands ("Node must be on PATH; see `.node-version`") and leave the local path in the user's environment.
7. **Shared generated files and fixture IDs.** Name owners: `crates/server/openapi.json` and `web/src/api/schema.gen.ts` are regenerated (never hand-merged) by whoever merges second; fixed UUIDs in `fixtures/api` are allocated per track (for example, by a track-specific prefix byte) so mocks stop colliding.

## Dropped findings

No finding was refuted by its verifier. The following were dropped only as duplicates and merged into the finding named:

| Reviewer id | Merged into |
|---|---|
| DS-1, OW-4, LC-7, CL-1 | X1 (SC-1) |
| CL-5, OW-4 (restore part) | X2 (SC-2) |
| DS-3, OW-2, LC-3 | X7 (SC-3) |
| LC-2 | X8 (OW-6) |
| SC-5, DS-4, LC-4, CL-2 | X9 (OW-5) |
| SC-3 (rule-limit part) | X10 (OW-8) |
| SC-4, OW-3 | X12 (LC-1) |
| DS-9, OW-9, LC-8, PR-5 | X20 (SC-11) |
| SC-6, DS-5, CL-4, PR-7 | X25 (OW-1) |
| SC-7, DS-2 | X26 (LC-5) |
| CL-7 | X28 (OW-7) |
| CL-10, PR-8 | X34 (DS-10) |
