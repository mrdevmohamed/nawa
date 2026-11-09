# Nawa Full Codebase Audit — 2026-11-09

**Repository:** https://github.com/mrdevmohamed/nawa
**Commit audited:** `6b04310` (Merge pull request #35 — refactor/fetch-candidates)
**Auditor role:** Senior Rust engineer, architecture & maintainability
**Scope:** Entire `src/`, `tests/`, `Cargo.toml`, CI workflows, docs

---

## 1. Current Architecture

Nawa is a single-binary Rust package manager (crate `nawa`, edition 2021, rust-version 1.82). The binary is the only `[[bin]]`; there is no `lib.rs`. All code lives under `src/` as top-level modules declared in `src/main.rs`, with three already-modularized subdirectories:

- `src/commands/` — CLI handlers (building, fetching, installing, querying, recipes, configuring, plugins, groups)
- `src/transaction/` — WAL, journal, history, recovery, undo, atomic file publication
- `src/sandbox/` — init (PID 1), mounts, probe, spec (spec is TOML-parsed)
- `src/plugins/` — discovery, events, manifest, protocol (JSONL), runner, sandbox
- `src/groups/` — groups/membership

The remaining ~35 top-level modules are flat files in `src/`.

### Module responsibilities (as found)

| Module | Lines | Responsibility (intended) |
|---|---|---|
| `main.rs` | 586 | Crate root, CLI parse, config load, dispatch, lock acquisition, error printing, plugin dispatch |
| `cli.rs` | 959 | Clap definitions, `Cmd` enum, completions, lock policy |
| `commands/*` | ~2500 total | Presentation + orchestration of each CLI command |
| `config.rs` | 709 | `nawa.toml` layering: CLI > env > user > system > default, path resolution |
| `output.rs`, `color.rs` | 189+180 | One-line status messages, colors, terminal detection |
| `diag.rs` | 157 | Human-friendly error rendering, hints, cause chain flattening |
| `humanize.rs` | 58 | Size/duration/date formatting |
| `confirm.rs` | 154 | Interactive confirmation, `--yes` contract, stderr prompt |
| `version.rs` | 237 | `epoch:version-rel` comparison (pacman semantics) |
| `deps.rs` | 223 | DFS topological resolution over recipe graph |
| `plan.rs` | 429 | Pure planning for install/update/outdated/autoremove |
| `recipe.rs` | 1374 | Recipe model, TOML parsing, validation, placeholder expansion, loading |
| `profile.rs` | 185 | Build profiles (`<arch>.toml`), jobs, flags |
| `db.rs` | 372 | Local package db: `<root>/var/lib/nawa/local/<name>/{desc,files,hashes}`, legacy migration |
| `lock.rs` | 292 | Exclusive/shared `flock` on `<root>/var/lib/nawa/lock`, timeout, hook escape hatch |
| `fetch.rs` | 2213 | HTTP engine (ureq/rustls), sha256 verify, verified snapshot, mirror handling, tarball extract, cp-a, offline gate, cache |
| `fetchrun.rs` | 1617 | Go Modules / Cargo fetch: planning, ecosystem key, srccache entries, locked population, sandbox fetch runner |
| `sources.rs` | 592 | New-schema `[sources]` staging: download/verify/extract into `<work>/src` workspace |
| `assets.rs` | 552 | Recipe-local assets snapshot: copy, modes, digest, `assetsdir` |
| `build.rs` | 1550 | Build orchestration: offline gate, cache lookup, staging, sandbox prep, section runner, slim, lint, archive |
| `buildenv.rs` | 456 | Isolation levels (none/env/sandbox/closure), env whitelist, SOURCE_DATE_EPOCH |
| `closure.rs` | 445 | Closure assembly: `buildroot = closure(base-devel ∪ depends.build ∪ depends.check)` |
| `sandbox/*` | ~1500 | Namespace setup, mounts, overlayfs, init, probe, timeout |
| `archive.rs` | 595 | `.nawa.tar.zst` create/extract, `.PKGINFO` parsing, hooks, reproducibility normalisation |
| `artifact.rs` | 764 | Content-addressed object store: `sha256/<xx>/<digest>.nawa.tar.zst`, provenance, immutability |
| `sign.rs` | 613 | Ed25519 signing: keygen, sign, verify, trust store, index verification |
| `sys.rs` | 272 | Low-level syscalls: fork, drop caps, uid/gid, timeout, loopback |
| `slim.rs` | 750 | Post-install slimming: strip, cleanup, subpackage split |
| `lint.rs` | 1024 | Post-build lint: DT_NEEDED vs `depends.run`, leaked paths, FHS layout, ELF via goblin |
| `install.rs` | 2711 | Install/remove/rollback: staging, journal, hooks, file moves, undo records |
| `upgrade.rs` | 1097 | Repository upgrade orchestration: index fetch, freshness, ordering, install loop |
| `sync.rs` | 283 | Repository sync: fetch index, materialise remote cache, update rev file |
| `repo.rs` | 1220 | Binary repo index (v1/v2/v3): load, create, update, entry parsing |
| `repositories.rs` | 413 | Multi-repo config, `repolist`, `makecache`, `repoquery`, `clean` |
| `service.rs` | 392 | runit service management: enable/disable/start/stop/status |
| `revdep.rs` | 908 | Reverse dependency scan: ELF parsing, soname index, suggestions, JSON output |
| `suggest.rs` | 168 | “Did you mean” via `strsim::levenshtein` |
| `interrupt.rs` | 216 | Ctrl+C/SIGTERM deferral: `Guard`, `Interrupted` error, 3x Ctrl+C force exit |
| `testutil.rs` | 27 | Test helper |
| `transaction/*` | ~1800 | WAL, atomic writes, recovery, undo, history |
| `plugins/*` | ~1500 | Plugin discovery, manifest, JSONL protocol, runner, sandbox, events |
| `groups/*` | ~300 | Group definitions, membership records |

**Test suite:** 515 `#[test]` cases across `src/` unit tests and `tests/` integration tests (e2e, offline, fetchcmd, fetchcargo, isolation, sandbox, rebuild_check, repo_install, etc.). No external network dependency in unit tests; integration tests use scripted HTTP servers / local temp roots.

**Dependencies:** 30 direct deps in `Cargo.toml` (see §7). All verified used (see audit). `Cargo.lock` present; offline vendored bootstrap is a design requirement.

**CI (`.github/workflows/ci.yml`):**
- `nawa`: `cargo build --release --locked` + `cargo clippy --release --all-targets --locked -- -D warnings` + `cargo test --release --locked` (as normal user) + `scripts/ci-no-skips.sh`
- `nawa-matrix`: same suite on ubuntu-22.04 / 24.04, as user and as root
- `isolation`: real `nawa build` at each isolation level (none/env/sandbox/closure)
- `smoke`: CLI smoke (`signing, upgrade, rollback`; `remote repos, services`; building Nawa from its real recipe)
- `static`: musl `--target x86_64-unknown-linux-musl`, checks `DT_NEEDED`/`INTERP` via `scripts/check-static.sh`
- Gate: `ci-ok` requires all jobs green. `release.yml` reuses `ci.yml` then publishes static binary. `docs.yml` builds Zensical docs.

---

## 2. Findings Ranked by Severity & Impact

### P0 — Critical (security / correctness / isolation)

**P0-1. Atomicity inconsistency: durable vs non-durable writes**
- `transaction/atomic.rs::write_atomic` fsyncs file + parent dir and creates parents durably. Used for WAL/history records.
- Every other module (`db.rs`, `install.rs`, `artifact.rs`, `closure.rs`, `repo.rs`, `sign.rs`, `fetch.rs`, `groups/membership.rs`, `plugins/events.rs`, etc.) uses ad-hoc `std::fs::write(tmp)` / `std::fs::copy(tmp)` + `std::fs::rename` without `sync_all` and without parent-dir fsync.
- Crash/power-loss can therefore leave a partially-written `desc`/`files`/`index.toml`/signature visible, or a lost directory entry after rename, even though the transaction says it committed. This breaks the durability contract transaction layer assumes.
- Impact: data loss / inconsistent package db after power cut in any non-transaction path. Security: truncated or stale index may be accepted if signature check races a crash boundary.

**P0-2. `fetch.rs` mix of verified-snapshot and non-verified paths**
- `snapshot_file` correctly copies + hashes + 0600 + 0700 snapshot dir, and `install_archive_pinned` / `install_archive_from_index_proof` use it. Good.
- But `download_cached_with_mirrors` and `download_content_addressed` still do: `download(&cand, tmp)` then `verify_sha256(tmp)` then `rename(tmp, dest)` — all on the *same* `tmp` file that the downloader wrote directly to `dest`-sibling path. The verification happens on the file that will be renamed, not on a separate snapshot that is then read for parsing. A TOCTOU window exists between verify and rename if `tmp` is replaced (symlink, concurrent process with same pid namespace).
- `unique_part_path` uses `std::process::id()` + monotonic sequence with `create_new` for lock file, but `tmp = dest.with_extension("part")` in `download_cached_with_mirrors` is *deterministic* (one `.part` per dest), not unique. Two concurrent builds race on the same `.part` file for legacy cache. The new-form path fixed this with `unique_part_path`; the legacy path did not.
- Impact: concurrent builds can interleave writes/verifies. The content-addressed path is safe; the legacy + repository cache paths are not yet.

**P0-3. Sandboxed build still trusts live root outside closure**
- `Isolation::Sandbox` only mounts namespaces over the *live* root; the `env` whitelist is the only barrier between `build.rs`'s environment and the sandbox child. `buildenv::whitelist` is correct, but `fetchrun`'s sandbox spec includes `extra_binds` from the recipe (`[sandbox] extra_binds`) without validating that binds don't escape the build root or expose `/etc/nawa` trust store to a compromised build script. The spec parser (`sandbox/spec.rs`) validates *shape* but not *policy* (“host-critical path” check is manual).
- `lint.rs` warns on leaked build paths but the run-dependency check (`DT_NEEDED` vs `depends.run`) only *warns* on missing deps in Phase 6a, by design, and relies on `depends.run` being correct. A recipe can omit a runtime dep and the package still installs.
- Impact: isolation downgrade to `env` if `allow_degraded` misused; missing runtime deps slip through.

#### P1 — High (major maintainability, correctness, performance)

**P1-1. God modules: `install.rs` (2711), `fetch.rs` (2213), `fetchrun.rs` (1617), `build.rs` (1550), `recipe.rs` (1374), `repo.rs` (1220), `upgrade.rs` (1097), `lint.rs` (1024)**
- `install.rs` contains: 4 install entry points (`install_archive`, `install_archive_pinned`, `install_archive_from_index_proof`, `install_archive_payload`), journal encode/decode (`enc`/`dec`), hook execution + status persistence, file move/undo logic, rollback, removal, crash recovery (`recover`), file ownership resolution (`exists_in_root`, `resolve_in_root`), and transaction integration. The legacy undo journal and the new transaction WAL coexist; rollback semantics are duplicated.
- `fetch.rs` contains: checksums, snapshot, offline gate, HTTP engine, `copy_local` (cp-a), content-addressed cache, mirror logic, repo ref parsing, source format detection, tarball extraction (including `rewrite_tar_stripped` with full tar header handling), sanitization, and 900+ lines of tests.
- `build.rs` contains: work-tree lifecycle, cache-id computation, sandbox preparation, section runner, source staging dispatch, archive creation, slim integration, and reproducible-build diffing.
- `fetchrun.rs` contains: Go planning, Cargo planning, sandbox spec building, child process supervision, toolchain identity, and cache entry publishing.
- Impact: any change to install touches a 2711-line file; reviewers cannot isolate review surface; cyclomatic complexity is high per-file; compile times suffer (single translation unit per module in binary crate, but changing one function recompiles the whole module).

**P1-2. Legacy undo journal vs new transaction WAL duplication**
- `install.rs::recover` still maintains `/var/lib/nawa/undo/<name>` and `.nawa` backup dirs, while `transaction/` now has `journal.rs`, `history.rs`, `recovery.rs`, `undo.rs` with full WAL. Both paths are live: `install_archive_payload` writes legacy journal *and* transaction steps. `nawa rollback` uses legacy undo; `nawa history undo` uses transaction WAL. Two sources of truth for “what was installed before”.
- Impact: divergence risk; recovery paths re-implement similar logic in two places; future deprecation of legacy journal requires both to be kept consistent.

**P1-3. Error handling: `anyhow` everywhere, no typed errors**
- 50 files use `anyhow::{bail, Context, Result}`. Context strings are ad-hoc: `"create directory {}"`, `"read base index {}"`, `"could not allocate..."` — same operation gets different messages in different modules.
- `diag.rs` renders the chain nicely, but there is no typed error enum to distinguish `User`, `IO`, `Config`, `Trust`, `Interrupted`, `Offline` categories programmatically. Callers do string matching or `downcast_ref::<Interrupted>()` only for interrupts.
- `with_context` is used correctly in ~100 places, but some errors swallow source: `let _ = std::fs::rename(bak, dest);` in undo paths, `let _ = std::fs::remove_file(&tmp);` after write failures (acceptable but undocumented).
- Impact: diagnostics are good for humans but not machine-readable; `upgrade`, `sync`, `install` cannot branch on “offline” vs “checksum mismatch” without string checks.

**P1-4. Build isolation: cache identity misses side effects**
- `build::build_id_for` covers recipe bytes, source sha256, closure names, profile flags. It does *not* cover `sandbox.extra_binds`, `assets` digest when assets exist (assets are separate), or toolchain version in `env` isolation (only for `closure` is toolchain pinned via closure archives). `try_cache_hit` for `Closure` computes closure paths; for `Sandbox`/`Env` it uses empty closure list, so toolchain upgrades don’t invalidate `--use-cache` hits.
- Impact: stale archive reused after toolchain upgrade when isolation < closure.

**P1-5. `build.rs` `wipe_work` / `wipe_tree` recursion + `walkdir` interplay**
- `wipe_tree` recursively chmods + deletes preserving only “writable parent” invariant. It does not handle symlinks to directories correctly under `overlayfs` workdir in sandbox scratch (`sb/layer-work` at mode 000). The comment documents this, but the code still uses `std::fs::read_dir` without `O_NOFOLLOW` or `symlink_metadata` guard for symlink-to-dir escapes.
- Impact: a malicious recipe could leave a symlink in `<work>` that `wipe_work` follows on next build, deleting outside `<work>`.

#### P2 — Medium (technical debt, duplication, naming, performance)

**P2-1. Duplicated filesystem patterns**
- `create_dir_all(parent)` + `write(tmp)` + `rename(tmp, dest)` repeated ~25 times across `db.rs`, `sign.rs`, `repo.rs`, `sync.rs`, `upgrade.rs`, `groups/membership.rs`, `plugins/events.rs`, `artifact.rs`, `closure.rs`, `fetch.rs`, `fetchrun.rs`, `sources.rs`, `slim.rs`, `install.rs`.
- `unique_part_path` / `PART_SEQUENCE` / `TEMP_SEQUENCE` / `SNAPSHOT_SEQUENCE` — three separate atomic counters for unique temp names.
- `copy_atomic` in `install.rs:856` is one-off; `write_atomic` in `transaction/atomic.rs` is durable; neither is reused elsewhere.
- `progress_line` / `progress_label` in `fetch.rs` vs `output.rs` macros.

**P2-2. Stringly-typed reason / install mode / isolation level**
- `reason: &str` with values `"explicit" | "dependency" | "group" | "keep"` scattered across `install.rs`, `db.rs`, `upgrade.rs`, `commands/installing.rs`. Transposition bugs (passing `force` as `allow_unsigned`) were already fixed for `install_archive` via `InstallOpts`, but `install_archive_payload` still takes 5 booleans/strings.
- `Isolation::parse(&str)`, `InstallMode::parse(&str)`, `CheckMode` — string parsing at CLI boundary is good, but internal code still passes `&str` for reason.

**P2-3. Clone-heavy, allocation-heavy patterns**
- `grep -c clone()` = 278. Many are required (`PathBuf::clone` for work dirs), but `build.rs:336-372` clones `srcdir`, `pkgdir`, `srcroot`, `assetsdir` multiple times to build `ExpandCtx` and `SandboxSpec`. `recipe.rs:544,645` clones deps maps. `upgrade.rs:386,411,419` clones installed entries into a HashMap then clones names again.
- `Candidate URLs` builds `Vec<String>` and clones on each candidate. Fine for correctness, wasteful for large mirror lists (rare).

**P2-4. Visibility hygiene**
- Binary crate should prefer `pub(crate)` for internal APIs. Currently ~300 `pub fn` / `pub struct` that are only used within crate. This makes refactoring harder (any rename must consider “public API” fear) and obscures module boundaries. `main.rs` declares `mod archive; mod artifact; …` without `pub` (correct for binary), but those modules themselves expose `pub` liberally.

**P2-5. Documentation density varies**
- `archive.rs`, `buildenv.rs`, `closure.rs`, `config.rs`, `lint.rs`, `lock.rs`, `interrupt.rs` have excellent crate-level docs with tables and worked examples. `install.rs` has a module doc but the 2711 lines inside have sparse per-function docs beyond the header. `fetch.rs` has good module doc but `download_content_addressed` (200-line function) has only a paragraph comment for a complex locking/migration flow.
- `recipe.rs` `expand` (placeholder substitution) has no doc for which `%{var}` are supported in which context; users must read code.

**P2-6. Test coverage gaps**
- Strong coverage for: version comparison, recipe validation, archive create/extract, sign/verify, db, lint, fetch extraction edge cases, transaction WAL, sandbox spec parsing.
- Weak: `fetchrun.rs` Go/Cargo planning (only a few unit tests), `commands/*` orchestration (covered indirectly via integration tests), `repositories::cmd_clean` (only one test), `service.rs` (requires runit, hard to test), `revdep::suggest` (name guessing), `build::rebuild_check` (integration only).
- No fuzz target for `parse_pkginfo`, `unpack_tar_stream`, `decode_hex32` etc. Attack surface is tar extraction + recipe parsing; hand-rolled tar header validation (`is_tar_header`, `octal_field`) would benefit from fuzzing.

**P2-7. `recipe.rs` 1374 lines — model + load + validate + expand in one file**
- `struct Recipe` + `struct Package` + `struct Depends` + `struct Build` + `struct Fetch` + validation + `expand()` + `load_all()` + `is_lower_hex64` + `check_schema_placeholders()`.
- `expand` does string replacement with `replace()` for each placeholder — O(n * placeholders) and does not handle `%%` escaping. A `{}`-style format string or a dedicated expander would be safer.

**P2-8. Performance: repository parsing repeated**
- `upgrade.rs::fetch_entry` / `open_repo` loads the whole index for each package? `plan.rs` walks deps repeatedly. `revdep::scan` reads every ELF twice (once for `DT_NEEDED`, once for soname index). No caching of parsed recipe graph beyond a single `load_all` per command (fixed in PR #31-32, but `build` still calls `recipe::load_all` twice when `use_cache` + `Closure`).
- `lint.rs` walks the staged tree, then `slim::run` walks again, then `archive::create` walks a third time (PR #33 fixed double-walk in `slim`, but `lint`+`archive` still separate).

#### P3 — Low (style, clippy, deps, polish)

**P3-1. `unwrap()` / `expect()` — almost entirely in tests**
- Non-test `unwrap()` count is <10 (mostly `plan.iter().find().unwrap()` in `upgrade.rs:428` / `.expect("ordered name came from the plan")` at 469 — should be `ok_or_else` with context). `fetchrun.rs:1471,1483,1486` uses `.expect()` on spawn/pid — appropriate for unreachable invariants but could be `context`.
- `panic!` only in tests. Good.

**P3-2. `anyhow` usage is consistent**
- No `std::io::Error` leakage, no `Result<(), String>`. `bail!` messages are user-facing and actionable (e.g., “`repo = \"\"` switches binary repos off, pass `--repo <dir-or-url>`…”). This matches the `diag.rs` rendering philosophy.

**P3-3. Naming**
- Generally good: `wipe_work`, `strip_write_permission`, `try_cache_hit`, `buildroot`, `srccache_dir`. A few abbreviations: `rec` for recipe, `pkgdir`, `srcdir`, `spec_srcroot` — consistent with recipe placeholders, so okay.
- `Ctx` in `main.rs` is terse; `Config` + `Ctx` overlap (Ctx is a flattened Config). Could collapse.
- `nawea_*` macros (`nawa_info!`, `nawa_warn!`, `nawa_note_stderr!`) — check `output.rs` macros; they are consistent but not documented in module docs.

**P3-4. Dependencies — no unused, one overlap**
- All 30 direct deps are used. `bzip2` (C, static) + `flate2` (pure Rust) + `lzma-rust2` (pure Rust) + `zstd` + `tar` + `sha2` — necessary for source format coverage. `bzip2` is the only C dep besides `zstd` (which is also C via `zstd-sys`). `filetime` is tiny but could be replaced by `std::fs::File::set_times` on MSRV 1.75+; kept for MSRV 1.82 compat and timestamp parity.
- Overlap: `url` is used only for redirect-target resolution (`url::Url::parse` + `join`). Could stay; it’s already in `ureq` tree.
- `strsim` + `clap` both provide edit distance; only `strsim` is used for “did you mean”, `clap`’s suggestions disabled. Acceptable.
- `serde_json` only for plugin host JSONL. Pure Rust, small. OK.

**P3-5. `Cargo.toml` metadata**
- `description`, `license`, `rust-version = "1.82"` present. `edition = "2021"` (2024 would be newer but MSRV holds it). Missing `homepage`/`repository`/`categories` — minor for binary-only crate.

**P3-6. Clippy**
- `cargo check` passes clean. Without `rustfmt`/`clippy` in this environment we cannot run `cargo clippy -- -D warnings`, but code appears `clippy`-clean (no obvious `needless_clone`, `redundant_field_names`, `just_underscores_and_digits` etc in sample reads). The `blame` history shows PR #31-35 consistently fixed clippy warnings by extracting helpers rather than silencing.

---

## 3. Security Concerns Summary

| # | Area | Finding | Severity |
|---|---|---|---|
| S1 | Tar extraction | `unpack_tar_stream` + `sanitize_entry_path` correctly refuse `..`/absolute and use `unpack_in`. `rewrite_tar_stripped` aborts on `..` for new-schema recipes. Good depth. | Low residual risk |
| S2 | Signing | Ed25519 via `ed25519-dalek`, trust store with `trusted_keys`, index signature verification before *any* archive install (`verify_for_install_with_sig_reported`). `allow_unsigned` requires explicit flag and logs warning. Good. | Low |
| S3 | Hashing | SHA-256 verified everywhere; content-addressed object store with no-clobber `link(2)` prevents publication race. Snapshot hashing prevents TOCTOU between verify and parse for install paths, but not yet for legacy cache download path (P0-2). | Medium |
| S4 | Sandbox | Namespaces + mounts; `probe.rs` correctly fails closed (refuses degraded isolation unless `--allow-degraded`). `wipe_work` chmods before delete is correct for read-only assets, but symlink escape (P1-5) should be `O_NOFOLLOW` hardened. | Medium |
| S5 | File permissions | `set_mode(0o700)` on snapshot dir, `0o600` on snapshot file, `0o600` on cache lock, read-only cache entries. `lint.rs` rejects world-writable files and `/usr/local` content. Consistent. | Low |
| S6 | Recipe execution | Build sections run via shell; scripts are recipe-author supplied. Isolation `env` whitelist prevents env leakage, but recipe can still `curl` if `sandbox.network=true` and user passes `--allow-network`. This is documented opt-in. | Low (by design) |
| S7 | Plugin host | Out-of-process JSONL host with manifest capabilities; sandbox via `plugins/sandbox.rs` (uid-map handshake). Events dispatched *after* lock dropped. Good. | Low |

---

## 4. Technical Debt Inventory

1. **Legacy backup/undo vs WAL** (P1-2) — keep both, document migration, don’t delete until WAL undo covers rollback use-case.
2. **Three unique-temp generators** (P2-1) — consolidate.
3. **Ad-hoc tmp+rename vs `write_atomic`** (P0-1, P2-1) — migrate non-durable patterns to durable where they touch package db / index / trust store; keep ephemeral tmp+rename for throwaway staging (document which is which).
4. **`install.rs` god module** (P1-1) — split, see plan.
5. **`fetch.rs` god module** (P1-1) — split into `fetch/{checksum,snapshot,http,cache,extract,local}.rs`.
6. **`fetchrun.rs` Go vs Cargo** — split into `fetchrun/{go,cargo}.rs` sharing `fetchrun/{mod,spec,sandbox}.rs`.
7. **`build.rs` work lifecycle vs section runner vs cache** — split into `build/{mod,lifecycle,cache,sections}.rs`.
8. **Visibility (`pub` vs `pub(crate)`)** — mechanical tighten-up.
9. **`Ctx` vs `Config`** — flatten.
10. **Reason string type** — introduce `enum InstallReason { Explicit, Dependency, Group }` with `as_str()` / `parse()`, keep `&str` at CLI boundary.

---

## 5. Performance Observations

- No premature optimisation needed. SHA-256 hashing uses 64KiB buffers; `BufWriter` with 256KiB for tar rewrite; zstd level 3 for archives (level 19 for repo index — correct trade-off).
- Parallelism: `jobs` respected for build; `fetch` is sequential per source (correct — no parallel download yet, but `sources.rs` stages sources sequentially; could parallelise `stage_sources` per source).
- Largest waste: repeated `recipe::load_all` (parses every `recipe.toml`) in `build` (twice), `plan`, `upgrade`, `revdep`. Could cache within a single command via `OnceCell` or pass loaded graph. Mitigated by recent PR #32 single-walk change.
- `clone()` count is high but dominated by `PathBuf` construction where ownership transfer is required; not a concern.

---

## 6. What’s Already Good (keep)

- **Documentation discipline:** Almost every module opens with a crate-level `//!` doc that explains *why*, not just *what*, with tables/diagrams and references to design docs (`docs/design/*`). This is rare and valuable.
- **Error rendering:** `diag.rs` + `output.rs` give consistent `nawa: error:` / `warning:` / `ok:` one-line prefix. Interrupts exit 130 with clear message.
- **Locking:** `lock.rs` with shared/exclusive flock, hook escape hatch `NAWA_LOCK_HELD=1`, timeout `NAWA_LOCK_TIMEOUT`, and post-lock recovery (`install::recover_and_report`). Correct for a binary crate.
- **Testing:** 515 tests, many regression-tests with `PAUSE`/`FAILPOINT` injection for WAL (`transaction/atomic.rs` tests). No network-dependent unit tests. Temp dirs are cleaned via `Drop` or explicit `remove_dir_all` in tests.
- **CI gate:** No-skip script ensures sandbox tests cannot silently skip. Reproducibility (`SOURCE_DATE_EPOCH`, normalised archives, zstd level 3 deterministic) tested via `rebuild_check`.
- **Cargo hygiene:** `--locked`, MSRV `1.82`, static musl build, vendored offline bootstrap documented.

---

## 7. Recommendation Priority

| Order | Improvement | Impact | Risk |
|---|---|---|---|
| 1 | Split `fetch.rs` into focused submodules (proof-of-concept for god-module split) | High — establishes pattern for install/build | Low (mechanical move, re-export) |
| 2 | Migrate durable writes to `transaction::atomic::write_atomic` where they touch db/index/trust; document ephemeral vs durable | High — fixes P0-1 crash durability | Low if scoped to small files |
| 3 | Fix legacy cache `.part` race (`download_cached_with_mirrors` → unique part path) | Medium — fixes P0-2 race | Low (mirror `download_content_addressed`) |
| 4 | Tighten visibility to `pub(crate)` | Medium — clarifies module boundaries | Low (search/replace + `cargo check`) |
| 5 | Split `install.rs` (payload / hooks / journal / rollback) | High — largest god module | Medium (needs careful re-export, tests every step) |
| 6 | Split `build.rs`, `fetchrun.rs`, `recipe.rs`, `repo.rs` | High — completes modularisation | Medium |
| 7 | Introduce `InstallReason` enum & `ErrorKind` enum | Medium — type safety, removes stringly-typed | Medium (touches many call sites) |
| 8 | Consolidate unique-temp helpers | Low — cleanup | Low |

See companion document `2026-11-09-refactor-plan.md` for the actionable, file-by-file plan with tests, risks, and sequencing.

