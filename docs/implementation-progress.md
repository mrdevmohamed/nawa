# Nawa implementation progress

> Live tracker for the system-intelligence-and-migrations implementation.
> Specification: `docs/design/system-intelligence-and-migrations.md`
> (design branch `docs/system-intelligence-migration-design`).
> Audit: `docs/architecture-audit.md`. Update this file before ending every
> working session. Do not redo completed work; read it first.

## Status snapshot

| Phase | State |
|---|---|
| 0 — Source audit | **DONE** (baseline audit; refresh against the integrated platform before final acceptance) |
| 1 — Structural refactor + service removal | **DONE** (1a, 1b, 1c, 1d, 1e — 1e is the deferred apply-transaction extraction) |
| 2 — Init identity & capability foundation | **DONE FOR THE FIRST VERTICAL SLICE** (the generic model exists, but observed capabilities and providers currently cover init-system only) |
| 3 — Metadata & capability resolution | **DONE FOR INIT PROVIDERS; PLATFORM SOLVER INCOMPLETE** (recipe → .PKGINFO → signed index v4 → solver is wired; version constraints, capability requirements beyond init, conflicts and replacements are not represented by the current index model) |
| 4 — System profiles & planning | **PARTIAL PLATFORM** (profile model and precedence work for init-system; system planning is being unified with the authenticated repository transition preview) |
| 5 — Migration engine | **FRAMEWORK IMPLEMENTED, BOOT EXECUTION BLOCKED**: typed state machine, durable journal, mutation gate, coordinator and recovery exist; Snapshot is not yet wired to coordinator/recovery and the real boot handler remains blocked. |
| 6 — Faseela runit→systemd migration | **BLOCKED** (current Faseela HEAD already uses systemd; the legacy runit migration is for older installations. Real BIOS/UEFI boot acceptance and rescue/recovery contracts remain open.) |
| 7 — Platform verification | **IN PROGRESS**: Nawa-side inspection and repository preview exist; unified system planning, full solver semantics, integrated recovery and real boot acceptance are not complete. |
| System Intelligence platform | **IN PROGRESS** — accepted direction: System Intelligence is the umbrella; migration is one high-risk feature within it. |

## Accepted platform direction (2026-11-18)

The selected product direction is to complete **System Intelligence as a coherent
system-aware planning and reconciliation layer**, with migration as one feature
that consumes the same observations, capability metadata, policy, solver and
explainable plan. Do not treat the runit-to-systemd handler as the product.

The current implementation is an init-system vertical slice, not a complete
system intelligence platform. Preserve the existing Feature-Oriented Hexagonal
Architecture with Functional Core / Imperative Shell; do not restart the
architecture or create a second dependency solver.

Platform completion requires these connected capabilities:

1. **Observation:** a normalized, read-only system snapshot that distinguishes
   runtime facts, installed state, desired profile, execution context, evidence,
   confidence, unknowns and contradictions. Never infer host facts for an
   offline root.
2. **Capabilities and providers:** versioned declarations with explicit
   trust/provenance, exclusivity, compatibility and ambiguity rules. Keep
   `init-system` as the first supported domain; do not invent additional
   provider domains before their metadata and policy contracts are specified.
3. **Unified solver:** one deterministic domain solver for package dependencies,
   provider selection and system constraints. The current v4 model carries
   runtime dependency names and capabilities, but not version ranges, conflicts
   or replacements; those are explicit platform gaps, not silently supported
   features.
4. **Profiles and policy:** merge distribution defaults and administrator
   overrides with clear precedence; represent required, preferred and forbidden
   decisions only when their semantics and trust boundaries are defined.
5. **Explainable plans:** show observed state, desired state, selected providers,
   exact package actions, constraints, unresolved facts, risks and the reason
   for every action. Pin trusted metadata and artifact identities before approval.
6. **Execution and reconciliation:** reuse Nawa's transaction/installer path,
   persistent locking and write-ahead journal. No second installer or generic
   shell-command migration engine.
7. **Verification and recovery:** verify postconditions from observed system
   state; make interrupted operations resumable or explicitly
   `needs-recovery`. Boot snapshots and external rescue paths must be wired and
   tested before a boot-changing migration can execute.

A normal package install/update must never silently switch the running init.
A migration requires its own explicit approval, a verified live-host context,
trusted package metadata, a boot/recovery contract and post-reboot verification.
`--yes` for an ordinary package transaction is not migration authorization.

The first implementation priority is to finish the common platform path and
its security invariants, then complete the init-system migration as its first
high-risk consumer. Current Faseela systemd installations must not be migrated
again; test the runit-to-systemd path only in disposable VMs representing
supported legacy installations.

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

## Current state

**Nawa-side implementation and Phase 7 verification are complete; Phase 6 boot acceptance remains blocked.** The branch includes read-only `system status`, `system profile show`, `system plan` and `system explain init-system` commands, plus a separate `upgrade` migration-candidate preview using trusted index-v4 provider metadata and the existing dependency resolver. The preview is not an executable migration transaction: the current metadata lacks version ranges, conflicts and replacements. The Nawa-side migration framework and safe preflight are implemented; the real Faseela boot transition remains fail-closed until the external contracts below are merged and accepted.
Historical Phase 3–6 notes are retained as an implementation log; later
entries and the blocker table record the current state.

> Historical Phase 3 notes below describe the proposal before B3 was wired.
> See “B3 wiring — Metadata Index v4” for the completed metadata chain.

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

### Done within Phase 3 so far (2): the normalized install planner

**The solver core** (design §8.1–8.2): `plan::install_plan`'s dependency
resolution moved into `domain/planning/install.rs` — the same resolution,
expressed over normalized inputs so the profile-driven provider selection
(Phase 4) and the migration planner consume the same values:

* `Candidate` (§8.1): name / version / rel / `deps` (build + run edges in
  declaration order) / `provides` (the B3 chain — `Provides::Unknown` for
  every candidate today; provider selection consumes it with the Phase 4
  system profile). Assembly is the application's job: `plan.rs` adapts
  `Recipe`/`db::Entry` → candidates and delegates, so there is exactly one
  resolver (§8: extend, not duplicate). Architecture filtering stays at
  candidate assembly (`sys::arch_fits`), documented in the module.
* `Why` moved to the domain enum; its database mapping (`db_reason`) and
  `label` stay in `plan.rs` as the adapter impl. `plan::Why` is re-exported,
  so no call site changed.
* Error wording is pinned byte-identical (`no recipe for …`,
  `dependency … (needed by …) is neither installed nor has a recipe…`,
  `circular dependency: a -> b -> a`) — it is observable behavior.
* `outdated` / `orphans` / `removal_order` stay in `plan.rs` for now; they
  move to `domain/planning/removal.rs` with their use cases (noted as the
  next Phase 3 slice).
* Verification: 11 new domain-planner tests (order, installed-dep
  satisfaction incl. the no-candidate-installed-dep branch, up-to-date,
  replacement, `--no-deps`, typo hint, cycle, missing-dep chain,
  determinism, candidate model) + the 12 plan.rs baseline tests pass
  unchanged through the adapter; `--help`/`--version`/3 completions
  byte-identical vs HEAD; the paired 8-step install/update scenario and a
  resolver-error probe (cycle / missing dep / --no-deps) produce identical
  transcripts against an isolated HEAD worktree build.

### Done within Phase 3 so far (3): the removal/query planners

`plan::outdated`, `plan::orphans` and `plan::removal_order` moved behind
`domain/planning/` the same way the install resolver did — the signatures
(and therefore every call site: `cmd_outdated`/`cmd_update`/
`cmd_autoremove`, group removal, undo) are unchanged, and the bodies are
now thin adapters over normalized inputs:

* `domain/planning/install.rs` gains `outdated(installed: &[(name,
  version-rel)], known: &BTreeMap<String, Candidate>) -> Vec<String>`: the
  update-candidate query with the same strictly-newer comparison the
  install planner applies to a target (`vercmp` on `version-rel`), in the
  order the installed records were given. A package without a candidate is
  not "outdated", just unknown.
* `domain/planning/removal.rs` (new): `InstalledPkg { name, is_dependency,
  run }` (§8.1 installed records; `is_dependency` ⇔ db reason
  `dependency`), `orphans(installed, known_deps, build_deps)` — kept-set
  walk over recorded run deps plus the candidate's full dependency list
  (skipped under `--build-deps`), filtered to installed names — and
  `removal_order(names, run_edges)` — dependents first, a cycle goes last
  unchanged. The adapter pre-filters the run edges to the removal set, so
  the domain function never sees out-of-set edges.
* `plan.rs` adapts: `outdated` maps the domain's name list back to
  `(&Entry, &Recipe)`; `orphans` stamps `is_dependency` from the exact db
  reason string and builds `known_deps` from `deps_of(rec)` (build + run,
  declaration order, the same sequence the candidate `deps` carry);
  `removal_order` builds the filtered run-edge map. Dead code dropped:
  `recipe_full`, `newer_than_installed`, the now-unused `vercmp`/`Ordering`
  imports.
* The 12 plan.rs baseline tests pass byte-identical through the adapters
  (they run the full adapter → domain path).
* Verification: 10 new domain tests (2 `outdated` in install.rs — strictly
  newer in input order, equal/older/unknown skipped; 8 in removal.rs —
  kept run/candidate deps, transitive reachability, only-installed deps
  walked, non-dependency roots, dependents-first, cycle last, no-edge
  input order); full suite 658 passed (+10); `--help`/`--version`/
  completions × 3 byte-identical vs HEAD; the paired 8-step scenario, a
  query probe (`outdated` + `autoremove -n` + `update -n` over a seeded db
  with explicit/dependency reasons and recorded run deps — autoremove
  prints the dependents-first removal order) and a real `remove --yes`
  probe (transcript + resulting db state) produce identical outputs
  against an isolated HEAD worktree build.

### Done within Phase 4 so far (1): the profile model, merge rules and loader

**The system profile** (design §7, §16.3): the desired-state model — what
the distribution or administrator has declared the system should use — as
opposed to the observed state the detector reports. Explicitly *not* the
build profile of `src/profile.rs` (`--profiles`): separate concepts,
separate models, separate paths, as the design demands.

* `domain/system/profile.rs` (new, pure): `SystemProfile` (schema pinned
  to 1, optional `distribution`/`profile`/`architecture` scalars,
  ordered `providers` role → identity map) with `from_doc` over
  normalized maps — strict like the B3 chain: unknown keys and unknown
  schema versions are parse errors, never silently misread (fail closed,
  §7); roles/identities normalize like `Capability` (§6.1).
  `merge(defaults, overrides)` produces the **effective target profile**:
  overrides take precedence field-wise, provider roles overlay per role
  (an override replaces or adds a role; it cannot remove one), either
  side may be absent, an override without defaults is a valid profile.
  `init_capability()` exposes the selected init provider as the §6
  capability the solver will compare against candidate declarations.
* `application/features/system/profile.rs` (new): the loader with
  injectable paths (B4 only pins the contract): recommended layout
  `<root>/usr/share/nawa/system-profile.toml` (distribution defaults) +
  `<root>/etc/nawa/system-profile.toml` (administrator override). Both
  missing → `Ok(None)` (no desired state declared — an absent profile is
  not read as a decision); an override may exist without defaults; an
  invalid file on either side **fails closed naming the file** — never
  silently ignored in favor of the other side. Read-only: a package
  update never touches these files (§7: local policy is never silently
  overwritten).
