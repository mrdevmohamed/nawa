# Undo and WAL consolidation — design

**Status:** proposal, 2026-11-12. Phase 1 (FileJournal identity and the
combined physical→logical recovery entry point) is implemented on top of
`faa52ef`; no CLI, package, archive, db, index, signature, or transaction
record format changed.
**Scope:** remove the *duplication of truth* between the legacy package undo
record, the transaction history/WAL, and the physical file journal, while
preserving every existing command, format and recovery guarantee.
**Companion documents:** `docs/reviews/2026-11-09-nawa-audit.md` P1-2,
`docs/design/package-management-roadmap.md` §5.2/§5.5/§5.9/§5.10,
`docs/reviews/2026-11-09-final-report.md` §7.
**Verified against:** `faa52ef`.

---

## 1. Summary

Nawa currently has three mechanisms that are easy to conflate under the word
"undo":

| Mechanism | On-disk location | What it actually is |
|---|---|---|
| Legacy undo record | `var/lib/nawa/undo/<name>/{desc,files}` | A one-slot **logical** "previous version" cache used by `nawa rollback` |
| Transaction history + TxWAL | `var/lib/nawa/history/` | The **logical** operation journal used by `nawa history undo`, audit, and crash reconciliation |
| Physical file journal | `.nawa/bak-*.journal` + `bak-*` trees | A **physical** all-or-nothing journal for file moves and db-entry replacement; it is the only thing that can restore files after a power cut |

They are not interchangeable:

- the transaction WAL records package-level before/after identity, not which
  files were moved;
- the physical journal knows which files were moved, but not why, or which
  operation/step owns the change;
- the legacy undo record stores a full `Entry`, but only the most recent slot
  and with no transaction identity.

The audit's P1-2 finding is therefore not "delete one journal". It is:

1. make the transaction history the **single logical source** of "what was
   installed before" (the previous slot);
2. give the physical journal the **operation identity** so logical and
   physical recovery are one process, not two unrelated scans;
3. keep the legacy undo directory as a **read-compatible mirror** for roots
   and binaries that predate history, never as a second authority;
4. keep `nawa rollback <name>` and `nawa history undo <id>` with their current
   semantics and formats.

The migration is incremental. No phase changes the CLI, the package/archive
format, the repository index, signatures, the package database, or the
transaction record schema.

---

## 2. Terms used in this document

- **Legacy slot** — the `undo/<name>/desc` record. One per package, replaced
  on every package change. `files` is written beside it but has no reader in
  the current tree (see §3.1).
- **History record** — an immutable
  `history/transactions/<id>.toml` `TransactionRecord`.
- **TxWAL** — `history/journal/<id>/{state.toml,steps/*}`: write-ahead
  package-step intents and results. It is package-granular.
- **FileJournal** — the existing `install::journal::Tx` / `Step` format:
  percent-encoded `H`/`C`/`R`/`D`/`COMMIT` lines plus a backup tree. It is
  file-granular.
- **Previous slot** — the state a package had immediately before its latest
  recorded package attempt/change. `nawa rollback` consumes this concept.
- **Committed change** — a `PackageChange` whose status is `committed` or
  `committed-hook-failed`.

---

## 3. Baseline, verified in the tree

### 3.1 Legacy undo record

Writer: `install::payload::save_undo`, called

- in `install_archive_payload` after conflict checks and before caching/moving
  files, only when a previous `Entry` exists;
- in `remove_with` before hooks and file moves.

Layout:

```text
<root>/var/lib/nawa/undo/<name>/
├── desc    # toml::to_string_pretty(Entry)
└── files   # db file list, newline separated
```

Reader: `install::rollback::rollback` reads **only `desc`**. A search of the
current tree and of the initial history finds no reader of `undo/<name>/files`;
it is dead data kept by the writer. `desc` is replaced by `std::fs::write` to
`.desc.tmp` followed by `rename` (no fsync).

Semantics today:

- every attempted install/upgrade/remove that reaches `save_undo` overwrites
  the slot with the package's current entry;
- a fresh install does not create a slot (there is no previous version);
- `nawa rollback` goes exactly one slot back, so two rollbacks ping-pong;
- after a removal the slot holds the removed entry, so rollback reinstalls it;
- if the slot equals the installed version, rollback refuses with
  "`<name>` is already at `<version>`";
