# Nawa build inputs and fetch phase — design

**Status:** Proposal (design only; no production code in this PR)
**Project:** Nawa
**Scope:** Multiple external source artifacts, recipe-local assets, and lockfile-based Go/Cargo dependency fetching
**Compatibility goal:** Existing recipes continue to work unchanged
**Package format:** No change to archive layout, .PKGINFO schema, repository index, or signing format in v1

---

## 1. Summary

A package build can need more than one kind of input:

1. One or more upstream source archives or files.
2. Files maintained beside the recipe, such as patches, scripts, and configuration fragments.
3. Language dependencies managed by tools such as Go modules or Cargo crates.

These inputs must not be conflated. In particular, Cargo crates are not extra source tarballs, and a kernel patch maintained by Faseela is not an upstream source archive.

This proposal gives each input type a clear place in the build:

~~~
resolve recipe
  -> fetch and verify external sources
  -> assemble the source workspace
  -> stage declared local assets
  -> fetch Go/Cargo dependencies
  -> build with network disabled
  -> install
  -> create the existing package archive
~~~

The build still produces one Nawa package unless a future, separate split-package feature is explicitly introduced.

The fetch phase runs only Nawa-defined ecosystem operations (for example, go mod download and cargo fetch --locked). It does not run recipe build scripts with network access.

## 2. Goals and non-goals

### Goals

1. Support one or many external source artifacts in one recipe.
2. Keep patches, scripts, and configuration files versioned beside their recipe without requiring extra tarballs.
3. Support Go and Rust dependencies without requiring upstream vendor directories or manually maintained vendor archives.
4. Keep build commands offline by default.
5. Preserve every existing recipe that uses package.source, package.sha256, package.mirrors, and package.source_dir.
6. Include every build input in the build identity so changing a patch, script, source, or lockfile invalidates stale build results.
7. Reuse verified caches and support explicit offline operation.

### Non-goals for v1

- npm, pnpm, yarn, pip, Python wheels, or other ecosystems.
- Automatic source generation or patch generation.
- Splitting one recipe into multiple binary packages.
- Running arbitrary recipe commands in the network-enabled fetch phase.
- Replacing Nawa's package archive, install, repository publication, or signing formats.
- Requiring every fetched dependency tree to have a recipe-maintained output hash. An optional output hash can be designed later.

## 3. Terminology

**External source:** A file or archive downloaded from an upstream URL and verified against its own SHA256.

**Local asset:** A file tracked in the recipe repository and explicitly declared by the recipe. Examples include patches, kernel config fragments, helper scripts, and service templates.

**Ecosystem dependency:** A dependency resolved by a language tool using a lockfile, such as a Go module or Cargo crate.

**Source workspace:** The isolated directory assembled for a package build. It contains the extracted external sources and a read-only copy of the declared local assets.

**Fetch cache:** Nawa-managed storage for verified external artifacts and language dependencies. It is separate from the recipe repository.

These are separate concepts and must remain separate in both the recipe schema and implementation.

## 4. Recipe schema

### 4.1 Existing single-source syntax remains supported

Existing recipes continue to use the current syntax without modification:

~~~toml
[package]
name = "gzip"
version = "1.14"
source = "https://example.org/gzip-1.14.tar.xz"
sha256 = "<SOURCE_SHA256>"
source_dir = "gzip-1.14"
mirrors = ["https://mirror.example.org/gzip-1.14.tar.xz"]
~~~

The current meanings remain unchanged:

- source is the primary URL.
- sha256 verifies the downloaded bytes.
- mirrors are fallback URLs for the exact same artifact; they are not additional sources.
- source_dir selects the extracted source directory using the existing behaviour.

This legacy form is treated internally as a one-entry source list. Recipes do not need a migration.

### 4.2 Multiple external sources

New recipes may declare an array of source artifacts:

~~~toml
[package]
name = "example"
version = "1.0.0"

