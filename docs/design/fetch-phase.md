# Nawa fetch phase — design

**Status:** Proposal (design only, no code yet)
**Project:** Nawa
**Scope:** Fetching language-ecosystem dependencies (Go modules, Cargo crates) before the offline build
**Integrity model:** Lockfile-verified (`go.sum`, `Cargo.lock`), anchored on the recipe's source `sha256`
**Related:** `docs/build-isolation.md` (§4.4 Network), `src/fetch.rs`, `src/build.rs`

---

## 1. Summary

Builds run with the network closed (`sandbox` and `closure` isolation, net
namespace). That is correct, but Go and Rust projects download their
dependencies at build time. Today a recipe has two ways out, both bad:

1. rely on the upstream tarball shipping `vendor/` (few projects do), or
2. hand-make a vendored tarball and pin it as a second source (the `nawa`
   recipe does this with its vendored cargo config).

Both push work onto the recipe author at every version bump, and (1) simply
does not exist for many projects (netavark, aardvark-dns, ...).

This proposal adds a **fetch phase** between "verify and extract the source"
and "run the build sections":

```
download source -> verify sha256 -> extract -> FETCH -> build (no network) -> install -> archive
```

The fetch phase runs a small, fixed set of commands defined by nawa
(`go mod download`, `cargo fetch --locked`) with the network open, into a
content-keyed cache. The build then runs offline against that cache.

## 2. Goals and non-goals

Goals

1. A Go or Rust recipe needs no vendored tarball and no manual dependency work.
2. The build sections still never see the network.
3. Dependency integrity comes from the project's own lockfile, which is itself
   covered by the recipe's pinned source `sha256`.
4. Fetch results are cached and reusable offline (`nawa fetch --all` before a
   bootstrap, then build with no network at all).
5. What was fetched is visible afterwards (audit manifest, §8).

Non-goals (this document)

* npm, pip and other ecosystems. The recipe schema (`[fetch.<ecosystem>]`) is
  chosen so they can be added later.
* Projects without a lockfile. They are rejected (§5.3).
* Network access during build sections. `[sandbox] network = true` with
  `--allow-network` stays the (impure) escape hatch and is unchanged.
* Pinning a hash of the fetched tree. See §10 for why it is left out and how
  it can be added without breaking recipes.

## 3. Recipe schema

```toml
[fetch.go]
dir = "."          # directory holding go.mod and go.sum (default ".")

[fetch.cargo]
dir = "."          # directory holding Cargo.lock (default ".")
```

Rules:

* Both tables are optional and may appear together (a project with a Go
  backend and a Rust helper).
* `[fetch.go]` adds `golang` and `[fetch.cargo]` adds `rust` to the build
  dependencies implicitly, so recipes do not repeat them.
* `dir` is relative to the extracted source directory and must not escape it.
* Unknown keys inside `[fetch.*]` are an error (keeps room for the reserved
  keys in §10).

## 4. What the fetch phase runs

The fetch phase executes only commands that nawa builds itself. It never
executes recipe scripts, which is what makes it safe to open the network for it.

### 4.1 Go

Preconditions: `<dir>/go.mod` and `<dir>/go.sum` exist.

If `<dir>/vendor/modules.txt` exists the project is already vendored and the
fetch step is skipped (Go builds with `-mod=vendor` by default in that case).
Podman is an example.

Environment (everything else is cleared, including `GOFLAGS`, `GOPROXY`,
`GONOSUMDB`, `GONOSUMCHECK`, `GOPRIVATE`, `GOINSECURE`):

```
GOTOOLCHAIN=local          # never download a toolchain
GOFLAGS=-mod=readonly      # never edit go.mod/go.sum
GOMODCACHE=<fetch cache>
GOPROXY=https://proxy.golang.org   # overridable by NAWA_GOPROXY
```

Command: `go mod download`.

Build-time environment added to every build section:

```
GOMODCACHE=<fetch cache, read-only view>
GOPROXY=off
GOFLAGS=-mod=readonly
GOTOOLCHAIN=local
```

`GOTOOLCHAIN=local` also means a `go.mod` that needs a newer Go than the
installed `golang` package fails with a clear message instead of reaching out.

