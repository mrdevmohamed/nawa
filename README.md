# nawa

A source-based package manager for from-scratch Linux distributions, written in
Rust. It builds packages from declarative TOML recipes, installs them atomically
with rollback, signs and verifies archives (ed25519), and can build in
increasingly strict isolation (env / namespaces sandbox / closure root).

nawa was developed inside the mydistro distribution and is now its
own project. Current version: **1.0.0**.

## Install

Release binaries are **static** (musl, no shared libraries, no libc requirement),
so the same file runs on any x86_64 Linux distribution:

```
curl -LO https://github.com/<owner>/nawa/releases/download/v1.0.0/nawa-1.0.0-x86_64-linux-musl
curl -LO https://github.com/<owner>/nawa/releases/download/v1.0.0/nawa-1.0.0-x86_64-linux-musl.sha256
sha256sum -c nawa-1.0.0-x86_64-linux-musl.sha256
install -m755 nawa-1.0.0-x86_64-linux-musl /usr/local/bin/nawa
```

## Build from source

All crates are vendored (`vendor/`, `.cargo/config.toml`), so no network is needed:

```
cargo build --release --offline --locked        # glibc binary, dynamic
cargo test  --release --offline --locked

# portable static binary
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools                     # musl-gcc: bzip2/zstd/ring contain C code
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --offline --locked --target x86_64-unknown-linux-musl
```

Re-vendor after changing dependencies with `scripts/vendor-nawa.sh`.

## Layout

```
src/        the package manager
tests/      integration tests (build engine, isolation, end to end)
examples/   mkfake (test archive builder), dump
scripts/    smoke tests and the isolation matrix (used by CI)
docs/       design notes (build-isolation.md)
vendor/     vendored crates (offline builds)
```

nawa does not ship recipes. Point it at a recipe tree with `--recipes` (or
`recipes` in `nawa.toml`) and at build profiles with `--profiles`.

## Commands

Commands (global flags work anywhere on the line — see the config section
below for making them unnecessary):

```
nawa install <name>...                          # repo when available, otherwise build + install
nawa install -n <name>                          # show that plan, change nothing (--dry-run)
nawa outdated                                   # installed packages whose recipe is newer
nawa update [name...]                           # rebuild + reinstall outdated packages (default)
nawa update --from repo [name...]               # update from the binary repository only
nawa update --from auto [name...]               # prefer repo; build when repo is missing/stale
nawa autoremove                                 # remove dependency packages nothing needs
nawa build --install <name>                     # build ONE recipe and install it (no dependency handling)
nawa install <archive.nawa.tar.zst>              # install a built archive into --root
nawa search <term>                              # find recipes by name/summary (--glob for patterns)
nawa info <name>                                # everything known about a package
nawa why <name>                                 # who depends on it (--tree also scans recipes)
nawa deps <name>                                # show dependency order
nawa list [pattern] [-e|-d]                    # installed packages; -e explicit only, -d dependencies only
nawa files <name>                               # files owned by package
nawa owner /usr/bin/foo                         # which package owns file
nawa revdep [name...] [--unowned] [--json]      # broken shared-library links (exit 1 if any)
nawa remove --yes <name>                        # remove package (--yes skips the confirmation)
nawa repo-create <dir-of-archives> --out index.toml --sign  # legacy v1 index
nawa repo-create <dir-of-archives> --index-version 2 --sign # signed-index v2 (base URL)
nawa repo-create <dir-of-archives> --index-version 3 --locations locations.toml --sign # distributed storage
nawa upgrade --yes --repo <dir-or-url>          # upgrade from a repo (repo also comes from nawa.toml)
nawa rollback <name>                            # restore previous version
nawa service list                               # runit services
nawa config show                                # effective config + where each value came from
nawa init                                       # write a commented nawa.toml template
nawa completions bash|zsh|fish                  # shell completion scripts
```

Any command accepts `--root` so everything can be tested safely against a
throwaway directory instead of `/`.

### Installing by name