[[package.sources]]
name = "main"
url = "https://example.org/example-1.0.0.tar.xz"
sha256 = "<MAIN_SHA256>"
unpack = true
into = "."
strip_components = 1

[[package.sources]]
name = "patch-bundle"
url = "https://example.org/example-patches-1.0.0.tar.gz"
sha256 = "<PATCH_SHA256>"
unpack = true
into = "patches"
strip_components = 1

[[package.sources]]
name = "bootstrap-data"
url = "https://example.org/example-bootstrap.dat"
sha256 = "<DATA_SHA256>"
unpack = false
into = "data/bootstrap.dat"
~~~

Proposed fields:

| Field | Required | Meaning |
|---|---:|---|
| name | yes | Unique, stable identifier within this recipe |
| url | yes | URL of this artifact |
| sha256 | yes | SHA256 of the exact downloaded bytes |
| mirrors | no | Fallback URLs for this same artifact only |
| unpack | no | Whether to extract an archive; default is auto-detect for compatibility with current source handling |
| into | no | Workspace-relative destination; default is the workspace root |
| strip_components | no | Number of leading archive path components to remove; default 0 |

Rules:

- Each external artifact has its own SHA256 and cache entry.
- A mirror must return bytes matching that artifact's sha256.
- The source array order is stable and is part of the recipe definition, but builds must not depend on download completion order.
- into must be a relative path inside the source workspace. Absolute paths, parent traversal, and paths escaping the workspace are rejected.
- For an archive, into is the extraction directory. For unpack = false, into is the destination file path.
- Archive extraction keeps the existing safe extraction checks and must also reject writes outside the destination.
- The implementation must detect file/directory collisions between artifacts. It must not silently overwrite files from an earlier source.
- Multiple artifacts may share a destination directory only when their extracted paths do not collide.
- The new package.sources form must not be mixed with legacy package.source/package.sha256/package.mirrors fields in the same recipe. Such a recipe is rejected with a clear lint/parser error rather than guessing which source definition wins.
- source_dir keeps its legacy meaning. For a new multi-source recipe, the working directory is the assembled workspace root unless a future, explicit workspace-root option is added. Recipes can use relative paths such as cd kernel when a source is placed under into = "kernel".

The new syntax is additive. It does not change the interpretation of existing recipes.

## 5. Recipe-local assets

### 5.1 Why assets are separate

Patches and configuration files maintained by Faseela should normally live beside the recipe in the package repository. They should not be disguised as upstream source tarballs and should not require a network fetch.

Example repository layout:

~~~
recipes/
└── boot/
    └── linux/
        ├── recipe.toml
        └── files/
            ├── patches/
            │   ├── 0001-faseela-fix.patch
            │   └── 0002-hardware-support.patch
            ├── config/
            │   └── x86_64.fragment
            └── scripts/
                ├── configure-kernel.sh
                └── verify-kernel-config.sh
~~~

### 5.2 Asset declaration

A recipe explicitly declares the local paths it needs:

~~~toml
[assets]
include = [
    "files/patches",
    "files/config/x86_64.fragment",
    "files/scripts/configure-kernel.sh",
    "files/scripts/verify-kernel-config.sh",
]
~~~

Rules for v1:

- Paths are relative to the recipe directory.
- A listed directory is included recursively.
- Only declared paths are staged; Nawa must not expose the entire repository or neighbouring recipes to a build.
- Wildcards/globs are not required in v1. Explicit paths and recursive directories are simpler to validate and deterministic.
- Missing paths are hard errors before the build starts.
- Absolute paths, parent traversal, and symlinks escaping the recipe directory are rejected.
- Special files such as device nodes, sockets, and FIFOs are rejected.
- Asset paths are preserved relative to a dedicated, isolated assets directory.
- Asset contents and relevant file modes are hashed and included in the build identity.

