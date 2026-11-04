# Future design: dependency-aware package rebuilds

> **STATUS: PROPOSED — NOT IMPLEMENTED — NOT READY FOR STABLE RELEASES**
>
> This document records a possible future direction only. It does not change
> Nawa's current package, cache, repository, upgrade, or installation behavior.
> The current stable implementation must remain the source of truth. Do not
> implement this design as an unreviewed change, and do not make stable
> publishers or clients depend on any of the proposed metadata.
>
> The project is not ready to change package identity or same-version
> replacement semantics today. The work may be revisited in the near future,
> but only as a sequence of independently reviewed, tested, opt-in steps.
> An experimental release/channel may be used to validate the design before
> any stable release adopts it.

## 1. Summary

The proposed feature would allow a package to be rebuilt when an input that
affects its build changes, even when its package version and release remain
unchanged.

For example, rebuilding `wpa_supplicant 2.12-3` after a build dependency is
updated should not require changing the recipe's `version` or `rel` merely
to force a rebuild.

The central design principle is to keep three identities separate:

- **Package identity (NEVR):** name, version, release, and architecture. This
  remains the user-facing package version.
- **Build identity (`build_id` / build fingerprint):** a digest of the
  relevant inputs used to produce an artifact.
- **Artifact identity (SHA-256):** a digest of the exact archive bytes that
  are downloaded, verified, cached, and installed.

These values answer different questions. NEVR says which package release this
is; the build identity says which build inputs produced it; the artifact
digest says which exact bytes are being handled.

This proposal does not prescribe changing NEVR when a rebuild occurs.

## 2. Current stable behavior — preserve it

Nawa already has an opt-in local build cache:

- `nawa build --use-cache` compares the current build ID with the
  `<archive>.buildid` sidecar in the output directory.
- The current build ID covers the recipe bytes, source SHA-256, closure
  archive set, and profile flags.
- A matching local build ID allows that build to be skipped.
- `--rebuild-check` bypasses the cache so reproducibility can be tested.

This existing local cache is useful and must not be confused with the broader
proposal in this document. In particular, it does not by itself define a
repository-wide dependency invalidation protocol, safe replacement of an
archive under an unchanged filename, or how an already-installed package
should notice changed artifact bytes with the same NEVR.

Until a future design is implemented and explicitly released:

1. Existing recipe version/release rules remain unchanged.
2. Existing archive naming and repository index formats remain unchanged.
3. Existing install, update, upgrade, rollback, signature, and trust behavior
   remain unchanged.
4. A publisher must continue using the currently supported release process.
5. Users and distributions must not assume that changing a dependency
   automatically invalidates every dependent package's build cache.

**Stability takes priority over automatic rebuild convenience.** If a rebuild
is required under today's rules, use the existing explicit workflow and
release conventions. Do not silently reinterpret an existing package archive
or installed package.

## 3. Goals

A future implementation should:

1. Detect when build-relevant inputs have changed, including the actual
   artifacts used for build dependencies.
2. Rebuild only affected packages, in dependency order.
3. Preserve a package's NEVR when the rebuild is caused by changed build
   inputs and the package maintainer has not intentionally changed its
   version/release.
4. Keep artifact integrity, signatures, repository consistency, and rollback
   guarantees.
5. Make cache decisions explainable: users should be able to see why a package
   is up to date or why it needs rebuilding.
6. Work incrementally and remain opt-in until compatibility and operational
   risks have been tested.
7. Preserve compatibility with older repositories and Nawa clients during a
   controlled migration.

## 4. Non-goals

This proposal does not aim to:

- Change the meaning of package version or release.
- Rebuild every package after every repository update.
- Treat every runtime dependency change as a reason to rebuild the consumer.
- Make builds reproducible merely by adding a fingerprint.
- Replace archive signatures, signed repository indexes, or SHA-256 checks.
- Enable same-NEVR artifact replacement in stable repositories before client
  behavior, caches, mirrors, and rollback have been designed and tested.
- Change the current stable defaults or force distributions to adopt the
  feature.

## 5. Proposed identities and metadata

### 5.1 Package identity (NEVR)

NEVR remains the package's declared identity:

```text
name = wpa_supplicant
version = 2.12
release = 3
arch = x86_64
```

A dependency-triggered rebuild does not automatically increment `release`.
A maintainer may still change version/release for an actual package release,
security update, source update, or other normal packaging reason.

### 5.2 Build identity

A future build identity should be a stable digest of normalized,
build-relevant inputs. Conceptually:

```text
build_id = SHA256(
    build-relevant recipe data
  + source identity and source checksums
  + build dependency artifact identities
  + toolchain identity
  + build profile and effective flags
  + relevant build environment identity
  + build-system rules that affect the output
)
```

The exact serialization and schema must be versioned and specified before
implementation. It must be deterministic: equivalent inputs must produce the
same fingerprint regardless of runner path, execution time, or unrelated
metadata.