```
nawa install curl          # shows the plan; uses repo when available, otherwise builds
```

`nawa install <name>` resolves `depends.build` **and** `depends.run`
transitively, skips whatever is already installed, and prints the order
before touching anything:

```
nawa: 3 package(s) to install into / (2 from the repository, 1 to build)
    #  Package                 Version       Source  Download  Installed  Reason
    1  zlib                    1.3.1-1       repo    ...       ...        dependency
    2  openssl                 3.3.1-1       repo    ...       ...        dependency
    3  curl                    8.9.1-1       build   -         -          explicit
```

Each step is fetched from the repository or built according to the selected
source policy, then installed. The database lock is held only around each
install, and a failure in the middle says how many packages already landed —
re-running the same command continues, because installed packages are skipped.
Packages you named are recorded as `explicit`, everything pulled in as
`dependency`.

* `-n/--dry-run` prints the plan and stops; `-y/--yes` is required when stdin
  is not a terminal (same contract as `remove` and `upgrade`)
* `--no-deps` installs/builds only the named packages
* `--isolation`, `--use-cache`, and `--allow-degraded` affect source builds;
  `--force` and `--allow-unsigned` also apply to repository/archive installs
* a bare word is a package name; anything ending in `.nawa.tar.zst` or
  containing a `/` is an archive path (the two cannot be mixed in one call)

`nawa outdated` lists installed packages whose local recipe has a newer
`version-rel`. `nawa update` plans those packages plus any newly required
missing dependencies, preserving each package's explicit/dependency reason.
It builds from source by default. Use `nawa update --from repo` to require
repository packages only, or `--from auto` to prefer the repository and build
only when the repository lacks a package at least as new as its recipe. A
repository-only update preflights the complete plan and refuses missing or
older repository packages before changing the target system. `--repo DIR_OR_URL`
selects a repository for that invocation. `nawa upgrade` remains the direct
repository-index updater: it can update installed packages even when the local
recipe tree has not yet been synced.
`nawa autoremove` removes `dependency` packages that no explicit package needs
any more, dependents first. Build-only dependencies of packages you keep
(compilers, headers) are **kept** unless you pass `--build-deps`, since the next
rebuild needs them.

### Configuration (nawa.toml)

nawa reads the nearest `nawa.toml` at or above the current directory (like git;
`NAWA_CONFIG` names one directly) and merges it **under** the command-line
flags:

```toml
root      = "/"                                  # env: NAWA_ROOT
recipes   = "recipes"                            # env: NAWA_RECIPES
builddir  = "build"                              # env: NAWA_BUILDDIR
profiles  = "profiles"                           # env: NAWA_PROFILES
jobs      = 8                                    # env: NAWA_JOBS
repo      = "https://mirror.example.com/stable"  # env: NAWA_REPO — install/upgrade; update with --from repo|auto|ask
```

Precedence per key: **flag > env var > nawa.toml > default**. Relative paths
inside the file resolve against the file's own directory, so the config means
the same thing from any subdirectory. `nawa init` writes a commented template;
`nawa config show` prints the effective values annotated with where each one
came from.

### Confirmations, colors, suggestions

* `remove`, `upgrade`, and source-based `update` print their plans and ask
  `y/N`. Non-interactive runs must pass `--yes`; piped stdin is refused without
  it. `install` does not use this second confirmation, although
  `install_mode = "ask"` can prompt for repo vs build on a terminal.
* Output is colored only on a real terminal; pipes and scripts get plain,
  byte-identical text. `NO_COLOR` disables colors, `NAWA_COLOR=always|never|auto`
  overrides the detection.
* Mistyped package names get suggestions built from the recipe graph:
  `nawa build gzp` → *did you mean `gmp` or `gzip` or `gcc`?*

### Shell completions

```
nawa completions bash | sudo tee /etc/bash_completion.d/nawa   # then restart the shell
nawa completions zsh > "${fpath[1]}/_nawa"                     # zsh
nawa completions fish > ~/.config/fish/completions/nawa.fish   # fish
```

