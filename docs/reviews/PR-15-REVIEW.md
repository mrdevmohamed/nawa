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
- [ ] Re-run the full CI matrix on the final head.

## Plugin subsystem observations

The plugin subsystem has good foundations: manifests reject unknown fields, requested capabilities are intersected with administrator grants, privileged plugin paths are validated, the protocol is bounded, and plugin execution has no unsandboxed fallback. These are positive design properties, not a formal security proof.

Do not merge until all high-severity transaction findings are resolved and CI passes on the final head.
