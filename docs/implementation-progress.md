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
| 1 — Structural refactor + service removal | **in progress** (1a, 1b, 1c done; 1d next) |
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

### Phase 1b — Service-subsystem removal (2026-11-17)

* Removed (the one approved removal; audit §8.3 justification — runit
  packaging left faseela-repo at `ac1c3bb`, `systemd-units` ships the
  distribution's service policy in `80-faseela.preset`):
  * `src/service.rs`, `src/commands/service.rs` deleted; `mod service;`
    + `pub(crate) use service::*;` + `cmd_service` dispatch + the
    `Cmd::Service`/`ServiceCmd` CLI types and the
    `application::lifecycle` match arm removed.
  * Completions: `service` removed from the bash commands list, the bash
    `service)` case, the zsh `cmds` entry + `service)` case, and the
    fish combined line + service actions.
  * `src/install/payload.rs`: the `crate::service::keep_disabled` skip
    in the staged-tree move is gone (dead code without runit packaging;
    admin `disabled/` marker files are never deleted — documented).
  * `src/plugins/manifest.rs`: `"service"` left `RESERVED_COMMANDS`
    (the `reserved_commands_cover_every_builtin_cli_command` test keeps
    reserved == built-ins in sync; a plugin may now legitimately
    register a `service` command — it is no longer a built-in).
  * `scripts/smoke-v06.sh`: §7 (runit services in a fake root) removed;
    header comment updated. Executed end-to-end after a release build:
    **ALL V0.6 SMOKE TESTS PASSED**.
  * Docs: `docs/commands.md` service rows removed;
    `docs/transactions-services-plugins.md` "runit services" section
    replaced by a short "Services" note (service management belongs to
    the init system / systemd presets; markers never touched);
    `docs/index.md` and `README.md` feature bullets updated.
  * CHANGELOG: new unreleased "removed: runit service subsystem" entry.
* **Pre-existing CHANGELOG.md corruption repaired** (found while adding
  the entry): from `b8f11bd` ("build: remove vendored Cargo
  dependencies") onward, every line after the "runit service control"
  section header had the header prepended to it (170 lines through EOF).
  Fixed by stripping the injected prefix; result verified byte-identical
  to the last clean ancestor (`290ea48`) plus the entries legitimately
  added after it. The historical "fix: runit service control" entry is
  kept (maintainer may drop it before release since the feature is now
  removed).
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → 564 passed /
  2 ignored / 3 env-failed (same 3 as always; −5 = exactly the deleted
  service unit tests); `python3 scripts/check-layering.py` green
  (53 modules — service removed one);
  `cargo fmt --all -- --check` unchanged violations only (no new ones in
  touched files). `nawa --help` and all three completions no longer
  mention `service`; `nawa service …` now reports "unknown command"
  (routed through plugin dispatch).

### Phase 1c — CLI split, `main.rs` slimming, bootstrap (2026-11-17)

* `src/cli.rs` (833 lines) split into `src/cli/{mod,args,completions}.rs`
  (args: `Cli`/`Cmd`/subcommand enums + the reserved-commands test;
  completions: the three `COMPLETION_*` constants, `completions_script`
  + the bash-syntax test). Pure move; one slice mistake
  (`completions_script` initially dropped) caught by `cargo check` and
  fixed.
* `main.rs` slimmed 430 → 110 lines: the whole top-level orchestration
  — lock acquisition + recovery + install-date migration, before/after
  installed-state snapshots, payload-change comparison, `should_run_…
  hooks`, and the `Cmd` → handler dispatch — moved verbatim into the
  new `src/commands/dispatch.rs` (with its test). Deviation from the
  planned checklist: the plan said `main.rs` keeps "lock acquisition";
  in the end the entire `run()` orchestration moved to
  `commands/dispatch.rs` because lock acquisition is command
  orchestration, not process bootstrap. `main.rs` now reads:
  parse → overrides → config → ctx → hook-suppression decision →
  `commands::run` → post-transaction triggers → plugin events → exit.
* New `src/bootstrap.rs`: `print_error` + `exit_on_error` (error
  rendering, Ctrl+C → 130, other errors → 1; success returns normally
  so stdout still flushes by natural return). Process-exit policy lives
  in one place.
* Crate-root bindings preserved: `use cli::*;` stays in `main.rs`
  because `commands/fetching.rs` addresses `crate::default_jobs` (a
  private crate-root glob is visible to all descendant modules). A
  `git grep` confirmed `crate::default_jobs` is the only such external
  reliance; `use commands::*;` was dropped (nothing outside
  `main.rs`/`dispatch.rs` used it).
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → 564 passed /
  2 ignored / 3 env-failed (identical to the Phase-1b tree; all moved
  tests still counted, verified individually in the log);
  `python3 scripts/check-layering.py` green (54 modules);
  `cargo fmt --all -- --check` — no new violations in touched files
  (dispatch.rs carries the moved pre-existing style only).
  **Byte-identical behavior check**: built HEAD in an isolated
  `git worktree` and diffed `--help`, `--version` and all three
  completion scripts against the Phase-1c build — identical.
  `scripts/smoke-v06.sh` executed against the release build: **ALL
  V0.6 SMOKE TESTS PASSED**.

## Current task

**Phase 1d — use-case extraction + layering direction rules** (last
Phase-1 sub-phase; establishes the functional-core boundary):

1. Extract the first package use cases from the `commands/*` handlers
   into `application/features/package/*` (start with the ones the
   design names: plan-install, apply-transaction, query). Handlers keep
   argument parsing + output formatting; use cases take typed requests
   and return domain results.
2. Extend `scripts/check-layering.py` with *direction* rules: domain
   (`db`, `version`, `plan`, `recipe::model`, …) must not import
   `cli`, `application`, `commands`, `output`, `color`, `diag`
   (presentation); `application` must not import `commands` or `cli`
   types (except the pragmatic `lifecycle` → `cli::Cmd` edge, or finish
   moving `lock_mode` onto a typed request enum — decide during the
   work).
3. Keep every behavior and CLI message identical; integration tests
   are the regression net.

## Changed files (this phase, updated as work lands)

* `docs/architecture-audit.md` (new, Phase 0)
* `docs/implementation-progress.md` (new, Phase 0; updated every phase)
* Phase 1a:
  * `src/application/mod.rs` (new), `src/application/lifecycle.rs` (new)
  * `src/cli.rs` (lock_mode removed, header note)
  * `src/main.rs` (`mod application;` + `use application::lifecycle::lock_mode;`)
  * `src/lock.rs` (fail-closed exclusive under `NAWA_LOCK_HELD` + docs)
  * `tests/lock.rs` (2 new forged-marker / control tests)
* Phase 1b:
  * `src/service.rs`, `src/commands/service.rs` (deleted)
  * `src/cli.rs`, `src/main.rs`, `src/commands/mod.rs`,
    `src/application/lifecycle.rs`, `src/install/payload.rs`,
    `src/plugins/manifest.rs` (service references removed)
  * `scripts/smoke-v06.sh` (§7 removed), `CHANGELOG.md` (removal entry +
    pre-existing corruption repaired), `README.md`, `docs/index.md`,
    `docs/commands.md`, `docs/transactions-services-plugins.md`
* Phase 1c:
  * `src/cli.rs` → `src/cli/{mod,args,completions}.rs`
  * `src/main.rs` (slimmed 430 → 110 lines)
  * `src/commands/dispatch.rs` (new — orchestration + dispatch moved)
  * `src/bootstrap.rs` (new — error rendering + exit codes)

## Tests actually executed

| Command | Result |
|---|---|
| `cargo check --all-targets --offline` | pass (Phase 0 + 1a + 1b + 1c) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 0, re-measured at HEAD) | 566 pass / 2 ignored / 3 env-failed (baseline, pre-existing) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1a tree) | 569 pass / 2 ignored / 3 env-failed (same 3; +3 new/moved tests) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1b tree) | 564 pass / 2 ignored / 3 env-failed (same 3; −5 = deleted service unit tests) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1c tree) | 564 pass / 2 ignored / 3 env-failed (identical to 1b; moved tests verified present) |
| `cargo clippy --all-targets --offline -- -D warnings` | pass (Phase 1a + 1b + 1c) |
| `python3 scripts/check-layering.py` | pass (54 after 1a; 53 after 1b; 54 after 1c) |
| `bash scripts/smoke-v06.sh` (release build, Phases 1b + 1c) | ALL V0.6 SMOKE TESTS PASSED |
| HEAD-vs-1c behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical |
| `cargo fmt --all -- --check` | fails repo-wide at HEAD (pre-existing ~100 files; CI does not run fmt) — re-scoped to "no new violations in touched/new code", satisfied (1a + 1b + 1c) |

## Known problems

* The three baseline test failures are environment limitations of this
  container, not code defects (audit §12.4). CI runners pass them.
* `cargo fmt --all -- --check` is red at HEAD across ~100 pre-existing
  files (no `rustfmt.toml`; CI runs clippy/tests but never fmt). The
  phase gate is re-scoped to "new/touched code is rustfmt-clean"; a
  repo-wide reformat is deliberately deferred (mass rewrite).
* `CHANGELOG.md` corruption (introduced by `b8f11bd`, repaired in
  Phase 1b): the historical "fix: runit service control" entry remains
  in the Unreleased list although the feature it improves is removed;
  the maintainer may delete that entry before the next release.
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

Execute the Phase 1d checklist in "Current task" above; verify with
`cargo check --all-targets --offline && cargo test --all-targets
--offline --no-fail-fast && cargo clippy --all-targets --offline -- -D
warnings` plus `python3 scripts/check-layering.py` (expect 564 pass /
3 env-failed, layering green *with the new direction rules enforced*)
and the byte-identical CLI check for any touched command; commit as one
coherent unit ("first use cases + layering direction rules"); then
Phase 1 is done — move to Phase 2 (init-system detection & system
capabilities, design §5) or the next unblocked phase.
