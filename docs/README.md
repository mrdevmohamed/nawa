# Nawa documentation

This directory contains user-facing guidance, design specifications, operational notes, and review records.

## Start here

| Document | Purpose |
|---|---|
| [Build isolation](build-isolation.md) | Isolation levels, build environment, sandbox and closure behavior |
| [Fetch phase design](design/fetch-phase.md) | Multi-source recipes, local assets, Go/Cargo dependency fetching, offline builds, cache identity, and compatibility rules |
| [PR #16 review](reviews/PR-16-REVIEW.md) | Remediation findings R1–R5, regression coverage, CI evidence, and remaining verification gaps |
| [Package management roadmap](design/package-management-roadmap.md) | Longer-term package-management direction |
| [Dependency-aware rebuild proposal](future-dependency-aware-rebuild.md) | Proposed dependency rebuild behavior |

## Fetch phase status

PR [#16](https://github.com/mrdevmohamed/nawa/pull/16) implements delivery phases 0–5 of the fetch-phase design:

1. Download and verify declared source artifacts.
2. Stage explicitly included recipe-local assets.
3. Fetch Go Modules or Cargo dependencies using lockfile-based inputs.
4. Cache fetched inputs and run the subsequent build offline.
5. Expose `nawa fetch`, `nawa build --offline`, and `nawa recipe check`.

The implementation preserves legacy single-source recipes and their byte-identical build IDs. It does not change the package archive format, package metadata, repository index, deployment, or signing formats. The bootstrap switch (design phase 6) is outside PR #16.

## Verification note — 2026-11-07

GitHub Actions run [42391105667](https://github.com/mrdevmohamed/nawa/actions/runs/42391105667) passed for implementation commit `a1144ccb55f22866ed229088c94890c742f435cd`. Later commits on the PR branch update documentation only; the successful run is not evidence that those later documentation commits were tested.

The general isolation matrix passed, but Go/Cargo fetch integration fixtures still use `--isolation env`. A live Go/Cargo fetch under `sandbox` and `closure` remains an explicit integration-coverage gap. Cargo registry resolution through a hermetic local registry is also not covered by the current end-to-end fixture. See the [PR #16 review log](reviews/PR-16-REVIEW.md) for the detailed matrix.

## Documentation conventions

- Design documents describe intended behavior and compatibility constraints.
- Review logs record observed implementation state, test evidence, and known gaps.
- A successful CI run is always tied to the exact commit it tested; later commits must not inherit that claim automatically.