Nawa stages the declared assets into the build sandbox as read-only files and exposes only that staged directory through a dedicated variable, proposed as %{assets_dir}. The variable points to the staged asset root, not to the recipe directory or repository checkout.

Example use:

~~~toml
[build]
configure = '''
make mrproper
make defconfig
patch -Np1 -i %{assets_dir}/files/patches/0001-faseela-fix.patch
sh %{assets_dir}/files/scripts/configure-kernel.sh
scripts/kconfig/merge_config.sh -m .config \
    %{assets_dir}/files/config/x86_64.fragment
make olddefconfig
sh %{assets_dir}/files/scripts/verify-kernel-config.sh
'''
~~~

The staged assets directory is read-only. Scripts may write generated output only into the normal build workspace or other declared writable build paths.

The name %{assets_dir} is proposed and must be checked against Nawa's existing variable expansion before implementation. Do not introduce a conflicting alias.

### 5.3 Integrity and trust

- Asset files are part of the recipe repository snapshot, not independent downloaded source archives.
- Nawa computes a deterministic asset-tree digest from the declared paths, file contents, and relevant file modes.
- The digest is included in the build identity.
- The repository's existing trust and signing model remains unchanged. This proposal does not invent a new package signature format or place assets into the binary package archive.
- If an asset changes while a build is in progress, the build must use the already staged snapshot or fail; it must never read a partially changing working tree.

## 6. Unified source workspace

All external sources and local assets are prepared before language dependency fetching or recipe build commands run.

Proposed layout inside the build area:

~~~
work/
├── source/                 # extracted external sources
│   ├── kernel/             # source artifact placed into "kernel"
│   ├── patches/            # optional external patch bundle
│   └── data/
│       └── bootstrap.dat
└── recipe-assets/          # declared, read-only local assets
    └── files/
        ├── patches/
        ├── config/
        └── scripts/
~~~

The exact mount paths are implementation details. The observable contract is:

- build commands start in the same working directory as today for legacy single-source recipes;
- new multi-source recipes get one deterministic workspace with paths controlled by each source's into field;
- local assets are available through %{assets_dir};
- source and asset inputs are read-only during fetch and are not mutated by Nawa's fetch commands;
- build commands retain only the writable paths they already need for compilation and installation staging.

All source downloads are verified before extraction. A failed source must stop the build before any recipe build command runs.

## 7. Go fetch backend

Preconditions: the selected dir contains go.mod and go.sum.

If dir/vendor/modules.txt exists, Go is already vendored and the backend may skip fetching for backward compatibility. The build uses the project's vendor behaviour. This exception is not a reason to keep vendor directories in Nawa itself.

Environment during fetch (everything else is cleared, including inherited GOFLAGS, GOPROXY, GONOSUMDB, GONOSUMCHECK, GOPRIVATE, and GOINSECURE):

~~~
GOTOOLCHAIN=local
GOFLAGS=-mod=readonly
GOMODCACHE=<fetch cache>
GOPROXY=https://proxy.golang.org
~~~

NAWA_GOPROXY may override the default proxy through Nawa's controlled configuration.

Command: go mod download.

Build-time environment:

~~~
GOMODCACHE=<fetch cache, read-only view>
GOPROXY=off
GOFLAGS=-mod=readonly
GOTOOLCHAIN=local
~~~

Rules:

- A missing go.mod or required go.sum is a hard error.
- No automatic Go toolchain download is allowed. GOTOOLCHAIN=local makes an incompatible toolchain requirement fail clearly.
- The project source and lockfiles are not modified by fetch.
- A Go workspace (go.work/go.work.sum) is not supported in v1 unless explicitly implemented and tested. It must not be silently ignored when it changes dependency resolution.
- The cache must be verified offline in the prototype; do not assume a read-only GOMODCACHE works without testing.

## 8. Cargo fetch backend

Precondition: the selected dir contains Cargo.lock.

Environment during fetch:

