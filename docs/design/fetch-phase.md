# Nawa build inputs and fetch phase — design

**Status:** Revision 3, implemented in this PR for delivery phases 0–5 (§16). Phase 6, the bootstrap switch (§13), is not part of it. Open decisions in §17 that the implementation has since settled have not been back-filled yet.
**Project:** Nawa
**Baseline:** the Nawa 1.0.0 tree (`src/recipe.rs`, `src/build.rs`, `src/buildenv.rs`, `src/sandbox/`, `src/fetch.rs`, `src/lint.rs`)
**Review:** revision 2 was checked claim by claim against the 1.0.0 tree; every correction is listed in Appendix B
**Scope:** Multiple external source artifacts, recipe-local assets, and lockfile-based Go/Cargo dependency fetching
**Compatibility goal:** Existing recipes keep working unchanged **and keep byte-identical build ids**
**Package format:** No change to archive layout, `.PKGINFO` keys, repository index, or signing format in v1
**Supersedes:** the last paragraph of `docs/build-isolation.md` §4.4 (the `[[source.extra]]` vendored-tarball answer for cargo/go) and the "everything fetched beforehand and hash-pinned" wording for ecosystem dependencies (see §12)

---

## 1. Summary

A package build can need more than one kind of input:

1. One or more upstream source archives or files.
2. Files maintained beside the recipe, such as patches, scripts, and configuration fragments.
3. Language dependencies managed by tools such as Go modules or Cargo crates.

These inputs must not be conflated. Cargo crates are not extra source tarballs, and a kernel patch maintained by Faseela is not an upstream source archive.

Proposed build pipeline (`[new]` marks what this design adds; everything else exists today):

~~~
resolve recipe
  -> download + verify external sources, extract them          [new: N sources]
  -> snapshot declared local assets                            [new]
  -> closure: assemble the build root                          (existing)
  -> ecosystem fetch (go mod download / cargo fetch --locked)  [new: network allowed,
                                                                fixed commands only,
                                                                inside the build root]
  -> prepare / configure / compile / check / install           (existing: network off)
  -> slim (strip, -doc/-lang split) -> lint -> archive         (existing)
~~~

Sources and assets are staged *before* the build root is assembled so that a wrong hash fails in seconds, not after an hours-long closure assembly. The ecosystem fetch runs *after* assembly because `go` and `cargo` come from the build root (§12).

The build produces the main package plus the `-doc` / `-lang` sub-packages that `slim.rs` already emits. This proposal does not change that.

The fetch phase runs only Nawa-defined ecosystem commands. It never runs recipe build scripts with network access.

### 1.1 Baseline: what Nawa does today

The design below is written against these facts, each verified in the tree:

- `Recipe.package` has `source`, `sha256`, `mirrors`, `source_dir`. `source` may be a URL, `local:<path>` (copied from the host), or empty (meta package: scripts run in an empty `src`).
- `%{version}` & co. are expanded in the source URL, the mirrors and `source_dir`. `fetch::candidate_urls` adds built-in GNU mirror fallbacks to every download; the recipe's `mirrors` are appended after them. Every candidate must still match the pinned sha256.
- Archive vs single file is decided by content (`detect_source_format`), not by file name. A single file is staged into `src/` under its URL file name. The working directory is `src/<source_dir>` (default `{name}-{version}`) when that directory exists, else `src/`.
- The source cache is `$NAWA_SRCCACHE` (default `<builddir>/distfiles`), keyed `<first 16 hex of sha256>-<file name>`, with no lock. `nawa clean sources` clears a *different* directory (`<root>/var/cache/nawa/sources`).
- `build_id = sha256(recipe.toml bytes ‖ source sha256 ‖ sorted closure archive names ‖ profile flags[+ slim version])` (`lint::build_id`). It is computed **before any download**, which is what makes `--use-cache` cheap.
- `SOURCE_DATE_EPOCH` is the newest mtime found under the extracted source tree (`buildenv::source_date_epoch`) unless `package.source_date_epoch` pins it.
- Every build section is a separate `bash -e -c` started in the working directory. Shell state (`cd`, variables) does not carry from one section to the next.
- The build environment is a closed whitelist (`buildenv::whitelist`), pinned by a unit test.
- The sandbox binds exactly three host directories (`src`, `pkg`, `home`) through pre-opened handles (`StagingHandles`) because the build directory and `/var/cache/nawa` are masked. `network = false` means a fresh network namespace; `network = true` skips the namespace entirely and only takes effect with `--allow-network`.
- `SandboxSpec` already has `strict_readonly_root`, `writable_paths`, `readonly_paths`, `run_uid/run_gid` and resource limits (added for plugins).
- `depends.build` is read directly in `deps.rs` (twice), `closure.rs`, `plan.rs` and `commands/querying.rs` (three places).
- `Recipe` and its sub-structs do **not** use `deny_unknown_fields`: unknown keys and tables are silently ignored.
- `repo::pack_recipes` packs the **whole** recipe tree (not only `recipe.toml`) into a deterministic tarball whose sha256 is listed in the signed index, and it refuses symlinks and special files. `nawa sync` verifies and unpacks that tarball.
- `nawa build` takes exactly one package name. There is no `--offline`, no `nawa fetch`, and no recipe-level lint command (`src/lint.rs` is the post-build package lint).

### 1.2 Further facts verified against the tree (revision 3)

Revision 2 was silent on, or wrong about, these. Each is read from the 1.0.0 sources.

- `%{srcdir}` / `NAWA_SRCDIR` are the **working directory** of the sections (`view_src`, which includes the `source_dir` sub-path), not the staging root. `ExpandCtx` is built twice in `build.rs` and once in a `recipe.rs` test.
- `buildenv::source_date_epoch` walks the working directory and counts directories too. A single-file source is staged with `fs::copy`, so its mtime is the staging time. `unpack_tar_stream` unpacks entry by entry (`unpack_in`) and has no deferred directory-mtime pass.
- The sandbox applies `memory_limit_bytes` as `RLIMIT_AS` (address space), `process_limit` as `RLIMIT_NPROC` (which counts threads), plus `RLIMIT_FSIZE` and `RLIMIT_NOFILE`. `readonly_paths` and `writable_paths` name paths **inside** the sandbox. With `strict_readonly_root` every other mount is remounted read-only. The plugin preset is 1 GiB / 32 / 64 MiB / 64 fds / 30 s with the network off.
- `StagingHandles` is a fixed `[OwnedFd; 3]` (`src`, `pkg`, `home`), opened with `O_DIRECTORY` before `/tmp` and the base tmpfs cover the host paths. Inside, `/tmp` is a 2 GiB tmpfs and the base (`/build`) is a 1 MiB tmpfs that only holds bind points.
- The build mask list contains `/run`; so does the plugin mask.
- `fetch.rs` is the only user of `ureq`: every HTTP request in Nawa goes through `fetch::download` / `download_opt`.
- `recipe::load_all` and `locate` match **every** file named `recipe.toml` under the recipes root, at any depth.
- `lint` flags the needles `/build/src` and `/build/pkg` (plus the host build directory) in package contents.
- `cmd_clean(root, category, dry_run)` has no access to the build directory. `clean sources` removes `<root>/var/cache/nawa/sources` wholesale.
- `tar::HeaderMode::Deterministic` writes mode 0755 when the owner-execute bit (`0o100`) is set and 0644 otherwise, with a fixed mtime. Extraction keeps `mode & 0o777`, so the executable bit survives `nawa sync`.
- `lint::build_id` has one caller shape (`build_id_for`) and one unit test; its hashed bytes are recipe, `\0`, source sha256, `\0`, each sorted archive name followed by `\0`, `\0`, flags.

