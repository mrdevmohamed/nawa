# Nawa implementation progress

> Live tracker for the system-intelligence-and-migrations implementation.
> Specification: `docs/design/system-intelligence-and-migrations.md`
> (design branch `docs/system-intelligence-migration-design`).
> Audit: `docs/architecture-audit.md`. Update this file before ending every
> working session. Do not redo completed work; read it first.

## Status snapshot

| Phase | State |
|---|---|
| 0 — Source audit | **DONE** (this session) |
| 1 — Structural refactor + service removal | **in progress** |
| 2 — Init-system detection & system capabilities | not started |
| 3 — Metadata & capability resolution | not started |
| 4 — System profiles & planning | not started |
| 5 — Migration engine | not started |
| 6 — Faseela runit→systemd migration | **BLOCKED** (see blockers) |
| 7 — Final verification | not started |

## Completed phases

### Phase 0 — Complete source audit (2026-11-17)

* Walked the full `src/` tree (115 files, ~39.3k lines, 53 top-level
  modules), measured the real module dependency graph with the CI
  layering parser (extended), read all command handlers, installer,
  transaction/recovery, lock, repo, recipe, archive, sign, plugins, CI,
  smoke scripts, and the `faseela` / `faseela-repo` / `saq` repos for
  migration-relevant facts.
* Deliverable: `docs/architecture-audit.md` (module ownership, measured
  dependency directions, mixed responsibilities, call flows,
  transaction/recovery behavior, lock/`NAWA_LOCK_HELD` analysis, hook
  trust boundary, service-subsystem compatibility analysis, saq
  init-selection contract, compatibility baseline, security risks,
  missing capabilities, migration map).
* Executed baseline: `cargo check --all-targets` clean;
  `cargo test --all-targets --offline --no-fail-fast` → 535 passed,
  1 ignored, **3 environment-caused failures** (documented in audit §12.4;
  pre-existing, unrelated to code): toolchain-under-`/home` sandbox test,
  2 plugin tests blocked by container EFI-vars permissions.
  `python3 scripts/check-layering.py` green (53 modules, no cycles).

## Current task

**Phase 1a — security-relevant lock refactor** (first structural change;
smallest safe unit with behavior preserved):

1. Create `src/cli/` module split (args, completions) — mechanical move.
2. Move lock policy (`lock_mode`) from `cli.rs` into the application
   lifecycle module.
3. Introduce in-process lock ownership: top-level command holds the
   `DbLock` guard and passes it down (no re-acquire inside use cases).
4. Harden `lock::acquire`: `NAWA_LOCK_HELD` no longer silently skips an
   **exclusive** acquisition — nested package-mutating commands fail
   closed; read-only (shared) nested commands keep the documented
   shortcut (deadlock analysis in audit §17).
5. Add tests: forged `NAWA_LOCK_HELD` cannot authorize exclusive
   mutation; nested read-only behavior unchanged; hook-spawned nawa
   cannot mutate while the gate is pending (gate comes in Phase 5; the
   fail-closed test lands now).

## Changed files (this phase, updated as work lands)

* `docs/architecture-audit.md` (new)
* `docs/implementation-progress.md` (new)

## Tests actually executed

| Command | Result |
|---|---|
| `cargo check --all-targets --offline` | pass |
| `cargo test --all-targets --offline --no-fail-fast` | 535 pass / 1 ignored / 3 env-failed (baseline, pre-existing) |
| `python3 scripts/check-layering.py` | pass |

## Known problems

* The three baseline test failures are environment limitations of this
  container, not code defects (audit §12.4). CI runners pass them.
* `scripts/smoke-v06.sh` §7 tests `nawa service`; Phase 1 removes that
  section together with the command.
* No `Cargo.lock` change needed so far.

## Unresolved external dependencies (blockers)

| ID | Blocker | Owner | Affects |
|---|---|---|---|
| B1 | **saq init selection**: disk-boot `init` fallback chain (`$INIT_ARG /usr/bin/runit /sbin/init /usr/sbin/init`) contains no systemd path, and Faseela's grub.cfg passes no `init=`. During the migration runit must stay installed, so `/usr/bin/runit` would win the chain and boot runit again. Needs: migration boot config sets `init=` explicitly **and/or** saq change (e.g. read a selection file) — exact interface TBD with Faseela/SAQ. | Faseela/SAQ | Phase 6 apply-safety, Phase 2 detection tests |
| B2 | **`/usr/bin/init` payload conflict**: old runit-services and the systemd package both ship `/usr/bin/init`. Staged coexistence (install systemd while runit is still installed) hits a file conflict or silently replaces the selector. Needs Faseela-side payload/policy decision (move init selector ownership, document force-install + journal-restore behavior). | Faseela | Phase 6 staged install |
| B3 | **Repository index v4 schema**: metadata chain (recipe → `.PKGINFO` → authenticated v4) needs a written schema proposal + tests before the solver consumes it (design §6.3, §16.1–2). | Nawa (this repo) | Phase 3 |
| B4 | **System profile location/merge contract** with Faseela (defaults path, override path, precedence) — design §16.3. | Faseela agreement | Phase 4 |
| B5 | **Bootloader/initramfs scope + recovery route** for the first migration (grub only? which initramfs tool versions) — design §16.4–5. | Faseela | Phase 6 |
| B6 | **VM boot-test harness**: boot-critical verification requires disposable VMs with snapshots (BIOS + UEFI). No harness exists in this repo yet. | Nawa/infra | Phase 6 acceptance |
| B7 | **Hook/trigger trust policy** (design §16.9): explicit documented policy + CI guard for `.HOOKS`/transaction-trigger service activation. | Faseela (+ Nawa docs) | Phase 1 docs, Phase 5 |

## Exact next task

Implement Phase 1a checklist above; verify with
`cargo fmt --check && cargo check --all-targets && cargo test --all-targets
--offline --no-fail-fast && cargo clippy --all-targets --offline -- -D
warnings` plus `python3 scripts/check-layering.py`; commit as one
coherent unit ("lock security + lock-policy ownership"); then proceed to
Phase 1b (service-subsystem removal, smoke-script update, CHANGELOG).