* Verification: 20 new tests (11 domain: §7 example, schema pinning,
  unknown keys, normalization, strict providers, empty `[providers]`
  section, merge cases incl. determinism; 9 loader: absent/defaults/
  override-only/merged, invalid override fails closed, invalid defaults,
  table-where-scalar, empty file is invalid not absent, integer schema);
  layering caught one doc-comment `crate::application` reference in the
  domain module (the parser counts doc links) — reworded, layering green.
  The loader is not wired into any command yet (next slice), so CLI
  behavior is trivially unchanged — verified anyway: `--help`/
  `--version`/completions × 3 and the paired 8-step scenario byte-identical
  vs HEAD (isolated builds).

### Done within Phase 4 so far (2): the profile as an explicit plan input (§8.1–8.4)

The effective target profile now reaches the planner and the dry-run
report. Only *reporting* landed: no transaction acts on the init decision
(§7 — a provider switch is the explicitly authorized migration, the
Phase 5/6 engine).

* `domain/planning/install.rs`: new `InitSelection` enum (`Declared {
  identity }` = the normalized identity the effective target profile
  selects for `init-system`, `Unresolved` = the explicit no-decision,
  default) carried on `Resolution::init` (§8.2: the exact selected
  provider or an explicit unresolved result — never a guess).
  `install_plan` gains the effective profile as its §8.1 plan input;
  package decisions are untouched by it (determinism pinned by test).
* `src/plan.rs`: `Plan::init` passthrough + `InitSelection` re-export;
  adapter only, signatures of the untouched planners unchanged.
* `application/features/system/profile.rs`: the loader returns
  `EffectiveProfile { profile, role_sources }` — per-role file
  attribution (§5.1: an observation includes its source; an override
  role points at the override file, a defaults-only role at the defaults
  file). Its `#![allow(dead_code)]` is **removed**: everything is
  production-reachable now.
* `application/features/package/plan_install.rs`: `plan()` takes the
  effective profile; the §8.4 explanation `init_note()` builds the note
  **only when a profile declares `init-system`** — pinned wording
  `target profile selects init-system=systemd (from
  <file>) — installing does not switch the init provider`; an unresolved
  decision prints nothing (no bytes change without a profile).
* `src/commands/installing.rs`: `cmd_install_with_why` and `cmd_update`
  load the effective profile (`sys_profile::load_effective(&ctx.root)?`)
  and pass it through; an invalid profile **fails closed** (§7): the
  command refuses (exit 1, error names the file + cause) instead of
  planning against a broken desired state. Scope boundary: plain archive
  installs (`nawa install foo.nawa.tar.zst`) bypass the planner entirely
  (no recipe, no plan) and return before the profile load — they are not
  recipe-backed planning paths.
* `application/features/system/detect.rs`: `DesiredInit` upgraded from
  `NotRead` to §5.5's real desired state — `Declared { identity, source }`
  / `NotDeclared` / `Invalid { reason }` — read via `desired_init(root)`
  in both `observe` branches. `detect()` still never fails: an invalid
  profile is *reported* (`Invalid`, file named), never guessed around.

Verification (this slice): 8 new tests (3 domain `InitSelection`, 1
loader role-source attribution, 1 plan-install note test, 3 `desired_init`
tests: declared-with-source / invalid-reports / role-less →
`NotDeclared`); gates green; byte-identity held (`--help`/`--version`/
completions × 3 + 8-step scenario identical vs HEAD — no fixture carries
a profile, so no bytes change), plus 6 positive probes on the new binary:
override profile → note (override path); defaults-only profile → note
(defaults path); `update -n` pending update → note; profile without the
init role → no note; invalid profile → `install` **and** `update` fail
closed with exit 1 naming the file.

### Done within Phase 5 so far (1): the migration domain model and record store (§10.1–§10.3)

Phase 5 = design Stage 4 (migration framework). First slice: the pure
model of the migration framework — no command wiring yet, so CLI
behavior is unchanged (verified; see below). The persistent mutation-gate
*check* and the coordinator are the next slices.

* `domain/system/migration.rs` (new, pure — no I/O, no serde):
  * `MigrationDefinition` (§10.1): typed, versioned, validated — the id is
    file-name-safe (lowercase `a-z0-9._-`, ≤ 128 bytes, traversal-proof),
    `from`/`to` are `init-system` capabilities limited to the §6 scope
    (`runit`/`systemd`), the transition must move somewhere, and the
    handler binding is enforced: the built-in runit→systemd handler owns
    exactly that transition and requires the reboot boundary.
  * `HandlerId` (§10.2): an allowlisted typed enum (`as_str`/strict
    `parse`) — a repository can never name an executable path or shell
    command; unknown handlers fail closed.
  * `MigrationState` + `Transition` + `transition()` (§10.3): the durable
    state machine — `planned → preparing → awaiting-reboot → verifying →
    completed`, with `preparing` failures split by whether compensation
    succeeded (`failed` after compensation / `needs-recovery` otherwise),
    post-reboot verification failure → `rollback-required` (only when a
    documented rollback exists) → `failed` after a verified rollback, and
    `Ambiguous` reachable from every gate-active state. Terminal states
    have no exits (a retry is a new, re-approved plan). Illegal
    transitions are named errors; states roundtrip through the exact
    §10.3 table names, unknown names fail closed.
  * The gate policy is model, not I/O: `keeps_gate()` is exactly §10.3's
    active set (`preparing`, `awaiting-reboot`, `verifying`,
    `rollback-required`, `needs-recovery`); `planned` is deliberately not
    gate-active (its record is created immediately *before* the gate is
    taken).
  * `StepRecord`/`StepOutcome` + `MigrationRecord` (§10.3, §11): ordered
    write-ahead steps (intent persisted before the side effect, outcome
    after; an intent without outcome stays interrupted until
    reconciliation), the record schema pinned to 1 with the definition
    version, source/target capabilities and handler bound in.
* `application/features/system/migrate.rs` (new): the durable record
  store — `<root>/var/lib/nawa/migrations/<migration-id>.toml`, one
  versioned file per migration:
  * `save` publishes through the shared `atomic::write_atomic`
    (temp + fsync + rename + dir fsync) at mode 0600 — restrictive
    permissions like the transaction journal, crash never leaves a torn
    record.
  * `load` treats absence as `Ok(None)` (not an error, not a decision)
    and validates fail closed: unknown schema/state/handler, a failed
    step without a reason, or a succeeded step with a failure is an
    error naming the file.
  * `active_gates` scans the record directory and reports every
    gate-keeping record sorted by id (`ActiveGate { id, state }`) — the
    gate-check wiring consumes it in the next slice.
  * Creation semantics belong to the use case, documented in the module
    header: the durable `planned` record is written only after live-host
    preflight + explicit approval; offline-root planning stays read-only.
* Both modules carry module-scope `dead_code` allows with justifications:
  nothing consumes them in production until the coordinator/gate wiring
  lands (next slice).
* Verification: 20 new tests (13 domain: the §10.1 example, fail-closed
  definition validation incl. path-looking ids, the handler allowlist vs
  paths, state roundtrip, the legal forward path, compensation split,
  rollback→failed, ambiguity reachability, illegal-transition errors,
  the §10.3 gate set, fresh record, step intents; 7 store: roundtrip,
  absent, restrictive 0600 + replace, corrupt/unknown records fail
  closed naming the file, traversal-proof ids, gate scan sorted,
  gate-active states survive the format). Gates green; byte-identity
  held: `--help`/`--version`/completions × 3 + 8-step scenario identical
  vs HEAD, and a seeded `preparing` migration record under a scenario
  root leaves `install -n`/`update -n` output unchanged (nothing reads
  records yet — the module is inert by construction). (Bookkeeping note
  found while wiring slice 2: that probe's heredoc record was actually
  malformed TOML — keys written after a `[to]` section header nested
  under it — harmless at the time because nothing parsed records; later
  slices seed records in the store's real format, scalar fields first,
  and a malformed record now *matters*: it fails the mutation gate
  closed with the precise cause.)

### Done within Phase 5 so far (2): the persistent mutation-gate check (§10.3)

The gate is now enforced on every package-mutating path, block-only —
no bypass exists because no coordinator permit exists yet (§10.3's
typed permit arrives with the coordinator slice).

Placement decisions first:

* The record store moved from `application/features/system/migrate.rs`
  to the top-level `migration_record.rs`. Reason: the lock mechanism
  must check the gate before every exclusive acquisition, and the lock
  module must not depend on the application layer — application owns
  the transaction-apply path, which itself locks (`apply_transaction.rs`
  imports `lock`), so a lock→application edge would close a cycle.
  §4.3's module organization explicitly keeps cohesive I/O subsystems
  top-level (`transaction::journal` is the precedent); the store's
  header documents this.
* The application layer got its own `LockNeed` vocabulary
  (`application/lifecycle.rs`): `lock_mode` → `lock_need`, returning
  `LockNeed::{Exclusive, Shared}`; `commands/dispatch.rs` maps that
  onto the mechanism's `lock::Mode`. The dependency direction stays
  application ← shell, and `lock::acquire` remains the single choke
  point every exclusive acquisition goes through — which is where the
  gate check lives.

Gate semantics (`lock.rs::check_migration_gate`, wired into `acquire`):

* Exclusive paths only, and always under a lock that covers the state:
  under this process's own exclusive flock (checked after
  `Attempt::Locked`), or — for the nested hook-spawned case — *before*
  the `NAWA_LOCK_HELD` marker is honored (§10.3: the marker can never
  authorize a mutation or bypass the gate; both outcomes refuse, and
  when a migration is pending its error is the more precise one).
