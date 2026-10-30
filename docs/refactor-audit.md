# Nawa professional refactoring — audit, baseline, plan

Started: 2026-10-30.
Status headers are updated as work proceeds; this file is written **before** any
code change so the baseline and intended changes are on record first.

## 0. Environment and baseline (measured, not assumed)

* rustc/cargo 1.99.1 (toolchain), host Linux x86_64, running as uid 1000.
  MSRV to preserve: 1.82 (rust-version in Cargo.toml), edition 2021.
* Targets installed locally: x86_64-unknown-linux-gnu.
  The static MUSL build is therefore validated on machines that have
  `musl-tools` (CI `static` job does this); locally `rustup target install
  x86_64-unknown-linux-musl` + `musl-gcc` are required to repeat it.
* Network access: available (crates.io API checked). All builds/tests below
  were run with `--offline --locked` against `vendor/` to prove the vendored
  workflow still holds.

### Baseline test run (before any modification)

```
cargo build --release --offline --locked      -> OK (1m58s)
cargo test  --release --offline --locked      ->
  lib (src)      148 passed, 0 failed, 1 ignored
  build_engine     4 passed
  src (second)     … (see full log below)
  by_name          7 passed
  e2e              9 passed
  isolation        4 passed
  repo_install     7 passed
  rootless         2 passed
  sandbox          8 passed
  upgrade_reg      1 passed
```
Total: **194 passed / 0 failed / 1 ignored**. The single ignored test is
`fetch::tests::real_world_tarballs_download_and_extract`
(`#[ignore = "requires network; run with: cargo test -- --ignored"]`) —
intentional, network-dependent; CI policy (`scripts/ci-no-skips.sh`) greps for
skipped isolation tests only, not cargo-level ignores.

## 1. Repository audit findings

### Module map (13325 lines total `src/`)

| module | lines | state |
|---|---|---|
| main.rs | 2081 | oversized: clap definition + completion strings + run dispatch + ~15 command handlers |
| sandbox.rs | 1428 | oversized; heterogeneous libc FFI inline; well documented but all in one file |
| fetch.rs | 1222 | HTTP engine + copy + tarball staging + tests, mostly cohesive but long |
| install.rs | 1133 | install + remove + rollback + hooks + etckeeper-style /etc merge |
| build.rs | 1032 | build engine: isolation selection, cache, recipe glue |
| lint.rs | 731 | hand-written ELF reader (~200 lines) + build id + layout checks |
| upgrade.rs | 727 | coherent (upgrade planning/execution) |
| config.rs | 644 | coherent (config layering) |
| archive.rs | 592 | archive create/extract with several security checks |
| sign.rs, closure.rs, plan.rs, recipe.rs, … | ≤472 | fine-grained, coherent |

### Hand-written implementations targeted for library replacement (mission §3)

1. **ELF reader in `lint.rs`** — a ~180-line `Cursor`/`u16/u32/u64` hand parser
   for PT_LOAD/PT_DYNAMIC/DT_NEEDED/DT_SONAME/DT_RPATH/DT_RUNPATH.
   Defects/risks found:
   * `Cursor::get()` indexes `data[off..off+len]` for the fixed header fields —
     malformed `e_phoff` can panic via out-of-range slice (the audit goal
     "malformed ELF must produce errors, never panics" is not yet literally
     true: e.g. `e_phoff = usize::MAX` → `p + phentsize` can overflow usize in
     debug; `c.u64(32)` on a 40-byte file panics before bounds are checked —
     the file length is only validated for the phdr table, not for the
     ehdr fields themselves).
   * `dyn_string` reads until NUL without a buffer cap beyond file end.
   * No ELF32 test coverage exists (only an ELF64 fixture).
   * Replacement: **goblin 0.9.x** (`elf32/elf64/endian_fd` features only —
     no mach/pe/arb pulls). goblin 0.9.3: MSRV 1.63, MIT; deps scroll 0.12
     (MSRV 1.63, MIT) and plain 0.2.3 (MIT/Apache-2.0). Compatible with
     MSRV 1.82 and the offline vendored build.
2. **`flock` FFI in `lock.rs`** — manual `extern "C" fn flock` + open-coded
   retry on `Interrupted`. Replacement: **rustix 1.x** `rustix::fs::flock`
   (MSRV 1.63, tri-license MIT/Apache-2.0/LLVM-exception). Same semantics:
   shared/exclusive/nonblocking, `WouldBlock` mapped to `Busy`.
3. **Levenshtein in `suggest.rs`** — hand-rolled. Replacement: **strsim**
   0.11.1 (MSRV 1.56, MIT, zero transitive deps). Keep Nawa's ranking
   (substring ⇒ distance 1, alphabetical tie-break, top-3); only the
   distance computation changes — verified offset by offset with the new
   tests in §3.
