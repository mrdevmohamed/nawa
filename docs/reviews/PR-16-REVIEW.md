# PR #16 — Fetch Phase Review and Remediation Log

**Pull request:** https://github.com/mrdevmohamed/nawa/pull/16
**Reviewed head:** e77164e32ee063e416b0c9e1a91540924c202702
**Review date:** 2026-11-07
**Initial recommendation:** Request changes before merge.

This review covers fetch-phase implementation, cache identity, Cargo configuration discovery, sandbox mount preparation, timeout handling, and compatibility with the current Faseela package repository.

## Findings

### R1 — Fetch timeout kills only the immediate shell
**Severity:** High · **Location:** src/fetchrun.rs::stream_and_wait

The non-sandbox fetch runner starts Bash and, at timeout, calls Child::kill(). That kills only the immediate child. A subprocess such as go mod download, Cargo, or a helper can survive, retain the inherited stdout pipe, and keep the reader thread blocked at join(). Thus the configured timeout may fail to return and may leave descendants running.

**Required fix:** Give each plain fetch run its own process group, kill the whole group at timeout, reap the direct child, and ensure inherited pipe writers cannot keep the log reader alive. Add a regression that launches a descendant which inherits stdout and proves both the runner and descendant terminate.

### R2 — Go implicit dependency name does not match Faseela
**Severity:** High · **Location:** src/recipe.rs::GO_TOOLCHAIN

Nawa declares the Go build dependency as go, but the current Faseela recipe is named golang (faseela-repo/recipes/dev/golang/recipe.toml). Recipes using [fetch.go] therefore request a package that does not exist in the repository.

**Required fix:** Use golang and add a regression/check that implicit fetch dependencies resolve to real recipe names in the Faseela repository.

### R3 — Cargo configuration discovery is not hermetic
**Severity:** High · **Location:** src/fetchrun.rs::scan_cargo_configs, run_populate_cargo

The scan covers the source directory up to r.src, but Cargo also discovers configuration relative to its current working directory and through parent directories, and reads configuration from CARGO_HOME. The command currently runs with the source directory as its cwd. An unscanned ancestor configuration can therefore affect registry/network behavior, while the current scan does not establish the exact config set Cargo will read.

**Required fix:** Run Cargo from a dedicated empty fetch working directory and pass the manifest path explicitly; give it a fresh, explicit CARGO_HOME; keep the intentional source-tree config scan for the explicitly supported vendored case. Add tests proving parent-directory and inherited Cargo-home configuration cannot redirect the fetch.

### R4 — Resolver bind can follow a symlink into masked /run
**Severity:** High · **Location:** src/fetchrun.rs::resolver_binds, src/sandbox/mounts.rs::StagingHandles::bind_at

The host's /etc/resolv.conf may be a symlink into /run. The extra file bind currently targets /etc/resolv.conf and the file placeholder logic follows that symlink. The fetch sandbox later masks /run, which can hide the bind target. In a directory/closure root, a dangling symlink or missing parent can also make placeholder creation fail.

**Required fix:** Resolve and stage the resolver target safely for the sandbox root, ensure the required target exists without following a dangling final symlink, and order live-root masking/binding so a resolver target under /run remains reachable without modifying the host filesystem. Add live and directory-root tests for direct-file, valid-symlink, and dangling-symlink cases.

### R5 — Build cache identity omits the actual Go/Cargo toolchain in non-closure isolation
**Severity:** Medium · **Location:** src/build.rs::build_id_for, try_cache_hit

The fetch extras are schema constants (fetch-go=1 / fetch-cargo=1). Cargo's ecosystem cache key includes the Cargo version, but the build cache ID does not; the Go ecosystem key omits the Go version too. In env and live sandbox modes, host toolchain upgrades can therefore leave --use-cache matching an archive built with a different compiler. Closure mode also needs the toolchain package archives to be present in the closure identity.

**Required fix:** Include the actual toolchain version strings in the build ID for non-closure modes, using exactly the PATH used by the fetch runner. For closure mode, ensure the Go/Rust toolchain packages are included in the build dependency closure and therefore in the identity. Add Go and Cargo cache invalidation tests for toolchain-version changes.

## Verification gaps

- Existing Go/Cargo integration tests intentionally use --isolation env; they do not prove the fetch path works end-to-end under sandbox or closure.
- The CI isolation matrix exercises ordinary builds, not necessarily the fetch runner under those profiles.
- A green build/Clippy/isolation matrix must not be described as a green sandboxed-fetch test until a fetch recipe actually runs in that profile.
- The package-name check must use the live Faseela repository, not placeholder names in the Nawa source.

## Remediation order

1. Fix the Go dependency name and add a Faseela package-name consistency check.
2. Fix process-group timeout and descendant cleanup.
3. Isolate Cargo's cwd and CARGO_HOME while retaining explicit source-tree vendoring checks.
4. Fix resolver binds for symlinked /etc/resolv.conf without touching the host.
5. Include toolchain identity in build IDs and add cache invalidation tests.
6. Add real Go/Cargo fetch integration coverage for sandbox and closure where the CI runner supports it.

## Current status

All findings above are open at the reviewed head. This log is the baseline for remediation. Items must only be marked complete after their code change and targeted regression test are committed and verified on the corresponding head.