~~~
CARGO_HOME=<fetch cache>
CARGO_NET_GIT_FETCH_WITH_CLI unset
~~~

Command: cargo fetch --locked.

Build-time environment:

~~~
CARGO_HOME=<fetch cache, writable overlay view>
CARGO_NET_OFFLINE=true
~~~

Recipes keep their own build commands, for example cargo build --locked --release --offline.

Rules:

- Cargo.lock is mandatory.
- --locked must fail rather than update the lockfile.
- Registry crate checksums are verified by Cargo.
- Git dependencies are pinned by commit in Cargo.lock but do not have registry-style crate checksums; nawa lint warns about them in v1.
- Cargo may write a package-cache lock file even during offline builds. The immutable cache therefore needs a throw-away writable overlay, or a sandbox-local copy when overlayfs is unavailable.
- The host/target triple policy must be verified by the prototype before a target filter is made the default.

## 9. Fetch declarations and dependency resolution

The ecosystem declarations remain optional:

~~~toml
[fetch.go]
dir = "."

[fetch.cargo]
dir = "."
~~~

Rules:

- Both tables may appear together.
- dir is relative to the assembled source workspace and must not escape it.
- [fetch.go] adds golang to build dependencies.
- [fetch.cargo] adds rust to build dependencies.
- These implicit dependencies must be visible to the dependency resolver, nawa deps, build closure calculation, build ID, and fetch planning. They must not be added only to the command environment.
- Unknown keys in a fetch table are errors.
- Fetch runs after all declared sources and local assets are staged, so a lockfile can reside in any declared source subdirectory.
- A recipe with no [fetch.*] table does not run an ecosystem fetch backend.

## 10. Integrity and build identity

### 10.1 External sources

Each source archive is verified against its own recipe-pinned SHA256 before extraction. A mirror is only an alternate transport for the same bytes.

The cache key for an external artifact uses the full SHA256, not a truncated prefix. The source URL/name may be recorded as metadata but must not replace content verification.

### 10.2 Language dependencies

Go uses go.sum checksums. Cargo uses Cargo.lock checksums for registry crates and commit pins for Git dependencies.

The lockfiles themselves come from a source archive whose SHA256 is pinned by the recipe, or from another verified source in the source list.

This does not make every upstream lockfile equally strong. Cargo Git dependencies remain a separately reported caveat.

### 10.3 Build identity

The build identity must cover at least:

- recipe bytes;
- every external source name, full SHA256, destination, extraction mode, and strip_components value;
- the declared local asset tree digest;
- all [fetch.*] declarations and lockfile bytes;
- the existing dependency closure and build flags.

A change to a patch, config fragment, helper script, source archive, destination, or lockfile must invalidate a previous build result.

### 10.4 Audit data and package format

The v1 implementation must not change the binary package archive, .PKGINFO schema, repository index schema, deployment behaviour, or signing format.

Fetch details may be recorded in build logs or an external build-side manifest/cache record. Do not append fetched_* fields to .PKGINFO in v1.

## 11. Cache design

The cache root remains NAWA_SRCCACHE (defaulting to the existing build distfiles directory).

Proposed layout:

~~~
<distfiles>/
├── sources/
│   └── sha256/
│       └── <full-sha256>
└── fetch/
    ├── go/
    │   └── <full-cache-key>/
    └── cargo/
        └── <full-cache-key>/
~~~

Rules:

- External artifacts are cached and re-verified by full SHA256.
- Ecosystem cache keys use a versioned, unambiguous encoding of ecosystem name, cache schema version, and the relevant lockfile/config inputs.
- Cache keys must not use only the first 12 or 16 hex characters.
- A cache entry is populated in a temporary directory, validated, marked complete, and atomically published.
- Concurrent fetches for the same key are serialized with a lock.
- An incomplete or corrupt cache entry is never treated as a hit.
- A completed cache entry is immutable. Backend-specific writable state belongs in an overlay or sandbox-local copy.
- Cache sharing between recipes is allowed only when the full cache key matches.