Package names complete dynamically — the scripts shell out to
`nawa __complete_pkgs`, which lists every recipe plus every installed package.

### Database locking

Every command that touches `<root>/var/lib/nawa` takes an `flock(2)` lock on
`<root>/var/lib/nawa/lock`: `remove`, `rollback`, and `upgrade` hold an exclusive
lock for the whole command; archive installs do too, while name-based `install`
and `update` lock around each package install (not the build/download). `query`, `list`,
`files`, `owner`, `info`, `why`, `deps`, `deps-all` and `upgrade --dry-run`
shared, so readers never see a half-applied transaction and don't block each
other (`build --install` locks around the install step only, never through
the build itself). A second nawa waits
and prints who holds the lock; `NAWA_LOCK_TIMEOUT=<seconds>` makes it give up
instead. The kernel releases the lock when the holder exits for any reason, so
there are no stale lock files to clean up. Package hooks get `NAWA_LOCK_HELD=1`
so a hook that calls nawa does not wait on its own parent.

### Build profiles

`profiles/<arch>.toml` (selected by the recipe's first `arch`) sets the flags
every package is compiled with. The build engine exports them to all build
sections as `CFLAGS`, `CXXFLAGS` and `LDFLAGS` (empty values are not exported)
and uses the profile's `jobs` as the default job count:

```
--jobs N                  # highest priority
profile `jobs`            # next
number of CPUs            # fallback
```

A missing profile is fine (toolchain defaults); flags already present in nawa's
own environment win over the profile (`CFLAGS=-O3 nawa build foo`). `--profiles
<dir>` selects another directory (default `profiles`; the bootstrap stages pass
it explicitly).

Build output is streamed to the console as it is produced and also saved to
`<builddir>/<pkg>-<ver>-<rel>/build-<section>.log` (stdout and stderr merged,
in real order).

### Slim packages (strip, cleanup, `-doc` / `-lang`)

After a recipe's `install` finishes, the build engine slims the staged tree
before it writes the archive — what Alpine's `abuild` does, with no recipe
changes needed:

* **cleanup** — libtool `*.la`, `share/info/dir`, `charset.alias`, perl's
  `perllocal.pod` / `.packlist` are removed (they only cause file conflicts).
* **split** — man pages, info and docs (`usr/share/{man,info,doc,gtk-doc}`)
  go to `<name>-doc`, gettext catalogs (`usr/share/locale`, except
  `locale.alias`) to `<name>-lang`. Both are ordinary archives of the same
  version that depend on `<name>`: `nawa install bash-doc`. A split that would
  leave the main package empty is skipped.
* **strip** — `strip --strip-unneeded` on ELF executables and shared objects,
  `--strip-debug` on `*.a`, `.o` and unsigned kernel modules. ELF files are
  found by header, not by name; `rustlib`, `/boot`, `/usr/lib/debug`,
  `/usr/src` are never touched. Without `strip` on the host the pass is
  skipped with a note.

Opt out per recipe with a `[slim]` table (`strip = false`, `cleanup = false`,
`split = false`), or for every package with `slim = false` in the profile's
`[build]` table; `NAWA_SLIM=0|1` overrides both. The slimming rules are part
of the build id, so `--use-cache` rebuilds archives made by older rules.

### Build isolation

`nawa build --isolation <level>` (or the profile's `[build] isolation`; default
`none`) controls what a build can see and do — see
`docs/build-isolation.md` for the full rationale:

| level | the build gets |
|-------|----------------|
| `none` | today's behaviour (nawa's environment, live filesystem) |
| `env` | only the whitelisted environment (LC_ALL=C, TZ=UTC, umask 022, ...), `SOURCE_DATE_EPOCH`, byte-reproducible archives; `--rebuild-check` verifies exactly that |
| `sandbox` | + user/mount/pid/net/ipc/uts namespaces: read-only root, private `/tmp` `/dev` `/proc`, no network, fake-root identity, per-section timeout |
| `closure` | + a build root assembled from built archives only (`base-devel` + the declared `depends.build`), so a missing declaration fails the build |