- a failed install/rollback that reached `save_undo` still overwrites the
  slot, so a retry may have lost the older target. This is current behavior
  and is deliberately preserved by the resolver rules in §6.

### 3.2 Transaction history and TxWAL

Entry point: `transaction::context::begin_operation` (under the exclusive db
lock). It allocates the id, writes `journal/<id>/state.toml`, and persists a
`preparing` record.

Per package step:

1. the caller writes a `StepIntent` containing the transaction id, step
   number, package, action, `before` `PackageState`, `target`
   `PackageState`, and artifact digest;
2. the existing installer/remover mutates files and db;
3. `record_install_outcome` / `record_remove_outcome` write the `StepResult`
   and append a `PackageChange` to the live record;
4. `OperationHandle::finish` writes the final immutable record, queues plugin
   events, and clears the TxWAL.

`PackageState` contains `version`, `rel`, `arch`, `reason`, and
`artifact_sha256` — enough to identify the previous artifact, but not `run`,
`install_date`, `size`, or hooks. The installer always re-reads those from the
archive, so a previous slot does not need them.

`nawa history undo <id>` (`undo::plan_undo` + `apply_undo`) is a separate,
all-preflighted compensating transaction:

- every committed step is compared with its recorded `after` state, and a
  later change is a conflict;
- every artifact is located and digest-verified before the first mutation;
- restore/remove go through the ordinary installer/remover;
- the result is a new history transaction linked with `undoes`/`undone_by`.

Recovery: `transaction::recovery::reconcile_incomplete` compares intents with
the live db. db matches target → committed; db matches precondition → failed;
neither matches → `needs-recovery`, and the journal is retained.

**Current call-site gap:** `reconcile_incomplete` is only called by
`cmd_history`. An install/remove/upgrade after a crash does not reconcile the
logical WAL; it only runs the legacy physical recovery.

### 3.3 Physical FileJournal

`install::journal::{Journal, Tx, recover}`:

- journal names: `.nawa/bak-<unique>.journal` and `.nawa/rm-<unique>.journal`;
- first line `H\t<encoded backup root>`; then `C` (created), `R` (replaced,
  with backup path), `D` (db directory snapshot), and finally `COMMIT`;
- every line is one `write(2)`; fsync at creation, every 32 steps, before a db
  snapshot, and at commit;
- `Tx::place`, `Tx::remove`, and `Tx::snapshot_db` journal **before** the
  corresponding rename/copy; `Tx::rollback`/`Drop` replay `undo_steps` newest
  first; `Tx::commit` makes `COMMIT` durable first, then deletes the backup
  tree.

`recover` scans `.nawa`:

- journal without `COMMIT` → `undo_steps` (files and db entry restored);
- journal with `COMMIT` → delete the backup tree only;
- `tmp-*` directories → remove (dead staging).

It is called from `main::run` for every exclusive-lock command, and again
inside `install_archive_payload` and `remove_with`. It is idempotent and
survives torn last lines.

**Current gap:** the FileJournal has no transaction id or step number. A
physical repair cannot tell the TxWAL which step it belonged to.

### 3.4 Commit boundaries

For one package install, the current order is:

```text
begin_operation (state + preparing record)
  StepIntent(before, target, digest)          [TxWAL, durable]
  extract/auth/conflict checks
  save_undo(previous)                         [legacy slot mirror]
  FileJournal.create + C/R/D records          [physical WAL]
  move files; db.add; snapshot + save_hooks
  FileJournal COMMIT                          [physical commit point]
  StepResult(committed)                       [logical result]
finish: final TransactionRecord + clear TxWAL [logical commit point]
```

`nawa rollback` uses the same install path and therefore also writes a legacy
slot and a history record. `history undo` does the same through
`install_archive_*`/`remove_with`.

### 3.5 Crash matrix today

| Crash point | Physical recovery | Logical recovery | Result |
|---|---|---|---|
| before StepIntent | nothing | nothing | no change |
| after StepIntent, before FileJournal | nothing to undo | none unless `history` runs | step intent remains; db matches before |
| after FileJournal line, mid file moves | replays FileJournal | only on `history` | files/db restored |
| after db.add, before FileJournal COMMIT | replays FileJournal including `Step::Db` | only on `history` | files and db restored |
| after FileJournal COMMIT, before StepResult | removes backup tree | infers committed from db on `history` | install committed |
| after StepResult, before finish | nothing | `history` reconstructs the final record from intents/results | install committed |
| after finish, before TxWAL cleanup | nothing | terminal record already final | cleanup on next recovery |

