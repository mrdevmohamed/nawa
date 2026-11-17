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
| 1 — Structural refactor + service removal | **DONE** (1a, 1b, 1c, 1d, 1e — 1e is the deferred apply-transaction extraction) |
| 2 — Init identity & capability foundation | **DONE** (detection + capability/identity domain model) |
| 3 — Metadata & capability resolution | in progress: B3 schema proposal written (unwired); next: the solver/planning use case |
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

### Phase 1d — use-case extraction + layering direction rules (2026-11-17)

* **Typed lock request — the pragmatic `application → cli` edge is gone**
  (the "decide during the work" item resolved in favor of the typed
  enum; no allowlist entry needed). `application::lifecycle` no longer
  matches on `cli::Cmd`: the CLI translates each parsed command into a
  new `application::lifecycle::Request` (`cli::Cmd::lock_request`,
  exhaustive match — a new `Cmd` variant cannot compile without a lock
  decision) and `lock_mode(&Request)` owns the policy. Application now
  imports no presentation module at all. The two old lifecycle tests
  moved into `cli/args.rs` as four request-translation tests; the lock
  policy is tested directly in `lifecycle.rs`;
  `commands/dispatch.rs` calls `lock_mode(&cmd.lock_request())`.
* **Direction rules in `scripts/check-layering.py`** (new check 3):
  presentation = {`bootstrap`, `cli`, `color`, `commands`, `confirm`,
  `diag`, `humanize`, `main`, `output`}; every module outside
  presentation must not import presentation, except the documented
  `LEGACY_PRESENTATION_IMPORTS` allowlist — 11 edges, each waiting on
  its feature extraction: `undo→confirm`; `fetch→diag`, `fetch→humanize`;
  `fetchrun→diag`; `install→diag`; `plugins→diag`;
  `repositories→humanize`; `slim→humanize`; `upgrade→color/confirm/diag`.
  Code under `application/features/**` may never import presentation
  (allowlist or not), and **stale allowlist entries fail the check**
  (the list must shrink as extractions land). All three failure modes
  verified with negative probes (fake adapter→presentation import,
  fake features import, fake stale entry) before proceeding.
* **First use cases extracted** into `application/features/package/`:
  * `plan_install.rs` — the decision core of install/update:
    `Source`, `Sourcing` (its `pick` now returns the strict-mode
    "repository has … — installing the repository version" stderr note
    as data; `run_plan` prints it verbatim), `open_sourcing` (takes
    typed booleans instead of `cli::PlanFlags` — no CLI type crosses
    the boundary; its notes are returned, not printed), and `plan`
    (targets → {plan, extras}: current repo-only packages become
    up-to-date notices, older ones become extras, unknown names go to
    the planner). Notices are `Notice::Note/Info` values accumulated in
    an out-parameter and printed by the handler *even when planning
    fails*, byte-identical to the inline order. `Notice::emit` lives in
    `output.rs` (presentation) so the use case renders nothing. 11
    unit tests with an offline in-memory `OpenRepo` fixture.
  * `query.rs` — read-side decisions: `why` (entry + installed
    dependents), `recipe_dependents` (the `--tree` half),
    `search_matches` (glob / name+summary substring). Handlers keep
    palettes, exit codes, `std::process::exit(1)` and every printed
    byte. 3 unit tests over a real db via `testutil`.
  * Deliberately not extracted yet: **apply-transaction** (`run_plan`,
    `install_one`, confirm/print stay in `commands/installing.rs`) —
    acquisition calls `commands::build_one` and the flow needs the
    user-interaction port (design §4.3) before it can move; that is
    the next extraction. `cmd_query`/`cmd_files`/`cmd_owner` are
    formatting around plain db reads — no decision logic to move.
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → **581 passed /
  2 ignored / 3 env-failed** (= 564 + 18 new tests − 1 net lifecycle
  test; the same 3 pre-existing env failures);
  `python3 scripts/check-layering.py` green (54 modules — the feature
  files live under the `application` module).