A post-build lint runs from `env` up: it warns about `DT_NEEDED` libraries
missing from `depends.run` and errors on leaked build paths or bad layout.
`nawa sandbox-probe` (hidden) tells you what this kernel allows; restricted
environments need `--allow-degraded` (loudly, never silently weaker).

On top of that:

- **Layer cache**: closure archives are extracted once into a content-addressed
  layer cache (`NAWA_LAYER_CACHE`, default `<root>/var/cache/nawa/layers`) and
  the build root is an overlayfs over them (kernel ≥ 5.11; older kernels
  automatically fall back to extract-per-build).
- **`--use-cache`**: skip a build whose `build_id` (recipe + source sha256 +
  closure + profile flags) matches the recorded one.
- **Rootless**: `nawa build` up to `closure` works as a normal user — no root
  required at build time.

### Signing & trust (nawa-sig-v1)

Nawa uses Ed25519 signatures over the SHA-256 digest of the exact file bytes.
The signed message is `nawa-v1:sha256:<hash>`; signatures are detached
`.sig` sidecars. The cryptographic format is unchanged by repository index
schema v2.

```
nawa keygen --out /etc/nawa/keys              # nawa.sec (0600) + nawa.pub
nawa sign <archive-or-index> --key nawa.sec   # writes <file>.sig
nawa --root / trust /etc/nawa/keys/nawa.pub  # add publisher key to trust store
nawa verify <file>                           # standalone file: trust store, else embedded key
```

#### Repository index schemas

**Legacy v1** is identified by the absence of a `[repository]` table. It keeps
the original policy: a trusted index authenticates the package list, but every
package archive also needs its own detached `.sig` when trusted keys are
configured. This behavior remains supported.

**Signed index v2** adds explicit authentication metadata:

```toml
[repository]
schema = 2
archive_auth = "signed-index"
hash_algorithm = "sha256"
index_signature = "nawa-sig-v1"
```

Create it with:

```sh
nawa repo-create ./packages --index-version 2 --sign --key nawa.sec
```

V2 requires `--sign`. Nawa activates index-based archive authentication only
when `index.toml.sig` verifies against a public key in the configured trust
store. In that case each package's SHA-256 and size in the signed index
authenticate the archive bytes; per-package `.sig` files are not fetched or
required. Nawa still validates the archive's `.PKGINFO` against the signed
entry and rechecks the digest immediately before extraction.

A v2 declaration by itself is not a trust anchor. If the index is self-signed
by an embedded, untrusted key, or if an unsigned index is explicitly accepted,
Nawa falls back to the legacy detached-archive-signature policy. A present but
invalid/unknown-key index signature is always rejected; `--allow-unsigned`
does not turn an invalid signature into an unsigned success.

Standalone archive installation (`nawa install ./file.nawa.tar.zst`) and
`nawa verify` are unchanged: they still use the detached-signature policy.
V1 repositories are unchanged as well.

Old Nawa binaries do not understand v2's signed-index archive-authentication
rule. During a mixed-client transition, publishers must continue publishing
package sidecars alongside the v2 index. Remove them only after the supported
client floor includes a v2-capable Nawa build.

Remote repositories still require a configured trusted key unless
`--allow-unsigned` is explicitly passed. Add the distribution key first:
`nawa --root / trust /path/to/nawa.pub`.

### Atomic upgrades & rollback

* every install stages into a hidden dir, conflict-checks against the db,
  then renames into place through an undo journal: files it replaces are
  first moved to `.nawa/bak-*`, and if any move fails everything is restored
  (all-or-nothing file placement); db writes are temp-file + rename
* replaced versions leave an undo record (`/var/lib/nawa/undo/<name>`) and
  their archive in the package cache (`/var/cache/nawa`) — `nawa rollback <name>`
  walks one step back, and keeps ping-ponging as long as there is history
* `nawa upgrade --repo <dir>`: verifies the index signature, checks sha256,
  resolves order through the recipe graph, then applies
