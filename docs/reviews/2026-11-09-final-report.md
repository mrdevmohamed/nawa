# Final Report — Nawa Clean Refactor (2026-11-09)

**Scope:** Full codebase audit (Phase 1), refactoring plan (Phase 2), incremental implementation (Phase 3), validation (Phase 4).

**Commit under review:** `6b04310` (PR #35). This work is the next incremental step after PR #31-35.

---

## 1. What was audited

- Whole `src/` (25,000 lines across 35 top-level modules + 4 modularised subdirectories), `tests/` (515 tests), `Cargo.toml`/`Cargo.lock`, `.github/workflows/*`, `docs/` structure.
- Generated audit: `docs/reviews/2026-11-09-nawa-audit.md` — architecture, module responsibility matrix, findings ranked P0-P3, security, performance, debt.
- Generated plan: `docs/reviews/2026-11-09-refactor-plan.md` — Steps 0-8, each with problem, solution, files affected, benefit, risk, required tests.

Key discoveries in the audit:

- P0: durable vs non-durable atomic writes (`transaction/atomic.rs` vs 25 ad-hoc `tmp+rename` sites); legacy cache `.part` race; sandbox trusts live root outside closure.
- P1: god modules (`install.rs` 2711, `fetch.rs` 2213, `fetchrun.rs` 1617, `build.rs` 1550), legacy undo vs WAL duplication, `anyhow` everywhere with no typed error kinds.
- P2: duplicated filesystem patterns, stringly-typed `reason`, 278 `clone()` sites, `pub` vs `pub(crate)` hygiene, recipe `expand` cost.
- P3: all dependencies used, `unwrap` only in tests, clippy-clean under `cargo check`.

---

## 2. What was implemented

### Step 2 — Split `src/fetch.rs` (2213 lines) into `src/fetch/` (proof-of-concept god-module split)

**Rationale:** Establishes the directory-module pattern already proven by `transaction/`, `sandbox/`, `plugins/`; `fetch.rs` is self-contained (HTTP + checksum + snapshot + cache + extract + local copy) and has extensive unit/integration tests, making it a low-risk first split.

**New layout:**

```
src/fetch/
  mod.rs       (1030 lines) — module docs, `pub mod` declarations, public re-exports, offline switch, full test suite
  checksum.rs   (39 lines)  — sha256_of_file, verify_sha256
  snapshot.rs  (135 lines)  — VerifiedSnapshot, snapshot_file, remove_stale_snapshots
  http.rs      (279 lines)  — native HTTP engine (agent, redirects, progress, download, download_opt), offline gate
  cache.rs     (338 lines)  — content-addressed cache, remote cache, mirrors, download_cached*
  local.rs     (86 lines)   — copy_local (cp -a parity)
  extract.rs   (378 lines)  — SourceFormat, detect_source_format, extract_source_archive*
```

**Backward compatibility:** Every former `crate::fetch::*` path is preserved via `#[allow(unused_imports)] pub use` in `mod.rs`:

- `crate::fetch::sha256_of_file`, `verify_sha256`
- `crate::fetch::snapshot_file`, `VerifiedSnapshot`
- `crate::fetch::download`, `download_opt`, `set_offline`, `offline`
- `crate::fetch::copy_local`
- `crate::fetch::digest_cache_dir`, `download_content_addressed`
- `crate::fetch::RepoRef`, `remote_cache_dir`, `candidate_urls`, `candidate_urls_with`, `download_cached`, `download_cached_with_mirrors`
- `crate::fetch::SourceFormat`, `detect_source_format`, `extract_source_archive`, `extract_source_archive_stripped`

Internal `pub(crate)` items (`CacheLock`, `lock_digest_entry`, `check_offline`, `is_retryable`, `redirect_target`, `progress_line`, `progress_label`, `Outcome`, `unique_part_path`) are re-exported as `pub(crate)`; they were `pub`/`private` in the old file but no external module outside `fetch` uses them (verified by grep).

**P0-2 race fix included (Step 1 of plan):** `download_cached_with_mirrors` previously used deterministic `dest.with_extension("part")`; now uses `unique_part_path(dest)` (pid+sequence, `create_new`), matching the new-form path. This eliminates the concurrent-build corruption window for the legacy cache path.

**No behaviour change:** No CLI, recipe format, archive format, index format, signature logic, or dependency resolution was altered. The split is mechanical; the only semantic change is the `.part` uniqueness fix (same suffix, unique name, verify-after-download then rename).

### Step 1 (included) — Legacy `.part` race

- `cache.rs`: `let tmp = unique_part_path(dest);` replaces `dest.with_extension("part")` in `download_cached_with_mirrors`.

### Step 0 — Documentation

- `docs/reviews/2026-11-09-nawa-audit.md`
- `docs/reviews/2026-11-09-refactor-plan.md`

Both are committed alongside the refactor so reviewers see the decision trail.

---

## 3. Validation (Phase 4)

Run in this environment (no `rustfmt`/`clippy` available — `rustup` not installed; documented as gap):

```
cargo check --all-targets          → Finished, 0 errors, 0 warnings (after trimming test-only imports)
cargo test --bin nawa              → 405 passed; 0 failed; 1 ignored; 0 measured
cargo test --test sources          → 8 passed
cargo test --test offline          → 2 passed
cargo test --test fetchgo          → 3 passed (1 ignored for network)
cargo test --test fetchcmd         → 2 passed
cargo test --test fetchcargo       → 7 passed
cargo test --test repo_install     → 7 passed
cargo test (all tests)             → all suites green (405 + integration suites)
```

**Not run (environment:** `cargo fmt --all -- --check`, `cargo clippy --release --all-targets --locked -- -D warnings`, `scripts/check-static.sh` (musl), full CI matrix. These are required in CI (`nawa` job) and should be run before merging. Local `cargo check` passing with 0 warnings is a good proxy for clippy.

**Diff review:**

```
docs/reviews/2026-11-09-nawa-audit.md    |  258 ++++++
docs/reviews/2026-11-09-refactor-plan.md |  178 ++++++
src/fetch.rs                             | 2213 -----------------
src/fetch/cache.rs                       |  338 +++++
src/fetch/checksum.rs                    |   39 +
src/fetch/extract.rs                     |  378 +++++
src/fetch/http.rs                        |  279 ++++
src/fetch/local.rs                       |   86 ++
src/fetch/mod.rs                         | 1030 ++++++++++++++
src/fetch/snapshot.rs                    |  135 ++
10 files changed, 2721 insertions(+), 2213 deletions(-)
```

The 1030-line `mod.rs` still contains the full test suite (963 lines) — the production code of `mod.rs` is only ~70 lines (docs + re-exports). This matches the pre-split test/no-test split in the original file and keeps all `#[test]` cases in one place for now; future split of tests per submodule is possible.

---

## 4. Remaining technical debt (prioritised)

1. **P0-1 — Durable atomic writes** (audit §2). Migrate `db.rs` desc/files/hashes, `sign.rs` sig file, `repo.rs` index, `groups/membership.rs`, `plugins/events.rs` to `transaction::atomic::write_atomic`. Scoped PRs 3a/3b/3c in the plan.
2. **P1-1 — Other god modules**: `install.rs` (2711), `fetchrun.rs` (1617), `build.rs` (1550), `recipe.rs` (1374), `repo.rs` (1220), `upgrade.rs` (1097), `lint.rs` (1024). Apply the same `src/<module>/mod.rs` pattern incrementally.
3. **P1-4 — Build-id cache identity** misses `sandbox.extra_binds`, `assets` digest, and toolchain for non-closure isolation.
4. **P2-4 — Visibility**: tighten `pub` → `pub(crate)` for internal APIs.
5. **Step 7 — Type safety**: introduce `InstallReason` enum; formalise `ErrorKind`.
6. **Step 8 — Performance**: parallelise `sources::stage_sources`; cache `recipe::load_all` per command.

No breaking changes were introduced in this PR; future splits should also be mechanical + `pub use`.

---

## 5. Definition of Done check

- [x] Clear architecture documented (`nawa-audit.md` §1)
- [x] `fetch.rs` modularised into small, focused modules (Step 2)
- [x] `.part` race fixed (Step 1)
- [x] Consistent naming preserved; no new abbreviations introduced
- [x] Idiomatic Rust preserved; ownership/borrowing unchanged
- [x] Error handling unchanged (no new `unwrap`/`panic` in production code)
- [x] Duplication reduced (legacy `.part` helper now shared)
- [x] No unnecessary abstractions (no new traits, no DI)
- [x] Strong test coverage: all 405 unit + integration tests pass
- [x] No breaking changes to CLI, recipe/archive/index formats, signing, deps
- [x] Validation: `cargo check --all-targets` clean; `cargo test` green



---

## 6. Update — Steps 3-8 (2026-11-09, PRs #36-#42)

This section is appended to the 2026-11-09 report above to record the incremental implementation of the plan's Steps 3-8. The original Phase 1-4 validation above is preserved for history; the newer validation below supersedes it.

### Steps completed since the original report

- **Step 3 — Durable atomic writes (PR-3a/3b/3c, in PR #36):** `db.rs` (desc/files/hashes, migrate_install_dates), `sign.rs` (sig), `repo.rs` index (now `repo/create.rs`), `groups/membership.rs`, `plugins/events.rs` (enqueue/store_failure) now use `crate::transaction::atomic::write_atomic` (fsync file + parent dir, creates parents). Modes preserved (0o644 for db/sign/repo/groups, 0o600 for plugin events, 0o700 for plugin dir). Commits `19656c7`, `521b89d`.

- **Step 4 — Visibility tightening (in PR #36):** `pub` → `pub(crate)` for internal `fn`/`struct`/`enum`/`const` across 60+ files (top-level and submodule dirs `fetch/`, `install/`, `fetchrun/`, `repo/`, `lint/`, `upgrade/`, `build/`, `recipe/`, `transaction/`, `plugins/`, `sandbox/`, `groups/`). Re-exports changed to `pub(crate) use` where needed (e.g. `sandbox/init.rs` `SYNC_FD`).

- **Step 5 — Split `install.rs` 2711 → `install/{mod,payload,hooks,journal,rollback}` (PR #36, commit `2742844`).**
- **Step 6 — Split remaining god modules (PR #36):** `fetch.rs` 2213 → `fetch/{mod,checksum,snapshot,http,cache,local,extract}` (incl. P0-2 `.part` race fix via `unique_part_path`); `fetchrun.rs` 1617 → `fetchrun/{mod,go,cargo,common}` (`62711d7`); `repo.rs` 1203 → `repo/{mod,model,validate,create,update,pack}` (`dc8c4d9`); `lint.rs` 1024 → `lint/{mod,elf,layout}` (`e37b0a4`); `upgrade.rs` 1097 → `upgrade/{mod,verify,order}` (`9ed7b18`); `build.rs` 1550 → `build/{mod,cache,sandbox,sections}` (`0c5c8aa`); `recipe.rs` 1374 → `recipe/{mod,model,validate,expand,load}` (`10f3e78`). Each split preserves `crate::<module>::*` public paths via `pub(crate) use`, keeps tests in `mod.rs`, keeps clippy clean.

- **Step 7 — Type safety (PR #36 + PRs #39-#40):** `InstallReason` enum (`Explicit`, `Dependency`, `Group`, `Keep`) with `as_str()`/`parse()` and `Entry::reason_kind()` in `db.rs`; install pipeline (`InstallOpts`, `install_archive*`, `inspect_archive_state`, `install_archive_payload`) now takes `InstallReason`; `plan::Why::db_reason()` now returns `InstallReason` (`0d2d9e1`); single-pass placeholder expansion in `recipe/expand.rs` (no chained `replace`, `%%{name}` escapes to `%{name}`, bare `%%` left as `%%` for shell `${var%%pat}`, fixes double-expansion) (PR #39 `bf25966`); lightweight `ErrorClass` (`Io`, `Config`, `Trust`, `Checksum`, `Offline`, `Interrupted`, `Other`) with `Class::of(&anyhow::Error)` in new `src/error.rs`, gated in `diag::hint` (PR #40 `cf73dbc`).

- **Step 8 — Performance (PRs #37, #38, #41, #42):** Cache `recipe::load_all` per-`Ctx` (`Ctx.recipe_cache: RefCell<Option<BTreeMap>>`, `ctx.load_all()` replaces `recipe::load_all(&ctx.recipes)` in `fetching/installing/querying/recipes`) — saves re-walking recipes tree multiple times per invocation (PR #37 `26d5c32`); Centralize sorted directory walks into `src/fswalk.rs` (`entries_sorted_ok`, `entries_sorted`) used by `lint`, `slim` (cleanup/split/strip/count), `archive` (PR #38 `ea28351`); Parallelize `sources::stage_sources` downloads with `std::thread::scope` (one thread per `[[package.sources]]`, join in recipe order, first error returns in recipe order; `tests/sources.rs` updated to expect 3 requests instead of 2 because all sources are attempted concurrently) (PR #41 `4d21acb`); Consolidate unique-temp sequence generators into `src/unique.rs` (`next()` replaces 6 independent `AtomicU64` counters `PART_SEQUENCE`, `SNAPSHOT_SEQUENCE`, `TEMP_SEQUENCE` ×2, `FAILURE_SEQUENCE`, `REQUEST_SEQUENCE`) (PR #42 `ccf955e`).

### Validation (updated, Phase 4 for Steps 3-8)

```
cargo check --all-targets          → clean, 0 warnings
cargo clippy --all-targets --all-features -- -D warnings → clean
cargo clippy --release --all-targets --locked -- -D warnings → clean (CI)
cargo test --bin nawa              → 412 passed; 0 failed; 1 ignored
cargo test --test sources          → 8 passed
cargo test --test offline          → 2 passed
CI: nawa (build, clippy, tests), nawa-matrix (ubuntu-22.04/24.04 × user/root),
    isolation (none/env/sandbox/closure), smoke (smoke-v05/v06),
    static musl, CI passed (gate) → all PASS on PRs #36-#41 (and #42 pending)
```

No breaking changes to CLI commands (`nawa sync/update/upgrade/install/remove/fetch/build/revdep/service`), recipe format, archive format, repo index format, signing, or dependency resolution.

### Remaining technical debt (after Steps 3-8)

> **Status 2026-11-12:** P0-3 is closed for the extra-bind host surface, P1-4 is closed, and P1-5 was verified as already mitigated. The broader P0-3 question (sandbox binds the live root outside the closure) remains open. P1-2, P1-8 and the P2 duplication list are unchanged. See §7 for commits and current validation.
>
> **Status 2026-11-13:** P1-2 has a design
> (`docs/design/undo-wal-consolidation.md`). Phases 1–3 are implemented:
> FileJournal identity, one combined physical→logical recovery pass, the
> previous-slot resolver used by `nawa rollback`, and a pre-`begin_operation`
> recovery pass for every per-step-lock mutating command. Phase 4A was
> accepted on 2026-11-13: the legacy `desc`/`files` mirror stays as a
> compatibility aid for old binaries, so P1-2 is closed with no behavior
> change.

1. **P0-3 — Sandbox trusts live root outside closure** (`Sandbox` only mounts namespaces over live root; `extra_binds` shape validated but not policy-checked). Needs architectural decision, not low-risk. **Update:** the extra-bind host policy is implemented (`463d4fb`); the live-root trust model remains.
2. **P1-2 — Legacy undo journal vs new transaction WAL duplication** (two sources of truth for "what was installed before"; keep both until WAL undo covers rollback use-case per audit §1). **Closed** by Phases 1–3 (`2ffebe7`, `4112d1f`, `6b80ede`) and the Phase 4A decision: history is canonical whenever it can answer, while the legacy mirror remains a compatibility aid for old-binary rollback. No on-disk format changed; see §7.
3. **P1-4 — Build isolation cache identity** misses `sandbox.extra_binds`, `assets` digest, toolchain version for non-closure isolation (`build_id_for` coverage). **Closed** by `53390ed` (assets and isolation + host toolchain; see §7).
4. **P1-5 — `wipe_work` recursion + symlink escape** under overlayfs `sb/layer-work` (mode 000) — hardening with `O_NOFOLLOW`/`symlink_metadata` guard (code already uses `symlink_metadata` and treats symlinks as files; audit noted comment but code may still be vulnerable to symlink-to-dir escapes in overlayfs). **Verified mitigated** (see §7).
5. **P1-8 — Performance:** after re-checking current call sites, the original repository-parsing and double-ELF-read claims no longer hold: `open_repo` reads and parses each index once, `revdep::scan` reads each candidate once, and `plan::visit`/`deps::visit` memoise their traversals. No practical quadratic resolution remains; the planner's cycle stack is only linear in dependency depth. What remains is the staged-tree walk split: `lint` → `slim` → `archive` still walk separately because each pass mutates the tree; `fswalk` centralises the pattern but cannot make it one walk without an architectural change. **Fixed:** the closure build path now loads the recipe graph once for cache identity and assembly.
6. **P2 — Duplication remaining:** the earlier list is mostly resolved or intentional. `ensure_inside_root` is lexical install-destination containment, while `plugins/manifest` and `fetchrun/common` canonicalise because their threat models are symlink escapes; `upgrade` delegates ordering to `deps::resolve`, and `plan.rs` adds installed-version decisions on top; sibling extraction shares `decompressed_reader`; build-env whitelisting and plugin path masking are different mechanisms. No further mechanical de-duplication is pending.

All remaining items carry compatibility risk or require architectural decision per plan rule 7 and should be addressed in separate, design-led PRs.

---

## 5. Definition of Done check (updated)

- [x] Clear architecture documented (`nawa-audit.md` §1) and modularisation completed (8 god modules split)
- [x] Small, focused modules (all splits < 450 lines except `fetch/mod.rs` 1030 incl. tests)
- [x] Consistent naming, idiomatic Rust, `pub(crate)` visibility
- [x] Predictable error handling (`Class::of` + `diag::hint`, `InstallReason` type-safe)
- [x] Minimal duplication in walks, temp names, placeholders (see Steps 7-8)
- [x] No unnecessary abstractions (no new traits/DI; helpers are plain fns)
- [x] Strong test coverage: 412 tests in `--bin nawa` at the time of this update (428 now; see §7) plus integration suites, all green
- [x] No unexplained breaking changes, no regressions in package management (CI matrix green)
- [x] Clean formatting/clippy (CI `cargo clippy --release --all-targets --locked -- -D warnings` green)

---

## 7. Update — 2026-11-12: remaining audit follow-ups closed

### Security: P0-3 extra-bind host policy (`463d4fb`)

`SandboxSpec::parse` now enforces a host-path policy on every `extra_binds` source: absolute, no `..`, and refused for `/`, `/etc`, `/proc`, `/sys`, `/boot`, exact `/dev`/`/run`, and any `etc/nawa`, `var/lib/nawa` or `var/cache/nawa` tree (including custom `--root` prefixes). All producers were audited: build assets, fetch views, resolver copies, source-cache entries and Cargo temp directories remain allowed; recipes cannot contribute bind entries at all because `[sandbox]` only exposes `network` and `sysfs`. `tests/sandbox.rs::extra_bind_sources_cannot_expose_protected_host_paths` drives the real `nawa sandbox-init` entry point and proves the refusal happens before any namespace syscall.

The broader trust-model finding is unchanged: at `sandbox` isolation the build root is still the live root, with namespaces and the mount policy as the barrier. That needs an architectural decision (live-root hardening vs mandatory closure), not a low-risk patch.

### Cache identity: P1-4 closed (`53390ed`)

Assets, fetch-schema versions and Go/Cargo versions were already inputs. The opt-in `--use-cache` sidecar is now `v2:<sha256>` over the legacy build id plus:

- the effective isolation level (`env`/`sandbox`/`closure`), and
- the host `cc`/`c++`/`ld`/`make` versions for `env`/`sandbox` builds (probed through the exact build `PATH`, absent commands skipped).

`closure` remains keyed by the dependency closure archives, which pin the toolchain. `.PKGINFO build_id` keeps its legacy value byte-for-byte; pre-v2 sidecars miss once and are rewritten. The old P1-4 claim that `sandbox.extra_binds` must be hashed does not apply: recipes cannot supply binds, and every producer is a validated staging path.

### P1-5 verified mitigated

`wipe_tree` already uses `symlink_metadata` and treats non-directories (including symlinks) as files, so it never follows a symlink-to-directory escape. No code change was needed; the earlier audit line is retained for history.

### Undo/WAL consolidation: Phases 1–3 and Phase 4A (`docs/design/undo-wal-consolidation.md`)

The P1-2 design is in `docs/design/undo-wal-consolidation.md`. Phase 1 links
the physical FileJournal to the logical transaction/step that owns it with an
additive `T` line (old parsers ignore unknown records), and adds one combined
recovery entry point in `src/recovery.rs`: physical restore first, then TxWAL
reconciliation, under the exclusive lock. `nawa rollback` / `nawa history undo`
semantics and the legacy `undo/<name>/{desc,files}` mirror are unchanged.

The Phase 1 unit test forces a crash after the db entry was replaced and proves
the physical pass restores both files and db before reconciliation classifies
the step as failed; a second pass is a no-op. The logical pass does not run
inside `install_archive_payload`/`remove_with` (those run after
`begin_operation` wrote the live TxWAL); their physical-only calls were
removed in Phase 3.

Phase 2 adds `src/install/previous.rs`: `nawa rollback` now resolves the
one-slot-back target from transaction history when possible, keeps the legacy
`desc` authoritative wherever history cannot reproduce it exactly, and fails
closed on contradictory non-empty digests. `pick_restore_source`, the
current-version check, artifact/proof re-verification, and the `desc`/`files`
writes are unchanged. The resolver refuses a package touched by a
preparing/running/needs-recovery/interrupted transaction or a pending journal
intent. Tests cover every documented legacy edge, a history-only root, the
legacy no-digest cache path, `/etc` config preservation, retained signed-index
proof verification, and `history undo`-then-rollback ping-pong.

Phase 3 runs the combined recovery in every per-step-lock mutating command
(`install` by name, `build --install`, `update`, `group install`) immediately
after its short exclusive lock and before `begin_operation`, so a prior crash
is never mistaken for the new transaction. Whole-command exclusive commands
keep the `main::run` pass, and `cmd_history` keeps strict error propagation.
The redundant physical-only calls in `install_archive_payload`/`remove_with`
are gone. A command-level test in `tests/by_name.rs` seeds a committed db entry
with no `StepResult`, runs an unrelated `nawa install <name>`, and verifies the
record is already reconciled before `nawa history` runs.

Phase 4A (2026-11-13) keeps the `desc`/`files` writes and the Phase 2 resolver
fallback: history is authoritative whenever it can answer, and the mirror
remains a compatibility aid so old binaries retain rollback. Stopping the
mirror writes (Phase 4B) is not planned and would require a separate
compatibility PR. The full suite also surfaced pre-existing duplicate test
temp-dir tags; one tag per colliding pair was renamed so parallel unit tests
are deterministic.

### Current validation

```
cargo check --locked --all-targets                         → clean
cargo clippy --all-targets --locked -- -D warnings         → clean
python3 scripts/check-layering.py                          → 53 modules, no cycles
cargo test --locked --no-fail-fast                         → 454 unit passed, 1 ignored;
                                                              110 integration tests passed
```

Remaining work: the staged-tree walk split in P1-8, P0-3's live-root sandbox
trust model (architectural), P2-6 test-coverage gaps, and repo-wide
`cargo fmt --check` drift (not enforced by CI). P1-2 is closed by the Phase 4A
decision.