## 12. Sandbox and network policy

There are two different network-sensitive operations:

1. Nawa-controlled downloading of external source artifacts and ecosystem dependencies.
2. Recipe-defined configure, compile, check, and install commands.

Only the first operation may use the network in a normal online build.

Rules:

- Recipe build commands run with the network disabled under the supported sandbox/closure isolation modes.
- Fetch backends run only fixed Nawa-defined commands with a sanitized environment; they never execute arbitrary recipe scripts while the network is open.
- The extracted source and staged recipe assets are read-only during Fetch.
- Only the temporary cache destination and private temporary directory are writable during Fetch.
- The build phase receives verified source inputs and an offline view of ecosystem caches.
- [sandbox] network = true and --allow-network remain a separate, explicit build escape hatch. They must not implicitly grant network access to the Fetch phase or vice versa.
- nawa build --offline must never attempt a network request. Missing source or ecosystem cache entries are hard errors with a clear message.
- nawa fetch may populate caches when explicitly run online; nawa fetch --offline, if supported, must only validate existing cache entries.
- Isolation levels that do not create a network namespace (for example, none or env) must not be described as providing real network isolation. If a caller requests strict network isolation and it cannot be enforced, Nawa must fail closed or clearly reject that isolation mode rather than claim the network is blocked.

## 13. Nawa's own recipe and Bootstrap

Nawa itself is a Rust project with Cargo.toml and Cargo.lock in the source repository. Its normal recipe should use the same Cargo backend as other Rust packages:

~~~toml
[fetch.cargo]
dir = "."

[build]
compile = "cargo build --release --offline --locked"
~~~

The Nawa recipe should no longer need a vendor directory or a hand-maintained vendored source tarball once the fetch backend is implemented.

Bootstrap is a special ordering problem: the first Nawa binary cannot use Nawa's own fetch implementation to fetch its own Cargo dependencies.

Therefore:

- Bootstrap must use an independent Seed mechanism available before Nawa is built.
- The Seed input must be pinned and integrity-checked independently (for example, a fixed toolchain plus a lockfile-driven cache populated by an already available Cargo, or a separately maintained seed manifest with verified hashes).
- The Seed mechanism must not depend on invoking Nawa.
- After Nawa is built, normal package builds use Nawa's Fetch Core.
- This proposal does not yet choose the exact Seed implementation. That is a separate design item and must be settled before removing the current vendor-based bootstrap path.
- Do not remove vendor from the active Nawa recipe until the independent Bootstrap path has passed an end-to-end test.

## 14. Lint and validation

Proposed lint findings:

| Finding | Severity |
|---|---|
| [fetch.go] declared but go.mod or required go.sum is missing | error |
| [fetch.cargo] declared but Cargo.lock is missing | error |
| [package.sources] mixed with legacy package.source/package.sha256 fields | error |
| Duplicate source names | error |
| Missing source URL or SHA256 | error |
| Source or asset destination escapes its allowed root | error |
| Declared asset path is missing | error |
| Asset is a special file or escaping symlink | error |
| Two source extractions collide on a file path | error |
| Cargo Git dependency | warning |
| [fetch.*] together with [sandbox] network = true | warning for review |
| Go toolchain requirement is newer than the selected local toolchain | error |
| Build script manually runs go mod download or cargo fetch | warning |

Lint should validate recipe declarations and paths without downloading sources.

## 15. Commands

Proposed user-facing commands:

~~~
nawa fetch <recipe>...
nawa fetch --all
nawa build <recipe>...
nawa build --offline <recipe>...
~~~

- nawa fetch <recipe> populates and verifies external-source and ecosystem caches required by that recipe.
- nawa fetch --all processes recipes that declare external or ecosystem fetch inputs.
- A normal online build prepares missing inputs through the controlled fetch phase, then builds offline.
- nawa build --offline performs no downloads and fails on any missing or invalid cache entry.
- A package without new fetch declarations continues through the existing build path.

