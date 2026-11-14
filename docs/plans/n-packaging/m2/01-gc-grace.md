# B1 — `gc.grace` up to 30 days

Read [00-design.md](00-design.md) first, especially § Design › S3 object retention (D-N-20).

**Goal:** `storage.gc.grace` accepts up to 30 days instead of 24 hours. Its default stays 5 minutes.
In S3 mode it is the retention that keeps a metadata-only backup restorable.

**Edit only:** `crates/storage/src/config.rs`, `docs/spec/03-storage.md`, `docs/spec/12-operations.md`.

**Commit:** `Allow gc.grace up to 30 days`

## Changes

`crates/storage/src/config.rs`:

1. In `validate`, the upper bound becomes `Duration::from_secs(30 * 24 * 60 * 60)` and the message
   `"must be between 1s and 30d"`. Add a named constant next to `default_gc_grace`:

   ```rust
   /// Longest `gc.grace`: it is also the window a metadata-only S3 backup stays
   /// restorable for (D-N-20).
   const MAX_GC_GRACE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
   ```

   Use it in the check. The lower bound stays 1 s.
2. Extend `GcConfig::grace`'s doc comment with: `In S3 mode it is also how long a metadata-only
   backup can be restored against the live bucket (D-N-20); at most 30 days.`
3. Tests, in the existing invalid-config table:
   - the two existing rows change their message to `"storage.gc.grace: must be between 1s and 30d"`;
   - the `"25h"` row becomes `"31d"`.

   Add a test `gc_grace_accepts_thirty_days` that parses `{"data_dir":"/data","gc":{"grace":"30d"}}`
   (the same way the existing parse tests do) and asserts that `validate()` passes and that `grace` is
   30 days. `humantime` accepts the `d` unit.

`docs/spec/03-storage.md` § 5, in the "GC of obsolete files" paragraph: after "at least the longest
query timeout", insert "; at most 30 days. In S3 mode it is also how long a metadata-only backup stays
restorable against the live bucket (D-N-20)".

`docs/spec/12-operations.md`, configuration table row `storage.gc.grace`: the default cell becomes
`` `"5m"` (at most `"30d"`; the window a meta-only S3 backup stays restorable, D-N-20) ``.

## Verify

```bash
cargo test -p llmobs-storage --all-features config
just check
```

## Done when

- [ ] `"30d"` is accepted, `"31d"` and `"0s"` are refused with the new message.
- [ ] Spec 03 and spec 12 say so.
- [ ] `just check` passes. One commit: `Allow gc.grace up to 30 days`.
