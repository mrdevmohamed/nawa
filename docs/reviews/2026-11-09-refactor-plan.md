# Nawa Refactoring Plan — Incremental, Low-Risk First

**Companion to:** `2026-11-09-nawa-audit.md`
**Date:** 2026-11-09

> **Status update (2026-11-12):** Steps 0-7 were implemented in PRs #36-#42. Step 8 is partial (recipe cache, parallel source staging and unique-temp consolidation landed). The post-roadmap security/cache follow-ups are recorded in `2026-11-09-final-report.md` §7.

**Principles:**
- Backward compatible: `nawa sync/update/upgrade/install/remove/fetch/build/revdep/service` CLI semantics unchanged.
- Small, reviewable diffs (rule 10). One logical concern per PR.
- Every migration preserves public paths via `pub use` re-exports; breaking change requires explicit justification.
- `cargo check --all-targets` + `cargo test` after each step; no regression allowed to block next step.

---

## Step 0 — Foundations (no code yet)

| Task | Files | Benefit | Risk | Tests |
|---|---|---|---|---|
| 0a. Create this plan + audit docs | `docs/reviews/2026-11-09-*.md` | Record decisions; prevent future “why was this split” questions | None | n/a |
| 0b. Verify `cargo fmt --check` unavailable in this env; document | — | Honest validation reporting | None | `cargo check --all-targets` passes; list fmt/clippy as gap |

---

## Step 1 — Fix P0-2 legacy `.part` race (small, targeted fix)

**Problem:** `fetch::download_cached_with_mirrors` uses deterministic `dest.with_extension("part")` — concurrent builds race on same `.part` path. New-form path already fixed via `unique_part_path`.
**Proposal:**
- Reuse `unique_part_path` (already `static PART_SEQUENCE` in same file) for legacy path. Rename helper to `unique_part_path_for(dest)` and make it `pub(crate)` so both paths share it.
- Keep `.part` suffix semantics; uniqueness comes from pid+sequence, not from deterministic name.
**Files:** `src/fetch.rs` (lines ~595-607, ~844-870)
**Risk:** Low — same semantics, only name uniqueness changes. Existing cache entries are `dest` itself, not `.part`; no migration needed.
**Tests:** Existing `download_cached_falls_back_to_recipe_mirror` + `gnu_urls_get_fallbacks` + `recipe_mirrors_are_appended_without_duplicates` already cover. Add concurrency test: two threads call `download_cached_with_mirrors` on same dest with scripted servers; assert both succeed and final digest matches.
**Validation:** `cargo test fetch` (integration) + `cargo check --all-targets`.

---

## Step 2 — Split `fetch.rs` into focused submodules (proof-of-concept god-module split)

**Problem:** `fetch.rs` 2213 lines mixes 7 responsibilities (P1-1).
**Proposal:** Create `src/fetch/` directory with `mod.rs` re-exporting; keep `src/fetch.rs` deleted.

Target layout (all former `pub fn` paths preserved via `pub use`):

```
src/fetch/
  mod.rs       — module docs, offline gate, re-exports, shared consts
  checksum.rs  — sha256_of_file, verify_sha256
  snapshot.rs  — VerifiedSnapshot, snapshot_file, remove_stale_snapshots
  http.rs      — Agent, parse_http_url, transient_status, http_get, redirect_target,
                progress_line/label, stream_body, single_transfer, fetch_to_file,
                download, download_opt, Outcome, check_offline
  cache.rs     — digest_cache_dir, CacheLock, lock_digest_entry, unique_part_path,
                strip_write_permission, download_content_addressed, RepoRef,
                remote_cache_dir, candidate_urls*, try_candidates,
                download_cached*, remote cache key
  local.rs     — copy_local, preserve_owner, copy_file_times
  extract.rs   — SourceFormat, detect_source_format, read_up_to, looks_like_tar,
                is_tar_header, octal_field, decompressed_reader,
                extract_source_archive*, rewrite_tar_stripped, unpack_tar_stream,
                sanitize_entry_path + tests
```

All internal `fn` that are not `pub` stay private. The `mod.rs` re-exports keep `crate::fetch::sha256_of_file`, `crate::fetch::snapshot_file`, `crate::fetch::download`, etc working.

