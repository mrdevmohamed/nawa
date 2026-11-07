# PR #16 — Fetch Phase Review and Remediation Log

**Pull request:** https://github.com/mrdevmohamed/nawa/pull/16  
**Branch:** docs/fetch-phase-design  
**Implementation commit verified by CI:** a1144ccb55f22866ed229088c94890c742f435cd  
**Branch state:** documentation-only follow-up commits are newer than the implementation commit verified by CI  
**Review date:** 2026-11-07  
**Initial recommendation:** Request changes before remediation; keep Draft until all required checks and remaining integration work are resolved.

This log tracks the implementation findings, their fixes, regression coverage, and what CI does—and does not—prove. The design contract is in docs/design/fetch-phase.md.

## Findings and remediation

### R1 — Fetch timeout kills only the immediate shell
**Status:** Implemented; targeted regression and current CI passed.

The plain fetch runner now starts each command in its own process group. Timeout cleanup kills the group, reaps the direct child, and bounds the log reader even when a descendant inherited stdout.

**Coverage:** src/fetchrun.rs descendant-inherited-stdout timeout regression.

### R2 — Go implicit dependency name did not match Faseela
**Status:** Implemented; targeted regression and current CI passed.

The implicit Go toolchain package ID is golang, matching the current Faseela recipe name. The implicit IDs golang, rust, and ca-certificates were checked against the current Faseela repository recipes.

**Coverage:** src/recipe.rs package-ID and implicit dependency-closure tests.

### R3 — Cargo configuration discovery was not hermetic
**Status:** Implemented; targeted regressions and current CI passed.

Cargo fetch and its version probe use an empty temporary working directory, a fresh per-fetch CARGO_HOME, and an explicit manifest path. Ambient Cargo configuration that would otherwise be discovered outside the downloaded source tree is rejected. The toolchain identity probe follows the same isolation policy.

**Coverage:** tests/fetchcargo.rs verifies inherited CARGO_HOME is ignored and ambient parent configuration is rejected before fetch.

### R4 — Resolver bind could follow a symlink into masked /run
**Status:** Path-resolution fix and unit regressions implemented; live fetch mount behavior still needs end-to-end coverage.

Resolver symlinks are resolved against the supplied sandbox root. In the live sandbox, /run is replaced with an empty searchable read-only staging mount before the resolver target is bound. File-bind targets do not follow a final symlink.

**Coverage:** src/fetchrun.rs tests direct, relative, dangling, chained, looping, and root-escaping resolver symlinks. These are path-resolution tests; they do not replace a live privileged sandbox/closure fetch test.

### R5 — Build cache identity omitted the actual Go/Cargo toolchain in non-closure isolation
**Status:** Implemented; targeted regression and current CI passed.

Non-closure build IDs include the actual Go/Cargo version strings obtained through the fetch runner's PATH. Closure identity includes the implicit toolchain package archives.

**Coverage:** src/build.rs Go/Cargo toolchain-version and closure-archive invalidation tests.

## Current CI verification

**Implementation commit:** a1144ccb55f22866ed229088c94890c742f435cd  
**PR branch state:** documentation-only commits follow the tested implementation tree  
**Workflow run:** [42391105667](https://github.com/mrdevmohamed/nawa/actions/runs/42391105667)  
**Conclusion:** **Success** (completed).

The successful run included:

- Build, Clippy with warnings treated as errors, and tests.
- User/root test jobs on Ubuntu 22.04 and 24.04.
- Ordinary build/isolation jobs for none, env, sandbox, and closure.
- Static musl binary verification.
- CLI smoke tests.

The earlier statement that the remediation head had no associated workflow run is superseded by this result. The tested implementation commit is the parent of the later documentation-only commits; no workflow run is currently associated with the latest documentation head.

## Remaining verification gaps

A green general CI run is not the same as proving every fetch mode:

1. Go/Cargo end-to-end fetch fixtures still run with --isolation env.
2. The sandbox/closure matrix runs ordinary builds; it does not run a Go/Cargo fetch recipe under those isolation modes.
3. Cargo registry-crate resolution through a hermetic local registry is not covered; the current Cargo end-to-end fixture covers a pinned local Git dependency.
4. Go workspace/multi-module success behavior is not supported: go.work is deliberately rejected and tested as a hard error.
5. Resolver unit tests do not prove live mount ordering, DNS reachability, or fetch behavior against a closure root.

Do not describe the fetch runner as end-to-end verified under sandbox or closure until dedicated integration coverage exists and passes.

## Existing coverage confirmed on this branch

- **Go:** real module download, offline build, cache hit, lockfile-key changes, vendored mode, missing go.sum, and go.work rejection.
- **Cargo:** pinned Git dependency fetch and offline build, cache hit, lockfile/Cargo-version key changes, vendored directory mode, missing lockfile, replacement-source failure, and Cargo configuration isolation.
- **External sources:** multiple archives plus a plain file, SHA-256 rejection before extraction, mirror fallback, destination-collision rejection, and rebuild-check determinism.
- **Offline mode:** cold-cache failure and seeded-cache build both assert that no socket is opened during the offline build.

## Scope and compatibility guardrails

- Existing single-source recipes remain supported through the legacy code path and retain byte-identical build IDs (pinned by a golden test).
- Multi-source recipes, recipe-local assets, Go/Cargo dependency fetch, and offline build behavior do not change the package archive format, .PKGINFO, repository index, deployment, or signing format.
- Phase 6 (the bootstrap switch) is not part of PR #16.
- Keep PR #16 Draft until the remaining fetch-specific integration gap is either covered or explicitly accepted as a documented limitation by the maintainer.
