# Nawa architecture and behavior audit (Stage 0)

> Status: complete audit of `master` @ `66ed209` ("Merge pull request #55
> from mrdevmohamed/feature/transaction-security-cache-hardening"), inspected
> 2026-11-17. This document records facts measured from the source tree, the
> CI definition, and the related `faseela` / `faseela-repo` / `saq`
> repositories. It is the input for the structural refactor (Stage 1) and the
> system-intelligence stages that follow. The design specification is
> `docs/design/system-intelligence-and-migrations.md`.

## 1. Scope and method

* Full walk of `src/` (115 files, ~39,300 lines, 53 top-level modules).
* Measured the real module dependency graph with the same parser the CI
  layering gate uses (`scripts/check-layering.py`), extending it to print
  edge counts.
* Read `main.rs`, `cli.rs`, `lock.rs`, `ctx.rs`, `config.rs`,
  `plan.rs`, `deps.rs`, `db.rs`, `version.rs`, `service.rs`,
  `commands/*` (all 13), `install/*` (all 7), `transaction/*` (all 6),
  `upgrade/*`, `repo/*` (entry points), `recipe/*`, `archive.rs`,
  `sign.rs`, `hooks.rs`, `fetch/mod.rs`, `plugins/*`, `recovery.rs`.
* Read CI (`.github/workflows/ci.yml`), `scripts/check-layering.py`,
  `scripts/smoke-v05.sh` / `smoke-v06.sh`, and the `tests/` suite list.
* Cross-checked migration-relevant behavior against `faseela-repo`
  (recipe tree incl. the runit→systemd packaging switch),
  `faseela` (boot/installer/initramfs docs and scripts) and `saq`
  (init selection in `share/saq/init` / `init-live`).
* Baseline executed locally: `cargo check --all-targets` clean;
  `cargo test --all-targets --no-fail-fast` — 535 passed, 1 ignored,
  3 failed **for environment reasons only** (see §12.4); layering gate green.

## 2. Existing module ownership

### 2.1 Flat root modules (~40 files)

| Module | Ownership today | Notes |
|---|---|---|
| `main.rs` (430) | process entry, global flag parse, config load, lock acquisition, recovery trigger, pre/post installed-set snapshotting, command dispatch table, post-transaction trigger dispatch, plugin event dispatch, error rendering | several responsibilities (see §4) |
| `cli.rs` (950) | clap `Cli`/`Cmd`/subcommand enums, three hand-written shell completion scripts, and **lock policy** (`lock_mode`) | policy lives in the CLI layer |
| `ctx.rs` | `Ctx` (resolved root/recipes/builddir/profiles + config), `resolve_mode`, `resolve_repo` | wiring helper |
| `config.rs` (709) | layered nawa.toml (defaults → system → local → env), `InstallMode`, trust keys, sources | adapter + model in one |
| `db.rs` (419) | installed-package **persistence** (TOML desc/files/hashes) *and* pure identity rules (`valid_name`, `valid_version`, `check_name`, `Entry`, `InstallReason`, legacy date normalization) | mixed: domain invariants inside the database adapter |
| `plan.rs` (429) | pure install planning (`install_plan`, `outdated`, `orphans`, `removal_order`, `Why`) — but typed over `db::Entry` | see §3.2 |
| `deps.rs` (223) | pure DFS build-order resolution over recipes | already pure |
| `version.rs` | `vercmp` version comparison | pure |
| `suggest.rs` | Levenshtein "did you mean" | pure |
| `recipe/` (model/validate/expand/load) | TOML recipe model + validation + expansion + filesystem loading | `model` stays pure; `load`/`expand` are I/O (§3.2) |
| `repo/` (model/validate/create/update/pack) | signed repository index v1/v2/v3 create/validate/update; `RepoEntry` = transport/index record | adapter + index model |
| `repositories.rs` | repo list/makecache/repoquery/clean commands | application use cases |
| `fetch/` (http/cache/extract/local/snapshot) | in-process HTTP, sha256 snapshots, tarball extraction, source cache | network/fs adapter |
| `sign.rs` | ed25519 keygen/sign/verify, trust store, detached sigs, index signing | signing adapter |
| `checksum.rs` | sha256 file hashing | adapter primitive |
| `artifact.rs` (764) | content-addressed object store, provenance records, retention | adapter |
| `archive.rs` (596) | .nawa.tar.zst create/extract, `.PKGINFO` parsing, `.HOOKS` reading | archive adapter |
| `install/` (7 files, ~3,900) | the installer: staging, payload application, conflict checks, file journal, hooks, undo, rollback, per-step outcome recording | facade + adapters mixed (§4) |
| `transaction/` (context/history/journal/outbox/recovery) | transaction IDs, step intents/results, history records, TxWAL, crash reconciliation | durable-state adapters + state model |
| `undo.rs` (828) | per-package undo log read/write, rollback decisions | mixed model + storage |
| `recovery.rs` | `recover_and_report` entry point calling journal+history recovery | application lifecycle |
| `lock.rs` (292) | flock lock file, holder pid, wait loop, `NAWA_LOCK_HELD` bypass | OS adapter + policy |
| `interrupt.rs` | Ctrl-C flag, deferred checkpoints | process adapter |
| `output.rs`, `diag.rs`, `color.rs`, `confirm.rs`, `humanize.rs` | terminal output, error rendering, palette, prompts, formatting | presentation |
| `service.rs` (395) | **runit service management**: list/enable/disable/start/stop/restart/status via `sv`, plus `keep_disabled` marker check | scheduled for removal (Stage 1) |
| `sandbox/` (5), `buildenv.rs`, `build/` (7), `fetchrun/` (4), `sources.rs`, `assets.rs`, `closure.rs`, `slim.rs`, `lint/` (3), `profile.rs` | the build pipeline: sandboxing (fork/mount/unshare), build env, sections, staging, lint, split packages, build profiles | cohesive subsystem; build profiles ≠ system profile |
| `groups/` (2) | group definitions + local membership records | model + storage |
| `plugins/` (7) | discovery, manifest, JSONL protocol, runner, events, sandbox | adapter + use cases |
| `sync.rs`, `sys.rs`, `pathutil.rs`, `fswalk.rs`, `glob.rs`, `atomic.rs`, `unique.rs`, `humanize.rs`, `error.rs`, `testutil.rs` | sync use case; syscall shims; path/fs primitives; test support | — |

### 2.2 Command handler modules

`commands/` (13 files, `mod.rs` re-exports everything `pub(crate) use x::*`):

`building` `configuring` `fetching` `groups` `history` `installing`
`plugins` `querying` `recipes` `sandbox` `service` `signing` — these are
application use cases invoked by `main::run_command`'s match on `Cmd`.

## 3. Actual dependency directions (measured)

Tarjan SCC over top-level modules: **no cycles** (the CI gate enforces this).
`use crate::*` glob imports: none (gate enforced). Edge summary (counts are
production-code `crate::x` references):

```
cli        -> pathutil, lock                     (lock policy inside CLI)
main       -> (dispatch into) commands, cli, config, ctx, lock, install, plugins
commands   -> db(12) humanize(12) ctx(11) color(10) cli(7) transaction(6) diag(6)
              recipe(5) fetch(4) config(4) archive(4) install(3) lock(3) plan(3)
              interrupt(3) confirm(2) groups(2) upgrade(2) build(2) buildenv(2)
              revdep(2) recovery(2) plugins assets closure fetchrun sources deps
              glob suggest pathutil repo version sys service profile sandbox
install    -> db(23) transaction(11) interrupt(5) lock(4) artifact archive sign
              fetch repo diag pathutil service(1)
transaction-> db(11) artifact(4) atomic(3) groups(1)
upgrade    -> db transaction repo sign groups fetch recipe color archive install
              version confirm interrupt deps diag sys
plan       -> db(6) recipe version suggest          (db::Entry inside planning)
recipe     -> db(2) slim suggest                    (name validation from db.rs)
archive    -> db(2) fswalk interrupt                (check_name from db.rs)
deps       -> suggest recipe
build      -> sandbox closure recipe buildenv assets fetchrun lint sources
              config slim fetch db archive
plugins    -> diag unique db sandbox transaction lock artifact atomic
groups     -> db atomic
repo       -> fetch version archive atomic groups
undo       -> transaction(15) install(10) db(7) artifact plan confirm
revdep     -> archive glob db install lint closure
service    -> sys                                    (geteuid check)
```

### 3.1 Reading the graph

* There is **no enforced layer structure**: `commands` reaches into 36
  modules including installer internals; `install` reaches into `service`,
  `transaction`, `sign`, `artifact`; `upgrade` orchestrates 16 modules.
  Everything is in one crate namespace and every module may import every
  other module; only *cycles* and *glob imports* are forbidden today.
* Pure-domain logic is already mostly isolated (`plan`, `deps`, `version`,
  `suggest`), but three purity violations are load-bearing:
  1. **`plan.rs` is typed over `db::Entry`** — the on-disk persistence
     record is the planner's input type.
  2. **Package identity validation lives in `db.rs`** — `recipe::model`
     (Recipe deserialization), `archive.rs` (`.PKGINFO` parsing) and
     `plan.rs` (through Entry) all call `db::valid_name`/`check_name`,
     making the *database adapter* the owner of a domain invariant.
  3. **`cli.rs` owns lock policy** (`lock_mode`) — the CLI layer decides
     which commands take which database locks, and `main.rs` implements
     the "lock once, top-level" rule that library functions rely on.
* Presentation (`output`, `diag`, `color`, `confirm`, `humanize`) is
  imported by nearly every layer including domain-adjacent modules; there
  is no user-interaction boundary to swap for tests or machine output.

### 3.2 Why these specific violations matter for the roadmap

* The capability/migration solver must receive normalized candidates built
  from *both* recipe and repository metadata (design §8.1). Today the only
  planner (`plan::install_plan`) cannot express repository candidates at
  all, and its input type (`db::Entry`) cannot express provides/conflicts.
* Removing service code will delete `install -> service` (the single
  `service::keep_disabled` call in `install/payload.rs:563`). The
  compatibility analysis is in §8.
* A port for user confirmation must exist before the migration engine can
  gate `apply` behind explicit authorization; today `confirm.rs` is called
  directly from `upgrade`, `commands/*`, and `undo`.

## 4. Large files and mixed responsibilities

| File | Lines | Mixed responsibilities |
|---|---|---|
| `commands/installing.rs` | 1055 | install (names + archives), update, autoremove, outdated, remove, plan printing, sourcing policy (auto/repo/build/ask), transaction begin/finish, per-step locking, recovery invocation, revdep warnings, hook retry |
| `install/mod.rs` | 1445 | public installer facade (5 entry points) + auth classification (`InstallAuth`) + payload application + db update + undo/cache writes + tests |
| `main.rs` | 430 | process startup + dispatch + lock + recovery + trigger gating + snapshot comparison |
| `cli.rs` | 950 | args + completions (3 shells, inline) + lock policy |
| `fetch/mod.rs` | 1024 | HTTP download orchestration, sig sidecar handling, per-source staging, progress output |
| `revdep.rs` | 908 | ELF scanning (via goblin), report model, suggestions |
| `install/previous.rs` | 866 | resolving "previous version" from history/cache for upgrades |
| `undo.rs` | 828 | undo record model + storage + rollback decisions |
| `lint/mod.rs` | 800 | recipe checks + ELF inspection |
| `sandbox/mounts.rs` | 785 | mount namespace construction |
| `artifact.rs` | 764 | content-addressed store + provenance + retention |
| `slim.rs` | 745 | post-install split passes |
| `transaction/history.rs` | 724 | record model + storage + parsing + validation |
| `config.rs` | 709 | layering + parsing + validation |
| `recipe/mod.rs` | 701 | model + helpers |

## 5. Package operation call flows (current behavior)

All flows start in `main::run`:

1. Parse `Cli` → build `Overrides` → `config::load` → `Ctx::from_config`.
2. Compute `lock_mode(&cmd)`; if `Some`, acquire the db lock **once for the
   whole command**; `Exclusive` additionally runs `recovery::recover_and_report`
   and `db::migrate_install_dates`.
3. Commands that can change the installed set take an "before" snapshot
   (`db::Db::installed()`, under a shared lock when the command locks
   per-step).
4. `run_command` dispatches; after it returns, take an "after" snapshot,
   compare payload identity, and if changed run
   `install::hooks::run_post_transaction_hooks` (exclusive lock, chroot- or
   live-executed `*.sh` scripts from `<root>/usr/lib/nawa/transaction-hooks.d`,
   guarded with `NAWA_POST_TRANSACTION_HOOKS_RUNNING=1`).
5. After lock release, `plugins::events::dispatch_pending` runs observer
   plugins.

### install (names)

`cmd_install` → hook-retry prepass (short exclusive lock + recovery per
retry) → load recipes + installed map → `open_sourcing` (repo mode:
`upgrade::open_repo` verifies index+signature, builds cache) → split targets
into recipe-backed and **binary-only `extras`** (`repo::RepoEntry`) →
`plan::install_plan` (recipe tree only) → transaction
`begin_operation` under a short exclusive lock → `run_plan`:
for each item: decide source (`Source::Build|Repo`) → `print_plan` →
dry-run? → confirm → per item `acquire_archive` (build via `build_one`, or
`upgrade::fetch_entry` with sha256+signature) → `install_one` →
`install::install_archive_pinned` / `install_archive_from_index_proof`
(short exclusive lock each) → `transaction::record_*_outcome` →
`op.finish(err)` under a fresh exclusive lock → revdep warning.
**Binary-only extras install with no dependency resolution at all**
(design §2.4 limitation, confirmed in code).

### install (archive paths)

`lock_mode` returns `Exclusive` for the whole command → recovery → for each
archive `install_archive_file` (detached-sig verify or allow_unsigned,
conflict checks, db update, history step).

### update

`cmd_update` → `plan::outdated` → per package: `build_one` then install
through the same `cmd_install_with_why` machinery (`Why::Update`).

### upgrade

`upgrade::upgrade` → `open_repo` (index fetch/verify, cache) →
`build_plan` compares db vs index per installed package (no recipe →
alphabetical order fallback; **no dependency ordering from repository
metadata**, no provider selection) → `print_plan` → confirm →
`apply_plan` → per package `fetch_entry` + `install_archive_from_index_proof`.

### remove / autoremove / rollback / history undo

Whole-command exclusive lock → recovery → `plan::removal_order` /
`plan::orphans` → `install::remove_with_linked` (file removal with
claimed-by-others protection, db removal, history steps) →
`undo.rs` records previous states for `rollback`.

## 6. Transaction, journaling, rollback, recovery

* **Transaction records** (`transaction/history.rs`): one immutable TOML
  record per logical command under `<root>/var/lib/nawa/history/transactions/<id>`,
  with per-step intents/results (`OperationHandle::step_intent*` /
  `step_result`), status (`running|completed|failed|interrupted`), package
  states (name/version/rel/reason/artifact digest), linked undo, group id.
  Allocation and every mutation happen under the exclusive db lock (one
  lock, no lock-ordering cycles).
* **TxWAL** (`transaction/journal.rs`, `outbox.rs`): write-ahead step
  journal so a crash between db commit and history write can be
  reconciled; `transaction::recovery.rs` + `install/journal.rs`
  `recover()` reconcile TxWAL vs history records.
* **File journal** (`install/journal.rs`): per-step physical file moves
  with `.bak` copies; `undo_steps` restores pre-step state on failure;
  recovery reverts journals that were not committed.
* **Undo log** (`undo.rs`): per-package previous-version record +
  cached archive → `nawa rollback <name>` installs it back.
* **Recovery timing**: every whole-command exclusive operation recovers
  first (`main::run`); per-step-lock commands (`install` by names, `update`,
  `build --install`, group install) call `recovery::recover_and_report`
  before `begin_operation` and before per-step operations.
* **Not covered**: boot configuration, initramfs, PID-1 identity — no
  journal exists for anything outside the package file/db/history scope.
  The design doc's migration journal requirement is genuinely new state.

## 7. Locking and nested invocations (`NAWA_LOCK_HELD`)

`lock.rs`:

* `<root>/var/lib/nawa/lock`, `flock(2)`; writers exclusive for the whole
  command, readers shared; `NAWA_LOCK_TIMEOUT` bounded waiting; holder pid
  written to the file (informational).
* **`acquire` returns `Ok(None)` when `NAWA_LOCK_HELD` is set in the
  environment — for BOTH shared and exclusive modes.** The lock file is
  not opened, no kernel lock is taken, and no error is reported.
* Producers of the marker:
  * `install/hooks.rs` `run_hook` (live) and `chroot_command` set
    `NAWA_LOCK_HELD=1` (+ `NAWA_ROOT`) in the hook's environment, so a
    package hook that runs `nawa` again does not deadlock on its parent.
  * Nothing else sets it; nothing *verifies* it.
* Consumers:
  * `lock::acquire` (skips all locking — see above).
  * `main.rs` suppresses post-transaction trigger dispatch when set
    (correct intent: the parent transaction will dispatch).
  * `plugins/events.rs` skips observer dispatch when set (same intent).

**Security finding (confirmed)**: the marker is forgeable by any process —
including a package hook — and grants silent, unlocked access to *every*
command, including exclusive package mutation. The parent's lock does not
extend to the child (flock is per open-file-description, and the child is
a different process after `fork`/`exec`), so a hook-spawned
`nawa install --root /` mutates the package database with no lock and no
serialization against the parent transaction. The design document
mandates (§10.3): gate checks run **before** honoring `NAWA_LOCK_HELD`;
a nested package-*mutating* command must fail closed when the marker is
present; only a read-only command whose snapshot consistency Stage 0 can
prove may keep the shortcut. The refactor must replace the generic
env-var bypass with in-process lock ownership (use cases share a lock
guard) plus a private typed migration permit.

## 8. Service-management subsystem and runit compatibility state

### 8.1 What exists in Nawa

* `src/service.rs` (395 lines): `/etc/sv` + `/etc/runit/runsvdir/default`
  listing, enable/disable (symlink into runsvdir), start/stop/restart and
  status via **the `sv` binary** (first-party service control), live-root +
  root checks, and the disabled marker (`<root>/etc/runit/disabled/<n>`).
* `src/commands/service.rs`: `cmd_service` dispatch for `ServiceCmd`.
* `cli.rs`: `Cmd::Service`, `ServiceCmd`, completion entries in all three
  shell scripts, and `lock_mode`'s `Cmd::Service => None`.
* `install/payload.rs:563`: the **only installer integration** —
  `keep_disabled(root, to)` skips installing a staged `/etc/runit/runsvdir/default/<name>`
  link when the admin-disabled marker exists and the link is absent,
  so a package upgrade cannot re-enable a disabled service.
* `scripts/smoke-v06.sh` §7 exercises `nawa service` end-to-end.
* `plugins/manifest.rs` `RESERVED_COMMANDS` includes `service` (a test
  asserts every built-in command is reserved).

### 8.2 What Faseela owns today (checked in faseela-repo @ main)

* **Runit is already removed from the distribution packaging.** Commit
  `ac1c3bb` ("Switch the init system from runit to systemd (phase 1:
  core)") deletes `recipes/system/runit` and `recipes/system/runit-services`
  and every runit dependency; `120b768` moves desktop services and PAM to
  systemd. No current recipe ships `/etc/sv` run scripts or default
  runsvdir links (grepped the tree).
* The `systemd` recipe ships `usr/bin/init -> ../lib/systemd/systemd`
  (PID 1 for direct disk boots via the merged-usr `/sbin/init` lookup);
  the `systemd-units` recipe ships **distribution-owned service
  configuration**: `80-faseela.preset` (preset policy, sshd explicitly
  "available, not enabled"), networkd/resolved/timesyncd/journald
  defaults, tmpfiles, PAM. Service management on Faseela is `systemctl`
  (outside Nawa).
* The old `runit-services` payload shipped default runsvdir links for 8
  services and `usr/bin/init -> /usr/bin/runit`. The `keep_disabled`
  mechanism existed to protect admin-disabled links from being recreated
  by package upgrades of that payload.
* `recipes/boot/linux/recipe.toml` rel 25 notes kernel options "for the
  runit -> systemd switch".

### 8.3 Compatibility analysis for removing the service code

1. **`nawa service` command and module**: safe to remove from Nawa; on
   current Faseela there is no `/etc/sv` tree to manage. Remaining users
   are pre-systemd installed systems that can only leave runit via the
   migration itself; `systemctl` is the service manager after migration.
   Documented user-visible removal (CHANGELOG entry required).
2. **`keep_disabled` payload integration**: the hazard it guards against
   is "a package payload recreates a runsvdir link". After `ac1c3bb` no
   distribution package contains runsvdir links, so **no distribution
   package can recreate them**; enablement policy for the systemd world
   is Faseela-owned (`80-faseela.preset`). Faseela's distribution-owned
   configuration policy therefore already exists in the form the design
   requires ("enablement links out of ordinary package payloads") — the
   decision to remove the marker handling is justified by the packaging
   state, but must be recorded here with this exact reasoning, and the
   removal must keep the `/etc/runit/disabled` *marker files themselves*
   untouched (Nawa never deletes administrator data; the files are inert
   after removal).
   Residual: third-party/local recipes that still ship runsvdir links
   would re-enable services on upgrade — acceptable and documented,
   because such recipes predate the systemd switch and Faseela's recipe
   review policy rejects them.
3. **`.HOOKS` and transaction triggers are NOT made harmless by removing
   the service APIs.** They remain privileged root-shell execution
   (§9). No hook trust-policy change is implied by service removal; the
   policy statement is recorded in §9 and the design doc.

## 9. Privileged script mechanisms (trust boundary, current state)

* **Package `.HOOKS`** (`archive::read_hooks`, TOML with
  `post_install`/`pre_remove`/`post_remove`): executed by
  `install/hooks.rs::run_hook` — `bash -e -c <script>` as root on the live
  root, or inside `chroot(2)` with the target root for `--root` runs.
  Scripts come from package archives; acceptance policy for an archive is
  (a) detached signature by a trusted key, (b) signed-index digest
  authentication, or (c) explicit `--allow-unsigned` (allowed when no
  trust keys are configured — the bootstrap case) — i.e. an unsigned
  archive's hooks DO run as root when the user explicitly allows unsigned
  packages. Hooks inherit `NAWA_ROOT` and `NAWA_LOCK_HELD=1`.
* **Post-transaction triggers**: distribution/packager-installed
  `*.sh` scripts under `<root>/usr/lib/nawa/transaction-hooks.d`, run
  after any package-changing command under an exclusive lock, same
  live/chroot rules, guarded against re-dispatch.
* **Static lint**: `lint/` checks recipe structure, build outputs and ELF
  health; there is no hook linter today. The design's statement stands:
  static policy checks are a guard, not a sandbox.
* **Plugins**: out-of-process JSONL hosts, discovered from user/system
  dirs, capability-gated (`events`, `run`), admin policy grants stored in
  a root-owned config; plugins cannot touch the package database directly
  (no such port exists) but can be granted `run` to execute arbitrary
  plugin-declared commands — they are outside the trusted core and must
  never become migration handlers.

## 10. Repository metadata and solver gaps (measured)

* `repo::RepoEntry` fields: `name, version, rel, arch, file, sha256,
  download_url?, size, installed_size`. **No provides, no conflicts, no
  replaces, no runtime-dependency list.**
* `.PKGINFO` is flat `key = value` (`parse_pkginfo`); used keys today:
  `pkgname, pkgver, pkgrel, arch, size, ...` (validated minimally).
  Runtime deps are recorded by the installer into the db (`Entry.run`)
  from recipe metadata; the archive-side `.PKGINFO` dependency list is not
  consumed for planning.
* `plan::install_plan` resolves only through the recipe tree; repository
  entries enter plans as untyped "extras" after planning.
* `upgrade::build_plan` is version-only with alphabetical fallback when
  the recipe tree is unavailable; `Sourcing::pick` compares recipe vs
  repository versions — no provider concept anywhere.
* Conclusion: the metadata chain required by design §6.3 (recipe →
  `.PKGINFO` → authenticated v4 index duplication, with
  unknown ≠ empty semantics) has **no existing precedent in the code**;
  v4 schema work must define the TOML spelling and tests before any
  solver consumes it.

## 11. saq init-selection contract (verified against `saq` @ f54918a)

* `share/saq/init` (installed-system initramfs `init`): parses `init=`
  from the kernel command line, then picks the first existing executable
  from:

  ```sh
  for candidate in "$INIT_ARG" /usr/bin/runit /sbin/init /usr/sbin/init; do
  ```

  **`/usr/lib/systemd/systemd` is not in the list.** systemd boots today
  only through `/sbin/init` → `/usr/bin/init` (the symlink the systemd
  package ships).
* `share/saq/init-live`: same chain plus `/usr/lib/systemd/systemd` last.
* Faseela's installer (`bootstrap/install.sh`) writes grub.cfg **without
  `init=`** (`linux /boot/vmlinuz root=UUID=… quiet`).
* Consequence for the planned migration (design Phase 7): while runit
  remains installed during preparation (required — it must not be removed
  before verification), `/usr/bin/runit` exists and **wins the fallback
  chain**, so the generated initramfs would boot runit again unless the
  migration handler (a) sets `init=/sbin/init` explicitly in the boot
  configuration, or (b) `saq` changes the chain. Note also the old
  runit-services payload pointed `/usr/bin/init` at runit while the
  systemd package points it at systemd — the two payloads **conflict on
  `/usr/bin/init`**, so staged coexistence of both providers (design
  §6.2) requires Faseela-side payload changes (move the init selector out
  of the runit-services payload) or an explicitly authorized selector
  swap recorded in the migration journal. This is a **blocker for
  Phase 7 apply-safety** that must be resolved with Faseela/SAQ before
  any live migration runs; planning-only work can proceed.
* BIOS and UEFI boot paths are supported by the Faseela installer
  (grub `i386-pc` + `x86_64-efi`; QEMU boot-test scripts exist:
  `scripts/qemu-boot-test.sh`, `--uefi` via OVMF).

## 12. Existing compatibility constraints (behavior baseline to preserve)

1. **Package database format**: `<root>/var/lib/nawa/local/<name>/{desc,files,hashes}`
   TOML, `Entry` field set incl. optional `artifact_sha256`, legacy
   `install_date` normalization, `.bak` migration copies.
2. **Recipe format**: schema-2 TOML with the fail-closed `SCHEMA_MARKER`
   rule for new-schema tables; `raw` bytes and `dir` used for the
   deterministic build id.
3. **Repository index**: v1 (legacy unsigned), v2 (signed index, sha256
   auth metadata), v3 (per-package HTTPS URLs + locations manifest);
   `repo-update` incremental rules (immutability, no-loss removals).
4. **Archive format**: `.nawa.tar.zst` (zstd tar), `.PKGINFO` key=value,
   optional `.HOOKS`; normalized creation (mtimes, ownership).
5. **CLI**: every command, flag, alias (`list`), env override
   (`NAWA_*`), completion script output, exit codes (130 for Ctrl-C).
6. **Locking**: wait-for-lock message, timeout, holder pid file.
7. **Transaction history**: record shape and `nawa history` output.
8. **Undo/rollback semantics**: undo records, cache retention.
9. **Hooks/triggers**: `.HOOKS` script names, trigger directory
   `<root>/usr/lib/nawa/transaction-hooks.d`, `NAWA_POST_TRANSACTION_HOOKS_RUNNING`
   guard.
10. **Plugin protocol**: manifest version, reserved commands
    (`RESERVED_COMMANDS` must drop `service` when the command is removed —
    reserved commands are a *namespace* reservation; removing a built-in
    command while keeping the reservation is acceptable but the CI test
    `reserved_commands_cover_every_builtin_cli_command` requires every
    built-in command to be reserved, not vice versa, so `service` can stay
    reserved or be dropped with the manifest version unchanged).

## 12.4 Test baseline (executed on this machine, 2026-11-17)

`cargo test --all-targets --offline --no-fail-fast`:
459 lib tests (458 pass, 1 ignored), 13 integration binaries — 535 passed,
3 failures, all environment-induced and pre-existing:

| Failing test | Cause |
|---|---|
| `fetchcargo::cargo_fetch_and_offline_build_work_under_sandbox` | toolchain lives under `/home`, which the sandbox masks (CI sets `RUSTUP_HOME=/usr/local/rustup`) |
| `plugins::plugin_run_executes_and_prints_json` | container cannot remount `/sys/firmware/efi/efivars` read-only (`EPERM`), so the plugin sandbox refuses; needs real kernel features |
| `plugins::wrong_request_id_is_an_error` | same cause |

These are recorded as the baseline; any *new* failure after refactoring is
a regression. CI (`ci.yml`) also runs: layering gate, clippy `-D warnings`,
release tests on 4 host/uid combinations, the isolation matrix, smoke
scripts v05/v06, and the musl static build.

## 13. Security risks (ranked)

1. **Forgeable `NAWA_LOCK_HELD` unlocks every command** (§7): no lock, no
   gate, silent. Highest priority; must be replaced by in-process lock
   ownership + fail-closed nested mutation + persistent migration gate
   checked before the marker is honored.
2. **`.HOOKS` and transaction triggers are root shell code** (§9): an
   unsigned-but-explicitly-allowed archive runs its hooks as root; hooks
   can invoke `sv`/`systemctl`. Trust policy must be documented
   explicitly (design §9); Nawa cannot make arbitrary shell hooks
   harmless.
3. **Unsigned archive acceptance** when no trust keys exist (bootstrap
   mode) and via `--allow-unsigned`: intentional, but every consumer of
   archive-derived data (including future capability metadata) must treat
   unsigned provenance as untrusted for privileged decisions.
4. **SHA-256 proves bytes, not publisher** (design §6.3): the signed index
   authenticates digests; detached sigs authenticate archives. Any future
   sidecar metadata is untrusted for migration decisions by definition.
5. **Service code** calls `sv` as root (`nawa service`): being removed.
6. **Path handling** is already hardened (no-follow openat2 extraction,
   containment checks, name validation) — keep it that way when moving
   modules.

## 14. Missing capabilities (per the design roadmap)

* No init-system detection (no `/proc/1` inspection exists; `sys.rs` has
  only the syscall shims).
* No system profile concept (only build profiles: `profile.rs` +
  `--profiles`).
* No capability/provides model in recipe, archive, index, or db.
* No migration state machine, journal, or persistent gate.
* No plan explanation structure (plans are printed ad hoc per command).
* No offline-root distinction: `--root` works for package operations, but
  no context classification (live host / chroot / container / offline)
  exists anywhere.

## 15. Migration map (current file → target module)

Ownership targets per design §4.5; moves are incremental and gated by the
existing CI layering check extended with direction rules (Stage 1 adds the
direction checker first). "—" = no move in early stages.

| Current | Target | Stage |
|---|---|---|
| `main.rs` | slim entry → `bootstrap.rs` wiring + `application::lifecycle` | 1 |
| `cli.rs` (types) | `cli/args.rs` + `cli/commands/*` | 1 |
| `cli.rs` completions | `cli/completions.rs` | 1 |
| `cli.rs` `lock_mode` | `application/lifecycle.rs` (lock policy out of CLI) | 1 |
| `commands/installing.rs` | `application/features/package/{install,update,remove,autoremove}.rs` + shared `use case` plumbing; CLI keeps thin handlers | 1 |
| `commands/{querying,building,fetching,groups,history,plugins,recipes,sandbox,signing,configuring}.rs` | matching `application/features/*` use cases | 1 (incremental) |
| `commands/service.rs` | **deleted** | 1 |
| `service.rs` | **deleted** (keep_disabled reasoning recorded in §8) | 1 |
| `plan.rs`, `deps.rs` | `domain/planning/`, `domain/dependency/` (retyped off `db::Entry`) | 1–2 |
| `db.rs` name/version validation | `domain/package/identity.rs` (db adapter keeps persistence only) | 1 |
| `db.rs` persistence | `adapters/database/` behind `application::ports::PackageDatabase` | 1–2 |
| `recipe/model.rs`, `validate.rs` | `domain/recipe/` (model stays pure; `raw`/`dir` move to a loader envelope) | 2 |
| `recipe/load.rs`, `expand.rs` | `adapters/recipe_source/` + `application/features/build` | 2 |
| `repo/*`, `repositories.rs`, `fetch/` | `adapters/repository/`, `adapters/fetch/`, `application/features/repository` | 2 |
| `install/*` | `application/features/package/install_use_case.rs` + `adapters/{archive,filesystem,transaction}/` (narrow facade kept) | 1–2 |
| `transaction/*`, `recovery.rs` | `domain/transaction/` (state model) + `adapters/transaction/` (storage) + `application/lifecycle.rs` | 2 |
| `undo.rs`, `revdep.rs` | domain analysis + adapters for storage/ELF scan | 2 |
| `upgrade/*`, `sync.rs` | `application/features/repository/{upgrade,sync}.rs` | 2 |
| `lock.rs`, `interrupt.rs` | `adapters/process/` + `application/lifecycle.rs` | 1 (lock first — security) |
| `sign.rs`, `checksum.rs`, `artifact.rs` | `adapters/signing/`, `adapters/artifact/` (digest types may go domain) | 2 |
| `archive.rs`, `atomic.rs`, `fswalk.rs`, `glob.rs`, `pathutil.rs`, `unique.rs` | `adapters/{archive,filesystem}/` + domain path/name rules | 2 |
| `config.rs`, `profile.rs`, `ctx.rs` | `adapters/configuration/` + `application/context.rs` | 2 |
| `build/*`, `buildenv.rs`, `fetchrun/*`, `sources.rs`, `assets.rs`, `closure.rs`, `slim.rs`, `lint/*`, `sandbox/*`, `sys.rs` | cohesive build subsystem behind the build feature (no flattening) | 3 (optional) |
| `output.rs`, `diag.rs`, `color.rs`, `confirm.rs`, `humanize.rs` | `presentation/` (+ user-interaction port in `application::ports`) | 1–2 |
| `groups/*` | `domain/groups/` + `application/features/groups/` | 2 |
| `plugins/*` | `adapters/plugins/` + `application/features/plugins/` | 3 |
| `testutil.rs` | stays test support | — |
| **new** | `domain/system/` (identity, profile, capability), `application/features/system/`, `adapters/host_inspection/`, migration engine modules | 2–5 (roadmap) |

## 16. Stage-1 exit criteria derived from this audit

1. Layering gate extended to enforce direction rules: `domain` imports no
   `cli`/`application`/`adapters`/`presentation`/`commands`/`main`
   modules; `adapters` import no `cli`/`presentation`; `cli` imports no
   adapter implementation modules.
2. `main.rs` contains only process entry + bootstrap call;
   `bootstrap.rs` composes concrete implementations.
3. Lock policy moves out of `cli.rs`; `lock::acquire` no longer honors
   `NAWA_LOCK_HELD` for exclusive mutations; nested mutation fails closed;
   read-only nested calls keep the shortcut only behind an explicit,
   documented consistency argument (snapshot reuse is impossible across
   processes; read commands re-read state under their own shared lock —
   the current behavior is already "re-read after the parent releases";
   see §17 note).
4. Service module, command, completions, tests, and smoke section removed;
   `keep_disabled` removal justified per §8 and recorded.
5. All package operations keep observable behavior: existing test suite
   green (minus the three documented environment failures), plus new
   tests for the lock and gate changes.
6. `docs/implementation-progress.md` updated with actual test results.

## 17. Note on the read-only `NAWA_LOCK_HELD` shortcut

Design §10.3 allows keeping the marker for nested *read-only* commands
"only where Stage 0 proves its read snapshot is consistent". Measured
behavior: a hook-invoked `nawa query` currently (a) skips locking and
(b) reads the db directory directly. Without a lock it can observe a
half-applied transaction of the parent (files moved, db entry written
between `desc`/`files`/`hashes` writes is only partially possible — each
file is published atomically, but the three files are not published as
one unit). The safe default for Stage 1 is: **read-only commands also
take their (shared) lock** — the parent holds *exclusive*, so the child
blocks until the parent finishes. Hooks are short-lived; blocking on the
parent's exclusive lock for a read is correct and cannot deadlock (the
parent is not waiting on the child's exit for anything but hook
completion… except hooks run *while* the parent holds the lock, so a
hook's nested read WOULD deadlock on the parent's own exclusive lock).
Therefore the only deadlock-free choices are: skip locking for read-only
nested commands (current behavior, tolerating torn reads) or fail closed
for them too. Stage 1 keeps the current read-only skip (documented
limitation, no behavior change), fails closed for mutations, and the
persistent migration gate is checked *before* the marker is honored for
mutations — read-only commands are unaffected by the gate.

---

*End of audit. Next: Stage 1 (structural refactor + service removal) — see
`docs/implementation-progress.md` for the live task list.*