**Files affected:** delete `src/fetch.rs`, create `src/fetch/{mod,checksum,snapshot,http,cache,local,extract}.rs`. Update `src/main.rs` (`mod fetch;` becomes `mod fetch;` still works because directory layout resolves). No other call sites changed.

**Steps:**
1. Create directory `src/fetch`, move file content in chunks via `git mv`-equivalent (copy then delete original).
2. Split with `read_file`/`patch` to keep line numbers accurate; preserve all `#[cfg(test)]` modules (move `tests` inline module with the functions they test).
3. Ensure `Cargo.toml` unchanged; `mod fetch` resolution automatically switches to directory.
4. Run `cargo check --all-targets`, `cargo test --lib` (unit), `cargo test --test fetch*` + `sources` + `offline`.

**Risk:** Low — mechanical move. Only risk is missing a `use` import in a split submodule (e.g., `hex`, `sha2`, `ureq` needed in multiple files). Mitigate by ordering: `cargo check` after each chunk.
**Expected benefit:** Each file 200-450 lines; reviewers can review HTTP vs cache vs extract separately; future splits (install, build) follow same pattern.
**Why this first:** `fetch.rs` is self-contained (depends mostly on `std`, `anyhow`, `sha2`, `ureq`, `tar`, `flate2`, `bzip2`, `zstd`, `lzma-rust2`), has extensive unit + integration tests, and validates the directory-module pattern that `transaction/`, `sandbox/`, `plugins/` already use.

---

## Step 3 — Durable atomic writes (P0-1, scoped)

**Problem:** ad-hoc `write(tmp) + rename` without fsync for db/index/trust files (P0-1).
**Proposal (incremental, not big-bang):**
- Document policy in `transaction/atomic.rs` header: which files need durability vs which are ephemeral.
- Migrate *small, critical* files first: `db.rs` `desc`/`files`/`hashes` (currently `std::fs::rename(&tmp, &desc)` at lines 95, 231-240), `sign.rs` `sig` file (155-156), `repo.rs` index write (480, 761), `groups/membership.rs` (66), `plugins/events.rs` (80, 347).
- Keep ephemeral `tmp`+`rename` for: staging dirs, `.part` downloads, per-build `<work>` trees (not crash-atomic by design).
- Each migration is `write_atomic(path, bytes, mode)` instead of manual tmp+rename; mode preserved (0o600 for keys, 0o644 for index).
- Keep API identical: no signature change for callers, only internal implementation.

**Files (one PR each to keep diffs small):**
- PR-3a: `db.rs` (5 rename sites)
- PR-3b: `sign.rs` + `repo.rs` (3 sites)
- PR-3c: `groups/membership.rs` + `plugins/events.rs` (3 sites)

**Risk:** Low — `write_atomic` is already tested (`atomic.rs` tests). Mode/permission semantics must be preserved; verify `create_dir_all_durable` matches prior `create_dir_all` behavior on existing dirs.
**Tests:** Existing `db`, `sign`, `repo` tests + `transaction/atomic` tests. Add test that `write_atomic` preserves mode 0o600 for key files (already covered by `write_atomic_durably_creates_missing_nested_parents`).

---

## Step 4 — Visibility tightening (`pub` → `pub(crate)`)

**Problem:** ~300 `pub` items in binary crate; hard to tell public API vs internal (P2-4).
**Proposal:**
- For each top-level module, change internal-only `pub fn/struct/enum` to `pub(crate)`.
- Only `pub use` re-exports that are genuinely used from other modules stay `pub` (in binary crate, `pub` and `pub(crate)` are both “crate-visible”, but `pub(crate)` signals intent).
- Use `grep -r "crate::fetch::"` etc to find cross-module uses; if only used within same module tree, `pub(crate)` is safe.
**Risk:** Low — `cargo check` catches any missed cross-crate use (there is no external crate, only `[[bin]]`, so `pub` vs `pub(crate)` has no ABI impact).
**Tests:** `cargo check --all-targets` + `cargo test`.

---

## Step 5 — Split `install.rs` (stretch, after Steps 2-4 proven)