Candidate inputs:

| Input | Include? | Notes |
|---|---:|---|
| Build commands and build-relevant recipe fields | Yes | Normalize parsed fields; do not hash comments or formatting. |
| Source URL/version and verified source SHA-256 | Yes | The verified content identity is essential. |
| Build dependency artifact digests | Yes | Prefer exact archive digests/build identities, not version strings alone. |
| Toolchain artifacts and compiler identity | Yes | Include compiler, linker, binutils, libc/headers, and relevant tools. |
| Effective profile flags and build options | Yes | Include values actually passed to the build. |
| Isolation mode and relevant build-environment identity | Yes | Changes that can affect visible inputs or output must invalidate. |
| Nawa build rules affecting output | Yes | Fingerprint schema/rule version must change when semantics change. |
| Summary, description, maintainer, documentation-only metadata | No | Unless a field is later proven to affect generated output. |
| Runtime-only dependencies | Usually no | They do not normally affect compilation; package-specific exceptions need an explicit rule. |
| Comments, TOML whitespace, key ordering | No | Formatting changes must not trigger rebuilds. |

This list is a design starting point, not a finalized contract. A future
implementation must document every included field and explain how it is
canonicalized.

### 5.3 Artifact identity

The artifact SHA-256 is the digest of the exact `.nawa.tar.zst` bytes. It is
not interchangeable with the build ID:

- Two builds with identical declared inputs may still produce different bytes
  if reproducibility is imperfect.
- A build ID match is a cache/input decision, not proof that downloaded bytes
  are authentic.
- Every downloaded or cached archive must continue to pass the existing
  signature/trust and digest checks.

A future repository entry may carry both values, but the current repository
schemas must not be changed implicitly.

## 6. Dependency-aware invalidation

The future build planner would use the recipe dependency graph and the
artifact identities of build dependencies.

Example:

```text
openssl [artifact changed]
  |
  +--> curl [build input changed: rebuild]
  |
  +--> wpa_supplicant [build input changed: rebuild]
  |
  +--> nginx [build input changed: rebuild]

bash [no relevant input changed: keep]
coreutils [no relevant input changed: keep]
```

Expected planning behavior:

1. Resolve the requested package set and its build dependency graph.
2. Determine the exact dependency artifacts selected for each build.
3. Compute the expected build identity in topological order.
4. Compare it with the identity recorded for the available artifact/cache entry.
5. Mark only mismatches or entries without trustworthy build metadata as stale.
6. Rebuild stale packages before their dependents.
7. Recompute dependent identities after each dependency artifact is finalized.

The planner must distinguish build dependencies from runtime dependencies.
A runtime dependency update should not automatically trigger recompilation
unless the recipe or a future explicit dependency policy says that it affects
the build.

### Missing or legacy metadata

An archive without a recognized build identity cannot be assumed to match the
new scheme. A migration must define how such archives are handled. Possible
choices include treating them as stale on the first opt-in run, or retaining
them until an explicit rebuild is requested. The choice must be made per
migration phase and must never silently affect current stable installations.

## 7. The difficult part: same-NEVR artifact changes

Keeping NEVR unchanged while replacing archive bytes creates compatibility
questions that must be answered before implementation.

### 7.1 Package manager behavior

Today, package identity and version comparisons are central to package
selection and upgrade planning. A future client would need a deliberate rule
for recognizing that the repository artifact has changed even though NEVR is
identical.

A possible future repository record could expose:

```toml
[[packages]]
name = "wpa_supplicant"
version = "2.12"
release = 3
arch = "x86_64"
build_id = "sha256:..."
artifact_sha256 = "..."
```

This is illustrative only. It is not a supported schema and must not be
published in stable repositories until a schema version and client behavior
are approved.

The client must define whether a changed artifact digest means reinstall,
upgrade-like replacement, or a separate explicit refresh operation. It must
also preserve configuration-file handling, hooks, dependency ordering,
transaction atomicity, and rollback.

### 7.2 Cache correctness

Every cache layer must be keyed or validated using content identity where
appropriate. A cache must never return old archive bytes merely because the
filename or NEVR is unchanged.

This includes:

- Local build output and build-ID sidecars.
- Download caches for remote repositories.
- Mirror and CDN caches.
- Installed-package state used by update planning.
- Rollback archives and undo records.

A future design must specify cache invalidation and retention rules, not just
the build planner.

### 7.3 Signatures and repository publication

Replacing an archive under the same filename is an operationally sensitive
operation. A safe publication sequence would need to ensure that a signed
index never points to missing, partially uploaded, or mismatched bytes.

At minimum, a future publisher must:

1. Build and validate the new archive.
2. Calculate and verify its artifact digest and metadata.
3. Upload the complete archive to its final immutable or versioned location.
4. Verify that the uploaded object matches the expected digest.
5. Publish a new signed index referencing that exact object and digest.
6. Retain the previous artifact long enough for mirrors, clients, and rollback
   policies to remain coherent.

