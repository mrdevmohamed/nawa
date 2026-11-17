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
| 1 — Structural refactor + service removal | **in progress** (1a done; 1b next) |
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
  `cargo test --all-targets --offline --no-fail-fast` → 566 passed,
  2 ignored, **3 environment-caused failures** (documented in audit §12.4;
  pre-existing, unrelated to code): toolchain-under-`/home` sandbox test,
  2 plugin tests blocked by container EFI-vars permissions.
  (The session's first reading of "535 passed / 1 ignored" was a
  transcription error; re-measured at HEAD 2026-11-17 with the Phase-1a
  working tree stashed: 566/2/3. Phase-1a tree measures 569/2/3 — +3 =
  the new/moved tests.)
  `python3 scripts/check-layering.py` green (53 modules, no cycles).

### Phase 1a — Lock security + lock-policy ownership (2026-11-17)

* `lock_mode` moved out of `cli.rs` into the new application layer:
  `src/application/mod.rs` + `src/application/lifecycle.rs`. Lock policy
  is transaction policy, not argument parsing (design §4.5: "Lock policy
  moves out of CLI definitions"). `main.rs` dispatches through
  `application::lifecycle::lock_mode`; `cli.rs` keeps argument
  definitions and completions. Match arms are behavior-identical; the
  `group install` no-outer-lock test moved with it (plus a CLI-coverage
  guard test).
* `lock::acquire` hardened against the forgeable `NAWA_LOCK_HELD` marker
  (set by package hooks / transaction triggers for their children):
  * `Mode::Shared` inside a held-marker process still returns `Ok(None)`
    — the documented read-only shortcut that prevents a deadlock against
    the parent's exclusive lock (unchanged).
  * `Mode::Exclusive` now **fails closed** with an error naming the
    hook/trigger context and stating that the process holds no lock: a
    nested package-mutating command can no longer run without a lock of
    its own. The marker is process-inherited state and forgeable; the
    parent's flock does not extend to the child.
  * Documented limitation + roadmap hook in `lock.rs`: the persistent
    migration mutation gate (Phase 5) will be checked *before* the
    marker is honored; the marker can never bypass it.
* Tests: `tests/lock.rs` gains
  `forged_held_marker_cannot_authorize_a_mutating_command` (remove /
  autoremove / all-archive install fail closed, error names the
  context, no db state created) and
  `mutating_command_without_the_marker_uses_its_own_lock`; the two
  lock-policy unit tests moved into `application/lifecycle.rs`.
* Checklist item 3 (in-process lock ownership) is satisfied by the
  existing structure: the shell acquires once per command and holds the
  `DbLock` across `run()`; library functions never lock. The invariant
  is now *enforced* by the fail-closed exclusive path instead of
  resting on convention, and covered by the new tests.
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → 569 passed /
  2 ignored / 3 env-failed (same 3 failures as the 566/2/3 HEAD
  baseline; +3 = the new/moved tests);
  `python3 scripts/check-layering.py` green (54 modules — `application`
  added one).
* Formatting: `cargo fmt --all -- --check` fails across ~100 files at
  HEAD (no `rustfmt.toml`; CI never runs fmt). Re-scoped the gate to
  "no new fmt violations in touched/new code": all new/changed code is
  rustfmt-clean (`mod application;`/use ordering fixed; the
  pre-existing violations — e.g. `assert!` wrapping style, `cli`/
  `checksum` mod-list disorder, `ServiceCmd` brace quirk at cli.rs:660 —
  are left as-is; a repo-wide `cargo fmt` would be a mass rewrite).

## Current task

**Phase 1b — service-subsystem removal** (the one approved removal;
everything else keeps its behavior):

1. Delete `src/service.rs`, `src/commands/service.rs`, `Cmd::Service` /
   `ServiceCmd` from `src/cli.rs`, `mod service;` from `main.rs` and
   `commands/mod.rs`, `cmd_service` dispatch from `main.rs`, the
   `Cmd::Service` match arm from `application/lifecycle.rs`.
2. Remove the service section from the shell completions (bash/zsh/fish
   blocks in `cli.rs`).
3. Remove the service unit tests; update `scripts/smoke-v06.sh` (§7).
4. `src/install/payload.rs:563`: drop the `keep_disabled` call with the
   audit §8.3 justification (runit packaging removed from faseela-repo
   at `ac1c3bb`; `systemd-units` ships the distribution's preset policy;
   nawa never deletes admin `disabled/` markers — only stops *skipping*
   the runit link install, which is dead code without runit packaging).
5. Check `plugins/manifest.rs` `RESERVED_COMMANDS` (and the CI test that
   pins it) so `service` leaves the reserved list consistently; add a
   CHANGELOG entry.

## Changed files (this phase, updated as work lands)

* `docs/architecture-audit.md` (new, Phase 0)
* `docs/implementation-progress.md` (new, Phase 0; updated every phase)
* Phase 1a:
  * `src/application/mod.rs` (new), `src/application/lifecycle.rs` (new)
  * `src/cli.rs` (lock_mode removed, header note)
  * `src/main.rs` (`mod application;` + `use application::lifecycle::lock_mode;`)
  * `src/lock.rs` (fail-closed exclusive under `NAWA_LOCK_HELD` + docs)
  * `tests/lock.rs` (2 new forged-marker / control tests)

## Tests actually executed

| Command | Result |
|---|---|
| `cargo check --all-targets --offline` | pass (Phase 0 + 1a) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 0, re-measured at HEAD) | 566 pass / 2 ignored / 3 env-failed (baseline, pre-existing) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1a tree) | 569 pass / 2 ignored / 3 env-failed (same 3; +3 new/moved tests) |
| `cargo clippy --all-targets --offline -- -D warnings` | pass (Phase 1a) |
| `python3 scripts/check-layering.py` | pass (54 modules after Phase 1a) |
| `cargo fmt --all -- --check` | fails repo-wide at HEAD (pre-existing ~100 files; CI does not run fmt) — re-scoped to "no new violations in touched/new code", satisfied |

## Known problems

* The three baseline test failures are environment limitations of this
  container, not code defects (audit §12.4). CI runners pass them.
* `cargo fmt --all -- --check` is red at HEAD across ~100 pre-existing
  files (no `rustfmt.toml`; CI runs clippy/tests but never fmt). The
  phase gate is re-scoped to "new/touched code is rustfmt-clean"; a
  repo-wide reformat is deliberately deferred (mass rewrite).
* `scripts/smoke-v06.sh` §7 tests `nawa service`; Phase 1b removes that
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

Execute the Phase 1b checklist in "Current task" above; verify with
`cargo check --all-targets --offline && cargo test --all-targets
--offline --no-fail-fast && cargo clippy --all-targets --offline -- -D
warnings` plus `python3 scripts/check-layering.py` (expect 569+Δ pass /
3 env-failed), confirm `service` is gone from `--help`, completions,
`RESERVED_COMMANDS` and the smoke script; commit as one coherent unit
("remove the service subsystem"), then proceed to Phase 1c (`cli.rs`
split into `cli/args.rs` + `cli/completions.rs`, `main.rs` slimming,
`bootstrap.rs`).