**Problem:** `install.rs` 2711 lines (P1-1, P1-2).
**Proposal:**
```
src/install/
  mod.rs      — InstallAuth, InstallOpts, install_archive*, InstallAuth::kind,
                inspect_archive_state, re-exports
  payload.rs  — install_archive_payload, staging, temp guard, move_tree,
                is_under_etc, contents_differ, path_hash
  hooks.rs    — save_hooks, load_hooks, run_hook*, hook_failed, chroot_command,
                hook status persistence
  journal.rs  — enc/dec, Parsed, Step, copy_dir, undo_steps, journal_path,
                recover* (legacy undo journal)
  rollback.rs — rollback, report_restored
```
Keep `pub use` from `install/mod.rs` so `crate::install::install_archive`, `crate::install::rollback`, `crate::install::exists_in_root`, etc keep working.
**Risk:** Medium — `install.rs` is tightly coupled; hooks and payload share `Db`, `Entry`, `artifact` types. Mitigate by moving only pure helpers first (journal enc/dec), then hooks, then payload, running `cargo test` after each.
**Tests:** `cargo test install` + integration `repo_install`, `e2e`, `upgrade_regression`.

---

## Step 6 — Split remaining god modules

- **`build.rs` (1550):** → `build/{mod,lifecycle,cache,sections,sandbox}.rs` — lifecycle = `wipe_work`, cache = `build_id_for`/`try_cache_hit`, sections = `run_section`, sandbox prep.
- **`fetchrun.rs` (1617):** → `fetchrun/{mod,go,cargo,sandbox,toolchain}.rs`
- **`recipe.rs` (1374):** → `recipe/{mod,model,expand,validate,load}.rs` — model = structs, expand = placeholder substitution, validate = `check_*`, load = `load_all`
- **`repo.rs` (1220):** → `repo/{mod,index,create,update,entry}.rs`
- **`upgrade.rs` (1097):** → `upgrade/{mod,open_repo,fetch_entry,order}.rs`
- **`lint.rs` (1024):** → `lint/{mod,elf,layout,paths}.rs`

Each follows Step 2 pattern: mechanical split + `pub use`.

---

## Step 7 — Type safety & error kinds (optional, larger)

- **`InstallReason` enum:** `enum { Explicit, Dependency, Group }` with `as_str()`/`parse()`. Replace `&str` reason plumbing (`install_archive_payload`, `inspect_archive_state`). Keep `&str` at CLI boundary via conversion. Touches `install.rs`, `db.rs`, `plan.rs`, `commands/installing.rs`, `upgrade.rs`.
- **`ErrorKind` enum:** Lightweight classification (`Io`, `Config`, `Trust`, `Checksum`, `Offline`, `Interrupted`) returned via `anyhow::Error::downcast_ref` or a wrapper `NawaError`. `diag.rs` already does kind-based hints; formalising it lets `upgrade`/`sync` branch without string matching.
- **Placeholder expander:** Replace `replace("%{jobs}", …)` chain in `recipe::expand` with single-pass scanner handling `%%` escapes. Low-risk, formatter-level change.

---

## Step 8 — Performance (only with measurement)

- Parallelise `sources::stage_sources` per source (currently sequential). Requires thread pool (`std::thread::scope` to avoid new dep).
- Cache `recipe::load_all` per-command (e.g., `OnceCell<BTreeMap>` injected into `Ctx`).
- Dedupe `lint → slim → archive` directory walks: single walk producing `Vec<FileInfo>` reused.

No perf change without before/after `time cargo test` measurement and a regression test that asserts behaviour unchanged.

---

## Validation Protocol (every step)

1. `cargo check --all-targets` must pass (CI runs `cargo clippy --release --all-targets --locked -- -D warnings`; this env lacks `rustfmt`/`clippy` — document as gap).
2. `cargo test --release --locked` (or `cargo test` in dev) — all 515 tests green; `scripts/ci-no-skips.sh` for sandbox.
3. `bash scripts/check-static.sh target/.../nawa` if musl target available (not in this env).
4. `git diff --stat` review: no CLI, recipe format, archive format, index format, or signature logic changes.

---

## Out of Scope / Explicit Non-Goals

- No rewrite from scratch (rule 1).
- No dependency removal unless genuinely unused (currently none unused — verified in audit §7).
- No behaviour change to `nawa sync/update/upgrade/install/remove/fetch/build/revdep/service` without justification (rule 7). Any such change needs migration note.
- No new design patterns (no DI container, no trait abstraction for single implementation).
- No file splits “arbitrarily” — each split follows a responsibility boundary already documented in the module’s `//!` header.