* Shared/lock-free paths are never checked, on purpose: §10.3 keeps
  read-only inspection available during a pending migration and readers
  are best-effort — even a corrupt record must not take read commands
  down. Mutations are strict: unreadable gate state fails closed
  ("cannot check the persistent migration mutation gate … refusing to
  change the package set fail-closed", exit 1).
* The refusal names every active migration id and its state, the gate
  rule, and the record directory: "a system migration holds the package
  mutation gate: <id> is <state> — package-mutating commands are
  blocked until the migration completes or is recovered (§10.3;
  migration records: <dir>)".
* Complete inventory of gated sites (everything that goes through
  `lock::acquire(Mode::Exclusive)`): the dispatch whole-command lock
  (remove, rollback, autoremove, history, all-archive install,
  non-dry-run upgrade, group remove); the per-step locks — install-by-
  name and update begin-operation/apply-step/finish (their `--dry-run`
  forms run the same begin-operation block), `build --install` steps,
  group install; the install hook-repair lock; post-transaction trigger
  hooks. Not gated: every Shared acquire (dispatch snapshots,
  `installed_map`) and lock-free dry-run short-circuits.
* Observable consequences, all verified on the new binary: under a
  pending migration, `remove`/`autoremove`/`upgrade`/archive install
  refuse as the very first thing (exit 1); by-name `install -n` /
  `update -n` refuse at the gated begin-operation lock *before*
  printing the plan (the dry-run still writes an operation record, a
  nawa-owned mutation, so the gate stops it); `upgrade -n` (Shared) and
  all reads still work. The gate lifts when the record stops keeping it
  — nothing sticky. `NAWA_LOCK_HELD=1` + pending migration → the gate
  error wins over the hook-context error; marker + reads still work.
  A parent command cannot race a gate into existence under it:
  `BeginPreparation` (§10.4) requires the coordinator to hold the
  exclusive package-database lock itself — documented in `lock.rs`,
  enforced when the coordinator slice lands.
* Tests: 4 lock.rs unit tests (gate blocks Exclusive and unblocks after
  record removal; planned/terminal records keep it open; readers never
  blocked, including a corrupt record; corrupt/unreadable records fail
  the exclusive gate closed) and 4 tests/lock.rs integration tests
  (mutating commands refuse naming id+state+gate+record dir and leave
  the database untouched; reads stay available; the forged marker
  cannot bypass and the gate error wins; the gate lifts when the record
  goes away). The existing forged-marker tests still pass unchanged —
  the gate-free marker error text is preserved exactly.

### Done within Phase 5 so far (3): the read-only inspection CLI (§12.2)

The record store's first production consumers: `nawa system migrate
list` / `show <id>` / `status` — strictly read-only per §12.2, under the
namespace §12.2 names (no second top-level `migrate` command).

Lock policy (application-owned, not per-command):

* All three commands translate to `Request::ReadDb` (shared lock) in
  `cli/args.rs::lock_request` — the records live under
  `<root>/var/lib/nawa`, which the database lock covers, so a listing is
  one coherent snapshot. Readers are never gate-checked (§10.3 keeps
  inspection available during a pending migration), and a shared
  acquire on a root with no lock file is `Attempt::NotNeeded`, so
  inspection never creates anything.
* `application/lifecycle.rs`'s `Request::ReadDb` doc updated to name the
  migration-inspection consumers.

Shape (functional core / imperative shell, same split as `history`):

* `application/features/system/migrate_inspect.rs` (new): the use cases
  return typed values — `list(root) -> Listing { records, unreadable }`
  sorted by id, `show(root, id) -> MigrationRecord`,
  `status(root) -> Status { dir, blocked, gates, unreadable, scan_error
  }` (+9 unit tests).
* `commands/system.rs` (new): `cmd_system`/`cmd_migrate` plus pure
  render functions returning `Vec<String>` (`render_list`,
  `render_show`, `render_step`, `render_status`, `render_gate`) so every
  presentation rule is unit-testable (+12 unit tests). Column layout
  mirrors `nawa history`. Registered in `commands/mod.rs`; dispatch arm
  `Cmd::System { action }` added in `commands/dispatch.rs`.
* `cli/args.rs`: `Cmd::System`, `SystemCmd`, `MigrateCmd::{List, Show {
  id }, Status}`; `src/plugins/manifest.rs` adds `"system"` to
  `RESERVED_COMMANDS` (plugins can never shadow it); bash/zsh/fish
  completions in `cli/completions.rs` gained the namespace.

Per-command semantics (§12.2, verified by tests):

* `list` — every record in deterministic (sorted-id) order with state,
  from→to capability keys and definition version; an empty store is a
  stated result ("no migration record exists under <dir> — no migration
  has ever been started under this root"), not silence. Corrupt records
  do not hide the readable ones: rows print first, then the command
  fails with exit 1 naming each unreadable id with its *actual*
  validation cause (the full error chain, file + failure).
* `show <id>` — the full durable record: schema, definition version,
  from/to capabilities, state, handler, and requires-reboot derived
  from the trusted handler binding (see below), plus every ordered
  write-ahead step with intent and outcome; an intent without an
  outcome prints as `INTERRUPTED` and never reads as a success (§10.3:
  reconciled on restart). Unknown id → exit 1 naming the record
  directory and pointing at `list`; a corrupt record → exit 1 naming
  the exact file and the validation failure (this command is allowed to
  fail loudly where the gate's best-effort scan must not).
* `status` — informational, exit 0 in every case: whether package
  mutations are blocked under the selected root, by which migration(s)
  (id + state, sorted), and the record directory. It mirrors the gate's
  fail-closed semantics rather than inventing its own: an unreadable
  record (or a record directory that cannot be read at all) is reported
  as "blocked fail-closed — the record state cannot be verified",
  because that is exactly what `lock::check_migration_gate` does with
  the same input. The machine-checkable refusal remains the gate's
  exit-1 error on mutating commands.

Derived reboot requirement (§10.1/§10.2):

* `domain/system/migration.rs` gains `HandlerId::requires_reboot()` and
  `MigrationRecord::requires_reboot()`: whether a migration needs the
  reboot boundary follows from the *trusted handler binding* (the
  built-in runit→systemd handler owns the reboot boundary, §10.4), not
  from a persisted flag a repository could set. Documented as never
  persisted; pinned by the new domain test
  `the_reboot_requirement_follows_the_handler_binding_not_a_field`.

Record-store repairs and refactors (`migration_record.rs`):

* Concrete defect: `load`'s schema/state/handler/step validation errors
  did not name the record file (only the TOML-parse and capability
  paths did), contradicting the store's documented fail-closed contract
  ("an error naming the file"). Fixed by making `load` the single site
  that names the file: `into_record`/`into_capability` lost their
  now-redundant file contexts (the capability path had produced the
  file name twice in one chain), the TOML parse keeps its
  "not valid TOML" cause under the same one-time context, and every
  failure renders as `<file>: <cause>`. The store test was strengthened
  to pin file-naming on the schema path. Observable consequence beyond
  `show`: gate refusals on corrupt records now carry the file path in
  their error chain (existing gate tests assert on cause substrings, so
  they pass unchanged).
* `record_ids(root)` extracted (sorted `.toml` ids, directory-read
  errors propagate); `active_gates` refactored onto it with identical
  semantics.
* Inspection scan policy: a record seen in the directory listing but
  absent when read (`Ok(None)` mid-scan) is reported as an
  `UnreadableRecord` ("removed while being read") rather than silently
  skipped — under the shared inspection lock nothing may remove
  records, so absence is a defect worth naming.

Verification:

* 35 new tests: 9 use case + 12 presentation + 13 integration
  (`tests/migrate.rs`: list on a fresh root / empty dir / several
  records / corrupt-record aggregate error, full `show` incl. an
  interrupted step, unknown id exit 1, corrupt `show` exit 1, `status`
  with an active gate / without one / with a corrupt record, a
  read-only proof (byte-for-byte record file unchanged + no new files
  after all three commands), rootedness under `--root`, an offline
  root), plus the domain reboot-binding test.
* Gates green; manual probes on the binary: seeded `awaiting-reboot`
  record → `list`/`show`/`status` report it and `status` says mutations
  are blocked; a by-name `install -n` still refuses at the gate while
  `system migrate status` exits 0; `--help` lists `system`; unknown-id
  and corrupt-record paths exit 1 with the messages above.
* Existing commands untouched: no fixture carries a migration record
  and the only production paths that read records are the three new
  commands plus the gate check from slice 2, which is already wired.
* Existing commands untouched, verified byte-level vs HEAD (isolated
  worktree build): `--version` identical; the only `--help` and
  completion-script (bash/zsh/fish) differences are the new `system`
  entries themselves — no other byte changes. Both release smoke
  suites (v05 + v06, which exercise real install/update/rollback
  transactions) pass on the new binary.

### Done within Phase 5 so far (4): the migration coordinator and the typed in-process permit (§10.3–§10.4)

The gate was block-only; this slice adds the *only* legitimate way past
it and the first real lifecycle driver over the durable state machine.
No CLI surface yet: the future `nawa system migrate apply` command
(§12.2) owns preflight + plan presentation and is a later slice, so
binary behavior is unchanged by construction.

**The permit (`MigrationPermit`, inside `migrate_coordinator.rs`):**

* Unforgeable in-process proof that this coordinator run may continue
  its approved package operations while the mutation gate is active
  (§10.3, §14.1). Zero-sized; the authority it proves is structural —
  the holder is inside the coordinator's call path, beside the lock
  guard that proves lock ownership. The `_seal` field is module-private,
  so the compiler rejects construction anywhere else; there is no
  `Clone`/`Copy`, no serde, no `FromStr`/`Display`, no environment
  spelling that could impersonate one. A hook-spawned nested `nawa`
  receives no permit and is still refused by the gate (`NAWA_LOCK_HELD`
  stays forge-proof and never authorizes a mutation).
* Placement finding: a top-level `migration_permit.rs` with a
  `pub(in super::application::features::system)` minting function was
  attempted first and rejected by rustc (E0742: visibility may only name
  *ancestor* modules — a top-level module cannot restrict to a path
  through its sibling `application`). The permit therefore lives inside
  the coordinator module; the only cross-feature import is
  `apply_transaction` naming the permit type (`features/package` →
  `features/system`, an intended application-internal edge).
* The durable record store is deliberately *not* permit-gated: the
  top-level `migration_record` module cannot import from the application
  layer (that would be an inversion; `migrate_inspect` already
  constructs application types). `migration_record::save` keeps its
  direct calls, with the coordinator as its only production caller.

**Lock-ownership inversion (`apply_transaction.rs`):**

* `install_one` gained an `ownership: LockOwnership<'_>` parameter —
  `PerStep` acquires and releases the exclusive lock around that one
  install exactly as before (every pre-existing call site passes it;
  ordinary behavior is unchanged), while `Coordinator { guard, permit }`
  performs *no* acquire at all: the flock is per open file description,
  so reacquiring inside the coordinator's held lock would deadlock
  against ourselves, and once the gate is active an ordinary acquire is
  refused anyway.
* `install_for_migration` is the coordinator's entry into the ordinary
  verified installer: the same `install_one` path (artifact-hash
  pinning, payload conflict checks, db update, WAL/history) with the
  borrowed guard + permit as the authorization. `MigrationInstall` is
  the request struct (staged archive, planned name, pinned sha256,
  reason/force/allow-unsigned, provenance). Nothing is fetched or built
  here — artifacts are staged before the mutation phase (§10.4 stage 4).

**The coordinator (`prepare`, §10.3–§10.5):**

* Signature pins the call-site order: `PreparationRequest` carries the
  validated definition, the `ApprovedPlan` (the exact steps the
  coordinator may perform, in order), the consumed `Approval::explicit()`
  — the caller's statement that preflight passed and the plan was shown
  and explicitly approved; what that means concretely is the future
  `apply` command's policy (§12.4: not a generic auto-confirm) — and the
  presentation port.
* Sequence: ordinary `lock::acquire(Exclusive)` (the gate check rides
  along: a pending migration's refusal names it precisely; a `None`
  acquire under an inherited held marker is an internal error — the
  coordinator requires the real lock); refusal when *any* record with
  this id already exists (re-running a failed migration is a
  recovery/re-plan decision, not a silent overwrite); the durable
  `planned` record is saved; one real transaction is begun via
  `begin_operation` with `OperationCommand::Migration` ("migrate") and
  `requested = [migration id]` so history links the transaction to the
  migration (§11); `BeginPreparation` under the held lock makes the gate
  active (from here every *ordinary* exclusive acquisition under this
  root is refused — ours continue through the held guard).
* Write-ahead step discipline (§10.3): `interrupt::check()` between
  steps (Ctrl+C was deferred until the previous side effect completed;
  between steps the system is consistent), the step's intent is
  persisted *before* the side effect and its outcome *after* — a crash
  between the two leaves exactly the interrupted shape
  (`intent` set, `outcome: None`) that the recovery slice reconciles.
  The first failing step breaks the loop.
* Failure split (§10.3's compensation rule): a failure with *no*
  committed step before it → `PreparationCompensated` → `failed`
  (nothing changed, compensation is trivially complete, the gate is
  clear, a retry is a new re-approved plan); a failure *after* earlier
  steps committed → `PreparationAmbiguous` → `needs-recovery` (the gate
  stays active; there is no automatic compensation yet — that is the
  fail-safe until the recovery slice adds verified compensation).
  Success → `PreparationCommitted` → `awaiting-reboot` (the gate stays
  active until the post-reboot resume; §10.4: Nawa exits with a
  pending-migration status and never reboots by itself). `prepare` never
  clears the gate on its own — only verified completion or a completed
  verified rollback may (later slices).
* The transaction ends through `op.finish` (Completed / Partial / Failed
  from the step outcomes); the lock guard drops with the return, after
  which a gate-active record refuses every ordinary package mutation.

**Tests (7, all against real scratch roots and real installs):** full
success (awaiting-reboot; the gate then refuses an ordinary exclusive
acquire naming id + state; the package really is in the db; history
shows command "migrate", requested = the migration id, status
Completed), first-step failure (Compensated; gate clear; nothing
installed; Failed transaction), mid-plan failure (Ambiguous;
needs-recovery gate stays; the committed first package is real; Partial
transaction), begin-refusal on an existing record (any state; the
record is not modified), begin-refusal while another migration is
pending (the gate refusal names *that* migration), an empty plan still
walks the full lifecycle, and the interrupted-step crash-window shape
round-tripping through the store. One fixture fix during bring-up: the
staged-archive `.PKGINFO` needed `pkgrel = 1` — the verified installer's
inspection requires a nonzero pkgrel, which the first test run caught.

**Verification:** `cargo check --all-targets --offline`,
`clippy --all-targets -- -D warnings`, layering (56 modules, no new
edges), fmt (new file + touched files clean), full test battery
(758 pass / 2 ignore / 1 known env-fail), release build + both smoke
suites (real install/update/rollback over the changed install path).
`main.rs` is byte-identical to HEAD (the attempted top-level
`migration_permit` registration was fully reverted when the E0742
approach died). No CLI surface change by construction — no `cli/`
completions/help edits; ordinary install behavior is pinned by the
pre-existing `apply_transaction` tests plus the smokes.

### Done within Phase 5 so far (5): durable lifecycle / write-ahead hardening (5B) (§10.3–§10.4)

Gap analysis first: §10.3–§10.5 and §14.1–14.2 re-read against the
landed slice-4 coordinator. The crash/edge review found six defects and
one undocumented area, all fixed in this slice:

* (A) a failed `begin_operation`/gate save *after* the planned record
  was published stranded an ungated `planned` record plus an open
  transaction, blocking every re-apply of the id;
* (B) record-save failures inside the step loop propagated with `?` and
  skipped the failure split — the record stayed `preparing` and the
  transaction never finished;
* (C) the crash windows around the gate save were unhandled and
  undocumented — decisive case: a publish-only failure (rename done,
  directory-entry fsync lost) returns Err from the save while the gate
  IS durably active on disk, so deleting the record "on failure" would
  clear a live gate;
* (D) the committed-anything classification trusted step *outcomes*, so
  an error after the database committed (failing post-install hook
  class, or a failed outcome/history write) could conclude `failed`
  with a cleared gate over a changed system;
* (E) `interrupt::check()` errors also skipped the failure split;
* (F) no interrupt guard was active during `prepare` — Ctrl+C killed
  the process mid-step instead of deferring to a clean split.

**Gap verdicts against the spec (each item explicit):**

| §10.3–§14.2 item | Verdict |
|---|---|
| One record-save order per lifecycle step (persist intent → effect → verify postcondition → persist outcome) | **done** — every boundary goes through one `persist` helper; ordering pinned by tests |
| Atomic-write durability of record saves incl. the publish-only window | **done** — save error is re-read: on-disk == intended ⇒ publication stands; anything else fails naming what is on disk; deletion is never used to "recover" a gate |
| Gate/begin race: no ungated record stranded by an in-process begin failure | **done** — `abandon_planned`/`abandon_gate_failure` (best-effort remove, remove failure leaves the record for recovery and names it); kill-in-window documented as a 5C input |
| Never trust the failing command's claim (failed command ≠ no side effect) | **done** — pre-step db snapshot + `step_committed` (identity + shared `committed_hook_failed` predicate); db unreadable is fail-safe `committed` |
| Never trust a success claim either (verify postconditions before persisting success) | **done** — `verify_install_postcondition` (planned identity present + plan-pinned digest) before `Succeeded` is persisted |
| Ending publication: an unpublishable conclusion is not a verified outcome | **done** — `conclude` escalates `Committed`/`Compensated` to `needs-recovery` re-declared from the still-durable `preparing` state; escalation failure keeps the previous gate-keeping state and is reported |
| Transaction history finish failures never mask the migration outcome | **done** — `op.finish` best-effort + explicit warn; the record on disk is authoritative for the gate |
| Deferred Ctrl+C semantics | **done** — coordinator holds its own `interrupt::Guard` (nests with the installer's payload guards); the deferred signal becomes the next between-steps failure and routes through the normal split; three signals still force-exit (documented crash window) |
| Crash/interruption behavior documented at every boundary | **done** — module header carries the seven-boundary inventory (before planned save / after planned save / after gate save / between intent and outcome / after outcomes before conclusion / after conclusion before finish) |
| Every crash-boundary shape pinned by a test (§14.1–14.2) | **done for preparation** — 14 new tests (list below); post-reboot shapes arrive with 5C |
| Rollback route (§10.5) and automatic compensation of committed steps | **deferred to 5C with reason** — `rollback-required` exists in the state machine; `prepare` only ever lands `failed`/`needs-recovery`/`awaiting-reboot`, and recovery decides the compensation route |
| Post-reboot resume/verify/finalize, interrupted-step reconciliation | **deferred to 5C with reason** — the exact on-disk shapes it must consume are now pinned by this slice's tests |
| Package WAL recovery kept separate from boot recovery | **deferred to 5C with reason** — unchanged in this slice (the file journal is still recovered by the ordinary install recovery) |
| Gate/permit security audit (§13) | **deferred to 5D** — slice 4's permit/lock surface unchanged here |

**Implementation notes.** The `planned` record is deliberately not
gate-active, so an abandoned one is removed rather than left lying
about; a *process kill* in that window still leaves it (no gate, begin
refuses to clobber) for 5C. History-status mapping stays honest: the
conclusion escalation of a step-less transaction finishes its history
Completed (no failing step); a failed step with recorded changes is
Partial; a failure whose own recording failed leaves no change → the
bare transaction is Failed — the migration record, not history, governs
the gate, and the user is told which is authoritative when they differ.

**Tests (14 new; 21 in the coordinator module, all against real
scratch roots and real installs).** Cross-module crash-boundary testing
needed an injector: `atomic::failure_injection::arm_at(point, nth,
path_contains)` arms the nth atomic write whose path matches (record
saves live under `migrations/`, history records under
`history/transactions/`, journal step files under `history/journal/`),
one-shot, RAII-cleared, with the three crash shapes (`Wrote`/`Synced` =
nothing published, `Published` = renamed but directory entry unsynced):

* publish-only failures stand: gate save `Published` → the run
  completes with the gate active (the reload-verify keeps a durably
  published gate from being deleted);
* pre-rename failures fail closed: gate save `Wrote` → ungated planned
  record abandoned, transaction finalized Failed, gate clear; planned
  save `Wrote` → immediate failure, nothing on disk, a retry starts
  clean;
* conclusion escalation: awaiting-reboot save failure → needs-recovery
  on disk, gate active, warn captured;
* transaction finish failure → awaiting-reboot stands, gate active,
  warn captured;
* intent-save failure → the step fails *before* its side effect
  (package absent), gate clear;
* outcome-save failure after a committed install → needs-recovery, the
  committed package stays, Partial transaction;
* failure after the db commit (journal result-write failure — the same
  shape as a failing post-install hook, which cannot run under a test
  `--root` as a non-root user) → needs-recovery with the gate active,
  never `failed` over a changed system;
* interrupts: before the first step (nothing committed → `failed`, gate
  clear) and after a commit (conclusion level → needs-recovery, gate
  active);
* crash-window gate consistency (seeded records): `planned` holds no
  gate (mutations stay possible; begin still refuses to clobber);
  `preparing` with an intent-without-outcome and `preparing` with every
  step resolved both hold the gate;
* unit level: `verify_install_postcondition` (missing entry / wrong
  version / digest-pin mismatch fail; planned identity passes) and
  `step_committed` (nothing-installed → not committed; installed-after-
  error → committed without reading a missing archive; same state →
  not committed; same state with a pending hook → committed).

**Verification:** check, clippy (`-D warnings`), layering (56 modules,
no new edges), fmt (coordinator 0 hunks, `migration_record.rs` clean;
the `atomic.rs` regions this slice touched are canonicalized — its
remaining hunks are untouched pre-existing red), full battery 772 pass
/ 2 ignored / 1 known env-fail (three fetch/go tests failed transiently
during iteration with "no space left on device" — the `/tmp` tmpfs was
100% full of stale test scratch; after cleanup they pass, unchanged by
this slice), release build + both smoke suites.

### Done within Phase 5 so far (6): crash recovery, resume, verification, finalization (5C) (§10.3–§10.5, §12.2, §14.1)

5B pinned the exact on-disk shapes a killed or failed preparation leaves; 5C
builds the recovery lifecycle on them. New use case
`application/features/system/migrate_recover.rs`; new CLI verb
`nawa system migrate resume` (the §12.2 namespace, no second top-level
namespace); `list`/`show`/`status` stay read-only.

* **Lock-defined gate-crossing capability (§13).** `lock.rs` gains
  `MigrationAuthority` (zero-sized, module-private seal, no Clone/serde/env/
  CLI representation) with `migration_authority()` and
  `acquire_for_migration(root, authority)`. This is the only path that skips
  the gate check; it still refuses `NAWA_LOCK_HELD` (a forged nested process
  can never cross the gate). `scripts/check-layering.py` now rejects a mint
  call from outside `application/features/system/`, so an ordinary package
  command cannot construct one. The coordinator's private `MigrationPermit`
  is unchanged; `lock::acquire` is factored onto the same inner acquire loop
  so the ordinary path's gate check cannot drift from the recovery one.
* **Case A — interrupted `planned`.** A `planned` record is not gate-active,
  so the absence of a gate proves nothing. Recovery inspects the linked
  transaction; only when no committed package change exists is the record
  discarded, and the removal re-read to prove it. Committed packages (or a
  persisted step) are preserved and retroactively declared `needs-recovery`,
  closing the gate (the legal `planned → preparing → needs-recovery` path).
* **Case B — intent without outcome.** Never re-runs the step. The linked
  package transaction is reconciled through the *existing*
  `transaction::recovery::reconcile` (package WAL, never duplicated), and the
  reconciled per-package before/after states are compared against the actual
  database using `PackageState::matches_entry` (identity + digest). A committed
  change whose after-state no longer matches, a failed change whose before-state
  does not match, and a mismatched success claim all fail closed to
  `needs-recovery` with the gate kept. A step with no journal and a package
  named in its intent present is likewise ambiguous.
* **Case C — every step resolved, conclusion unpublished.** Reconstructs the
  conclusion from durable outcomes *after verifying their postconditions in
  the database* (a success claim without a committed change is rejected),
  then publishes through the same reload-verified atomic path as 5B.
* **Case D — `awaiting-reboot` with unfinished history.** Finalizes the
  linked transaction record via the existing WAL reconciliation without
  changing the authoritative migration record; a discrepancy is reported
  honestly, never silently "fixed".
* **Case E — resume.** Live-host only (`observe`/`observe_canonical`): refuses
  offline roots, chroots, containers and unattributable contexts before
  touching a record; requires root. Verifies PID 1 identity, the target
  provider staged in the root, explicit init-entry-point selection (B1),
  the retained old provider (recovery route), and the recorded source/target
  against the trusted handler. No service is inspected or managed. Success →
  `completed` (gate clear); failure → `needs-recovery` (gate kept). Finalization
  deliberately does **not** retire runit — that needs its own verified rollback
  window and package plan (B2/B5).
* **Case F — rollback.** Only a verified compensation with a documented boot
  restoration may move `rollback-required → failed`. None exists until B1/B2/B5
  are resolved, so verification failure and `verifying`/`rollback-required`
  states stay `needs-recovery` with the gate active — no invented rollback.
* **CLI** (`commands/system.rs`, `cli/args.rs`, `cli/completions.rs`):
  `MigrateCmd::Resume` maps to `Request::NoDb` (it takes its own authority-
  gated lock; an outer lock would deadlock the non-reentrant flock); all three
  completions list `resume`. Recovery/resume decisions are rendered from typed
  values (`render_recovery_note`, `render_resume_outcome`), all unit-tested.
* **Tests:** 18 new recovery use-case tests (all five cases, the gate
  invariants, the zero-sized authority, forged-marker refusal, deterministic
  scan, offline-root refusal) + 3 presentation render tests + 3 integration
  tests (`tests/migrate.rs`: help/completions, offline-root refusal with a
  byte-identical record snapshot, no lock created on refusal).
* **Verification:** `cargo check --all-targets --offline` clean;
  `cargo clippy --all-targets --offline -- -D warnings` clean; layering green
  (56 modules — no new top-level modules; the new mint rule enforces the
  authority boundary); full battery **664 pass / 2 ignored / 1 known env-fail**
  (the pre-existing toolchain-under-`/home` sandbox test); release build +
  both smoke suites pass; every new/touched file is rustfmt-clean.

### Done within Phase 5 so far (7): adversarial security verification (5D) (§13, §14.1–14.2)

5D demonstrates the §13 invariants with real processes and durable state
instead of inspecting the source. New `tests/migration_security.rs` (8
adversarial integration tests), store/record hardening and negative probes:

* **Mutation gate:** an unreadable record directory (a file at the path) makes
  a mutating command fail closed naming the cause; `status` reports the same
  fail-closed answer with exit 0; multiple gate records are named in sorted
  id order; a corrupt-but-parseable record (unknown state) fails closed naming
  the exact file and cause; terminal records keep no gate (an ordinary
  "not installed" error, not a gate refusal). Process-restart/reboot survival
  is inherent: every test spawns a fresh `nawa` process reading only the
  durable record on disk.
* **Lock ownership:** a forged `NAWA_LOCK_HELD` marker cannot resume a
  migration (offline-root preflight refuses, no record change, no lock created)
  and cannot authorize a nested mutating command even with no gate present.
  The migration authority's mint is restricted to the migration feature
  subtree by `check-layering.py`; a negative probe appending a mint call to
  `src/color.rs` was rejected (`only migration feature code may mint ...`)
  and the file restored.
* **Filesystem:** the record store now creates its directory with mode 0700
  (previously the process umask decided), refuses symlinked record *files* on
  both read and write, refuses a symlinked record *directory*, and fails the
  gate closed on all of them. `migration_record.rs` gains 2 unit tests for the
  restrictive directory mode and the no-follow rules; the gate scan shares
  the same refusal.
* **Privileged execution:** the typed `HandlerId` allowlist test now includes
  repository/plugin/shell/`.HOOKS` attack strings; a CLI probe proves an
  arbitrary handler path cannot be submitted through the migration namespace.
  `RESERVED_COMMANDS` gains `migrate` (no second top-level namespace) and a
  plugin-manifest test proves no `migration.handler` capability can be
  requested and `system`/`migrate` cannot be shadowed.
* **Verification:** `cargo check`, `clippy -D warnings`, layering (56 modules),
  full battery (667 bin tests + all integration targets, 1 pre-existing
  toolchain-under-`/home` env failure), all new/touched files rustfmt-clean.

### B3 wiring — Metadata Index v4 (recipe → archive → signed index → solver)

The proposal in `docs/design/package-metadata-chain.md` is now the shipped
contract; the pure reference model lost its `dead_code` allowance and every
item has a production caller.

* **Recipe → archive:** `[capabilities]` is an additive, optionally-present
  table (`Option<Capabilities>`: absent = unknown, empty = explicit none);
  entries are normalized once at validation and written verbatim to the main
  archive's `.PKGINFO`. Sub-packages get `provides = ` (never the main
  package's capability). Legacy recipes/archives are byte-identical.
* **Index v4:** opt-in `repo-create --index-version 4`; the signed index
  duplicates `rundeps`/`provides`, rejects unknown fields (strict entry
  parse), requires both fields, and refuses publication of an archive with no
  `provides` line. v1/v2/v3 indexes remain readable and map to `unknown`.
* **Fetch chain:** the verified snapshot's `.PKGINFO` is compared with the
  signed index fields after digest verification and before any payload step;
  a mismatch is a hard error.
* **Solver:** source candidates carry normalized `provides`; repository
  metadata is normalized from authenticated index bytes (legacy → unknown);
  the plan explains which planned package provides the selected init.
* **Tests:** 4 new repo tests (v4 creation/strictness/no-provides refusal and
  legacy compatibility), 2 build tests, 2 recipe tests, 1 slim test, 1
  install chain test, 1 plan explanation test. Full battery: **678 bin tests**
  (all integration targets green, 1 pre-existing env failure), clippy/layering
  clean, both smoke suites pass, all touched files rustfmt-clean.

### Phase 6 re-inspection (2026-11-18) and Nawa-independent preflight

Re-fetched `faseela` (`42dec56`), `faseela-repo` (`3f3cf60`) and `saq`
(`f54918a`) read-only and re-read the boot scripts, initramfs init, grub.cfg,
recipes and QEMU harness. Major finding: **current Faseela has already moved
to systemd** (`profiles/x86_64.toml` declares `init = "systemd"`, runit and
runit-services recipes are gone, `initramfs.sh` switch_roots to systemd), so a
"runit → systemd" migration of current HEAD is obsolete; B2 is resolved, B1,
B4, B5, B6 and B7 remain (exact files/owners/acceptance tests in the blocker
table above). Phase 6 stays blocked; no external repository was modified.

Nawa-independent slices landed:

* `migrate_apply.rs`: the built-in, typed migration definition; a read-only
  `plan` (offline-safe); real `preflight` (canonical live host, no
  container/chroot, root privileges, observed source init == `runit`, declared
  target == `systemd`); a migration-specific `MigrationApproval` that a
  generic `--yes` cannot construct; and `apply`, which performs the preflight
  and then **refuses with the exact external blockers** — no record, gate,
  package change or boot modification. This is honest fail-closed behavior,
  not a fake success.
* CLI: `system migrate plan ID` and `system migrate apply ID --from … --to …
  --acknowledge-reboot --acknowledge-recovery` in the single §12.2 namespace;
  completions updated. `plan` is read-only; `apply`/`resume` take their own
  authority-gated lock or preflight and reject offline roots.
* Tests: 9 use-case tests, 4 new CLI integration tests (20 in `tests/migrate.rs`).
  Verification: check, clippy `-D warnings`, layering (56 modules), full
  battery (688 bin tests + integration, 1 pre-existing env failure), both
  smoke suites, touched files rustfmt-clean.

### Phase 7 — source-tree ownership and quality audit (in progress)

* **Presentation layer consolidated:** `color`, `confirm`, `diag`,
  `humanize` and `output` moved from `src/` into `src/presentation/`; all
  `crate::<module>::` references updated; `main.rs` now declares one
  `presentation` module. `Notice::emit` moved to `commands/installing.rs` so
  presentation never imports application (which would close a dependency
  cycle with the five merged modules). The layering gate's `PRESENTATION` set
  and legacy allowlist were updated (11 → 8 entries, all naming the single
  `presentation` module).
* **Dead-code review:** removed obsolete module-level `#![allow(dead_code)]`
  from `domain/{system/{migration,capability,identity,profile},
  planning/install}` and the record store and detector once their consumers
  landed. Retained only two justified item-level allowances (the future
  verified-rollback transitions, blocked on B5). The B3 domain model lost its
  allowance because every item is now wired.
* **Provider explanation wired:** `Candidate::provides` is consumed by
  `domain::planning::install::init_provider_names`; `Plan::init_providers`
  carries the solver's authenticated provider set into `provider_note`, which
  now uses the domain `InitProvider` classification (Single / Ambiguous /
  Absent) instead of re-deriving it. The profile's `init_capability()` is the
  single source of the declared decision.
* **Git tree (final):** 149 Rust files, **7 directly under `src/`** (down from
  35 at the start of Phase 7). `adapters/` now owns all 14 concrete I/O
  mechanisms; the build pipeline (incl. `fetchrun/`) lives under `build/`;
  `config`, `lock`, `migration_record` are adapters; `ctx`/`recovery` are
  under `application/`; `plan`/`revdep`/`sync` are feature use cases;
  `repositories` is a command handler; `version` is pure domain. The remaining
  root files (`main`, `bootstrap`, `deps`, `glob`, `suggest`, `undo`,
  `testutil`) each have one clear responsibility, documented in the audit.
* **Verification:** check, clippy `-D warnings`, layering (**24 top-level
  modules**, no cycles) green; presentation-direction and mint-authority
  rules were negative-probed after every move (both rejected, files restored);
  full battery green except the known toolchain-under-`/home` env failure;
  both smoke suites pass; all touched files rustfmt-clean.
* `docs/architecture-audit.md` §0 now contains the complete final-tree audit:
  responsibility of every top-level module/directory, misplaced/obsolete/
  duplicate findings and how each was resolved, cycle breaks, and verification
  results. §§1–17 remain the historical Stage-0 snapshot for the behavior
  baseline.

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
  * Phase 3 (solver core):
  * `src/domain/planning/{mod,install}.rs` (new — normalized `Candidate`,
    `Why`, `Resolution`/`ResolvedItem`, the dependency-resolution DFS +
    11 tests; `plan::install_plan` delegates to it)
  * `src/plan.rs` (resolution core removed; recipe/db → candidate adapter;
    `Why` re-export + db-reason/label impls stay; `outdated`/`orphans`/
    `removal_order` unchanged; baseline tests kept byte-identical)
  * `src/domain/mod.rs` (one-line `pub(crate) mod planning;`)
  * Phase 3 (removal/query planners):
  * `src/domain/planning/removal.rs` (new — `InstalledPkg`, `orphans`,
    `removal_order` + 8 tests)
  * `src/domain/planning/install.rs` (gains `outdated` + 2 tests)
  * `src/domain/planning/mod.rs` (registers `removal`, doc updated)
  * `src/plan.rs` (`outdated`/`orphans`/`removal_order` bodies become
    delegation adapters; signatures unchanged; `recipe_full`/
    `newer_than_installed` + `vercmp`/`Ordering`/`BTreeSet` imports
    dropped; baseline tests byte-identical)
* Phase 4 (profile model, in progress):
  * `src/domain/system/profile.rs` (new — `SystemProfile`, `from_doc`
    validation, `merge`, `init_capability` + 11 tests; strict/fail-closed
    documented; `#![allow(dead_code)]` until the planner consumes it)
  * `src/application/features/system/profile.rs` (new — the loader with
    injectable paths, recommended §7 layout, fail-closed semantics + 9
    tests)
  * `src/domain/system/mod.rs`, `src/application/features/system/mod.rs`
    (one-line registrations)
* Phase 4 (planner wiring, in progress):
  * `src/domain/planning/install.rs` (`InitSelection` + `Resolution::init`
    + `install_plan` profile param; `ResolvedItem` gains `PartialEq`/`Eq`
    for test assertions; 3 new tests)
  * `src/plan.rs` (`Plan::init` + `InitSelection` re-export; signature
    gains the profile param)
  * `src/application/features/system/profile.rs` (`EffectiveProfile` +
    `role_sources` provenance, `Debug`; 1 new test; `allow(dead_code)`
    removed)
  * `src/application/features/package/plan_install.rs` (`plan()` profile
    param, `init_note()` §8.4 explanation; 1 new test + 2 helpers)
  * `src/commands/installing.rs` (`cmd_install_with_why`/`cmd_update` load
    the effective profile, pass it through, emit the note; fail closed on
    invalid profiles)
  * `src/application/features/system/detect.rs` (`DesiredInit` →
    `Declared{identity, source}`/`NotDeclared`/`Invalid{reason}` +
    `desired_init(root)`; 3 new tests)
  * `src/domain/system/profile.rs` (module `dead_code` justification
    comment updated — no code change)
* Phase 5 (migration model, in progress):
  * `src/domain/system/migration.rs` (new — `MigrationDefinition`,
    `HandlerId` allowlist, `check_id`, `MigrationState`/`Transition`/
    `transition()`, `StepRecord`/`StepOutcome`, `MigrationRecord` +
    13 tests; pure)
  * `src/migration_record.rs` (new — the durable record store moved
    here from `application/features/system/migrate.rs` in the gate
    slice; `save` via `atomic::write_atomic` 0600, `load` fail closed,
    `active_gates` scan + 7 tests; header documents the placement)
  * `src/domain/system/mod.rs` (one-line registration + doc update)
* Phase 5 (gate wiring, in progress):
  * `src/lock.rs` (`check_migration_gate` + wiring into `acquire` —
    exclusive-after-lock check and the restructured marker branch that
    checks the gate before honoring `NAWA_LOCK_HELD`; module docs and
    `HELD_ENV` doc updated to present tense; 4 new unit tests)
  * `src/application/lifecycle.rs` (own `LockNeed` vocabulary:
    `lock_mode` → `lock_need`, doc header explains the application ←
    shell direction; tests updated)
  * `src/commands/dispatch.rs` (`LockNeed` → `lock::Mode` mapping)
  * `src/cli/args.rs` (lock-policy test updated to `lock_need`/`LockNeed`)
  * `src/application/features/system/migrate.rs` (deleted — the store
    moved to top-level `migration_record.rs`; `features/system/mod.rs`
    doc updated)
  * `src/main.rs` (`mod migration_record;`)
  * `src/domain/system/migration.rs` (doc pointer to the record store)
  * `tests/lock.rs` (4 new gate tests + 3 pre-existing over-long
    `assert!` calls canonicalized to rustfmt's exact preferred form)
* Phase 5 (inspection CLI, in progress):
  * `src/application/features/system/migrate_inspect.rs` (new —
    `list`/`show`/`status` use cases over the record store, typed
    `Listing`/`UnreadableRecord`/`Status` + 9 tests)
  * `src/commands/system.rs` (new — `cmd_system`/`cmd_migrate` + pure
    renderers `render_list`/`render_show`/`render_step`/
    `render_status`/`render_gate` + 12 tests)
  * `src/commands/mod.rs`, `src/commands/dispatch.rs` (one-line
    registration + `Cmd::System` dispatch arm)
  * `src/cli/args.rs` (`Cmd::System`/`SystemCmd`/`MigrateCmd` +
    `lock_request` → `Request::ReadDb`)
  * `src/cli/completions.rs`, `src/plugins/manifest.rs`
    (RESERVED_COMMANDS gains `"system"`; completions for bash/zsh/fish)
  * `src/domain/system/migration.rs` (`HandlerId::requires_reboot()` +
    `MigrationRecord::requires_reboot()` — derived from the trusted
    handler binding, documented as never persisted; 1 new test)
  * `src/migration_record.rs` (`record_ids()` extracted, `active_gates`
    refactored onto it; `load` is now the single site that names the
    record file on every validation failure — the capability path's
    duplicate file context removed, the schema/state/handler/step paths
    gained it + strengthened test)
  * `src/application/features/system/mod.rs`,
    `src/application/lifecycle.rs` (registration + `Request::ReadDb`
    doc naming the inspection consumers)
  * `tests/migrate.rs` (new — 13 integration tests for the three
    commands, incl. the read-only proof and offline-root coverage)
  * `docs/commands.md` (the three commands documented, System and
    maintenance section)
* Phase 5 (coordinator, in progress):
  * `src/application/features/system/migrate_coordinator.rs` (new —
    `MigrationPermit` (module-private seal, no Clone/serde/env spelling,
    `#[cfg(test)] mint_for_tests()` seam), `Approval::explicit()`,
    `PreparationStep`/`PackageOperation::Install`/`ApprovedPlan`/
    `PreparationRequest`, `prepare()` (ordinary exclusive acquire with
    the gate check, existing-record refusal, durable planned save,
    `begin_operation` with the "migrate" command + requested list,
    `BeginPreparation` under the held lock, write-ahead step loop,
    the three-way failure split) + 7 tests; `#![allow(dead_code)]`
    until the `apply` command lands)
  * `src/application/features/system/mod.rs` (registration + doc)
  * `src/application/features/package/apply_transaction.rs`
    (`LockOwnership<'_>` with `PerStep`/`Coordinator { guard, permit }`,
    `MigrationInstall`, `install_for_migration`, `install_one` gains the
    ownership param — both pre-existing call sites pass `PerStep` so
    ordinary behavior is unchanged; dead-code allows with justifications)
  * `src/transaction/context.rs` (`OperationCommand::Migration` →
    "migrate", the coordinator's preparation transaction in history)
* Phase 5 (5B hardening, in progress):
  * `src/application/features/system/migrate_coordinator.rs`
    (durable lifecycle: `persist` with reload-verify publication,
    `abandon_planned`/`abandon_gate_failure`, pre-step snapshots +
    `step_committed`/`same_install_state`/`verify_install_postcondition`,
    the `conclude` ending extraction, the coordinator's own
    `interrupt::Guard`, the module-header crash-boundary inventory;
    7 → 21 tests)
  * `src/migration_record.rs` (`store::remove` — validated id, missing
    is Ok; for the abandon path and the recovery slice)
  * `src/atomic.rs` (test-only: `failure_injection` — path-filtered
    one-shot failpoint arming for cross-module crash-boundary tests;
    `FailPoint` is `pub(crate)` under `cfg(test)`; production paths
    unchanged)

* Phase 7 (in progress):
  * `src/presentation/{mod,color,confirm,diag,humanize,output,error}.rs` (new
    home for the presentation modules; `git mv` + path updates)
  * `src/adapters/{mod,archive,artifact,atomic,checksum,config,db,fswalk,
    interrupt,lock,migration_record,pathutil,sign,sys,unique}.rs` (new home
    for all 14 low-level I/O mechanisms)
  * `src/build/` expanded with `assets,buildenv,closure,profile,slim,sources`
    and the whole `fetchrun/` subsystem; `[slim]` recipe model moved to
    `src/recipe/model.rs` (cycle break)
  * `src/application/{context,recovery}.rs`,
    `src/application/features/{repository/{mod,sync},package/{plan,revdep}}.rs`
    (use cases moved from the root)
  * `src/commands/repositories.rs` (command handler moved from the root)
  * `src/domain/package/version.rs` (pure utility moved from the root)
  * `src/main.rs`, `src/commands/installing.rs` (module registration,
    `Notice::emit` moved here), all files with moved-module paths updated
  * `scripts/check-layering.py` (PRESENTATION set + allowlist + mint-path
    update; stale entries for moved modules removed)
  * `src/domain/{system,planning}/*.rs`, `src/adapters/migration_record.rs`,
    `src/application/features/system/detect.rs` (obsolete `dead_code` removed)
  * `src/application/features/package/{plan,plan_install}.rs`,
    `src/domain/planning/install.rs` (provider explanation)
  * `docs/architecture-audit.md` §0 (complete final-tree audit)
* Phase 6 preflight (in progress):
  * `src/application/features/system/migrate_apply.rs` (new — built-in
    definition, read-only `plan`, live-host `preflight`, explicit
    `MigrationApproval`, `apply` refusal + 9 tests)
  * `src/application/features/system/mod.rs` (registration)
  * `src/cli/args.rs` (`MigrateCmd::Plan`/`Apply`, `Request::NoDb` for both
    apply and resume), `src/cli/completions.rs`
  * `src/commands/system.rs` (`print_plan`/`print_apply`, `render_plan`,
    `render_apply_refusal`), `tests/migrate.rs` (4 tests)
  * `docs/commands.md` (plan/apply/resume documented)
* B3 wiring:
  * `src/domain/package/metadata.rs` (central `normalize_list` /
    `normalize_provides` / `normalize_rundeps` / `metadata_from_options`,
    `IndexEntryV4::validate` exposed; `dead_code` allowance removed)
  * `src/recipe/{model,validate,mod}.rs` (additive `[capabilities]` table and
    validation tests)
  * `src/build/package.rs` (`pkginfo_header` writes `provides`, 2 tests),
    `src/slim.rs` (sub-packages clear `provides`, 1 test)
  * `src/repo/{model,create,validate,mod}.rs` (schema v4, strict entry parse,
    publication rule, 4 tests)
  * `src/install/mod.rs` (`IndexProof::metadata` + chain check after digest,
    1 test), `src/application/features/package/{apply_transaction,plan_install}.rs`
    (`Source::Repo` boxed, provider note, test), `src/upgrade/{source,mod}.rs`,
    `src/plan.rs`
* Phase 5 (5D security, in progress):
  * `tests/migration_security.rs` (new — 8 adversarial integration tests:
    unreadable directory, multiple/corrupt/terminal records, symlink record,
    forged marker on resume/mutation, handler-path CLI probe)
  * `src/migration_record.rs` (`ensure_records_dir` 0700, `check_no_follow` on
    load/save/scan, 2 new unit tests)
  * `src/plugins/manifest.rs` (`migrate` reserved; plugin-boundary test)
  * `src/domain/system/migration.rs` (expanded handler allowlist attack corpus)
* Phase 5 (5C recovery, in progress):
  * `src/application/features/system/migrate_recover.rs` (new — cases A–F:
    `recover`, `recover_planned`, `recover_gate_active`, `finalize_history`,
    `resume`/`resume_with`, `verify_boot_postconditions`, the reload-verified
    `persist`, plus 18 tests)
  * `src/application/features/system/mod.rs` (registration + doc)
  * `src/lock.rs` (`MigrationAuthority`, `migration_authority`,
    `acquire_for_migration`, `acquire_unchecked`; the ordinary `acquire`
    refactored onto the shared loop; gate check preserved)
  * `scripts/check-layering.py` (new mint-authority rule)
  * `src/cli/args.rs` (`MigrateCmd::Resume`, `Request::NoDb` mapping so the
    use case owns its lock), `src/cli/completions.rs` (bash/zsh/fish)
  * `src/commands/system.rs` (resume handler + `recovery_ui`,
    `render_recovery_note`, `render_resume_outcome` + 3 tests)
  * `tests/migrate.rs` (3 resume integration tests)

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
| `cargo test --all-targets --offline --no-fail-fast` (Phase 3 solver tree) | 648 pass / 2 ignored / 3 env-failed (= 637 + 11 new domain-planner tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 3 removal tree) | 658 pass / 2 ignored / 3 env-failed (= 648 + 8 removal + 2 outdated domain tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 4 profile tree) | 678 pass / 2 ignored / 3 env-failed (= 658 + 11 domain + 9 loader profile tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 4 wiring tree) | 686 pass / 2 ignored / 3 env-failed (= 678 + 3 InitSelection + 1 loader provenance + 1 plan note + 3 desired-init tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5 migration-model tree) | 706 pass / 2 ignored / 3 env-failed (= 686 + 13 domain migration + 7 record-store tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5 gate-wiring tree) | 714 pass / 2 ignored / 3 env-failed (= 706 + 4 lock unit + 4 lock integration tests; same 3 env failures) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5 inspection tree) | 751 pass / 2 ignored / 1 env-failed (= 714 + 35 new: 9 inspection use case + 12 presentation + 13 `tests/migrate.rs` + 1 domain reboot-binding test; plus the 2 plugin tests that were env-blocked at the Phase-0 baseline passed this run — container permission state differs between sessions; the remaining env failure is the toolchain-under-`/home` sandbox test) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5 coordinator tree) | 758 pass / 2 ignored / 1 env-failed (= 751 + 7 new coordinator lifecycle tests; same 1 env failure. First run of the two install-driven lifecycle tests failed on the fixture — the staged `.PKGINFO` lacked `pkgrel`, which the verified installer requires nonzero — fixed in the fixture, re-run clean) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5B tree) | 772 pass / 2 ignored / 1 env-failed (= 758 + 14 new 5B crash-boundary tests; same 1 env failure. During iteration 3 fetch/go tests failed transiently with "no space left on device" — the `/tmp` tmpfs was 100% full from stale test scratch dirs; after cleanup they pass, unchanged by this slice) |
| `cargo test --all-targets --offline --no-fail-fast` (Phase 5C tree) | ~796 pass / 2 ignored / 1 env-failed across all targets (bin target 664 = 643 + 18 recovery + 3 presentation; 16 `tests/migrate.rs` incl. 3 new resume tests; all other integration targets green); same 1 pre-existing toolchain-under-`/home` env failure |
| `cargo test --offline` (bin only, 5B iteration) | 643 pass / 1 ignored (the coordinator module: 21 tests = 629 + 14) |
| `cargo clippy --all-targets --offline -- -D warnings` | pass (Phase 1a + 1b + 1c + 1d + 1e + 2 + 3/B3 + 3/solver + 3/removal + 4/profile + 4/wiring + 5/model + 5/gate + 5/inspection + 5/coordinator + 5B) |
| `python3 scripts/check-layering.py` | pass (54 after 1a; 53 after 1b; 54 after 1c; 55 after 2 — the new `domain` layer; 55 after 3/B3, 3/solver, 3/removal and 4/profile — the count is top-level modules, the new `domain/package`, `domain/planning`, `domain/system/profile` + `features/system/profile` submodules are scanned; 55 after 4/wiring and 5/model — `domain/system/migration` + `features/system/migrate` are submodules of existing top-level modules; 56 after 5/gate — the record store moved to the top-level `migration_record` module; 56 after 5/inspection — `features/system/migrate_inspect` and `commands/system` are submodules of existing top-level modules, no cycles: commands → application/features/system → migration_record → domain/atomic, lock untouched; **56 after 5/coordinator** — `features/system/migrate_coordinator` is a submodule of an existing top-level module; the one new edge `features/package → features/system` (the permit type import) is an intended application-internal edge the checker's intra-application skip does not count; **56 after 5B — no new modules or edges (the store `remove` and the test injector live in existing modules)**) |
| `bash scripts/smoke-v06.sh` (release build, Phases 1b + 1c + 1d + 2) | ALL V0.6 SMOKE TESTS PASSED |
| `bash scripts/smoke-v05.sh` (release build, Phases 1d + 2) | ALL SMOKE TESTS PASSED |
| `bash scripts/smoke-v05.sh` (release build, 5/inspection tree) | ALL SMOKE TESTS PASSED |
| `bash scripts/smoke-v06.sh` (release build, 5/inspection tree) | ALL V0.6 SMOKE TESTS PASSED |
| `bash scripts/smoke-v05.sh` (release build, 5/coordinator tree) | ALL SMOKE TESTS PASSED |
| `bash scripts/smoke-v06.sh` (release build, 5/coordinator tree) | ALL V0.6 SMOKE TESTS PASSED |
| `bash scripts/smoke-v05.sh` (release build, 5B tree) | ALL SMOKE TESTS PASSED |
| `bash scripts/smoke-v06.sh` (release build, 5B tree) | ALL V0.6 SMOKE TESTS PASSED |
| HEAD-vs-1c behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical |
| HEAD-vs-1d behavior diff (`--help`, `--version`, completions × 3 + 12 read/planning commands, isolated worktree build) | byte-identical |
| HEAD-vs-2 behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical (detection adds no CLI surface) |
| HEAD-vs-1e behavior diff (`--help`, `--version`, completions × 3, isolated worktree build) | byte-identical |
| HEAD-vs-1e paired 8-step install/update scenario (pick note → plan → confirm → fetch → install → success; dry run; non-interactive refusal; planner error; strict-repo update refusal; real update; up-to-date dry run; local signed repo, worktree build) | identical transcripts |
| HEAD-vs-3(solver) behavior diff (`--help`, `--version`, completions × 3, isolated CARGO_TARGET_DIR builds) | byte-identical |
| HEAD-vs-3(solver) paired 8-step install/update scenario (same script, isolated builds) | identical transcripts |
| HEAD-vs-3(solver) resolver-error probe (cycle a→b→a / missing dep ghost / `--no-deps` dry run, isolated builds) | identical transcripts + exit codes |
| HEAD-vs-3(removal) behavior diff (`--help`, `--version`, completions × 3, isolated CARGO_TARGET_DIR builds) | byte-identical |
| HEAD-vs-3(removal) paired 8-step install/update scenario (same script, isolated builds) | identical transcripts |
| HEAD-vs-3(removal) query/remove probes over a seeded db (reasons explicit/dependency, recorded run deps): `outdated` / `autoremove -n` (prints the dependents-first removal order) / `update -n` / real `remove --yes` (transcript + resulting db state), isolated builds | identical transcripts + db state |
| HEAD-vs-4(profile) behavior diff (`--help`, `--version`, completions × 3, paired 8-step scenario, isolated CARGO_TARGET_DIR builds) | byte-identical (the loader is unwired; no production path changed) |
| HEAD-vs-4(wiring) behavior diff (`--help`, `--version`, completions × 3, paired 8-step scenario, isolated CARGO_TARGET_DIR builds) | byte-identical (no fixture carries a profile; the note prints only when one declares `init-system`) |
| HEAD-vs-4(wiring) positive probes on the new binary (seeded profiles in a fake root): override declaring `init-system` → `install -n`/`update -n` print the §8.4 note with the override path; defaults-only profile → note names the defaults path; profile without the init role → no note; invalid profile → `install` and `update` refuse with exit 1 naming the file + cause | pass |
| HEAD-vs-5(model) behavior diff (`--help`, `--version`, completions × 3, paired 8-step scenario, isolated CARGO_TARGET_DIR builds) | byte-identical (the migration model + record store are unwired; no production path reads records) |
| HEAD-vs-5(model) inertness probe (a `preparing` migration record seeded under a scenario root: `install -n` and `update -n` on both builds) | identical transcripts (the gate check is not wired yet) |
| HEAD-vs-5(gate) behavior diff (`--help`, `--version`, completions × 3, paired 8-step scenario, isolated CARGO_TARGET_DIR builds) | byte-identical (the gate check only runs on exclusive acquisitions and no scenario fixture carries a migration record) |
| HEAD-vs-5(gate) gate probes (a valid `preparing` record seeded under a scenario root, both builds): `remove --yes` / `autoremove --yes` / `install <archive>` refuse with exit 1 naming id+state+gate+record dir; `install curl -n` on a fresh root refuses at the gated begin-operation lock *before* printing the plan; `query` unaffected; the HEAD build shows none of this (`install -n` prints its plan, `remove --yes` fails with the ordinary "not installed" error) | pass |
| `cargo fmt --all -- --check` | fails repo-wide at HEAD (pre-existing ~100 files; CI does not run fmt) — re-scoped to "no new violations in touched/new code", satisfied (1a–1e, 2, 3/B3, 3/solver, 3/removal, 4/profile, 4/wiring,
5/model: new files clean; plan.rs dirty hunks 19 → 13 → **12** — the
deleted planner bodies carried six away across the two slices, one
over-long test call was canonicalized while adding the profile arg;
installing.rs stays at its pre-existing 18 hunks, none on lines touched
here; 5/gate: the moved `migration_record.rs` and the doc-edited module
files clean, lock.rs stays at its pre-existing 7 hunks (the one
violation my gate tests introduced was fixed before commit) and
tests/lock.rs hunks 5 → **2** — three pre-existing over-long `assert!`
calls were canonicalized to rustfmt's exact preferred form while adding
the gate tests, zero new violations; **5/inspection: the three new
files (`migrate_inspect.rs`, `commands/system.rs`, `tests/migrate.rs`)
are rustfmt-clean; the `manifest.rs` hunks are pre-existing at HEAD
(12, verified against the pristine file — the one-line
RESERVED_COMMANDS edit adds none)**; 5/coordinator: the new
`migrate_coordinator.rs` and the three touched files
(`apply_transaction.rs` incl. my hunks, `context.rs`, `system/mod.rs`)
are rustfmt-clean (a stray `--edition 2024` rustfmt detour during
bring-up was fully normalized back to the project's 2021 edition)**;
**5B: `migrate_coordinator.rs` is rustfmt-clean with the 14 new tests
included (0 hunks), `migration_record.rs` clean, and every `atomic.rs`
region this slice touched (the injector, `FailPoint`, `maybe_fail`)
canonicalized — the file's remaining hunks (the import blank line,
pre-existing one-liner regions, its own pre-existing tests) are
untouched pre-existing red** |

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

## Phase 7 update — system-transition preview wired into `nawa upgrade` (2026-11-18)

The last preview integration was repaired without weakening the module-layering gate:

* `src/domain/system/observation.rs` owns the pure, source-attributed snapshot types. Runtime PID 1, installed candidates, desired policy, execution context, and the validated effective profile remain distinct facts.
* `application/features/system/detect.rs` reads the runtime and profile once and returns the snapshot. `commands/dispatch.rs` composes that observation before calling the upgrade feature.
* `upgrade/system_transition.rs` consumes the domain snapshot and has no dependency on `application`; it no longer reopens profile files while planning.
* The built-in Faseela runit-to-systemd migration definition now lives in `domain/system/migration.rs`. The application migration coordinator delegates to that definition.
* `repo::INDEX_SCHEMA_V4` is exposed through the repository facade rather than duplicated as a magic number.
* The normal test-facing/package-upgrade path remains available without fabricating a runtime observation; the CLI-only `upgrade_with_observation` path is the one that adds the separate transition preview.

**Historical verification:** commit `f79f82181f8581594e1bed08d3fc6198107bc492`; Actions run [44248582860](https://github.com/mrdevmohamed/nawa/actions/runs/44248582860) completed with `success`. Later migration-branch verification and current CI state are recorded in the closeout section below.

**Scope:** this is a read-only runtime-dependency-closure preview, not a complete migration solver or a boot transition. Index v4 still has no version ranges, conflicts, replacements or complete dependent-impact model. It must not be described as an executable migration plan. `--yes` remains limited to the ordinary package upgrade.

**External PR state (2026-11-18, refreshed):** Faseela Repo [#203](https://github.com/mrdevmohamed/faseela-repo/pull/203), Faseela [#59](https://github.com/mrdevmohamed/faseela/pull/59), and SAQ [#4](https://github.com/mrdevmohamed/saq/pull/4) are all merged. Their merge commits are `26b9c7c6613f57ff6551a53f2e747a6dbb388792`, `181e3524142971d1138af25594d96b232314a4ab`, and `5fc8c19c3a4df14a1a57e191b3a50bd3cbc6284a`, respectively. This integrates the profile/preset policy and explicit installed-GRUB `init=` selection. It does not constitute real BIOS/UEFI installed-disk acceptance or migration rollback acceptance.

**Transaction-trigger source audit:** Nawa's `src/install/hooks.rs` separately executes regular `.sh` files from `/usr/lib/nawa/transaction-hooks.d` after package-changing transactions. The Faseela Repo's previous guard only inspected `[hooks]`; PR #203 now also checks recipe declarations of that trigger directory and scans their build install scripts and local shell assets. The regression test covers both a forbidden service-manager command and an allowed non-service trigger. This does not replace process sandboxing or static analysis of arbitrary generated code.

**Legacy ownership finding (B2):** inspecting the parent of Faseela Repo commit `ac1c3bb` confirmed that the removed `runit-services` recipe shipped `/usr/bin/init -> /usr/bin/runit`, while `systemd` also owns `/usr/bin/init`. Nawa now grants a typed, migration-only handoff authority for this exact package/path/old-target/new-target tuple. The installer records both owners during the pending-reboot window; after successful verification, removing `runit-services` leaves the systemd-owned link in place. Regression tests cover ordinary rejection, the exact handoff, extra-owner rejection and safe old-package removal. This does not yet wire the post-boot finalizer or verified rollback sequence; B5 still blocks production apply. No generic `--force` bypass is accepted.

**External policy finding (B7):** Faseela Repo PR #203 is merged. Its recipe-wide CI guard rejects direct service-manager state mutations in `[hooks]` and scans shell sources installed as Nawa transaction triggers. The five service-package hooks no longer run `systemctl preset`; Faseela PR #59 applies vendor presets once to the completed offline image root. This is a static policy check, not a shell sandbox; actual installed-package acceptance remains open.

## Unresolved external dependencies (blockers)

| ID | Blocker | Owner | Affects |
|---|---|---|---|
| B1 | **explicit SAQ init selection — MERGED; INSTALLED-DISK RUN PENDING**: Faseela #59 and SAQ #4 are merged. Installed GRUB entries pass `init=/usr/lib/systemd/systemd`, and SAQ tries explicit `init=` before generic fallbacks. Faseela PR #60 adds a BIOS+UEFI install-to-disk harness and makes the release workflow require both `pid1=systemd` and `system-profile=systemd` markers from the installed system. CI currently validates the harness and the installer test mode; no real release ISO has yet been run through that full two-firmware flow. | Faseela + SAQ | Phase 6 VM acceptance |
| B2 | **legacy `/usr/bin/init` ownership collision — NARROW HANDOFF IMPLEMENTED; FINALIZATION PENDING**: Nawa now grants a typed authority only to the built-in Faseela runit→systemd migration. It accepts only the exact `systemd` payload path `/usr/bin/init`, with sole owner `runit-services`, old symlink `/usr/bin/runit` and staged symlink `../lib/systemd/systemd`. Both package records intentionally retain the path during the pending-reboot window; removing `runit-services` after verification leaves the systemd-owned path untouched. Tests cover denial without authority, exact co-ownership and safe old-package removal. No generic `--force` is used. Production sequencing remains blocked until B5 supplies the post-boot finalizer and recovery/restore route. | Nawa + Faseela Repo | Phase 6 legacy-package preparation |
| B3 | **Repository index v4 schema — RESOLVED (2026-11-18)**: recipe `[capabilities]` → archive `.PKGINFO` → authenticated signed index v4 → verified fetch comparison → solver provider explanation is wired and tested. Legacy v1–v3 remains readable and maps missing metadata to `Unknown`. | Nawa (this repo) | Phase 3 — complete |
| B4 | **System Profile — LOADER PATHS TESTED; INSTALLED-IMAGE ACCEPTANCE PENDING**: Faseela Repo #203 is merged and `base-files` writes `/usr/share/nawa/system-profile.toml` with schema 1, `distribution=faseela`, `architecture=x86_64` and `init-system=systemd`. Nawa's production `load_effective(root)` path now has a test using those exact locations; it verifies the administrator file at `/etc/nawa/system-profile.toml` overrides only the declared provider and is reported as the source. The test passed in Nawa CI (700 Rust tests, 0 failures). Faseela PR #60 adds an installed-disk marker for the same profile; actual BIOS/UEFI image execution remains pending. | Faseela Repo + Nawa integration | Phase 6 apply preflight |
| B5 | **Boot artifact retention — SNAPSHOT ADAPTER IMPLEMENTED; INTEGRATION/RESCUE OPEN**: Nawa has a migration-specific durable snapshot adapter for the installed GRUB config, stable kernel/initramfs aliases and their versioned targets, `/usr/bin/init`, `/etc/fstab`, the UEFI removable GRUB file and old runit artifacts. It publishes a hash-verified manifest last, rejects symlinked parent paths, validates all payloads before restore and restores only an explicit allowlist. Tests cover idempotent restore and tamper rejection. It is not wired into coordinator/recovery, does not capture BIOS raw sectors or EFI NVRAM, and has no tested external-rescue workflow. B5 remains open until the adapter is integrated and a failed migration is proven to boot the old route again. | Nawa + Faseela | Phase 6 handler |
| B6 | **VM boot-test harness — BIOS+UEFI DISK PATH IMPLEMENTED; FAILURE-SCENARIO ACCEPTANCE OPEN**: Faseela PR #60 adds a harness that boots the ISO, installs to a disposable qcow2 disk, reboots without the ISO and requires actual PID 1 plus installed System Profile markers in both BIOS and UEFI. The release workflow invokes this after the existing Live ISO smoke test and uploads serial logs. PR CI validates CLI construction and test-only installer guards; it does not execute a real VM boot. Remaining: run the harness against a real release ISO in both firmware modes, then add snapshot-driven interrupted-install, failed-boot and restore scenarios required by §14.3. | Nawa/infra + Faseela | Phase 6 acceptance |
| B7 | **Hook/trigger service-control policy — MERGED; RUNTIME ACCEPTANCE OPEN**: Faseela Repo #203 is merged. Recipe CI rejects direct service-state changes in `[hooks]` and shell sources installed as transaction triggers. The five service-package hooks no longer call `systemctl preset`; Faseela #59 runs vendor presets once against the completed offline root. The check is static, not a sandbox for arbitrary privileged shell code. The installed image still needs boot-time validation of the expected enabled/disabled units and an audit that no package hook/trigger bypasses the policy. | Faseela Repo + Faseela provisioning | Phase 6 trust boundary |

## Exact next task

**Phase 6 — external Faseela/SAQ integration and real boot acceptance.** The
Nawa-side migration framework and safe preflight are implemented, but the
real runit-to-systemd handler must remain fail-closed until the following
distribution-owned contracts are resolved:

* B1/B6: run Faseela PR #60's release-workflow installed-disk harness against a real release ISO in BIOS and UEFI; require the installed system's serial markers for PID 1 and System Profile, and retain both logs as acceptance evidence.
* B4: verify the profile marker from the actual installed root and keep the unit-level defaults/administrator-override test.
* B5: wire the existing snapshot adapter into the coordinator and recovery state machine; define the old GRUB entry/initramfs restoration contract; handle BIOS sectors and EFI NVRAM through a documented Live-ISO rescue procedure; then prove rollback by booting runit again.
* B6: extend the installed-disk harness with snapshot-driven interrupted-install, failed-boot, manual rescue and successful rollback scenarios; the normal boot path alone does not close §14.3.
* B7: validate built/installed packages and runtime service policy; the static recipe guard is not a shell sandbox.

Do not invoke the migration coordinator, remove runit, edit boot configuration
or claim a successful transition while these conditions are unresolved.
Continue read-only inspection and package-planning work independently. The
feature-branch CI workflow passed on 2026-11-18 at commit
`a51c423f4a9d7c3db6dd20acce6495757ab97d98` (Actions run
[44323984302](https://github.com/mrdevmohamed/nawa/actions/runs/44323984302)); the profile loader path/override regression test is included. The new durable boot snapshot adapter is a later change and is still awaiting its own CI result.
The source-layering failure and schema-v4 facade export are fixed; the
upgrade preview now receives a single immutable `ObservedSystem` snapshot
from command composition, and its effective target profile is not reread
inside `upgrade`. This verifies Nawa preview code only; it does not resolve
Faseela/SAQ boot blockers.


## Phase 7 closeout — final verification (2026-11-18)

* The source ownership reorganization and read-only system-inspection/upgrade-preview integration are complete on the Nawa side. The final source tree has 149 Rust files, with seven files directly under `src/`; the 24-module layering gate passes with no dependency cycles.
* CI run [#44312508908](https://github.com/mrdevmohamed/nawa/actions/runs/44312508908) passed on commit `c433c38dfc5a13efc7f9d11fc34877ed93b8d1fe`: build, Clippy, tests, Ubuntu 22.04/24.04 root/user coverage, CLI smoke, all four build-isolation levels, and static musl.
* The preceding root-only test failure was an inherited hook-marker issue in the CI launcher. The root test runner now removes `NAWA_LOCK_HELD` and `NAWA_POST_TRANSACTION_HOOKS_RUNNING` from its own environment; real hook children still receive `NAWA_LOCK_HELD` explicitly from `run_hook`. The adversarial integration test `a_forged_held_marker_cannot_resume_a_migration_or_cross_the_gate` remains enabled and passed.
* This closes Phase 7 for Nawa's source and preview work only. It does **not** close Phase 6: the real runit-to-systemd handler remains blocked by B1/B2/B4/B5/B6/B7, and no real BIOS/UEFI migration is claimed.