## 2. Goals and non-goals

### Goals

1. Support one or many external source artifacts in one recipe.
2. Keep patches, scripts, and configuration files versioned beside their recipe without extra tarballs.
3. Support Go and Rust dependencies without upstream vendor directories or hand-maintained vendor archives.
4. Keep build commands offline by default.
5. Preserve every existing recipe, including `local:` and meta (empty-source) recipes.
6. Include every build input in the build identity, **without changing the identity of any existing recipe**.
7. Reuse verified caches and support explicit offline operation.
8. Never let an older Nawa silently misbuild a new-schema recipe (§19).

### Non-goals for v1

- npm, pnpm, yarn, pip, Python wheels, or other ecosystems.
- Automatic source or patch generation.
- New multi-package splitting beyond what `slim.rs` already does.
- Running arbitrary recipe commands in the network-enabled fetch phase.
- Changing Nawa's package archive, install, repository publication, or signing formats.
- Requiring a recipe-maintained output hash for fetched dependency trees.
- Restricting fetch-phase network egress to specific hosts (§12 records this as an accepted risk).

## 3. Terminology

**External source:** A file or archive downloaded from an upstream URL and verified against its own SHA256.

**Local asset:** A file tracked in the recipe repository and explicitly declared by the recipe: patches, kernel config fragments, helper scripts, service templates.

**Ecosystem dependency:** A dependency resolved by a language tool using a lockfile (Go module, Cargo crate).

**Source workspace:** The `src/` directory of a build: extracted external sources only. Assets and fetched dependencies are never inside it.

**Fetch cache:** Nawa-managed storage for verified external artifacts and ecosystem dependencies, separate from the recipe repository.

**Fetch** in this document means fetching *build inputs*. `nawa sync` (fetching the *recipe tree*) is unrelated.

## 4. Recipe schema

### 4.1 Legacy syntax is unchanged

Existing recipes keep `package.source`, `sha256`, `mirrors`, `source_dir` with exactly today's meaning (§1.1), including `local:<path>` and the empty-source meta form.

Internally the three legacy shapes (remote, `local:`, none) and the new `sources` array are modelled as one `SourceSet`. **v1 keeps the existing legacy code path in `build()` for legacy recipes** and uses the new engine only when `package.sources` is present. Moving legacy recipes onto the new engine is a later, separate change that requires byte-identical archives under `--rebuild-check` for the whole recipe tree.

### 4.2 Multiple external sources

~~~toml
[package]
name = "example"
version = "1.0.0"
workdir = "kernel"

[[package.sources]]
name = "main"
url = "https://example.org/example-%{version}.tar.xz"
sha256 = "<64 lowercase hex>"
into = "kernel"
strip_components = 1

[[package.sources]]
name = "patch-bundle"
url = "https://example.org/example-patches-%{version}.tar.gz"
sha256 = "<64 lowercase hex>"
into = "patches"
strip_components = 1

[[package.sources]]
name = "bootstrap-data"
url = "https://example.org/example-bootstrap.dat"
sha256 = "<64 lowercase hex>"
unpack = false
into = "data/bootstrap.dat"
~~~