### 4.2 Cargo

Preconditions: `<dir>/Cargo.lock` exists.

Environment: `CARGO_HOME=<fetch cache>`, `CARGO_NET_GIT_FETCH_WITH_CLI`
unset, everything else cleared.

Command: `cargo fetch --locked` (optionally with `--target <triple>`, default
the build host triple, to avoid fetching crates for other platforms).

Build-time environment added to every build section:

```
CARGO_HOME=<fetch cache, writable overlay view>
CARGO_NET_OFFLINE=true
```

Recipes keep their own `cargo build --locked --release` lines.

## 5. Integrity model

### 5.1 The trust chain

```
recipe sha256  ->  source tarball  ->  go.sum / Cargo.lock  ->  every module / crate
```

* The recipe's `sha256` is already verified before extraction.
* `go.sum` holds a hash for every module version (and its `go.mod`). With
  `-mod=readonly` the Go tool refuses a module that is missing from `go.sum`
  or whose bytes do not match it.
* `Cargo.lock` holds a checksum for every registry crate. `--locked` makes
  cargo fail instead of changing the lockfile.

So a proxy or CDN that serves different bytes is detected without any new
hash in the recipe.

### 5.2 What this does not guarantee

* **No output hash.** nawa does not hash the fetched tree. Two fetches are
  equivalent only because the lockfile says so. That is sufficient for
  integrity, but it means the fetched tree cannot be mirrored or verified by
  its own hash (see §10).
* **Cargo git dependencies** have no checksum in `Cargo.lock`, only a commit
  id. They are accepted but `nawa lint` warns (§9).
* **Availability.** If the proxy drops a module version, the fetch fails.
  Mitigation is a configured mirror (`NAWA_GOPROXY`, a cargo source
  replacement), not a cache of last resort.
* **Lockfile quality.** A lockfile that lists fewer modules than the build
  needs fails at build time, offline, with the tool's own error.

### 5.3 Missing lockfile

`go.sum` or `Cargo.lock` missing is a hard error naming the file. There is no
fallback to an unlocked fetch.

## 6. Cache

Same cache root as source tarballs (`NAWA_SRCCACHE`, default
`<builddir>/distfiles`):

```
<distfiles>/fetch/go-<sha256(go.mod + go.sum)[..16]>/
<distfiles>/fetch/cargo-<sha256(Cargo.lock)[..16]>/
```

* Key is the lockfile content, not the recipe name or version, so two
  recipes (or two versions) with identical dependencies share one entry.
* A directory is populated under a `.part` name and renamed atomically, like
  tarball downloads.
* A cache hit needs no network and no toolchain at fetch time.
* The cache entry is mounted into the build **read-only**, with one wrinkle:
  `cargo` writes a `.package-cache` lock file into `CARGO_HOME` even for
  offline builds. For Cargo the entry is therefore exposed through the
  existing overlayfs machinery (lowerdir = cache entry, throw-away upper under
  the sandbox scratch), the same mechanism the layer cache already uses. When
  overlayfs is unavailable (the existing `sandbox-probe overlay` says no) the
  entry is copied into the sandbox scratch instead.
* Go needs no writes to `GOMODCACHE` for an offline build; to be confirmed by
  the prototype (§11).

## 7. Sandbox and network

The fetch phase reuses the sandbox machinery with the network namespace
**shared** instead of unshared:

* root filesystem read-only;
* writable: the cache entry being populated and a private `/tmp`;
* the extracted source directory visible read-only;
* no recipe-defined commands, only the ones in §4;
* same capability drop and PID namespace as the build sandbox.

Below `sandbox` isolation (`none`, `env`) there is no isolation to preserve,
so fetch simply runs on the host with the sanitised environment.

`nawa build --offline` forbids the fetch phase from touching the network: a
cold cache is an error. This is the mode for bootstrap stages and air-gapped
builds, and pairs with `nawa fetch --all` run beforehand.

## 8. Audit manifest

After a successful fetch nawa records what the build will see, as lines in the
archive's `.PKGINFO`:

```
fetched_go = golang.org/x/sys@v0.28.0 h1:...
fetched_cargo = serde@1.0.215 sha256:...
```

(abbreviated form; the full list can live in a sidecar `<archive>.fetched`
file if `.PKGINFO` size matters.) The values are the lockfile's own hashes, so
this costs nothing to compute and gives a per-package dependency inventory.

`build_id` already covers the recipe bytes and the source `sha256`, hence the
lockfile; the `[fetch.*]` tables are part of the recipe bytes. No change to the
id is needed.

## 9. Lint

New `nawa lint` findings:

| Finding | Severity |
|---|---|
| `[fetch.go]` / `[fetch.cargo]` but lockfile missing | error |
| `[fetch.*]` together with `[sandbox] network = true` | warning (probably unnecessary) |
| Cargo git dependency in `Cargo.lock` | warning |
| `go.mod` declares a newer Go than the `golang` recipe provides | error |
| recipe `build` text runs `go mod download` / `cargo fetch` | warning (should use `[fetch.*]`) |

## 10. Upgrade path: an optional output hash

This design deliberately has no hash of the fetched tree. If one is wanted
later (mirroring by hash, detecting toolchain-dependent differences, Nix-style
fixed-output pinning), it is added as an **optional** key:

```toml
[fetch.go]
dir = "."
sha256 = "..."   # reserved, not accepted yet
```

Absent key means today's behaviour, so existing recipes are unaffected. The
tree would need a canonical serialisation (sorted entries, normalised
mode/mtime/owner, and an ecosystem-specific rule for dropping non-deterministic
files such as Cargo's registry index cache), which is why it is not part of
the first version.

## 11. Prototype plan (before any production code)

1. **Go, offline build from a fetched cache.** `go mod download` into a cache
   directory, then build a real project (podman is vendored, so use a
   non-vendored one) inside the sandbox with the cache mounted read-only and
   `GOPROXY=off`. Confirm no writes are attempted.
2. **Cargo, offline build with overlay.** `cargo fetch --locked`, then
   `cargo build --locked --offline` in the sandbox with `CARGO_HOME` behind an
   overlay. Confirm `.package-cache` is the only write.
3. **Toolchain directive.** A `go.mod` with a `toolchain` line newer than the
   installed Go must fail cleanly under `GOTOOLCHAIN=local`.
4. **Cache sharing.** Two recipes with the same lockfile reuse one entry; a
   different Go minor version reuses it correctly (module cache content is
   keyed by version and verified by `go.sum`).
5. **Offline mode.** `nawa fetch --all` then `nawa build --offline` for netavark
   and aardvark-dns with the network namespace blocked end to end.
6. **Size.** Record the cache sizes for podman-class Go projects and for
   netavark to decide whether `--target` filtering for Cargo is worth it by
   default.

## 12. Commands

```
nawa fetch <recipe>...     populate the fetch cache for these recipes
nawa fetch --all           every recipe with a [fetch.*] table
nawa build ...             runs the fetch phase automatically when the cache is cold
nawa build --offline ...   never touches the network; cold cache is an error
```

## 13. Alternatives considered

| Design | Used by | Why not (here) |
|---|---|---|
| Hand-made vendor tarball as a second source | Fedora, many Arch packages, current `nawa` recipe | Manual work at every version bump |
| Download post-process plus pinned hash of the generated tarball | Buildroot | Ecosystems hard-coded in the tool and a hash to maintain per bump; may be revisited as the §10 upgrade |
| Fixed-output derivation (script plus tree hash) | Nix | Hash maintenance per bump and sensitivity to toolchain version |
| Every dependency listed or packaged separately | Gentoo (`CRATES`, `EGO_SUM`), Debian, Guix | Recipes become very large (hundreds of entries for podman) |
| Typed fetchers inside the build tool | Yocto/BitBake | Same ecosystem coupling as this design, with far more machinery |

## 14. Open questions

1. Whether Cargo `--target` filtering should be the default (§11.6).
2. Whether the audit manifest belongs in `.PKGINFO` or a sidecar (§8).
3. Whether a mirror for Go and crates should be a global config or per recipe.