Prefer immutable content-addressed archive locations over overwriting an
existing object. If a stable filename must be retained for compatibility, the
index and client cache semantics must still bind to the new digest.

Index signing and archive verification remain mandatory. A build fingerprint
does not replace cryptographic verification.

## 8. Rollout strategy — incremental and opt-in

No single change should introduce the whole feature. A possible rollout is:

| Step | Scope | Expected behavior | Stable impact |
|---|---|---|---|
| 0 | Documentation and design review | Record requirements, risks, and unresolved questions | None |
| 1 | Build metadata experiment | Produce and inspect a versioned fingerprint without changing cache decisions | None; diagnostic only |
| 2 | Local cache experiment | Compare fingerprints in an opt-in test mode | None by default |
| 3 | Dependency planner experiment | Report which packages would rebuild; do not rebuild automatically | None by default |
| 4 | Experimental repository/client | Test artifact-digest-aware selection and same-NEVR refresh in an isolated channel | Experimental only |
| 5 | Compatibility and migration | Test old/new clients, signatures, mirrors, rollback, and interrupted publication | No stable rollout until approved |
| 6 | Stable adoption decision | Consider a schema/client release only after evidence and explicit approval | Separate release decision |

Each step should be independently reviewable and reversible. A feature flag
or experimental build must not change the behavior of stable installations
when disabled.

An experimental release/channel is strongly preferred for steps that change
repository metadata, package selection, or same-NEVR installation semantics.
It must use an isolated test repository and must not publish experimental
metadata into the stable repository.

## 9. Required tests before any stable adoption

A future implementation is not ready for stable use until tests cover at
least:

### Build planner

- Unchanged inputs produce the same build identity.
- Recipe comments, whitespace, and descriptive metadata do not invalidate it.
- A changed source checksum invalidates the package.
- A changed build dependency artifact invalidates only affected dependents.
- Transitive dependencies are processed in topological order.
- Runtime-only changes do not cause unnecessary rebuilds.
- Cycles, missing dependencies, and unavailable artifacts fail safely.
- Legacy archives follow the explicitly selected migration policy.

### Nawa client and package database

- Same-NEVR/different-digest behavior is explicit and tested.
- A cached old archive cannot satisfy a request for a new digest.
- A failed install leaves the previous package intact.
- Hooks and configuration-file protection behave correctly.
- Rollback restores the exact previous artifact.
- Concurrent package operations remain correctly locked.
- Older clients fail safely or continue using the compatibility path.

### Repository and signatures

- The signed index binds the artifact digest and size.
- An archive with the wrong digest is rejected.
- An invalid or untrusted index signature is rejected.
- Interrupted uploads never result in a published index pointing to incomplete
  data.
- Concurrent publishers cannot overwrite each other's repository state.
- Mirrors and download caches cannot serve stale bytes as the new artifact.
- Previous artifacts remain available according to the rollback/retention
  policy.

### Distribution integration

- Bootstrap rebuild selection is deterministic.
- A dependency update rebuilds only the intended consumers.
- A full build can still be reproduced from a clean environment.
- Stable repository generation and existing installation workflows remain
  unchanged when the feature is disabled.

## 10. Open design decisions

These questions must be resolved before implementation:

1. What exact normalized recipe fields enter the fingerprint?
2. Should build dependencies be represented by artifact SHA-256, their own
   build ID, or both?
3. How is the compiler/toolchain identity captured across bootstrap stages?
4. How are environment and Nawa build-rule changes versioned?
5. How should a client represent installed artifact identity in its database?
6. What user-facing command should refresh a same-NEVR package?
7. How do rollback and package hooks behave when only artifact identity
   changes?
8. Should repositories use content-addressed archive paths, immutable release
   assets, or another storage model?
9. How long must old artifacts remain available?
10. What is the minimum supported Nawa client version during migration?
11. How are experimental and stable repository channels isolated?
12. What evidence is required before changing any stable default?

## 11. Readiness and change-control rule

**This design is for future work, not the current stable implementation.**

The current priority is stability. Do not start changing archive identity,
repository schemas, package database semantics, or same-NEVR upgrade behavior
as part of routine maintenance.

Implementation should begin only when:

- The current stable build, repository, install, upgrade, and rollback paths
  are healthy.
- The relevant Nawa and distribution maintainers explicitly approve the
  implementation plan.
- The work is split into small changes with tests and rollback paths.
- Any behavior-changing prototype is isolated in an experimental release and
  repository.
- Compatibility with existing stable archives, repositories, and clients is
  demonstrated.

The design may be revisited in the near future, one capability at a time.
Until then, this document is informational and imposes no new requirements on
Nawa users, recipe maintainers, or distribution publishers.