| Field | Required | Meaning |
|---|---:|---|
| `name` | yes | Unique within the recipe; `[a-z0-9][a-z0-9._-]*`, at most 64 characters |
| `url` | yes | `https://` URL; `%{version}`, `%{name}`, `%{rel}`, `%{arch}` are expanded as in legacy recipes |
| `sha256` | yes | Exactly 64 lowercase hex characters (validated at parse time; it becomes a cache path component, so it must never carry `/` or `..`) |
| `mirrors` | no | Fallback URLs for this artifact only |
| `unpack` | no | Omitted = detect by content, like today; `true` = must be an archive (error otherwise); `false` = copy the file as-is |
| `into` | no | Workspace-relative destination. Archive: extraction directory (default `.`). File: destination file path (default: the URL's file name in the workspace root, like legacy) |
| `strip_components` | no | Leading archive path components to remove; default 0 |

Rules:

- Each artifact has its own sha256 and its own cache entry. Built-in GNU mirror fallbacks apply per artifact; no mirror needs trust because the bytes must match.
- `into` and `workdir` must be relative and must not contain `..`. Extraction keeps the existing safe-extraction checks and additionally rejects any write outside its destination.
- `strip_components` is applied **before** path sanitisation, and also to hard-link targets. Entries with fewer components than the strip count are skipped (GNU tar semantics). Today's `extract_source_archive` has no strip support, so this is new extractor work with its own traversal tests.
- Collision detection: each archive is extracted into `<work>/stage/<name>` (same filesystem as `src/`) and its entries are moved into `src/` with `rename`, in array order, so file mtimes survive the merge. Directories merge; a file path claimed twice is an error naming both sources. Nothing is silently overwritten. `unpack = false` files are copied in and stamped (§6). Download order never matters: v1 downloads sequentially (the roadmap's parallel downloader is independent of this design).
- The new form must not be mixed with legacy `source`, `sha256`, `mirrors` **or `source_dir`**. A mixed recipe is rejected at parse time. `local:` sources are legacy-only in v1. The one exception is the schema marker that `package.source` must carry in a new-schema recipe (§19).

### 4.3 `workdir`

`[package] workdir = "kernel"` selects the working directory of every build section, relative to the workspace root (default: the root). It is valid only with `package.sources`, and the directory must exist after assembly.

This field is needed in v1 (the earlier draft deferred it): each build section is its own shell, so "recipes can `cd kernel`" would have to be repeated in every section.

**Placeholders.** `%{srcdir}` / `NAWA_SRCDIR` keep their legacy meaning: the directory the sections start in, which here is `workdir` (default: the workspace root). A recipe that needs the workspace root, for example to reach `into = "patches"` from `workdir = "kernel"`, uses the new `%{srcroot}` / `NAWA_SRCROOT`. Both exist only for recipes that use `package.sources`, so the whitelist of legacy recipes is unchanged. (Revision 2 never said what `%{srcdir}` meant for several sources.)

## 5. Recipe-local assets

### 5.1 Why assets are separate

Patches and configuration files maintained by Faseela live beside the recipe. They are not upstream artifacts and need no network, no per-file hash in the recipe, and no cache.

~~~
recipes/
└── boot/
    └── linux/
        ├── recipe.toml
        └── files/
            ├── patches/0001-faseela-fix.patch
            ├── config/x86_64.fragment
            └── scripts/configure-kernel.sh
~~~

### 5.2 Declaration

~~~toml
[assets]
include = [
    "files/patches",
    "files/config/x86_64.fragment",
    "files/scripts/configure-kernel.sh",
]
~~~

Rules for v1:

- Paths are relative to the recipe directory. `Recipe` gains a `dir` field (not part of the TOML) set by `Recipe::load`, because today a recipe only remembers its bytes.
- A listed directory is included recursively. No globs.
- Absolute paths and `..` are rejected. **Any symlink or special file (device, FIFO, socket) inside a declared path is an error.** This matches `repo::pack_recipes`, which already refuses them when a recipe tree is published, so a synced tree can never contain one; the earlier "escaping symlinks" rule is therefore unnecessary.
- A missing path is a hard error before any download.
- A file named `recipe.toml` anywhere inside a declared path is an error: `load_all` and `locate` match that name at any depth and would parse it as a recipe. `nawa recipe check` reports it and staging refuses it (open decision 10 asks whether to fix this at the source).
- Only declared paths are staged. Neighbouring recipes and the rest of the repository are not visible to the build.

### 5.3 Snapshot, digest, exposure

1. **Snapshot:** declared files are *copied* into `<work>/assets` (a fresh directory outside `src/`) with no-follow opens, regular files only. The digest is computed from the **copy**, in the same pass, so the build can only ever see the bytes that were hashed, even if the checkout changes during the build. The copy is normalised (files 0644 or 0755 by the owner-execute bit, directories 0755, owned by the building user) so that the sandbox's mapped identity can read it, the same reason `grant_source_write` exists for `src/`.
2. **Digest:** sha256 over the records of all files in byte-wise sorted relative-path order: `path \0 x|- \0 size \0 sha256(content)`. The mode contributes **only the executable bit** (`mode & 0o100`, the same test `HeaderMode::Deterministic` applies): `pack_recipes` writes that header mode, so a synced tree and a git checkout differ in every other mode bit and must still produce the same digest. Empty directories are not preserved and are not hashed.
3. **Exposure:** the placeholder is `%{assetsdir}` and the environment variable `NAWA_ASSETSDIR`, matching the existing `%{srcdir}` / `NAWA_SRCDIR` and `%{pkgdir}` / `NAWA_PKGDIR` style (the earlier draft's `%{assets_dir}` was inconsistent with it and is dropped). Neither name collides with an existing placeholder. `NAWA_ASSETSDIR` is added to the environment whitelist **only for recipes that declare `[assets]`**, so the whitelist, and therefore every legacy build, is unchanged.
4. **Read-only:** inside the sandbox the snapshot is bound read-only at `<build_inside>/assets`. At `none`/`env` isolation there is no sandbox: the directory is made non-writable with `chmod`, which is best effort and not a security boundary against root. The document must not claim otherwise.

**Two moments, one digest.** `--use-cache` must decide before `<work>` exists, so the *probe* digest is computed by reading the checkout (`Recipe.dir`). The digest that is *recorded* (the `.buildid` sidecar and `.PKGINFO build_id`) is the one computed from the snapshot. They differ only if the checkout changes between the two steps, and then the recorded id describes what was actually built.

**Extra binds.** `SandboxSpec` gains `extra_binds: Vec<ExtraBind>` with `host`, `inside`, `readonly` and `kind` (`dir` or `file`), `serde(default)` so older specs keep parsing (the same pattern as `root.layers`). Requirements read from `mounts.rs`:

- the handles are opened in `StagingHandles::open`, before `/tmp` and the base tmpfs cover the host paths (a path bind made after the cover fails with ENOENT);
- `inside` lives under `build_inside`, or under a mount point created before the root turns read-only (in `dir` mode: before the pivot, the way `setup_dev` prepares device nodes);
- a read-only bind is remounted with the existing `remount_ro`; a writable one must also be listed in `writable_paths` when `strict_readonly_root` is set;
- a `file` bind creates its placeholder first;
- `SandboxSpec::parse` validates `inside` like it validates `readonly_paths` (absolute, no `..`).

`EnvSpec` and `ExpandCtx` gain optional fields (`assetsdir`, `srcroot`), `None` for legacy recipes. The pinned whitelist test builds `EnvSpec` as a literal, so it needs the new fields with `None`; its expected key list stays the same, and a second test pins the keys for a recipe that has `[assets]`.

### 5.4 Trust

The recipe tree is distributed as one tarball whose sha256 is in the signed repository index, and `pack_recipes` includes every file in the tree (§1.1). Assets are therefore already covered by the existing trust chain. No new signature format and no index change are needed. Consequence: assets are downloaded by every `nawa sync`, so they must stay small. `nawa recipe check` (§14) warns above 1 MiB per file or 5 MiB per recipe (proposed thresholds, adjustable). Large blobs belong in `package.sources` with their own sha256.

## 6. Source workspace layout

~~~
<builddir>/<name>-<version>-<rel>/        (wiped on every build, as today)
├── src/        # external sources only; bound at <build_inside>/src
├── assets/     # snapshot; bound read-only at <build_inside>/assets
├── fetch/      # per-build views of the ecosystem caches (§8, §11)
├── pkg/        # DESTDIR
└── sb/         # sandbox scratch and home
~~~

Observable contract:

- Legacy recipes: working directory and layout exactly as today.
- New recipes: working directory is `workdir` (default the workspace root).
- Legacy recipes: `SOURCE_DATE_EPOCH` is unchanged (newest mtime under the working directory, directories included).
- Recipes with `package.sources`: the pinned `package.source_date_epoch`, else the newest mtime among the **regular files extracted from archive sources**. Directories are ignored because merging and later child creation bump them. `unpack = false` files are ignored because their staged copy carries the staging time; once the value is known, every such file is stamped with it. A recipe whose sources are all `unpack = false` must pin `source_date_epoch` (parse-time error otherwise).
- It must **never** include `assets/` (checkout mtimes would make archives irreproducible) or `fetch/`. This is why assets live outside `src/`, and `--rebuild-check` for a multi-source recipe with a plain file is a required test (§16).
- `grant_source_write` (ownership/permission normalisation) applies to `src/` only.
- `lint` adds `<build_inside>/assets` and `<build_inside>/fetch` to its leak needles for recipes that use them (today: `/build/src`, `/build/pkg`).
- All sources are verified before extraction. Any failed source stops the build before the closure is assembled and before any build section runs.

## 7. Go backend

Preconditions: the selected `dir` contains `go.mod` and `go.sum`. This is checked at fetch time, when the source exists (§14).

The fetch environment is **built from scratch** (an allowlist). The earlier draft's list of variables to clear (including one that may not exist in Go) is replaced by this principle: only the variables below exist.

Fetch environment:

~~~
PATH, TMPDIR, LC_ALL=C, TZ=UTC     (same values as the build whitelist)
HOME=<per-build scratch>            GOPATH=<per-build scratch>
GOTOOLCHAIN=local
GOFLAGS=-mod=readonly
GOWORK=off
GOMODCACHE=<temporary cache entry>
GOPROXY=<proxy>                     GOSUMDB=sum.golang.org
~~~

- `<proxy>` defaults to `https://proxy.golang.org` **without** a `,direct` fallback: `direct` would make Go run VCS tools against arbitrary hosts. `NAWA_GOPROXY` may override it and accepts `https://` or `file://` (the latter keeps tests hermetic).
- Command: `go mod download` in `dir`.
- `go.work` / `go.work.sum` in `dir` or any ancestor inside the workspace is an error (never silently ignored); `GOWORK=off` additionally keeps a workspace file from outside the tree from applying.

Build-time environment (added to the whitelist only for recipes with `[fetch.go]`):

~~~
GOMODCACHE=<per-build view of the cache>   GOPROXY=off
GOFLAGS=-mod=readonly                       GOTOOLCHAIN=local
GOWORK=off                                  GOCACHE=<under HOME scratch>
~~~

Vendored projects: if `dir/vendor/modules.txt` exists, the backend skips the fetch **and the build uses `GOFLAGS=-mod=vendor` instead**. The earlier draft kept `-mod=readonly` in that case, which overrides Go's automatic vendor mode and would make the offline build look in an empty module cache.

Rules:

- Missing `go.mod` or `go.sum` is a hard error. A `go`/`toolchain` directive newer than the local toolchain fails clearly under `GOTOOLCHAIN=local`; no toolchain is ever downloaded.
- Project sources and lockfiles are not modified.
- After a fetch, the entry is validated offline (for example with `go mod verify`) before it is published.
- Whether a read-only `GOMODCACHE` works for the offline build is a prototype question. The fallback is a per-build writable copy (§11).
- The fetch run adds `-modcacherw` to `GOFLAGS`: Go otherwise makes the module cache read-only, and a failed or interrupted populate could not be removed without restoring write permission first. Nawa strips write permission from the published entry itself (§11). The prototype confirms that `go mod download` honours the flag through `GOFLAGS`. A hermetic test with a `file://` proxy sets `GOSUMDB=off`.

## 8. Cargo backend

Precondition: the selected `dir` contains `Cargo.lock`.

Fetch environment (from scratch, same base as §7): `CARGO_HOME=<temporary cache entry>`, `HOME=<per-build scratch>`. Command: `cargo fetch --locked --manifest-path <dir>/Cargo.toml`.

Build-time environment (only for `[fetch.cargo]` recipes): `CARGO_HOME=<per-build view>`, `CARGO_NET_OFFLINE=true`. Recipes keep their own commands, e.g. `cargo build --release --locked --offline`.

Rules:

- `Cargo.lock` is mandatory. `--locked` must fail rather than update it. Registry crate checksums are verified by Cargo.
- **Targets:** v1 fetches all targets of the lockfile (no `--target`). It is the simplest deterministic choice, at the cost of a larger cache. A target filter can come later.
- **Source-tree Cargo config:** Cargo reads `.cargo/config.toml` from the working directory upwards, so a hostile or accidental config could redirect the registry. Rule: if the tree replaces `crates-io` with a `directory` source that exists in the tree, the project is vendored; skip the fetch and build as today (symmetric to Go's vendor rule). Any *other* `[source.*]`, `[registries]`, `[http]` or `[net]` setting in a source-tree config is an error in v1.
- **Git dependencies:** pinned by commit in `Cargo.lock` but without registry checksums. They need `git` in the build root; Nawa warns at fetch time. Registry and git dependencies are tested separately.
- **Cache mutation:** Cargo may write `.package-cache` even offline. Options, in order of preference, decided by the prototype: (1) read-only bind if Cargo tolerates it; (2) overlayfs, only where `nawa sandbox-probe overlay` passes; (3) a per-build **copy**. Never hard-link clones: a Cargo write would modify the shared cache entry.

## 9. Fetch declarations and dependency resolution

~~~toml
[fetch.go]
dir = "."

[fetch.cargo]
dir = "."
~~~

- Both tables may appear. `dir` is relative to the workspace root and must not escape it. Both structs use `deny_unknown_fields`.
- `[fetch.go]` adds the Go toolchain package and `[fetch.cargo]` the Rust toolchain package to the build dependencies. The package names are constants defined in **one** place and **must be checked against faseela-repo before implementation**; this tree does not contain them. The Rust/Go toolchain recipes themselves must not declare these tables (rejected as a self-dependency).
- Implicit dependencies must be visible everywhere, not only in the command environment. Today `depends.build` is read directly in `deps.rs` (×2), `closure.rs`, `plan.rs` and `commands/querying.rs` (×3). All of them switch to a single `Recipe::build_deps()` (declared + implicit, deduplicated), so `nawa deps`, closure assembly, install planning, `nawa info`/`why` and the build id agree.
- The closure must also provide a CA bundle for TLS, and the fetch run needs DNS (§12). Verify the package name for CA certificates in faseela-repo.
- A recipe without `[fetch.*]` runs no backend.
- Typos. `Fetch` and `Assets` are declared structs with `deny_unknown_fields`, so `[fetch.cago]` and `[assets] incude = [...]` are hard errors at parse time. What stays silent is a misspelled *top-level* table (`[fetc.go]`, `[asets]`) or a misspelled key directly under `[package]` (`workdr`). `load_all` parses every recipe on every command, so warnings must not come from `Recipe::load`; they come from `nawa recipe check` and, for the target recipe only, from `nawa build` / `nawa fetch`. Using `%{assetsdir}`, `NAWA_ASSETSDIR` or `%{srcroot}` in a recipe that does not enable it is a hard error at build time, because it would otherwise fail far from the cause.
- The behaviour-neutral `Recipe::build_deps()` refactor lands first (Phase 0, §16).

## 10. Integrity and build identity

### 10.1 External sources

Each artifact is verified against its recipe-pinned sha256 before extraction. A mirror is only an alternate transport for the same bytes. The cache key is the full sha256.

### 10.2 Language dependencies

Go uses `go.sum`; Cargo uses `Cargo.lock` checksums for registry crates and commit pins for git dependencies. Lockfiles come from a source archive whose sha256 is pinned, or from the recipe's assets. A lockfile from upstream is not stronger than upstream's own care; Cargo git dependencies remain a reported caveat.

### 10.3 Build identity

Constraint: the id must be computable **before any download**, because `--use-cache` compares it first. That has two consequences:

- **Lockfile bytes are not an input.** They are inside a source archive whose sha256 is already pinned, or inside assets whose digest is included. Hashing them again would force a download just to decide whether a download is needed. (The earlier draft listed them.)
- The recipe bytes already cover every source name, url, sha256, `into`, `strip_components`, `unpack`, `workdir` and every `[fetch.*]` declaration. Only genuinely new inputs are added:
  - the asset-tree digest, when `[assets]` is present;
  - a per-backend fetch-schema version constant, when `[fetch.*]` is present (so a change in how Nawa fetches invalidates old results);
  - the implicit toolchain dependencies, which already enter through the closure archive names.

For recipes with `package.sources`, the existing `source_sha256` argument becomes the sorted list of `name=sha256`.

**Implementation.** `lint::build_id` keeps its signature and its bytes. A sibling `build_id_with(..., extras: &[(&str, &str)])` appends `\0label=value` for each extra after the flags, and `build_id` calls it with an empty slice. Legacy ids are therefore identical by construction, and the golden test pins that. Extras, in this order: `assets`, `fetch-go`, `fetch-cargo`. The probe/snapshot rule for `assets` is in §5.3.

**Legacy ids must not move.** The new components are appended only when the corresponding table exists; a recipe without `[assets]`, `[[package.sources]]` or `[fetch.*]` hashes exactly the same bytes as today. A golden-id test on a legacy recipe is mandatory (§16). Otherwise one release would invalidate every `--use-cache` sidecar.

### 10.4 Audit data and package format

No change to the archive, the `.PKGINFO` key set (`build_id` merely takes a different value for new-schema recipes), the repository index or signing. Fetch details go to the build log or a build-side record, not `.PKGINFO`.

## 11. Cache design

The root stays `$NAWA_SRCCACHE` (default `<builddir>/distfiles`).

~~~
<srccache>/
├── <sha16>-<file>          # legacy entries, still read
├── sha256/<full-sha256>    # new-form external sources
└── fetch/
    ├── go/<key>/
    └── cargo/<key>/
~~~

Rules:

- **Migration:** a lookup tries the new path first, then the legacy `<sha16>-<file>` path (verified against the full sha256, then copied or linked into the new path). Legacy recipes keep writing the legacy path in v1; nothing is rewritten.
- The sha256 is validated as 64 lowercase hex before it is used as a path component.
- **Ecosystem key:** sha256 over a versioned, unambiguous encoding (length-prefixed fields): `nawa-fetch-v1`, ecosystem, then Go: `go.mod` bytes and `go.sum` bytes; Cargo: `Cargo.lock` bytes **and the Cargo version string**. The Cargo registry directory names embed a hash that has changed between Cargo releases, so a cache from another Cargo is a miss, not a bug. The Go module cache layout is stable, so the Go key does not include the Go version (the prototype must confirm).
- **Publication:** populate in a sibling temporary directory, validate, write a completion marker last, then `rename` atomically. A directory without the marker is never a hit.
- **Concurrency:** an advisory `flock` on `<key>.lock` serialises fetches for the same key. The legacy downloader has no lock and every build of the same file shares one `<file>.part`; new-form source downloads take `sha256/<digest>.lock` and use a unique `.part` name, so two concurrent builds cannot corrupt each other.
- **Immutability:** a published entry is never modified, and Nawa strips write permission from it after publication. Writable state belongs in the per-build view under `<work>/fetch/` (§8).
- The per-build directory is wiped on every build; only the cache outlives it.
- **Retention:** `cmd_clean` takes only `root`, `category` and `dry_run`, so the resolved source-cache path is passed in. `clean sources` removes only what Nawa wrote there: legacy `<sha16>-*` files, `sha256/`, `fetch/`, and leftover `*.part` / `*.lock` files. It never removes the cache root itself, because `$NAWA_SRCCACHE` may point at a shared directory. Entries that may be read-only get `u+w` restored before removal. `--dry-run` as today.
- The sandbox masks the builddir and `/var/cache/nawa`, so any cache a build must read is bound through the extra-bind mechanism of §5.3, not by path.

## 12. Sandbox and network policy

There are three different execution contexts:

| Context | What runs | Network |
|---|---|---|
| Download and verify | Nawa's own code, in-process | yes (unless `--offline`) |
| Ecosystem fetch | `go mod download` / `cargo fetch --locked` | yes (unless `--offline`) |
| Build sections | Recipe scripts | **no** (network namespace), unless `[sandbox] network = true` **and** `--allow-network` |

Rules:

- The fetch toolchain is third-party code that parses untrusted lockfiles and talks to the network, so at `sandbox`/`closure` isolation the fetch **runs inside the sandbox**, in the strict mode plugins use: `strict_readonly_root`, `run_uid/run_gid` dropped when Nawa runs as root. It does **not** reuse the plugin resource preset. 1 GiB `RLIMIT_AS`, 32 `RLIMIT_NPROC`, 64 MiB `RLIMIT_FSIZE`, 64 descriptors and 30 s would kill `go` and `cargo`: `RLIMIT_AS` limits address space, which Go reserves generously, and `RLIMIT_NPROC` counts threads. The fetch profile (starting values, tuned in the Phase 3 spike):

| Setting | Fetch value | Reason |
|---|---|---|
| `memory_limit_bytes` | 0 | address-space cap breaks Go and Rust runtimes; the timeout is the bound |
| `process_limit` | 4096 | threads count |
| `open_files_limit` | 4096 | parallel downloads and module extraction |
| `file_size_limit_bytes` | 0 | a module or crate can exceed any small cap |
| `timeout_secs` | `NAWA_FETCH_TIMEOUT`, default 1800 | separate from the build-section timeout; the sandbox kills the whole PID namespace |

  `/tmp` inside is a 2 GiB tmpfs; very large modules are the reason `TMPDIR` stays there and the cache entry is a bind (below).
- **Mounts.** `src/` and `assets/` are bound read-only (extra binds, §5.3). Writable: `/tmp`, the existing `home` bind (the per-build scratch), and the cache entry being populated, which is an extra bind from the host directory to `<build_inside>/fetch-out`. `writable_paths` names *inside* paths, so both `home` and `<build_inside>/fetch-out` are listed there; strict mode remounts everything else read-only. When Nawa runs as root the dropped identity must be able to write the entry, so the populate directory is a private temporary with mode 0777 (the plugin sandbox does the same for its home). Before publication Nawa normalises ownership and modes and strips write permission (§11).
- **Network.** `network = true` in the spec skips the network namespace entirely. This is an **accepted v1 risk**, stated completely: the fetch process shares the host network namespace. It can reach loopback services and the LAN, **and abstract `AF_UNIX` sockets** (D-Bus and X11 abstract sockets belong to the network namespace; the `/run` mask hides filesystem sockets only). The residual risk is bounded by: fixed commands only, no recipe scripts, a sanitized environment, a read-only root, and the dropped identity when root. An egress proxy is a possible later hardening.
- **DNS and TLS.** Nawa follows the symlink chain of the host's `/etc/resolv.conf` and copies the content, together with `/etc/hosts`, into `<work>/fetch/etc/`, then binds the copies read-only as `file` binds. Binding the host path directly is wrong twice over: an assembled closure root has no `/etc/resolv.conf`, and on systemd-resolved hosts it is a symlink into `/run`, which the mask hides. A stub nameserver such as 127.0.0.53 works only because the network namespace is shared. TLS roots come from the build root (§9); at `sandbox` isolation, from the host.
- **Root modes.** Strict mode has only been exercised with a `live` root (plugins). Strict + `dir`/overlay root + shared network + extra binds is untested and is the first item of the Phase 3 spike (§16). In `dir` mode the placeholders for the resolv.conf/hosts binds must exist before the pivot.
- At `none`/`env` isolation there is no sandbox. The fetch runs as a subprocess with a cleared environment and the host toolchain. This provides no network or filesystem isolation and must not be described as if it did.
- The fetch run's network permission comes only from "a fetch backend is running". It is independent of `--allow-network` and `[sandbox] network`, in both directions.
- **`--offline`.** `fetch.rs` is the only user of `ureq`, so the guard lives in `fetch::download` and `download_opt`: a process-wide flag, set from `--offline` or `NAWA_OFFLINE`, makes them fail with the missing entry and the command that populates it. That covers legacy recipes, mirrors, index downloads and new sources without touching the legacy staging code. Ecosystem backends are not started when offline; missing or incomplete entries are hard errors that name them.
- `--offline` combined with a recipe that sets `[sandbox] network = true` is rejected as contradictory.
- Isolation levels without a network namespace (`none`, `env`) must never be reported as network-isolated. If a caller requires isolation that cannot be enforced, Nawa fails closed instead of claiming it.

`docs/build-isolation.md` must be updated with this design: §4.4's `[[source.extra]]` guidance is replaced (it was never implemented: `recipe.rs` has no such key), and the principle "everything fetched beforehand and hash-pinned" becomes "pinned by checksum": sources by sha256, Go by `go.sum`, Cargo by `Cargo.lock`. That is a deliberate relaxation, and Cargo git dependencies are weaker still.

## 13. Nawa's own recipe and bootstrap

Today the `nawa` recipe and the bootstrap stages build offline from the committed `vendor/` directory and `.cargo/config.toml`, generated by `scripts/vendor-nawa.sh`.

Target state: the published `nawa` recipe uses the normal backend:

~~~toml
[fetch.cargo]
dir = "."

[build]
compile = "cargo build --release --offline --locked"
~~~

The first fetch-capable Nawa cannot fetch its own dependencies, and an older Nawa cannot even parse the new recipe (§19). Proposed direction (the implementation choice is open, but these constraints are fixed):

- The vendored path stays as the only bootstrap path until an independent test has passed end to end. One option is a distinct bootstrap recipe, built only in the early stages and never published, that keeps `vendor/`; the published `nawa` recipe uses `[fetch.cargo]`.
- The Seed input must be pinned and verified independently of Nawa, and must not require running Nawa.
- Removing `vendor/` and removing `.cargo/config.toml` happen in **the same commit** (§8: a source replacement pointing at a missing `vendor/` is an error).
- Do not remove `vendor/` from the active recipe before that test has passed.

## 14. Validation

There was no recipe linter before. `src/lint.rs` is the post-build package lint, so the earlier draft's "Lint should validate..." had no home. Checks are split by *when they can run*, because several findings need the downloaded source and cannot be checked without it:

**A. Parse time (`Recipe::validate`, hard errors, every command):**
mixed legacy/new source forms (including `source_dir`; `source` may only hold the §19 marker); the marker missing when a new table is used, or present without one; duplicate source names; missing `url`/`sha256`; sha256 not 64 lowercase hex; bad `name`; `into`/`workdir`/`dir`/asset path escapes or is absolute; unknown keys under `[fetch]`, `[assets]` and `[[package.sources]]` (declared structs with `deny_unknown_fields`); implicit-dependency self-reference; a `package.sources` recipe whose sources are all `unpack = false` without `package.source_date_epoch` (§6).

Parse-time checks stay cheap and silent: `load_all` runs them for every recipe on every command, so nothing here prints a warning.

**B. `nawa recipe check <name>|--all` (new command, local files only, no network):**
asset path missing; symlink or special file in assets; a file named `recipe.toml` inside an asset path; asset size thresholds (warning); unknown top-level tables and unknown keys directly under `[package]` (warning, read from the raw TOML); unknown `%{...}` placeholders in sections (warning); build script calls `go mod download` or `cargo fetch` itself (warning); `[fetch.*]` together with `[sandbox] network = true` (warning). The command name is open; it must not collide with a built-in, since plugins cannot shadow built-ins. No existing subcommand is called `recipe` or `fetch`.

**C. Stage/fetch/build time (needs the source, or runs for one target recipe):**
`go.mod` / `go.sum` / `Cargo.lock` missing (error); `go.work` present (error); Go requirement newer than the local toolchain (error); disallowed source-tree Cargo config (error); Cargo git dependency (warning); extraction collision (error); `%{assetsdir}` / `NAWA_ASSETSDIR` / `%{srcroot}` used in a recipe that does not enable it (error); the unknown-table and unknown-`[package]`-key warnings of B, for the target recipe only.

## 15. Commands

~~~
nawa fetch <name>... | --all [--isolation LEVEL] [--sources-only] [--offline]
nawa build <name> [--offline]            # still a single name, as today
nawa install | update ... [--offline]    # they build through the same engine
~~~

- `nawa fetch` downloads and verifies external sources, checks assets, and runs the ecosystem backends. The backends need the toolchain, so the command uses the same `--isolation` as `build`: at `closure` it assembles the same build root (using the layer cache). `--sources-only` skips the ecosystem step and needs no toolchain, which is the right CI pre-seed step. `--all` covers every recipe with a remote source or a `[fetch.*]` table, legacy recipes included.
- `nawa fetch --offline` is verify-only: it validates existing entries and exits non-zero if any is missing or incomplete. It is the CI gate before `build --offline`.
- `--offline` applies to legacy recipes too. The guard is in `fetch::download*` (§12), so the legacy staging code needs no other change and keeps byte-identical output.
- A normal online build prepares missing inputs automatically and then builds offline. `NAWA_OFFLINE=1` is the environment equivalent of `--offline`, honored by every command that builds from recipes through `BuildOpts`.
- A recipe with no new tables follows the existing path unchanged. The earlier draft's `nawa build <recipe>...` is dropped: it is not needed and would change the CLI contract.

## 16. Delivery plan and test plan

### Delivery plan (each phase lands green on its own)

0. **Behaviour-neutral refactors**, one commit each, no output change: `Recipe::build_deps()` over the seven direct reads; `Recipe.dir`; optional `assetsdir` / `srcroot` fields on `ExpandCtx` and `EnvSpec` (`None` for legacy); `BuildOpts.offline` plus the guard in `fetch::download*`; `lint::build_id_with`. The golden build-id test, the whitelist test and a legacy `--rebuild-check` land here, so every later phase has a baseline. The legacy `--rebuild-check` doubles as the check that `source_date_epoch` is stable across re-extraction on this tree (the walk includes directories, the unpack is per entry; §1.2).
1. **Schema and sources:** parse + validate (§4, §14A, §19 marker), `workdir`, `%{srcroot}`, `strip_components`, multi-source extraction through `<work>/stage` with `rename` merge, plain-file stamping (§6), new cache layout with legacy read fallback and per-digest locks (§11). No ecosystems yet.
2. **Assets:** snapshot + digest, `%{assetsdir}` / `NAWA_ASSETSDIR`, `ExtraBind` in `SandboxSpec` and `mounts.rs`, build-id extension with legacy ids pinned (§5, §10.3), lint needles.
3. **Spike, then fetch runner + Go.** The spike runs first on a throwaway branch, and its exit criteria gate the rest of the phase: (a) `go mod download` and `cargo fetch --locked` complete under the fetch profile (§12) in strict mode, in a `live` root and in a `dir`/overlay root, as root and unprivileged; (b) DNS and TLS work in a closure root with the copied resolv.conf; (c) a read-only `GOMODCACHE` supports offline `-mod=readonly` builds, and what Cargo needs from a read-only `CARGO_HOME` (answers §17 items 3 and 4); (d) `-modcacherw` is honoured through `GOFLAGS`. Then: sandboxed network run, Go backend, ecosystem cache publication.
4. **Cargo.**
5. **Commands:** `nawa fetch`, `--offline` wiring, `nawa recipe check`, `clean sources` (§15).
6. **Bootstrap switch (§13)** only after its end-to-end test.

### Tests

The existing tests use scratch directories and local listeners, so most can run hermetically: a local HTTP server for source downloads, `NAWA_GOPROXY=file://…`. Live-network ecosystem checks belong in a separate, non-blocking CI job.

**Backward compatibility**

1. Legacy recipes (source/sha256/mirrors/source_dir, `local:`, meta) produce byte-identical archives.
2. Golden test: the build id of a legacy recipe is unchanged by the new code.
3. The environment whitelist is unchanged for legacy recipes (the existing whitelist test stays green).
4. Mixed legacy/new forms are rejected, `source_dir` included.
5. Mirrors remain alternate URLs for the same bytes; built-in GNU fallbacks still apply per artifact.

**Multiple sources**

1. Two archives with separate hashes extract into separate destinations; one plain file is staged unpacked.
2. A wrong hash fails before extraction, before the closure, and before any build section.
3. Traversal paths, unsafe entries, `strip_components` edge cases (including hard-link targets) and destination collisions are rejected.
4. A mirror serving different bytes is refused.
5. `workdir` is honored in every section.
6. `--rebuild-check` passes for a multi-source recipe with assets, which proves `SOURCE_DATE_EPOCH` ignores assets and fetch trees.

**Assets**

1. Patch, config fragment, script and nested directory are staged; undeclared siblings and neighbouring recipes are not visible.
2. Staged assets are read-only inside the sandbox.
3. Missing paths and any symlink or special file fail before any download.
4. Changing one asset without touching `recipe.toml` changes the build id.
5. A checkout change during the build does not affect the staged snapshot.
6. A synced tree (deterministic tar modes) and a git checkout give the same digest.

**Go**

1. Fetch a non-vendored project; build offline; `go.mod`/`go.sum` unchanged.
2. `GOPROXY=off` build works with the chosen cache view (read-only or copy).
3. Toolchain directive newer than the installed Go fails clearly.
4. Vendored project: no fetch, `-mod=vendor` build.
5. `go.work` is rejected.

**Cargo**

1. `cargo fetch --locked` then `cargo build --locked --offline` on a real non-vendored project; `Cargo.lock` unchanged.
2. `.package-cache` writes stay in disposable storage; compare read-only, overlay and copy strategies.
3. Registry and git dependencies tested separately.
4. Source-tree config rules: vendored directory source skips fetch; a registry redirect is an error.

**Cache, isolation, offline, bootstrap**

1. Identical inputs share one complete cache key; a different Cargo version is a miss.
2. A partial cache entry (no marker) is never accepted; concurrent fetches serialise.
3. `nawa fetch --all` then `nawa build --offline` with the network blocked (for example in a network namespace) succeeds; the same without the fetch step fails with a clear message.
4. The fetch run cannot execute recipe-defined commands; the build sections still have no network.
5. A closure-root fetch can resolve DNS and verify TLS.
6. Package archive, `.PKGINFO` keys, repository index and signing outputs are unchanged.
7. The independent bootstrap path builds Nawa without `vendor/` (gate for §13).

**Added in revision 3**

1. A recipe with an `unpack = false` file builds identical archives at two different times (the file's staging time never reaches `SOURCE_DATE_EPOCH`).
2. A `package.sources` recipe whose sources are all plain files and that does not pin `source_date_epoch` is rejected at parse time.
3. `%{srcdir}` equals `workdir` and `%{srcroot}` equals the workspace root; a legacy recipe sees neither new name.
4. A `recipe.toml` inside an asset path is refused; the probe digest equals the snapshot digest when nothing changes.
5. The fetch run succeeds under the fetch resource profile and, as a control, fails under the plugin preset (this documents why the preset is not reused).
6. DNS resolves when the host's `/etc/resolv.conf` is a symlink into the masked `/run`.
7. With `--offline`, a legacy recipe with a cold cache fails without opening a socket (a local listener sees zero connections).
8. Removing a populated Go cache entry as a non-root user succeeds (`u+w` restored); `clean sources` never removes the cache root and leaves unrelated files in a shared `$NAWA_SRCCACHE` alone.
9. Two concurrent builds that need the same new-form source download it once and neither sees a truncated file.
10. An old binary (the 1.0.0 build) given a recipe that carries the §19 marker stops with the marker's message and does no download and no closure work.

## 17. Open decisions

Resolved in revision 2: Cargo targets (fetch all in v1, §8); whether assets need an index manifest (no, the recipe tarball already covers them, §5.4); the asset variable name (`%{assetsdir}`, §5.3).

Resolved in revision 3: the meaning of `%{srcdir}` and the new `%{srcroot}` (§4.3); the `SOURCE_DATE_EPOCH` rule for multi-source recipes (§6); where `--offline` is enforced (§12); the fetch resource profile's shape (§12; the numbers stay open); a fail-closed marker for old binaries (§19).

Still open:

1. Exact toolchain and CA-bundle package names in faseela-repo (§9).
2. The Seed / bootstrap implementation (§13).
3. Read-only vs overlay vs copy for the Cargo and Go cache views (§7, §8).
4. Whether the Go cache key may omit the Go version (§11).
5. Whether to restrict fetch-phase egress later (§12).
6. Whether and when to move legacy recipes onto the new engine (§4.1).
7. Name of the recipe lint command and the asset size thresholds (§14, §5.4).
8. Whether an optional fetched-tree output hash is useful after the first implementation.

9. Final numbers of the fetch resource profile (§12), after the Phase 3 spike.
10. Whether `load_all` / `locate` should treat a directory that contains a `recipe.toml` as a leaf, so asset files can never be parsed as recipes. It needs a check of faseela-repo for recipes nested under other recipes; until then the §5.2 rule applies.

None of these may block legacy compatibility. Unresolved details are documented and tested, not guessed.

## 18. Alternatives considered

| Design | Why it is not the default |
|---|---|
| One hand-made vendor tarball per package (`[[source.extra]]`, build-isolation §4.4) | Manual work on every dependency update; couples recipes to ecosystems; never implemented in `recipe.rs` |
| Patches and scripts as extra source tarballs | Needless download/cache/hash machinery for files already versioned with the recipe |
| Put every local file into `recipe.toml` | Unreviewable for kernel and toolchain recipes |
| Give build scripts network access | Weakens reproducibility; builds depend on uncontrolled remote state |
| One undifferentiated `sources` list | Confuses upstream artifacts, local files and lockfile-managed dependencies |
| Run ecosystem fetch with the host toolchain, outside the build root | Toolchain mismatch between fetch and build; not reproducible |
| Run ecosystem fetch inside the normal build sections with `[sandbox] network = true` | Hands the network to arbitrary recipe scripts |

## 19. Compatibility with older Nawa binaries

`Recipe` does not use `deny_unknown_fields`. An older Nawa therefore **silently ignores** `[[package.sources]]`, `[assets]`, `[fetch.*]` and `workdir`, and treats a new-schema recipe with an empty `package.source` as a meta package, running its scripts in an empty directory.

**Fail-closed marker (new in revision 3).** An old binary can be made to refuse a new-schema recipe without changing it, because its source staging handles `package.source = "local:<path>"` by failing when the path does not exist, before any download or closure work (`build.rs`). A recipe that uses any new table (`package.sources`, `[assets]`, `[fetch.*]`, `workdir`) must therefore also set

~~~toml
source = "local:/nawa-schema-2-requires-a-newer-nawa"
~~~

An old binary stops with "local source /nawa-schema-2-requires-a-newer-nawa does not exist", which names the problem. The new Nawa treats exactly this value as the schema marker: it is required together with new-schema tables, rejected without them, and never staged. It is the only mechanism here that fails closed for binaries that are already deployed. The cost is one boilerplate line per new-schema recipe. Because it is a deliberate trade-off, revision 3 records it as a recommendation: if it is rejected, the process gate below is the whole protection.

**Process gate (baseline, kept).** No new-schema recipe lands in faseela-repo until every Nawa that builds that tree supports it, and the first release with this feature is announced in `CHANGELOG.md` as a schema change. As a forward guard, new Nawa honours an optional `[package] nawa_min = "x.y.z"` (older binaries ignore it, so it protects future schema steps, not this one), and all *new* tables use `deny_unknown_fields`.

## Appendix A — Changes from the first draft

- Pipeline: added the closure-assembly ordering and the slim/lint stages; sources and assets are staged first, ecosystem fetch runs inside the assembled root.
- Corrected "one package": `slim.rs` already emits `-doc` / `-lang` sub-packages.
- Legacy forms now include `local:` and empty (meta) sources; legacy code path stays until byte-identical equivalence is proven.
- Added `workdir` (sections do not share shell state); `source_dir` joins the no-mix rule; `unpack` is tri-state (omitted = detect); strict sha256 and `name` validation.
- Assets: copy-then-hash snapshot; only the executable bit hashed (matches `pack_recipes`); any symlink rejected; `%{assetsdir}` / `NAWA_ASSETSDIR` naming; whitelist change only for asset recipes; extra sandbox binds specified.
- `SOURCE_DATE_EPOCH` explicitly excludes assets and fetch trees.
- Build id: lockfile bytes removed (not computable before download); legacy ids pinned by a golden test; only genuinely new inputs added.
- Go: `-mod=vendor` for vendored projects; no `,direct`; `GOWORK=off` plus `go.work` detection; allowlist environment; `file://` proxy for tests.
- Cargo: all targets in v1; source-tree config rules; no hard-linked cache clones.
- Cache: legacy-layout migration, path-safe keys, Cargo version in the key, `clean sources` integration.
- Network: sandboxed fetch run reusing the plugin strict mode; DNS and CA requirements; accepted egress risk recorded; `--offline` / `--allow-network` interaction; build-isolation §4.4 explicitly superseded.
- Validation split by when it can run; a recipe lint command is introduced because none existed.
- Commands match today's CLI (`build` takes one name); `fetch` needs `--isolation` and `--sources-only`.
- Implicit dependencies go through one `Recipe::build_deps()` replacing seven direct reads.
- Bootstrap: concrete constraints and same-commit rule; new §19 on older binaries; delivery plan added.

## Appendix B — Review of revision 2 (changes in revision 3)

Revision 2 was compared with the 1.0.0 sources. Its baseline facts (§1.1) were accurate; the problems were in what the design asked the existing code to do.

| # | Finding in revision 2 | Evidence | Fix |
|---|---|---|---|
| 1 | Fetch "reuses the plugin strict mode ... resource limits set". The plugin preset would kill `go` and `cargo`. | `memory_limit_bytes` is `RLIMIT_AS`, `process_limit` is `RLIMIT_NPROC` (threads); preset is 1 GiB / 32 / 64 MiB / 64 fds / 30 s | Dedicated fetch profile, §12 |
| 2 | `writable_paths` described as limited to "the cache entry". They are inside paths, not host paths. | `spec.rs`, `remount_mount_tree_ro` | Writable extra bind plus `writable_paths`; 0777 populate directory for the dropped identity, §12 |
| 3 | Extra binds specified without the mechanics. | `StagingHandles` is `[OwnedFd; 3]`, `O_DIRECTORY`, opened before the covers; base tmpfs is 1 MiB | `ExtraBind` with `dir`/`file`, ordering and `writable_paths` rules, §5.3 |
| 4 | Fetch binds the host `/etc/resolv.conf`. On systemd-resolved hosts it is a symlink into `/run`, which the mask hides. | `prepare_sandbox` mask list | Bind a copy of the resolved content plus `/etc/hosts`, §12 |
| 5 | Accepted network risk omitted abstract `AF_UNIX` sockets. | They belong to the network namespace that `network = true` shares | Risk stated completely, §12 |
| 6 | `%{srcdir}` undefined for several sources. | Legacy `%{srcdir}` is the working directory (`view_src`) | `%{srcdir}` = `workdir`, new `%{srcroot}`, §4.3 |
| 7 | "`--rebuild-check` must pass" for a recipe with a plain-file source. It could not. | `fs::copy` stamps the staging time; the SDE walk includes it and directories | SDE rule, stamping, pinned-epoch requirement, §6 |
| 8 | "Extract privately, then merge" without saying how mtimes survive. | `unpack_in` per entry; directory mtimes are bumped by children | `rename` merge from `<work>/stage`, §4.2 |
| 9 | Asset files named `recipe.toml` would be loaded as recipes. | `load_all` / `locate` match the name at any depth | Rule in §5.2, check in §14, open decision 10 |
| 10 | Snapshot readable by the sandbox identity and the exact "executable bit" test unspecified. | `grant_source_write`; `HeaderMode::Deterministic` uses `0o100` | Normalisation and `mode & 0o100`, §5.3 |
| 11 | One asset digest used for the cache probe and for the record, but the probe runs before `<work>` exists. | `build()` probes before `create_dir_all(work)` | Probe digest from the checkout, recorded digest from the snapshot, §5.3 |
| 12 | Build-id extension would have changed `build_id`'s signature and test. | `lint::build_id`, one unit test | `build_id_with` with extras, §10.3 |
| 13 | `clean sources` "extended to the new layout". `cmd_clean` has no build directory, `$NAWA_SRCCACHE` may be shared, Go caches are read-only. | `repositories.rs` | Plumbing, safe deletion set, `u+w` restore, §11 |
| 14 | Downloads "serialised" only for ecosystem keys. | Legacy `download_cached` shares one `.part`, no lock | Per-digest lock and unique `.part` for new-form sources, §11 |
| 15 | `--offline` had no enforcement point. | `fetch.rs` is the only `ureq` user | Guard in `fetch::download*`, covers legacy too, §12 and §15 |
| 16 | §9 said `Recipe::validate` warns; §14 put the warning in `recipe check`; `load_all` would repeat it for every recipe. `[fetch.cago]` is already a hard error through the struct. | `load_all` runs `Recipe::load` for every recipe | One rule, §9 and §14 |
| 17 | Delivery plan had no `build_deps()` refactor, no baseline tests first, and prototype questions without exit criteria. | Seven direct `depends.build` reads | Phase 0 and the Phase 3 spike, §16 |
| 18 | Go cache entries are read-only by default, which blocks cleanup. | Go module cache behaviour | `-modcacherw` in the fetch run, §7 |
| 19 | Old binaries protected by a process gate only. | `Recipe` ignores unknown keys | Fail-closed `local:` marker, §19 |

Verified and unchanged: the §1.1 baseline; legacy path kept for legacy recipes; assets covered by the signed recipe tarball (`pack_recipes` refuses symlinks, uses `HeaderMode::Deterministic`); the seven `depends.build` reads; the pinned whitelist; `clean sources` pointing at a different directory than the source cache; no existing `fetch` or `recipe` subcommand.

Unverified, to be checked in the spike or Phase 0: that `go mod download` honours `-modcacherw` from `GOFLAGS`; what Cargo needs from a read-only `CARGO_HOME`; whether legacy `source_date_epoch` is stable across re-extraction (directory mtimes).