The physical side is safe. The logical side lags until `nawa history` is run,
and the two sides are not linked.

---

## 4. Problems to solve

1. **Two logical authorities.** `rollback` reads the legacy slot; `history
   undo` reads committed history before-states. They agree by construction in
   normal new-binary operations, but can diverge after failed attempts, old
   binaries, or partial migrations.
2. **Dead duplicated data.** `undo/<name>/files` duplicates `db.files` and has
   no reader.
3. **Unlinked recovery.** A FileJournal repair cannot update the matching
   TxWAL step, and TxWAL reconciliation is not run after ordinary commands.
4. **Recovery entry-point drift.** Physical recovery runs in three places;
   logical recovery in one. Running the wrong one after a crash leaves either
   files or history in an intermediate state.
5. **Different "previous" semantics.** The legacy slot is a *last attempted
   save*, while history before-states are *attempted/completed steps*. The
   resolver must make this distinction explicit instead of silently choosing
   one.
6. **Retention coupling.** If history is ever pruned, the newest committed
   before-state per package is required for `rollback` and `history undo`.
   No pruning exists today; the design must keep that invariant explicit.

---

## 5. Goals, non-goals, invariants

### 5.1 Goals

- One operation identity for logical and physical work.
- One logical resolver for "the previous state of package P".
- One recovery entry point, run under the exclusive db lock, in the order
  physical → logical.
- Preserve `nawa rollback`, `nawa history undo`, their messages where
  reasonable, artifact/proof verification, and `/etc` config protection.
- Preserve old-root and old-binary interoperability.

### 5.2 Non-goals

- Do not merge `nawa rollback` and `nawa history undo` into one command.
- Do not change package, archive, db, index, signature, or transaction record
  formats.
- Do not add automatic history pruning, GC changes, or a new CLI command.
- Do not delete legacy undo directories in this design cycle.
- Do not replace the physical FileJournal with the TxWAL. The TxWAL cannot
  restore partially moved files.

### 5.3 Compatibility invariants

- `nawa rollback <name>` remains one slot back, ping-pong capable, works after
  removal, refuses an equal version, and verifies the exact artifact bytes and
  retained proof before touching the package.
- `nawa history undo <id>` remains an all-preflighted compensating
  transaction and records itself.
- Old nawa binaries can still roll back packages on a new root during the
  transition: `undo/<name>/desc` stays present and readable.
- New nawa binaries still recover `.nawa/bak-*.journal` and
  `.nawa/rm-*.journal` left by an old binary.
- New FileJournal records remain parseable by the old `recover`: keep the
  `bak-`/`rm-` filename prefixes and the existing line grammar; add only an
  ignored `T` line (old parsers treat unknown lines as no-ops).
- `.PKGINFO`, archive layout, `desc`/`files`/`hashes` db files, repository
  index, and signatures are untouched.

---

## 6. Previous-slot resolver

### 6.1 API

New internal module (proposed `src/install/previous.rs`):

```rust
enum PreviousSource {
    History { tx: u64, step: u32 },
    LegacySlot,
}

struct PreviousVersion {
    /// Name, version, rel, arch, reason, artifact digest.
    state: PackageState,
    source: PreviousSource,
}

fn resolve_previous(root: &Path, name: &str) -> Result<Option<PreviousVersion>>;
```

The caller (`install::rollback::rollback`) synthesizes an `Entry` for the
existing `pick_restore_source` path. Only `version`, `rel`, and
`artifact_sha256` are consumed there; `run`, `install_date`, and `size` are
left empty because the installer re-parses the archive before installing.
This must be documented at the synthesis site so no future caller treats the
synthetic `Entry` as installable db state.

### 6.2 Inputs

- `C` — current installed `Entry` (`None` if removed).
- `D` — parsed legacy `desc`, if present. A present but unparseable `desc` is
  an error, matching today's "corrupt undo record" behavior.
- `H_c` — the before-state of the newest **committed** change for `name`,
  scanning transaction records newest-id first.
- `H_a` — the before-state of the newest **attempted** change for `name`
  (`failed` counts; `skipped` does not; `committed`/`committed-hook-failed`
  count).
- The package-change status, the owning transaction status, `undone_by`, and
  the transaction's `finished_at`/`started_at`.