* Formatting (re-scoped gate): the three new files are rustfmt-clean;
  dirty-hunk counts *decreased* in both touched handler files
  (installing.rs 28→26, querying.rs 21→20 — extraction deleted dirty
  code); every remaining hunk in touched files is a pre-existing one
  outside the changed ranges, or the same import-block reorder HEAD
  already had.
* **Byte-identical behavior**: `--help`, `--version` and all three
  completion scripts identical to a HEAD release build; additionally
  12 read/planning commands (`search`, `--glob`, summary hit, no-hit,
  `why`, `why --tree`, `info`, `query`, `deps`, `install -n`,
  `outdated`, `update -n`) diffed between the HEAD and Phase-1d
  binaries with and without a recipes tree (stdout+stderr+exit code) —
  all identical, including the extracted `open_sourcing` note and the
  planner's "did you mean" error.
  `scripts/smoke-v06.sh` (release build): **ALL V0.6 SMOKE TESTS
  PASSED** — install/upgrade/rollback run through the extracted
  `plan_install`.

### Phase 2 — init identity and capability foundation (2026-11-17)

* **Read-only detection use case** — `application/features/system/detect.rs`
  observes the environment into typed data (design §5), with every source an
  injectable path so each rule is unit-testable without booting and without
  touching the host. [`ObservedSystem`] carries the §5.5 contract as four
  separate fields — execution `Context`, observed `RuntimeInit`, target-root
  `InstalledInit`, `DesiredInit` — never collapsed into one `init-system`
  value:
  * `Context`: `LiveHost` (PID 1's root is this process's root, no container
    markers), `Container` (file markers `/.dockerenv`, `/run/.containerenv`
    or a `/proc/1/cgroup` marker), `Chroot` (this process's root is not
    PID 1's root), `OfflineRoot` (explicit non-canonical `--root`), and
    `Unknown` with a reason (e.g. unreadable `/proc`). Attribution rules:
    host `/proc` describes the managed root only when it is canonical; in a
    chroot the runtime init is `Unknown` because `/proc/1` belongs to the
    outer environment; in a container PID 1 *is* the managed environment's
    init and is reported with evidence — the context field is what a
    migration gate must check.
  * `RuntimeInit`: classified from `/proc/1/exe` + `/proc/1/comm` with
    `Evidence{source, value}` per §5.1 and `Confidence::{Proven,
    Indicated}`; contradictions are reported (`Unknown` + reason), never
    resolved; a bare `init` name identifies nothing (unknown ≠ guess).
  * `InstalledInit`: staged init candidates from the target root
    (`usr/lib/systemd/systemd`, `lib/systemd/systemd`, `etc/runit/2`,
    `usr/bin/runit`, `sbin/runit-init`, plus the generic `init` entry
    points classified by symlink-target *text*, never resolved through the
    filesystem — detection never leaves the target root; a broken symlink
    still counts as installed intent). Presence is evidence of intent,
    never proof of the running PID 1 (§5.5); two staged providers are both
    reported (§6.2 / blocker B2's coexistence scenario).
  * `DesiredInit::NotRead`: system profiles are Phase 4 (blocker B4); an
    absent profile is not read as a decision.
  * Read-only per §5.4: no writes, no `systemctl`/`sv`, no service-manager
    queries; nothing is wired into any command yet (no CLI surface).
* **Capability / identity domain model** — new top-level `domain` layer
  (design §4.3 `domain/system`; pure types, no I/O, imports nothing above
  it; the layering gate covers it):
  * `domain/system/identity.rs`: closed `InitKind` (Systemd/Runit/Other),
    `classify_pid1` (Confirmed/Indicated/Ambiguous/Contradictory) and the
    `init-system` selection rule `InitKind::from_capability` — limited to
    the exact `runit`/`systemd` identities (§6); declared-but-unknown
    identities and other domains select nothing.
  * `domain/system/capability.rs`: normalized `Capability` (`domain=identity`
    parse: lowercase, exactly one `=`, no whitespace), and `Provides` with
    the §6.3(5) distinction — a missing field is `Unknown`, **not** an
    empty declaration, so no future solve can treat missing metadata as
    "provides nothing"; `InitProvider::{Single, Ambiguous, Absent}` reports
    ambiguity instead of choosing (§6.2). Phase 2 deliverable is the
    *proposal* (illustrative recipe spelling documented in the module; §6.1):
    the recipe/`.PKGINFO`/signed-index chain stays unwired until the exact
    field syntax is specified (§16.1; B3 governs index v4). No package or
    index format changed.
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → **614 passed /
  2 ignored / 3 env-failed** (= 581 + 33 new tests: 16 detect + 9 identity +
  8 capability; the same 3 pre-existing env failures);
  `python3 scripts/check-layering.py` green (**55 modules** — the new
  `domain` top-level module). All new files rustfmt-clean; no existing file
  reformatted (only two one-line `mod` registrations).
* **Byte-identical CLI**: `--help`, `--version` and all three completion
  scripts identical to a HEAD release build (isolated `git worktree`;
  detection adds no CLI surface). `scripts/smoke-v06.sh` and
  `scripts/smoke-v05.sh` (release build): **ALL PASSED**.

### Phase 1e — apply-transaction extraction behind the user-interaction port (2026-11-17)

The deliberately-deferred Phase 1d tail (design §4.3/§4.4): the execution
half of `nawa install` / `nawa update` is now an application use case, and
the handlers own only parsing, rendering and prompts.

* New **application ports** layer (`src/application/ports/`):
  * `user_interaction.rs`: the `UserInteraction` port — owned boxed
    closures (`stderr_line`, `info`, `warn`, `show_plan`, `choose`,
    `confirm`) plus the `PlanView`/`PlanRow` display contract. The use case
    decides *what* is shown and asked, including the exact wording; the
    presentation adapter decides *how* it is rendered (colours, column
    widths, prompt mechanics).
* `src/application/features/package/apply_transaction.rs` (new use case):
  * `apply_transaction(ctx, plan, extras, sourcing, op, request, ui)`:
    source picking (strict-repo failure before anything is touched), the
    header info line, the plan table, dry run, the `ask`-mode
    repository-vs-build choice, confirmation, and the per-package
    fetch/build + install loop with the failure continuation notices.
    Returns `Applied` (`NothingToDo` | `Done { changed, seconds }`) — the
    raw elapsed seconds go back so the handler keeps `format_duration`.
  * `ApplyRequest` carries plain values + the injected `BuildStep`
    (`commands::build_one` stays presentation); no CLI flag struct crosses
    the boundary (1d precedent).
  * Moved from `commands/installing.rs`: `install_one` (per-item
    `InstallRequest` now carries the per-run force/allow_unsigned knobs),
    `acquire` (fetch with the auto-mode build fallback), `count_repo`,
    `plan_view` (row computation), `archive_name_of` (still used by the
    handler's `install_archive_file`).
* `src/commands/installing.rs`: `interaction(yes)` wires the port to the
  real `confirm::` calls, status macros and `render_plan` (the moved
  table/totals renderer); the success line and `warn_revdep` are
  handler-side again. cmd_install/cmd_update match on `Applied`.
* Facade narrowed (design stage 1 tail): `pub(crate) use installing::*` →
  the six dispatched commands (`cmd_autoremove`, `cmd_install`,
  `cmd_outdated`, `cmd_remove`, `cmd_update`; `cmd_install_with_why` stays
  on the module path for `groups.rs`).
* Tests: 8 new unit tests on the use case (recorder port): empty plan,
  dry run, declined confirm, ask-mode extras bail, ask-mode build switch
  (build closure invoked, no fetch), strict pick failure before the plan
  is shown, pick-note-before-plan ordering, first-item failure without a
  continuation note. Real-install success paths stay covered by the
  integration tests and smoke suites.
* **One intentional normalization** (everything else byte-identical):
  `nawa update` used to run the reverse-dependency warning **twice** on
  success — once inside `run_plan` (over items+extras, before the
  transaction finished) and once in `cmd_update` (over items, after it).
  The extraction keeps the post-finish call only (always, over the plan
  items; failure-path order unchanged: finish → warn → return the error).
  Failure/dry-run/nothing-to-do paths print exactly as before.
* Verified: `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean;
  `cargo test --all-targets --offline --no-fail-fast` → **622 passed /
  2 ignored / 3 env-failed** (= 614 + 8 new; the same 3 pre-existing env
  failures); `python3 scripts/check-layering.py` green (55 modules).
  (One full-suite run mid-session reported 8 failures: the container's
  `/tmp` tmpfs had filled with leaked per-run test fixture dirs — cleaned,
  re-run green.)
* **Byte-identical behavior**: `--help`, `--version` and all three
  completion scripts identical to a HEAD release build (isolated worktree,
  removed afterwards); a paired 8-step install/update scenario (pick note
  → plan table → confirm → fetch → install → success; dry run; the
  non-interactive refusal; the planner error; the strict-repo update
  refusal; the real update; the up-to-date dry run) produced **identical
  transcripts**; `scripts/smoke-v06.sh` and `scripts/smoke-v05.sh`:
  **ALL PASSED**.
* Formatting: all new files rustfmt-clean; `commands/installing.rs`'s
  rustfmt dirty hunks went 26 → 18 (the moved code's violations became
  clean inside the new files; `render_plan` carries the old `print_plan`
  body verbatim — the same pre-existing violation relocated, none new).

## Current task

**Phase 3 — profile and planning** (design §6–§8). B3 is done as a
*proposal* (below); next step: the **solver/planning use case** that consumes
`domain/system/capability.rs` + `domain/package/metadata.rs` + `detect.rs`
(`plan_install`'s dependency resolution moves behind it):

1. Design the use-case boundary: what `plan_install` currently resolves from
   repo entries + db records, and which of it becomes normalized candidate
   input (§8.1) to a presentation-free use case.
2. Keep package/index/db formats unchanged — the B3 schema is agreed only as
   a proposal; the solver consumes `PkgMetadata` normalized from *existing*
   v1–v3 entries (unknown-vs-empty) and recipe-declared rundeps.
3. Then Phase 4 (system profile, §7/§16.3) as the next externally blocked
   step (B4).

### Done within Phase 3 so far

**B3 — package metadata chain proposal** (design §6.3, §16.1–2):

* `docs/design/package-metadata-chain.md` (new): exact recipe
  (`[capabilities] provides`), archive (`.PKGINFO provides =`, comma-joined
  single value), and index-v4 spellings (`[[package]]` + required
  `rundeps`/`provides`, `deny_unknown_fields`); the chain rules (one value,
  authentication, exact comparison on fetch, legacy = no comparison); the
  unknown-vs-empty table; what binary-only solving requires; the upgrade
  story (opt-in `--index-version 4`, old readers fail closed via the
  existing unknown-schema message, no downgrade, side-by-side v3+v4 during
  migration).
* `src/domain/package/{mod,metadata}.rs` (new, unwired —
  `#![allow(dead_code)]` + justification, Phase-2 proposal style): pure
  normalized model — `RunDeps{Declared,Unknown}`, `PkgMetadata`
  (`unknown()`, `from_pkginfo_map`), `check_chain` (index-vs-archive exact
  compare, §6.3(3)), `IndexEntryV4` (serde + `parse_toml`/`validate`/
  `metadata`) + 15 tests (flat-format rules incl. the no-comma rule,
  v4 required-field fail-closed errors, legacy→unknown mapping, chain
  mismatch cases). Domain stays pure: std/serde/toml +
  `domain/system/capability.rs` only.
* Notable finding pinned by a test: the current install-time reader
  (`Pkginfo::from_map`) is lenient (absent rundeps → `[]`, empty slots
  filtered); the normalized reader is strict and preserves
  unknown-vs-empty. Wiring replaces it on the new path only.

Nothing in this step changes package, index or db formats yet.

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
* Phase 1d:
  * `src/application/lifecycle.rs` (typed `Request` + `lock_mode`;
     `cli::Cmd` import removed)
  * `src/cli/args.rs` (`Cmd::lock_request` translation + 4 tests)
  * `src/commands/dispatch.rs` (`lock_mode(&cmd.lock_request())`)
  * `src/application/features/{mod,package/mod}.rs` (new)
  * `src/application/features/package/plan_install.rs` (new —
    `Source`/`Sourcing`/`open_sourcing`/`plan`/`Notice` + 11 tests)
  * `src/application/features/package/query.rs` (new —
    `why`/`recipe_dependents`/`search_matches` + 3 tests)
  * `src/output.rs` (`Notice::emit` renderer)
  * `src/commands/installing.rs` (handler keeps rendering +
    transaction loop; sourcing/plan calls the use case)
  * `src/commands/querying.rs` (`cmd_search`/`cmd_why` call the use case)
  * `scripts/check-layering.py` (direction rules + legacy allowlist +
    stale-entry detection + `application/features/**` strictness)
* Phase 2:
  * `src/domain/{mod,system/mod}.rs` (new top-level `domain` layer)
  * `src/domain/system/identity.rs` (new — `InitKind`, `Pid1`,
    `classify_pid1`/`identify`, `InitKind::from_capability` + 9 tests)
  * `src/domain/system/capability.rs` (new — normalized `Capability`,
    `Provides` with the Unknown-vs-empty distinction, `InitProvider`
    + 8 tests; §6 schema proposal documented in the module header)
  * `src/application/features/system/{mod,detect}.rs` (new —
    `ObservedSystem`/`Context`/`RuntimeInit`/`InstalledInit`/`DesiredInit`,
    injectable-source `observe()` + 16 tests)
  * `src/main.rs`, `src/application/features/mod.rs` (one-line
    `mod` registrations)
* Phase 1e:
  * `src/application/ports/{mod,user_interaction}.rs` (new —
    `UserInteraction` closure port + `PlanView`/`PlanRow`)
  * `src/application/features/package/apply_transaction.rs` (new —
    `apply_transaction`/`Applied`/`ApplyRequest`/`BuildStep`, moved
    `install_one`/`acquire`/`count_repo`/`plan_view`/`archive_name_of`
    + 8 tests)
  * `src/commands/installing.rs` (handlers call the use case; `interaction`
    adapter + `render_plan`; success line + `warn_revdep` handler-side)
  * `src/commands/mod.rs` (installer facade narrowed to the six commands)
  * `src/application/{mod,features/package/mod}.rs` (port + use-case
    registrations, layering doc updated)
* Phase 3 (B3, in progress):
  * `docs/design/package-metadata-chain.md` (new — the B3 schema proposal,
    answers design §16.1–2)
  * `src/domain/package/{mod,metadata}.rs` (new — pure normalized metadata
    model: `RunDeps`, `PkgMetadata`, `check_chain`, `IndexEntryV4` + 15
    tests; unwired with justification)
  * `src/domain/mod.rs` (one-line `pub(crate) mod package;`)

## Tests actually executed

| Command | Result |
|---|---|
| `cargo check --all-targets --offline` | pass (Phase 0 + 1a + 1b + 1c) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 0, re-measured at HEAD) | 566 pass / 2 ignored / 3 env-failed (baseline, pre-existing) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1a tree) | 569 pass / 2 ignored / 3 env-failed (same 3; +3 new/moved tests) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1b tree) | 564 pass / 2 ignored / 3 env-failed (same 3; −5 = deleted service unit tests) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1c tree) | 564 pass / 2 ignored / 3 env-failed (identical to 1b; moved tests verified present) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1d tree) | 581 pass / 2 ignored / 3 env-failed (= 564 + 18 new − 1 net lifecycle test; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 2 tree) | 614 pass / 2 ignored / 3 env-failed (= 581 + 33 new: 16 detect + 9 identity + 8 capability; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 1e tree) | 622 pass / 2 ignored / 3 env-failed (= 614 + 8 new apply-transaction tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 3 B3 tree) | 637 pass / 2 ignored / 3 env-failed (= 622 + 15 new metadata tests; same 3 env failures) |
| `cargo clippy --all-targets --offline -- -D warnings` | pass (Phase 1a + 1b + 1c + 1d + 1e + 2 + 3/B3) |
| `python3 scripts/check-layering.py` | pass (54 after 1a; 53 after 1b; 54 after 1c; 55 after 2 — the new `domain` layer; 55 after 3/B3 — the count is top-level modules, the new `domain/package` submodule is scanned) |
| `bash scripts/smoke-v06.sh` (release build, Phases 1b + 1c + 1d + 2) | ALL V0.6 SMOKE TESTS PASSED |
| `bash scripts/smoke-v05.sh` (release build, Phases 1d + 2) | ALL SMOKE TESTS PASSED |
| HEAD-vs-1c behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical |
| HEAD-vs-1d behavior diff (`--help`, `--version`, completions × 3 + 12 read/planning commands, isolated worktree build) | byte-identical |
| HEAD-vs-2 behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical (detection adds no CLI surface) |
| HEAD-vs-1e behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical |
| HEAD-vs-1e paired 8-step install/update scenario (pick note → plan → confirm → fetch → install → success; dry run; non-interactive refusal; planner error; strict-repo update refusal; real update; up-to-date dry run; local signed repo, worktree build) | identical transcripts |
| `cargo fmt --all -- --check` | fails repo-wide at HEAD (pre-existing ~100 files; CI does not run fmt) — re-scoped to "no new violations in touched/new code", satisfied (1a–1e; 1e new files clean, installing.rs dirty hunks 26 → 18, render_plan carries the relocated pre-existing print_plan violation verbatim) |

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
| B1 | **saq init selection**: disk-boot `init` fallback chain (`$INIT_ARG /usr/bin/runit /sbin/init /usr/sbin/init`) contains no systemd path, and Faseela's grub.cfg passes no `init=`. During the migration runit must stay installed, so `/usr/bin/runit` would win the chain and boot runit again. Needs: migration boot config sets `init=` explicitly **and/or** saq change (e.g. read a selection file) — exact interface TBD with Faseela/SAQ. | Faseela/SAQ | Phase 6 apply-safety (Phase 2 detection tests were unblocked: read-only, fake `/proc`) |
| B2 | **`/usr/bin/init` payload conflict**: old runit-services and the systemd package both ship `/usr/bin/init`. Staged coexistence (install systemd while runit is still installed) hits a file conflict or silently replaces the selector. Needs Faseela-side payload/policy decision (move init selector ownership, document force-install + journal-restore behavior). | Faseela | Phase 6 staged install |
| B3 | **Repository index v4 schema**: ~~needs a written schema proposal + tests~~ **proposal written** (`docs/design/package-metadata-chain.md` + unwired reference model `domain/package/metadata.rs`, 15 tests). Awaiting maintainer agreement before wiring (recipe/build/repo-create/validate/fetch changes listed in §10 of the proposal). | Nawa (this repo) | Phase 3 (proposal done; wiring when agreed) |
| B4 | **System profile location/merge contract** with Faseela (defaults path, override path, precedence) — design §16.3. | Faseela agreement | Phase 4 |
| B5 | **Bootloader/initramfs scope + recovery route** for the first migration (grub only? which initramfs tool versions) — design §16.4–5. | Faseela | Phase 6 |
| B6 | **VM boot-test harness**: boot-critical verification requires disposable VMs with snapshots (BIOS + UEFI). No harness exists in this repo yet. | Nawa/infra | Phase 6 acceptance |
| B7 | **Hook/trigger trust policy** (design §16.9): explicit documented policy + CI guard for `.HOOKS`/transaction-trigger service activation. | Faseela (+ Nawa docs) | Phase 1 docs, Phase 5 |

## Exact next task

**Phase 3, solver/planning use case**: move `plan_install`'s dependency
resolution behind a presentation-free application use case that consumes
normalized candidates — `PkgMetadata` (unknown-vs-empty) from existing v1–v3
repo entries, recipe-declared rundeps, and `domain/system/capability.rs` +
`detect.rs` for the init-system role (design §8.1–8.2). No format changes
(the B3 schema stays a proposal until agreed). Then verify with `cargo check
--all-targets --offline && cargo test --all-targets --offline
--no-fail-fast && cargo clippy --all-targets --offline -- -D warnings` plus
`python3 scripts/check-layering.py`; new files rustfmt-clean; commit as one
coherent unit.
