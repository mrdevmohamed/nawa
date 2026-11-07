# PR #16 — Fetch Phase Review and Remediation Log

**Pull request:** https://github.com/mrdevmohamed/nawa/pull/16
**Remediation head:** 7280dfbfa640488a05355fb024e4ae164730e24b
**Review date:** 2026-11-07
**Initial recommendation:** Request changes before merge.

This review covers fetch-phase implementation, cache identity, Cargo configuration discovery, sandbox mount preparation, timeout handling, and compatibility with the current Faseela package repository.

## Findings

### R1 — Fetch timeout kills only the immediate shell
**Status:** Implemented; CI pending

**Severity:** High · **Location:** src/fetchrun.rs::stream_and_wait

The non-sandbox fetch runner starts Bash and, at timeout, calls Child::kill(). That kills only the immediate child. A subprocess such as go mod download, Cargo, or a helper can survive, retain the inherited stdout pipe, and keep the reader thread blocked at join(). Thus the configured timeout may fail to return and may leave descendants running.

**Required fix:** Give each plain fetch run its own process group, kill the whole group at timeout, reap the direct child, and ensure inherited pipe writers cannot keep the log reader alive. Add a regression that launches a descendant which inherits stdout and proves both the runner and descendant terminate.

### R2 — Go implicit dependency name does not match Faseela
**Status:** Implemented; CI pending

**Severity:** High · **Location:** src/recipe.rs::GO_TOOLCHAIN

Nawa declares the Go build dependency as go, but the current Faseela recipe is named golang (faseela-repo/recipes/dev/golang/recipe.toml). Recipes using [fetch.go] therefore request a package that does not exist in the repository.

**Required fix:** Use golang and add a regression/check that implicit fetch dependencies resolve to real recipe names in the Faseela repository.

### R3 — Cargo configuration discovery is not hermetic
**Status:** Implemented; CI pending

**Severity:** High · **Location:** src/fetchrun.rs::scan_cargo_configs, run_populate_cargo

The scan covers the source directory up to r.src, but Cargo also discovers configuration relative to its current working directory and through parent directories, and reads configuration from CARGO_HOME. The command currently runs with the source directory as its cwd. An unscanned ancestor configuration can therefore affect registry/network behavior, while the current scan does not establish the exact config set Cargo will read.

**Required fix:** Run Cargo from a dedicated empty fetch working directory and pass the manifest path explicitly; give it a fresh, explicit CARGO_HOME; keep the intentional source-tree config scan for the explicitly supported vendored case. Add tests proving parent-directory and inherited Cargo-home configuration cannot redirect the fetch.

### R4 — Resolver bind can follow a symlink into masked /run
**Status:** Implemented; sandbox CI pending

**Severity:** High · **Location:** src/fetchrun.rs::resolver_binds, src/sandbox/mounts.rs::StagingHandles::bind_at

The host's /etc/resolv.conf may be a symlink into /run. The extra file bind currently targets /etc/resolv.conf and the file placeholder logic follows that symlink. The fetch sandbox later masks /run, which can hide the bind target. In a directory/closure root, a dangling symlink or missing parent can also make placeholder creation fail.

**Required fix:** Resolve and stage the resolver target safely for the sandbox root, ensure the required target exists without following a dangling final symlink, and order live-root masking/binding so a resolver target under /run remains reachable without modifying the host filesystem. Add live and directory-root tests for direct-file, valid-symlink, and dangling-symlink cases.

### R5 — Build cache identity omits the actual Go/Cargo toolchain in non-closure isolation
**Status:** Implemented; CI pending

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

## Remediation status at 897e208

The implementation changes and targeted regressions are now committed on the PR branch:

| Finding | Implementation | Regression coverage |
|---|---|---|
| R1 | Plain fetch commands use a dedicated process group; the reader deadline also covers descendants that retain stdout after Bash exits. Timeout kills the group and reaps Bash. | src/fetchrun.rs: descendant-inherited-stdout timeout test |
| R2 | GO_TOOLCHAIN is now golang; rust and ca-certificates were also checked against the current Faseela recipes. | src/recipe.rs: package-ID and implicit dependency-closure tests |
| R3 | Cargo fetch and its version probe use an empty temporary cwd, fresh per-fetch CARGO_HOME, explicit manifest path, and reject ambient Cargo configs that the recipe build cwd would discover outside the downloaded source. | tests/fetchcargo.rs: inherited CARGO_HOME is ignored; ambient parent config is rejected before fetch |
| R4 | Resolver symlinks are resolved against the sandbox root. Live /run is replaced with an empty searchable tmpfs before binding the resolver target; the bind and /run are read-only. File-bind targets no longer follow final symlinks. | src/fetchrun.rs: direct, relative, and dangling resolver symlink path tests. Full live/closure mount execution still needs CI confirmation. |
| R5 | Non-closure build IDs include actual Go/Cargo version strings from the fetch PATH. Closure IDs already include the implicit toolchain package archives. | src/build.rs: Go/Cargo version and closure archive invalidation tests |

**CI:** workflow run 42388465305 was created for the earlier code head 7280dfb and has not returned jobs/results. No workflow run is currently associated with final code head 897e208. The latest code therefore remains unverified; do not treat the earlier green PR checks as coverage for these remediation commits.

**Recommendation:** keep the PR Draft and do not merge yet. In particular, confirm the process-group test, Cargo integration test, and sandbox/closure resolver behavior in CI.