Rules:

- `skipped` changes were never attempted and are ignored.
- A transaction that is still `preparing`/`running`, or is `needs-recovery`
  after reconciliation, makes the resolver **refuse** for that package: the
  on-disk state is not trustworthy enough to choose a rollback target.
- `interrupted` records should have been converted by recovery; if one remains,
  refuse.
- Changes from a record with `undone_by` are superseded; the compensating
  transaction is newer and supplies its own committed `before` for each
  package. If the compensating record is missing or partial in a way that
  leaves no usable candidate, fall back to `D` when present (rule 2); refuse
  only when neither source can explain the slot.

### 6.3 Selection rules (preserve current behavior)

This order is deliberate. It keeps the legacy slot authoritative wherever
the old code had information that history cannot reproduce exactly, and uses
history everywhere else.

1. **No `D`:** use `H_c` if it is installed. If `H_c` is absent or its before
   state is absent, return `None` (same user-visible failure as "no undo
   record"). A newer failed `H_a` alone does not create a slot: a failed
   attempt with no legacy mirror is the one case the old code left no record
   of.
2. **`D` present, no history candidate:** use `D` (old root or old binary).
3. **`D` and a candidate agree:** use the candidate with the stronger
   information:
   - same `version`/`rel`, both digests non-empty and equal → equivalent;
   - same `version`/`rel`, one digest empty → prefer the non-empty digest for
     verification, but the target version is the same;
   - same `version`/`rel`, both digests non-empty and different → treat as a
     disagreement, not agreement.
4. **`D` matches `H_c` (including before a newer failed attempt):** use `D`.
   This is the crash-before-`save_undo` case: the failed attempt's before is
   the currently installed state, while the real previous slot is older.
5. **`D` matches `H_a` but not `H_c`:** use `D`. This is the
   failed-after-`save_undo` case: the slot was consumed by the attempt, which
   is current legacy behavior.
6. **`D` equals the current installed version (`C`):** use `D`; the caller's
   existing "already at" check then preserves today's message and behavior.
   This covers a failed rollback retry and a history undo that reinstalled the
   removed version.
7. **Fresh install over a stale slot:** `D` is installed, `C` is installed at
   a different version, and the newest committed change is an install whose
   `before` state is absent. Use `D`. This preserves today's behavior after
   "remove R, then fresh-install N": `save_undo` was not called for the fresh
   install, so the old slot still points at R and `nawa rollback N` restores
   it.
8. **`D` is clearly newer than the newest history record** (file mtime later
   than `finished_at` with a small clock-skew allowance): use `D`. This is the
   old-binary-wrote-after-history case.
9. **Anything else:** fail closed with a diagnostic naming both versions and
   telling the user to inspect `nawa history`. Do not silently roll back to
   one of two contradictory targets.

A present but corrupt `desc` is an error in every case (rule from today).
A corrupt or unsupported transaction record makes `list_records` fail; the
resolver propagates that error rather than guessing. This is stricter than
today's `rollback`, which never reads history, but it is the safe behavior for
a state that is already corrupt. The diagnostic should make that explicit.

### 6.4 Why this preserves the existing rollback edges

| Scenario | Legacy behavior | Resolver result |
|---|---|---|
| Fresh install, no previous | "no undo record" | `None` |
| Upgrade A→B | rollback to A | `H_c`/`D` agree → A |
| Ping-pong | alternates | each rollback commits a `restore`; next `H_c` is the other side |
| Remove B | rollback reinstalls B | `H_c` before=B, `D`=B → B |
| Remove R, fresh-install N | stale slot R → rollback to R | rule 7 → R |
| History undo of remove | desc equals current → "already at" | rule 6 → "already at" |
| Install fails before `save_undo` | desc still A → rollback to A | `D` matches `H_c` → A |
| Install fails after `save_undo` | desc now B (current) → "already at" | `D` matches `H_a` or `C` → "already at" |
| Failed `nawa rollback` retry | slot was overwritten with current → "already at" | rule 6 → "already at" (known legacy quirk, preserved) |
| Old binary completes an upgrade after history | desc newer, history stale | rule 7 → desc |
| New binary history committed, mirror write failed | ambiguous | fail closed instead of silently using a stale desc |

The last line is the only intentional behavior difference, and it only occurs
after a write failure or manual tampering. Failing closed is preferable to
rolling back to the wrong version.

### 6.5 `nawa rollback` integration

`install::rollback::rollback` changes from:

```text
read undo/<name>/desc
```

to:

```text
resolve_previous(root, name)
  -> synthesize Entry
  -> existing current-version check
  -> existing pick_restore_source (artifact/proof verification)
  -> existing begin_operation + install_restore_source
```

Everything from `pick_restore_source` onward is unchanged, so the security
properties (digest re-verification, retained signed-index proof, retained
detached signature, legacy-cache fallback) are preserved byte-for-byte.

`save_undo` remains in place during the migration and continues to write
`desc`. The writer's `files` output is left untouched in Phases 1–3. It has no
reader in this repository's history and duplicates `db.files`, but removing a
file that an older out-of-tree consumer might inspect is a compatibility
decision, not a prerequisite for making history authoritative. Revisit it in
the Phase 4 gate.

`history undo` continues to use the committed before/after states; it does
not re-read the legacy slot. Because it installs/removes through the same
installer, the mirror stays consistent.

---

## 7. Physical journal identity and unified recovery

### 7.1 FileJournal format extension

Add one optional line after `H`:

```text
T\t<transaction id>\t<step number>
```

Everything else stays byte-compatible:

- filename prefixes `bak-` and `rm-` are unchanged;
- `H`, `C`, `R`, `D`, `COMMIT` grammar and percent encoding are unchanged;
- old `parse_journal` ignores the unknown `T` line (its catch-all is a
  no-op) and old `recover` still finds the journal by filename.

`Parsed` and `Tx` carry an optional `FileJournalId { tx, step }` (a single
field rather than two independent options, so a half-identity cannot be
constructed). `OperationHandle::tx()` and `RecorderHandle::tx()` expose the
allocated transaction id, and the install/remove entry points accept the
identity as an optional parameter (a field on `InstallOpts`, a last argument
on `install_archive_from_index_proof`, and `remove_with_linked`). Production
callers already hold both values when they write the `StepIntent`; tests and
direct internal callers leave it `None`, which means "unlinked physical
repair" and is handled exactly as today. Do not use a process-global/ambient
transaction id for this: the explicit parameter keeps direct and concurrent
callers honest.

### 7.2 Recovery order

New combined entry point in a small top-level `src/recovery.rs` module. It
cannot live in `transaction::recovery`: the layering check forbids a
`transaction -> install` edge (the physical journal belongs to `install`), and
`install` already depends on `transaction`. A top-level orchestrator can call
both without creating a cycle.

1. **Physical pass** — `install::journal::recover`: scan `.nawa`, replay
   non-committed FileJournals, clean committed backup trees and `tmp-*`
   staging. Linked journals name their transaction/step in the repair note.
2. **Logical pass** — `transaction::recovery::reconcile_incomplete(root)`
   over all TxWAL transactions. It already infers outcomes from the db; after
   the physical pass the db is in its true restored/committed state.
3. **Report** — print one line per repair. A linked journal and its
   reconciliation may produce two notes; that is acceptable for the first
   phase, or they can be deduplicated by transaction id.

This order is required: reconciliation must see the db **after** any
`Step::Db` restoration, otherwise a partially committed step looks ambiguous.

Run it:

- in `main::run` while holding the exclusive db lock, replacing the current
  `install::recover_and_report` call;
- from `cmd_history` with strict error propagation, preserving today's
  behavior of surfacing a corrupt/unrecoverable history record (main has
  already run the combined pass for this exclusive command, so this is a
  second idempotent check);
- **not** from `install_archive_payload` or `remove_with`. Those keep their
  existing physical-only recovery call. Running the logical pass there would
  be wrong: `begin_operation` writes the current transaction's TxWAL before
  the per-step exclusive lock is taken, so reconciliation would mistake the
  live intent for an interrupted one. Phase 3 moves the combined pass to
  before `begin_operation` for commands that take per-step locks, then removes
  the inner physical calls.

The physical replay is already idempotent and torn-line tolerant. Logical
reconciliation is idempotent: a missing journal after a partial recovery
leaves the intent in place, and the db comparison yields the same
committed/failed/needs-recovery result on the next run.

### 7.3 Crash matrix after unification

| Crash point | Physical pass | Logical pass | Final state |
|---|---|---|---|
| before intent | nothing | nothing | unchanged |
| intent, no FileJournal | nothing | db matches before → failed | failed |
| mid files | undo files/db | db matches before → failed | previous state |
| after db, before COMMIT | undo files/db | db matches before → failed | previous state |
| after COMMIT, before StepResult | clean backups | db matches target → committed | committed |
| after StepResult, before finish | nothing | intent+result → committed | committed |
| crashed during recovery | idempotent replay | idempotent reconciliation | converges |

A FileJournal with a `T` line that was replayed is also a direct statement
that its step did not commit; the logical pass confirms it from the db rather
than trusting the physical file alone.

### 7.4 Naming

To reduce confusion in code and diagnostics, the design recommends (in a
later mechanical refactor) moving `src/install/journal.rs` under the
transaction layer or renaming it `filejournal`, so "journal" in
`transaction::journal` always means the logical TxWAL. This is a naming
change only and must keep the re-export used by tests until the move is
complete. It is not required for correctness and can be deferred.

---

## 8. Migration plan

Each phase is independently buildable and keeps the full suite green. No
phase is allowed to weaken artifact/proof verification, locking, path
containment, or config protection.

### Phase 0 — Characterization (tests only)

- Add/keep tests that pin the legacy slot layout, including the dead `files`
  file and the current rollback messages.
- Add old-`recover` parser fixtures (`bak-*`, `rm-*`, torn line, COMMIT
  leftover) that a new binary must still accept.
- Record the exact crash windows from §3.5 as integration tests where the
  harness can `SIGKILL` a child.
- Exit criterion: tests describe today's behavior without changing it.

### Phase 1 — FileJournal identity and combined recovery

Files: `src/install/journal.rs`, `src/install/payload.rs`,
`src/install/rollback.rs`, `src/recovery.rs` (new), `src/main.rs`, tests.

- Add `T` to the FileJournal, parse it, and set it from the existing
  `OperationHandle`/`RecorderHandle` identity when the caller has one.
- Add the top-level combined recovery entry point and call it from `main::run`
  in place of the install-only recovery. Keep the inner calls physical-only
  (see §7.2 for why the logical pass must not run during an active
  operation).
- Keep old journal compatibility and the strict `cmd_history` path.
- Add crash tests: intent only, mid-file move, db snapshot only, post-COMMIT,
  post-result; assert both file state and history status without running
  `nawa history` manually.
- Exit criterion: physical recovery notes and logical reconciliation agree;
  old journals still recover; full suite green.

### Phase 2 — Previous-slot resolver and `rollback` integration

Files: new `src/install/previous.rs`, `src/install/rollback.rs`,
`src/install/payload.rs`, `src/install/mod.rs`, tests.

- Implement `resolve_previous` exactly as §6.3. Start with unit tests over
  synthetic `TransactionRecord`s and legacy desc fixtures; no filesystem
  races needed for the selection rules.
- Switch `rollback` to the resolver, keeping `pick_restore_source` and the
  current-version check unchanged.
- Keep the `desc` and `files` writes byte-for-byte; the mirror stays a
  compatibility artifact during the behavior-preserving phases.
- Add tests for every row of §6.4, including a history-only root, desc-only
  root, failed attempt before/after `save_undo`, old-binary-newer-desc,
  corrupt desc, corrupt history, and `history undo`-then-rollback.
- Exit criterion: every existing rollback test passes unchanged; new tests
  demonstrate the same target and the same security checks.

### Phase 3 — Logical recovery on every mutating command

Files: `src/main.rs`, `src/commands/installing.rs` (or wherever
`begin_operation` is called), `src/commands/building.rs`,
`src/commands/groups.rs`, `src/upgrade/mod.rs`, `src/install/payload.rs`,
`src/install/rollback.rs`, tests.

- For commands whose `lock_mode` is `None` because they take a short
  exclusive lock per transaction (`install` by name, `build --install`,
  `update`, `group install`), run the combined recovery while holding that
  lock and **before** `begin_operation`. That is the only safe point: once
  the current transaction's state/intent exists, it must not be reconciled
  as if it were abandoned.
- Remove the redundant physical recovery calls from
  `install_archive_payload` and `remove_with` once every production caller
  runs the combined entry point before opening its transaction; update tests
  that relied on the implicit call.
- Ensure `cmd_history` keeps strict errors; main keeps warning-only behavior
  for a repair it cannot complete.
- Add a test: crash after db commit before StepResult, then run an unrelated
  `nawa install`; `nawa history` must already show the crashed transaction as
  completed without a manual `history` reconciliation.
- Exit criterion: one recovery entry point; no double side effects; history
  is consistent after any mutating command.

### Phase 4 — Demote the legacy mirror (decision gate)

This phase is optional in this design cycle and requires an explicit product
decision because it is the only phase that can change a rollback edge.

Two acceptable end states:

**A. Keep the mirror indefinitely (recommended for now).**
`desc` is written but never authoritative when history can answer. It costs
one small TOML file per changed package and preserves old-binary rollback.
The dead `files` write may be removed later as a separate compatibility
decision. The resolver still follows §6.3. This is already a single
source of truth in the sense that matters: no decision uses the mirror when
history provides a candidate, and every disagreement is either explained by
the documented legacy edges or fails closed.

**B. Stop writing `desc` after the old-binary compatibility window.**
Then:

- history is the only writer/reader of the previous slot;
- a one-shot exclusive-lock migration may delete a legacy `desc` only when its
  content matches the newest history candidate (never merely because it is
  old); otherwise keep it as a read-only fallback;
- old binaries lose rollback for changes made after the migration, which is
  the compatibility cost of the removal;
- the failed-attempt slot edge changes unless a small slot marker (for
  example a `PackageChange` field or a TxWAL marker written at the
  `save_undo` boundary) is added. Do not hand-wave this: add a characterization
  test and get approval before changing it.

If B is chosen, implement it as its own PR with an explicit compatibility
note. Do not combine it with Phases 1–3.

### Phase 5 — Retention guard (only if history pruning is added)

- Document and enforce that the newest committed before-state per package
  (and the newest before-state needed by a pending undo) is pinned.
- `referenced_artifacts` already retains before/after digests; keep that
  behavior.
- Add a test that a simulated prune cannot remove the artifact required by
  `nawa rollback` or `nawa history undo`.

No pruning is implemented in this design.

---

## 9. Test plan

### 9.1 Resolver unit tests

- no desc, no history → `None`;
- no desc, one committed install → before absent → `None`;
- no desc, upgrade → previous version;
- desc only (old root) → desc;
- desc + matching committed history → same target, stronger digest source;
- desc + failed attempt before `save_undo` → desc older target;
- desc + failed attempt after `save_undo` → desc current target → "already at";
- desc equals current installed → "already at";
- remove R, fresh-install N with the stale slot still present → R;
- old-binary desc newer than history → desc;
- desc/history contradiction → error, no target;
- corrupt desc → error;
- corrupt/unvalidated history record → error;
- needs-recovery transaction touching the package → refuse;
- skipped step ignored; failed step treated as attempted.

### 9.2 Rollback integration tests

Keep all existing tests in `src/install/mod.rs` unchanged:

- `rollback_reverts_upgrade_and_ping_pongs`;
- `rollback_after_remove_reinstalls`;
- `rollback_rejects_corrupted_object_bytes`;
- `rollback_rejects_missing_object`;
- `rollback_restores_through_verified_object_and_ping_pongs`;
- `rollback_is_recorded_and_can_be_undone`.

Add:

- same behavior with `undo/<name>/desc` deleted (history-only root);
- same behavior with history records deleted (desc-only root);
- resolver path still re-verifies signed-index and detached proofs;
- legacy cache path (no digest) still works;
- `/etc` config protection is unchanged through a rollback.

### 9.3 Recovery tests

- old `.nawa/bak-*` journal, no `T` line → same repair as today;
- new journal with `T` → repair plus matching history step failed;
- committed journal with `T` → backup cleaned, history infers committed;
- torn `T` line / unknown future field → ignored, steps still replay;
- crash after StepIntent only → history failed, files untouched;
- crash mid-file move → files and db restored, history failed;
- crash after db snapshot → all three db files restored;
- crash after COMMIT before StepResult → history completed/partial as
  appropriate, files kept;
- run a second recovery → no notes, no changes (idempotent);
- recovery after an old binary left a journal → new binary repairs it.

### 9.4 Command-level tests

- `nawa history undo` still plans and applies after resolver integration;
- `nawa rollback` after `nawa history undo` (both directions, ping-pong);
- `nawa autoremove`/`remove` crash recovery still works;
- exclusive-lock command after a killed transaction reconciles history;
- concurrent processes still serialize through the existing lock (no new
  lock, no new lock order).

---

## 10. Compatibility matrix

| State / actor | New binary | Old binary |
|---|---|---|
| Old root, old desc only | resolver uses desc | unchanged |
| Old root, no desc, new history | resolver uses history | old binary says "no undo record" (same as before history) |
| New root after Phase 1, old binary reads `.nawa` | new recovery understands old journals | old recovery understands `T`-bearing new journals |
| New root after Phase 2, old binary runs rollback | desc still written, so old rollback works | works |
| New root after optional Phase 4B, old binary rollback | history only; old binary has no slot | old binary loses rollback for new changes (declared cost) |
| Corrupt history / desc | fail closed with diagnostic | old binary behavior unchanged |

No change to package identity, digests, signatures, or the repository index is
involved in any row.

---

## 11. Risks and open questions

1. **mtime-based old-binary detection.** Rule 7 uses `desc` mtime because the
   legacy format has no transaction id. Filesystem mtime is coarse and can be
   affected by clock changes. Mitigation: only use it to prefer desc when it
   is clearly later; otherwise fail closed. A future optional marker could
   remove the heuristic, but it would be a legacy-format extension and is not
   needed while the mirror is written.
2. **Strict history parsing.** A single corrupt unrelated record blocks
   `rollback`. This is deliberate (fail closed), but the diagnostic must say
   how to inspect/repair history rather than looking like "no undo record".
3. **History growth.** No pruning exists. If one is added, the newest
   committed before-state per package must be pinned; see Phase 5.
4. **Synthetic `Entry` fields.** The resolver synthesizes an `Entry` with
   empty `run`/`install_date`/`size`. This is safe only because
   `pick_restore_source` uses only identity/digest and the installer re-reads
   the archive. Enforce that with a narrow type or a comment at the synthesis
   site; do not pass the synthetic entry to `db.add` or hooks.
5. **`undo/files` is dead data.** The current repository history shows no
   reader. It is left in place during the behavior-preserving phases; removing
   it is a Phase 4 compatibility decision, not part of making history
   authoritative.
6. **Fsync of the legacy mirror.** `save_undo` currently writes+renames
   without an fsync. Once the mirror is only a compatibility aid, a lost desc
   is recoverable from history. If Phase 4B is ever chosen, the mirror's
   durability no longer matters for new roots. Do not add fsync work here
   without measuring the install-path cost.
7. **One recovery entry point vs direct callers.** Removing the inner
   `recover_and_report` calls changes behavior for internal callers that do
   not go through `main` (mostly tests). Phase 3 must update those callers
   explicitly rather than relying on implicit repair.
8. **Locking across repair.** Combined recovery must stay under the exclusive
   db lock, exactly as the current physical recovery is. Do not introduce a
   second lock or run reconciliation during a shared-lock snapshot read.

---

## 12. Decision log

| # | Decision | Rationale |
|---|---|---|
| D1 | Keep the physical FileJournal | The TxWAL is package-granular and cannot restore partially moved files |
| D2 | History is the canonical logical source; the legacy desc is a compatibility mirror | Removes the "two authorities" problem without breaking old roots/binaries |
| D3 | Link FileJournal entries to transaction/step with an additive `T` line | One recovery can update both file state and history; old parsers ignore it |
| D4 | Recover physical first, logical second, under the exclusive lock | Reconciliation must see the post-repair db |
| D5 | Keep `nawa rollback` and `nawa history undo` separate | Different user intents; both already share the installer/remover and verification |
| D6 | Preserve the legacy slot's failed-attempt/quirk semantics through the resolver rules | No user-visible behavior change in the migration |
| D7 | Leave the unused `undo/<name>/files` write in place during the behavior-preserving phases | It has no reader here, but removing an on-disk artifact is a compatibility decision; revisit with Phase 4B |
| D8 | Defer stopping `desc` writes to an explicit Phase 4 decision | The only remaining change that can alter rollback behavior or old-binary compatibility |

---

## 13. What this design deliberately does not do

- It does not delete `/var/lib/nawa/undo` or rewrite historical records.
- It does not add a new CLI command, a new dependency, or a new on-disk
  schema version.
- It does not change rollback verification: exact bytes, retained signed-index
  proof, retained detached signature, and legacy-cache behavior remain the
  same.
- It does not make `nawa rollback` a whole-transaction undo; that remains
  `nawa history undo`.
- It does not implement history pruning or garbage-collection changes.