* pacman-style config protection: `/etc` files are hashed at install time
  (`<pkg>/hashes` in the db). Untouched configs upgrade in place; locally
  modified ones are preserved and the new version lands next to them as
  `<path>.nawa-new`
* files dropped between versions are removed; obsolete-file deletion only
  touches paths the package still owns

### Remote repositories (http/https)

`nawa upgrade --repo` accepts a local directory **or** an http(s) URL:

```
nawa --root / upgrade --repo https://mirror.example.com/mydistro/stable
```

* the index (`index.toml` + optional `index.toml.sig`) is fetched fresh on
  every run; if the mirror is unreachable, the cached copy is used with a
  warning (so upgrades still run offline once a mirror was seen)
* the index signature goes through the same trust policy as local repos
* archives are downloaded on demand into a stable per-URL cache under
  `/var/cache/nawa/remote/<key>/`, sha256-checked against the index on every
  reuse (a corrupted cache entry is transparently re-downloaded)
* archive paths recorded in the index are validated — no `/`, no `..`, no
  dotfiles — so a compromised index cannot write outside the cache
* packages without a recipe in the local tree (third-party repo content)
  are applied in alphabetical order after the graph-ordered ones

### Service management (runit)

```
nawa service list                 # /etc/sv tree + enabled state
nawa service enable sshd          # link into /etc/runit/runsvdir/default
nawa service disable dhcpcd       # remove the link (runsvdir stops it live)
nawa service status [name]        # static state + live `sv status` when supervised
```

A service is *available* when `/etc/sv/<name>/run` exists and *enabled* when
`/etc/runit/runsvdir/default/<name>` links to it — runsvdir rescans that
directory every few seconds, so enable/disable take effect without a
supervisor restart. `disable` refuses to remove anything that is not a
symlink, and `enable` refuses services without a run script.

## Recipe format (example)

```toml
[package]
name    = "gzip"
version = "1.13"
rel     = 1
arch    = ["x86_64"]
summary = "GNU compression utility"
license = ["GPL-3.0"]
source  = "https://ftp.gnu.org/gnu/gzip/gzip-%{version}.tar.xz"
sha256  = "..."

[depends]
build = ["zstd"]     # recipe-level build deps (seed tools are implicit)
run   = []

[build]
configure = "./configure --prefix=/usr"
compile   = "make -j%{jobs}"
install   = "make DESTDIR=%{pkgdir} install"

[hooks]
post_install = ""    # runs after install (also used after upgrade)
pre_remove   = ""    # runs before `nawa remove`
post_remove  = ""    # runs after `nawa remove`
# hooks are stored in the archive (.HOOKS); they run on the live system, or in a
# chroot of --root when nawa runs as root and the target has /bin/sh; else skipped
```

Placeholders available in build sections: `%{name} %{version} %{rel} %{arch}
%{jobs} %{pkgdir} %{srcdir}`.


### Distributed binary repositories (schema v3)

Schema v3 keeps the signed index at one stable repository URL while each
package and recipe archive can live in a different HTTPS location. The
publisher supplies a TOML locations manifest; the signed index binds every
URL to the archive SHA-256 and size.

```toml
schema = 1

[package_urls]
"bash-5.2-1.nawa.tar.zst" = "https://github.com/owner/repo/releases/download/packages-0001/sha256-bash.tar.zst"

[recipe_urls]
"recipes-<commit>.recipes.tar.zst" = "https://github.com/owner/repo/releases/download/packages-0001/sha256-recipes.tar.zst"
```

Create the index only after all referenced assets have been uploaded:

```sh
nawa repo-create ./archives \
  --index-version 3 --locations locations.toml \
  --recipes ./recipes --recipes-rev <commit> \
  --sign --key nawa.sec
```

Schema v3 requires a trusted index signing key. Nawa refuses per-archive
URLs from an unsigned or untrusted index. URLs must be absolute HTTPS URLs
without embedded credentials or fragments. Schema v1 and v2 retain their
existing base-URL behavior.