4. **`sandbox.rs` FFI** — libc usage: geteuid/getegid, unshare, fork,
   waitpid/read-status, mount suite incl. overlayfs/devpts/tmpfs, pivot_root
   with chroot fallback, chdir, open(O_PATH|O_DIRECTORY|O_NOFOLLOW), close,
   umask, setrlimit, prctl(NO_NEW_PRIVS), capset via SYS_capset, execve,
   alarm+signal handler, kill, socket(AF_INET/SIOCSIFFLAGS) for `lo` bringup,
   dup2/read/write/fcntl on the sync fd.
   Plan: consolidate every unavoidable `unsafe` call into one small,
   documented syscall layer (`src/sys.rs`) and switch the calls that rustix
   already covers (flock, mounts where API shapes fit, O_PATH opens, dup2 via
   std's `CommandExt`, etc.) to it. `unshare`, `fork`, `execve`, capset and
   the raw `SIOCSIFFLAGS` ioctl have no safe library equivalent we can adopt
   without new unsafe C deps; they stay, confined and commented.
5. **`archive.rs` extraction hardening** — current defences: path component
   validation (`safe_relative`), symlink-parent rejection, planted-symlink
   unlink, hard-link rejection. Gaps audited:
   * `reject_symlinked_parents` checks parent dirs with `symlink_metadata`
     *after* earlier entries may have planted them — validated per entry, so
     TOCTOU window exists between check and `entry.unpack`.
   * special file types (char/block devices, FIFOs) are not explicitly
     rejected; tar-rs `unpack` creates whatever it can for them.
   * Plan: **rustix `openat2` (RESOLVE_BENEATH)**-based extraction when the
     kernel supports it (≥5.6), with the existing checks as the portable
     fallback — defence in depth, no weakened rules. cap-std was evaluated and
     rejected: it would drag a sandboxing abstraction + async-free traits in,
     needs its own ABI touches to install, and cannot protect against escapes
     through directories created earlier in the same process (it only caps the
     process, which the build sandbox already does); openat2 gives per-op
     resolution guarantees instead.

### Specialized algorithms kept (mission §4) — verified by testing

* `version.rs` pacman-style vercmp: reimplementing with an external crate
  (`semver`-pace libs) cannot reproduce Arch semantics; kept, and extended
  with conformance tests (very large numeric components: libalpm saturates
  numerics at integer limits, ours maps them to `u64::MAX` — same ordering
  behaviour, documented in tests).
* `deps.rs` DFS + topological resolution: no version constraints exist in the
  recipe format, so PubGrub is not applicable (mission confirms; revisit only
  if constraint syntax lands).
* `fetch.rs copy_local` (`cp -a` parity through filetime), install.rs
  transaction/rollback (`Tx`), closure.rs layer assembly: kept, tests extended.

### Architecture findings (mission §5)

* `main.rs` mixes: clap definitions (~400 lines incl. two completion blobs),
  Ctx, lock policy, dispatch, and every command handler → split into
  `src/cli.rs` (clap + completions + lock policy) and command-handler modules
  grouped by domain (build/install/query/config), keeping the dispatch thin.
* `sandbox.rs` mixes: TOML spec types, sandbox-init executive logic, mount
  recipes, probes, spawn/parent protocol → split by responsibility with the
  syscall layer extracted.
* `build.rs`, `fetch.rs`, `install.rs`, `upgrade.rs` are large but their
  functions are cohesive; splits there would be one-function-per-file churn
  (mission forbids unnecessary abstraction). Only fetch's source-staging
  (tarball→staging) half is a candidate if a clean seam is found.

## 2. Planned changes (each landed as a separate, tested, documented step)

| step | change | tests added |
|---|---|---|
| 1 | `suggest.rs` → strsim distance | offset-equivalence vs. old algo, ranking invariance |
| 2 | `lock.rs` → rustix flock | two-process/three-thread contention, shared-vs-exclusive |
| 3 | `lint.rs` → goblin |elf32/elf64/endian_fd| malformed-ELF fuzz-ish table, ELF32+BE fixtures, huge-field panics |
| 4 | `sandbox.rs` → sys.rs abstraction + spec/init/mounts split | unchanged behaviour, existing sandbox suite green |
| 5 | `archive.rs` → openat2 extraction w/ fallback | traversal/symlink/hardlink/device/nl tests incl. both kernels modes |
| 6 | main.rs → cli.rs + commands/ | behaviour-free moves; e2e/smoke |
| 7 | vendor refresh + offline/static proof | cargo build/test --offline --locked |
| 8 | version.rs conformance vs pacman (tests only, plus big-number) | pinned table |

Non-goals kept visible: no CLI/format/policy changes; no MSRV bump without a
decision record (none needed — all new deps MSRV ≤ 1.65).

## 3. Results (filled as steps complete)

* step 1 (suggest → strsim 0.11.1): **done** — reference-DP equivalence test,
  ranking invariance; 199 tests green, offline.
* step 2 (lock → rustix flock 1.1.5): **done** — in-process exclusivity +
  no-leak tests; cross-process suite (tests/lock.rs, util-linux flock(1)).
* step 3 (lint → goblin 0.9.3): **done** — ELF32/ELF64 LE+BE fixtures,
  truncation/mutation fuzz table, huge-field non-panic, dangling string
  offsets, ET_REL/ET_CORE → no error; 203 tests green, clippy `-D warnings`
  clean.
* step 4 (sandbox split into sys/spec/mounts/init/probe): **done** — old
  monolith deleted; rustix for unshare/mount/waitpid/rlimit/umask/chdir/
  pivot_root/chroot/O_PATH/#fd-ioctl; raw prctl/capset/fork/alarm/SIOCSIFFLAGS
  confined to `src/sys.rs`. 203 tests + 1 ignored green offline, clippy clean,
  vendor refreshed (141 crates).
* step 5 (archive openat2 + entry-table hardening): **in progress**.
* step 6 (main.rs split): **pending** (docs said done prematurely — corrected).
* step 7 (vendor/offline/static): part 1 done (offline vendored build proof);
  static MUSL pending local toolchain (CI job covers it).
* final report + dependency evaluation: see `docs/refactoring-report.md`
  (to be written at the end).

## 4. Upstream data recorded for tests

* libalpm vercmp semantics (numeric saturation at long long; epoch; prerelease
  ordering) — used for the conformance tables in `version.rs`.
* curl retry/status semantics already mirrored by fetch.rs — unchanged.
