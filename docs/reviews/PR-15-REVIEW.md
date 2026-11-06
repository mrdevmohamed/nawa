# PR #15 — Code Review and Remediation Log

**Pull request:** [mrdevmohamed/nawa#15](https://github.com/mrdevmohamed/nawa/pull/15)  
**Reviewed head:** `9d7cb6763da37c2d95f5db3e2df54175a4ead3f3`  
**Review date:** 2026-11-06  
**Initial recommendation:** Request changes before merge.

The review focused on transaction durability, crash recovery, undo, plugin execution boundaries, and package/group mutation paths. The initial CI run on the reviewed head passed all 12 reported checks; CodeRabbit was skipped by its repository eligibility condition. Passing CI did not cover the crash windows below.

## Findings

### R1 — Recovery discarded completed WAL steps after a crash
**Severity:** High · **Locations:** `src/transaction/recovery.rs`, `src/transaction/journal.rs`

A crash after `step_result()` but before `OperationHandle::finish()` left the initial `preparing` record with no package changes. The old recovery code treated an empty pending-step list as proof that the journal could be deleted and omitted steps whose results had already been written.

### R2 — Install/update WAL intents did not describe the intended installed state
**Severity:** High · **Locations:** `src/commands/installing.rs`, `src/commands/building.rs`, `src/upgrade.rs`

Several install paths wrote `target = None`, even though recovery interpreted a missing target as a removal. The recipe/repository path also passed an empty package name for upgrades, suppressed WAL errors, and used a helper that returned an empty version string.

### R3 — Undo performed the package mutation before writing its WAL intent
**Severity:** High · **Location:** `src/transaction/undo.rs`

`apply_undo()` removed or restored a package first, then wrote its intent and result. It ignored failures from both writes.

### R4 — Failed/partial undo permanently marked the original transaction as undone
**Severity:** High · **Locations:** `src/transaction/context.rs`, `src/transaction/undo.rs`, `src/transaction/history.rs`

The original record's `undone_by` annotation was written before compensation started. A partial failure still blocked another undo attempt.

### R5 — Undo removal order did not respect reverse dependencies
**Severity:** High · **Location:** `src/transaction/undo.rs`

The previous ordering preserved dependency-first install order for removals. Undoing an install could remove a dependency while its dependent remained installed.

### R6 — Group removal changed packages without per-package transaction steps
**Severity:** High · **Location:** `src/commands/groups.rs`

Group removal called `remove_with()` without step intents/results. Partial failure could leave removed packages absent from history.

### R7 — Upgrade ignored failure to persist the final transaction record
**Severity:** Medium · **Location:** `src/upgrade.rs`

The upgrade path discarded the result of `OperationHandle::finish()`.

### R8 — History undo attempted to acquire the same exclusive flock twice
**Severity:** High · **Locations:** `src/cli.rs`, `src/transaction/undo.rs`, `src/lock.rs`

`main::run()` already acquires an exclusive lock for `Cmd::History`, but `history_undo()` opened the lock file and requested another exclusive flock. Separate open file descriptions are not re-entrant, so `nawa history undo` could wait on a lock held by the same process.

## Remediation status

- [x] R1 — recovery consumes completed and pending WAL steps; inferred results are persisted.
- [x] R3 — undo writes intent before mutation and propagates WAL errors.
- [x] R4 — original transaction is marked undone only after successful finalization.
- [x] R5 — undo ordering respects dependency direction; partial undo can resume without replaying committed steps.
- [x] R8 — remove nested exclusive-lock acquisition.
- [x] Classify post-remove hook failures as committed package removals rather than failed mutations.
- [x] Resume partial undo without replaying committed compensations, and recognize package-complete partial records after hook failures.
- [x] Restore the original package install reason when reinstalling archived packages during history undo.
- [x] R2 — install, archive, build-install and repository-upgrade paths persist validated target snapshots and propagate WAL errors.
- [x] R6 — group removal records every package change as a WAL step and preserves membership for retries.
- [x] R7 — upgrade propagates transaction finalization errors.
- [x] Add regressions for completed-WAL crash recovery, legacy null-target ambiguity and dependency-aware undo order.
- [ ] Expand fault-injection coverage to every filesystem/DB commit boundary.
- [x] Re-run the full CI matrix on final head `a20d843ce4c082031df3ccd5f3340cbf96f218b2` — all 10 executable checks passed; CodeRabbit was skipped by repository eligibility.

## Plugin subsystem observations

The plugin subsystem has good foundations: manifests reject unknown fields, requested capabilities are intersected with administrator grants, privileged plugin paths are validated, the protocol is bounded, and plugin execution has no unsandboxed fallback. These are positive design properties, not a formal security proof.

R1–R8 have been remediated in the PR branch, and the final CI matrix passed on 2026-11-06. Keep the broader filesystem/database fault-injection expansion as follow-up hardening before treating transaction recovery as exhaustively proven.


### Additional integrity hardening

Install/update WAL target metadata is now tied to the exact archive digest passed into the installer. Local archive, source-build, and repository paths pin the snapshot by SHA-256 so the bytes installed cannot diverge from the identity recorded before mutation.


### CI follow-up

The first run after the implementation changes exposed Clippy-as-error findings, an undo-link test that did not apply the compensating package step, and an interrupt error conversion missed by an intermediate commit. The follow-up changes correct these issues. The latest PR head must still pass a fresh CI run before merge.


## Final verification — 2026-11-06

Final PR head: `a20d843ce4c082031df3ccd5f3340cbf96f218b2`  
GitHub Actions run: [42250148164](https://github.com/mrdevmohamed/nawa/actions/runs/42250148164)

All 10 executable jobs passed: root and user tests on Ubuntu 22.04, root tests on Ubuntu 24.04, CLI smoke tests, static musl build, Clippy/build/tests, and all four sandbox isolation modes (`none`, `env`, `sandbox`, `closure`). CodeRabbit remained skipped by its repository eligibility condition and was not counted as a passing review.

The remaining unchecked item is expanded fault-injection coverage across every filesystem/database commit boundary. This is additional hardening beyond the regressions added for the reproduced crash windows.


## Follow-up remediation — 2026-11-06

**Implementation code revision:** `d67dfa7570e52fdea5e66f37ce5db70518f7932c`

The follow-up review found and addressed three additional transaction-integrity gaps:

### R9 — Install reason was missing from undo conflict detection
`PackageState::matches_entry()` now compares a non-empty recorded install reason with the live entry. Legacy snapshots with an empty reason retain the identity/digest fallback. Regression coverage verifies that an identical archive/version promoted from `dependency` to `explicit` blocks an older undo, both at the state-comparison layer and through `plan_undo()`.

### R10 — `nawa rollback` was outside Transaction History
Rollback now opens an `OperationCommand::Rollback` operation, persists a `restore` intent before calling the existing verified archive restore path, and records the committed/failed/hook-failed result. The resulting transaction is compensatable by `nawa history undo`. Observer events distinguish a restore from an absent package (installed) from a restore over an existing package (upgraded). A regression test checks the generated history record and undo plan.

### R11 — History/WAL atomic renames lacked a durability boundary
Added `src/transaction/atomic.rs` and routed transaction records, administrative record rewrites, the `last-id` marker, WAL intents/results/state through it. It writes a unique temporary file, calls `sync_all()`, renames it, then syncs the containing directory. New transaction records refuse to overwrite an existing id. If `last-id` is absent or stale, allocation scans existing transaction records and WAL directories before selecting the next id.

### Fault-injection expansion
The atomic writer now has test-only fault points after the write, after file sync, and after rename. Tests assert that pre-rename failures preserve the previous file and clean the temporary, while a post-rename failure leaves a complete new file visible for recovery. Additional coverage exercises nested parent creation, a final history record surviving a crash before WAL cleanup, stale/missing id-marker recovery, reason-only undo conflicts, and rollback history.

### Verification status
The implementation changes above are committed to the PR branch. GitHub Actions verification for the updated branch is pending at the time this log entry was written; no new passing result is claimed here. The prior 10-job result cited above belongs to the earlier head and must not be treated as verification of this follow-up.


### CI-driven corrections after the first follow-up run

The first follow-up CI run ([42269118543](https://github.com/mrdevmohamed/nawa/actions/runs/42269118543)) correctly exposed two implementation issues before merge:

- The transaction store rejected the intentional `preparing` → terminal record transition. `write_record()` now permits only that transition after verifying the transaction's immutable identity fields; writes over an existing terminal record remain rejected.
- Clippy rejected an explicit `drop(recorder)`, an `.ok()` pattern, and test-only fault-point naming/initialization. These were corrected by scoping the recorder, matching `Ok(id)`, and making the fault-point variants and thread-local initialization Clippy-clean.

The rollback recorder now leaves scope before finalizing the operation. The latest code fixes are committed through `8d0fbfc411d4740fc8b02fd96c566c1e9a682266`. A fresh CI run on the new head is required; the failed run above is retained as evidence of the issues found and is not counted as passing.


The next run ([42269835662](https://github.com/mrdevmohamed/nawa/actions/runs/42269835662)) confirmed that the Clippy and transaction-finalization corrections compiled; its remaining test-build error was three test references still using the pre-rename fault-point variants. Those references now use `Wrote`, `Synced`, and `Published` in code revision `6c4e922f42196dad7b98e41ef8650b48657efda7`. This correction has not yet been verified by a new CI run.
