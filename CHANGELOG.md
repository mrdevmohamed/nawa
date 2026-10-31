# Changelog

## Unreleased — feat: automatic package slimming (strip, cleanup, `-doc`/`-lang` split)

- New `src/slim.rs`, run by the build engine between `install` and the
  archive: removes junk files (libtool `*.la`, `share/info/dir`,
  `charset.alias`, `perllocal.pod`, `.packlist`), splits man/info/doc into
  `<name>-doc` and gettext catalogs into `<name>-lang` (separate
  `.nawa.tar.zst`, same version/rel, `rundeps = <name>`), and strips ELF
  files (`--strip-unneeded`; `--strip-debug` for `*.a`, `.o`, unsigned `.ko`).
  A split that would empty the main package is skipped; `rustlib`, `/boot`,
  `/usr/lib/debug` and `/usr/src` are never stripped.
- Opt-outs: recipe `[slim] strip|cleanup|split = false`, profile
  `[build] slim = false`, environment `NAWA_SLIM=0|1` (wins).
- The slimming rules version is part of the build id: `--use-cache` rebuilds
  archives made before this change.
- `repo-create` / `sign` need no change: sub-packages are ordinary archives.

## Unreleased — fix: `nawa deps <pkg>` ignored the dependencies of an installed package

- `nawa deps <name>` seeded the resolver with the installed set, the requested
  package included, so for an installed package it printed nothing and never
  looked at its dependencies. A package that ships in the rootfs but is being
  rebuilt (glibc in the min build rootfs) therefore never got its build tools
  (bison) installed, and its configure failed with "critical programs are
  missing". The requested package is now always walked: its missing
  dependencies are listed first and the package itself last, as for an
  uninstalled one. `deps-all` is unchanged (it still skips what is installed).

## Unreleased — feat: `nawa sync` — signed recipe tree from the repository

- New `nawa sync [--repo <dir-or-url>] [--force] [--allow-unsigned]`: fetches
  the newest recipe tree published by the repository and installs it at the
  configured `recipes` path. Replaces shipping the tree as the `nawa-recipes`
  package, whose pinned source always lagged the code it was cut from.
- The tree is carried by the signed `index.toml` (`[recipes]` rev / file /
  sha256), so it gets the same checks as `nawa upgrade`: trusted key,
  signature, rollback (`index.seen`) and freeze (`expires`) protection; the
  tarball must match the sha256 in the authenticated index.
- The tree is unpacked beside the target and swapped in with two renames
  (an interrupted sync never leaves a mixed tree); an archive without any
  `recipe.toml` never replaces a working tree. The synced revision is kept in
  `<root>/var/lib/nawa/recipes.rev`; syncing the same revision is a no-op.
- `nawa repo-create --recipes <dir> --recipes-rev <rev>` packs the tree
  (`recipes-<rev>.recipes.tar.zst`, deterministic, `.git` skipped, symlinks
  refused) and records it in the index; `--sign` signs it as before.

## Unreleased — feat: system config `/etc/nawa/nawa.toml`, default repository, install from repo or build

- New config layer `/etc/nawa/nawa.toml` (dir overridable with
  `NAWA_SYSCONFDIR`) below the project `nawa.toml`: precedence is now
  flag > env > nawa.toml > /etc/nawa/nawa.toml > default. `nawa init --system`
  writes it; the `nawa` package ships it (`dist/nawa.toml`, kept in sync
  with `config::system_template()` by a unit test; recipe rel 2).
- Built-in default repository
  `https://raw.githubusercontent.com/mrdevmohamed/faseela-repo/main`
  (`repo = ""` / `none` switches it off). `nawa upgrade` uses it too.
- New key `install_mode` (`NAWA_INSTALL_MODE`): `auto` (default) | `repo` |
  `build` | `ask`. `nawa install <name>` takes each package from the
  repository when it has it (at least as new as the recipe, right
  architecture), otherwise builds it. Flags: `--build/-b`, `--from-repo/-r`,
  `--from <mode>`, `--repo <dir-or-url>`. The plan lists `[repo ]`/`[build]` per
  package; `repo` mode fails before touching anything; `auto` falls back to a
  build when the repository is unusable or a download/verification fails.
  Repository-only packages (no recipe) install as binary packages.
- `upgrade::open_repo` / `fetch_entry` extracted from `upgrade` (same
  trust, signature, freshness and archive checks) and shared with `install`.
- `nawa update` and `bootstrap/build-packages-inner.sh` always build
  (`--build`), so stage builds stay reproducible source builds.
- Tests: config layering/defaults (config.rs), `choose` prompt (confirm.rs),
  `tests/repo_install.rs` (auto / --build / --from-repo / binary-only /
  fallback / system-config precedence). Existing integration test helpers pin
  `NAWA_SYSCONFDIR` and `NAWA_INSTALL_MODE=build`.

## 1.0.0

Renamed from `dpm` to **nawa** (Arabic نواة, "the date stone a seedling grows
from"). This is a clean break, not a drop-in upgrade:

| was | now |
|---|---|
| binary / crate `dpm` | `nawa` |
| `dpm.toml`, `DPM_*` env vars | `nawa.toml`, `NAWA_*` |
| `*.dpm.tar.zst` archives | `*.nawa.tar.zst` |
| `/var/lib/dpm`, `/var/cache/dpm`, `/etc/dpm` | `/var/lib/nawa`, `/var/cache/nawa`, `/etc/nawa` |
| signed message `dpm-v1:sha256:<hash>` | `nawa-v1:sha256:<hash>` (old signatures do not verify) |

First release as a standalone project. The code is the `pkgmgr` 0.8.0 tree of the
mydistro repository (history before this release lives there), with the version
bumped to 1.0.0.

- Own repository: sources, tests, vendored crates, smoke scripts, CI.
- `scripts/smoke-v06.sh` no longer needs a distribution recipe tree
  (`RECIPES=` overrides it; default is an empty fixture directory).

### Known issues

- `ureq` 2.x (`proxy-from-env`) ignores `NO_PROXY`: with `HTTP(S)_PROXY` set,
  requests to `127.0.0.1` also go through the proxy. Four `fetch::` unit tests and
  `scripts/smoke-v06.sh` fail on such hosts; unset the proxy variables to run them.