## 16. Prototype and test plan

No production implementation should begin until the following cases are covered by a prototype or focused tests.

### Backward compatibility

1. Parse and build representative legacy recipes that use source, sha256, mirrors, and source_dir.
2. Confirm mirrors still mean alternate URLs for the same artifact.
3. Confirm recipes without [assets], [package.sources], or [fetch.*] retain their current behaviour.
4. Reject ambiguous recipes that mix the legacy and new source forms.

### Multiple external sources

1. Download and verify two independent archives with separate hashes.
2. Extract them into separate workspace destinations.
3. Stage one plain file without unpacking.
4. Reject a wrong hash before any build command runs.
5. Reject traversal paths, unsafe archive entries, and destination collisions.
6. Confirm mirrors cannot substitute different artifact contents.

### Local assets

1. Stage a patch, a config fragment, a script, and a nested assets directory.
2. Confirm undeclared sibling files and neighbouring recipes are not visible in the sandbox.
3. Confirm staged assets are read-only.
4. Confirm missing paths and escaping symlinks fail before build.
5. Change one asset without changing recipe.toml and confirm the build ID changes.
6. Confirm a build sees one stable staged snapshot even if the checkout changes during the build.

### Go

1. Fetch a non-vendored Go project.
2. Build it offline with the Go module cache mounted read-only.
3. Confirm go.mod and go.sum remain unchanged.
4. Test a toolchain directive newer than the installed Go with GOTOOLCHAIN=local.
5. Test vendor/modules.txt compatibility.

### Cargo

1. Run cargo fetch --locked for a real non-vendored project.
2. Build it with cargo build --locked --offline.
3. Confirm Cargo.lock remains unchanged.
4. Confirm .package-cache writes stay in disposable overlay/scratch storage.
5. Test registry and Git dependencies separately.

### Cache, isolation, and Bootstrap

1. Confirm two recipes with identical inputs share the same complete cache key.
2. Confirm partial cache population is never accepted as complete.
3. Run nawa fetch --all followed by nawa build --offline with network access blocked.
4. Verify online Fetch cannot run recipe-defined build commands.
5. Prototype the independent Nawa Seed path and build Nawa without its Git vendor directory.
6. Confirm the final package archive, .PKGINFO, repository index, and signing outputs are unchanged.

## 17. Open decisions

1. Whether Cargo target filtering is enabled by default or all lockfile targets are fetched.
2. The exact independent Seed implementation for Nawa Bootstrap.
3. Whether a future asset manifest should be explicit in the repository index or remain covered by the repository snapshot and build-side digest.
4. Whether an optional fetched-tree output hash is useful after the first implementation.
5. The exact name and compatibility check for the build variable that exposes staged assets.

These decisions must not block legacy recipe compatibility. Unresolved implementation details must be documented and tested rather than guessed.

## 18. Alternatives considered

| Design | Why it is not the default |
|---|---|
| Require one hand-made vendor tarball per package | Repeats manual work on every dependency update and couples recipes to language ecosystems |
| Treat patches and scripts as extra source tarballs | Adds needless download/cache/hash machinery for files already maintained with the recipe |
| Put every local file directly into recipe.toml | Makes complex kernel and toolchain recipes too large and difficult to review |
| Give all build scripts network access | Weakens reproducibility and makes builds depend on uncontrolled remote state |
| Put all inputs into one undifferentiated sources list | Confuses upstream artifacts, local recipe files, and lockfile-managed dependencies |

---

**Implementation boundary:** This PR remains design-only. The schema and behaviour above are proposals until the compatibility tests, Go/Cargo prototypes, multi-source staging, Local Assets staging, and independent Bootstrap path have been implemented and reviewed.
