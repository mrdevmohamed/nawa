# Changelog

## Unreleased — cli: readable errors and no noise for unprivileged runs

- Top-level errors are rendered over several lines: the message first, then
  each underlying cause indented as `caused by:` (without the `(os error N)`
  suffix), instead of one colon-joined line. Colors follow the existing
  `NO_COLOR` / `NAWA_COLOR` policy.
- A permission-denied failure of a normal user now ends with a hint telling
  them to re-run the same command with `doas`/`sudo` (or as root when neither
  is installed).
- The package database lock error now reads `cannot open the package database
  lock <path>`.
- Unprivileged runs (`nawa update`, queries) no longer print `could not lock
  plugin event outbox` when the root-owned outbox is unreadable to them; the
  pending events are delivered by the next privileged command.

## Unreleased — repo-update: incremental repository index updates

- Add `nawa repo-update`. It builds the next schema-2/3 index from the previous
  signed index plus a delta (`--add-entries`, `--remove`, `--drop`) without
  opening any archive, instead of re-reading every archive like `repo-create`.
- The base index is verified against `--trust-key` over the exact bytes that
  are then parsed; an unverified base is refused. The result is always signed.
- Newest-per-name selection is the same `vercmp` rule as `repo-create`. Removing
  the indexed archive of a package without a replacement is an error (use
  `--drop NAME` to remove a package on purpose); archive file names are
  immutable, so re-adding one with a different sha256/size is an error.
- `validity` is refreshed on every update, so `repo-update` with no delta also
  renews an index that is close to its 90-day expiry.
- `repo-create` now shares its atomic write with `repo-update` (no behaviour
  change). Unit tests check that `repo-update` equals `repo-create` over the
  resulting archive set.

## Unreleased — fix: fetch-phase env lost at isolation none

- At `--isolation none` (the default) the `[fetch.*]` build-time environment
  (`CARGO_HOME`, `CARGO_NET_OFFLINE`, `GOFLAGS`, ...) was computed but dropped,
  so an offline `cargo build` looked in the default, empty Cargo home and failed
  with `no matching package named ...` although the fetch had succeeded. The
  additions are now appended to the profile env at `none`. Add a regression test.

## Unreleased — fix sandbox device staging mounts

- Keep the temporary bind mounts under the sandbox build scratch `devnodes/`
  writable, just like their final `/dev/*` mounts. They are device interfaces,
  not ordinary build-root files; strict read-only setup must not try to remount
  them read-only. Add a regression test for staged device paths and nearby
  paths that must remain subject to normal read-only handling.

## Unreleased — build: remove checked-in Cargo vendor tree

- Remove `vendor/`, the Cargo source-replacement config, and the obsolete
  `scripts/vendor-nawa.sh` generator.
- Keep `Cargo.lock` as the dependency version and checksum authority.
- Fetch dependencies with `cargo fetch --locked` (or Nawa's
  `[fetch.cargo]` phase), then compile with
  `cargo build --release --offline --locked`.
- Bootstrap builds must fetch before their offline compile; ordinary package
  builds use Nawa's lockfile-keyed Cargo fetch cache.

## Unreleased — feat: fetch phase, multiple sources and recipe-local assets

Design: `docs/design/fetch-phase.md`. Existing recipes keep working unchanged
and keep byte-identical build ids (pinned by a golden test).

- New `nawa fetch [<name>...|--all] [--isolation <level>] [--sources-only]
  [--offline] [--allow-degraded]`: downloads and verifies every external
  source, then runs the recipe's fetch backends, so that later builds can run
  with no network. `--sources-only` needs no toolchain; `--offline` is a
  verify-only check that every cached entry exists and is complete.
- New `nawa build --offline` (also `NAWA_OFFLINE`): every download is refused,
  only cached and verified entries are used.
- New `nawa recipe check [<name>|--all]`: source-independent warnings for
  assets, unknown tables and keys, unknown placeholders and fetch
  anti-patterns. Local files only, no network.
- Recipes can declare several external sources with `[[package.sources]]`
  (`name`, `url`, `sha256`, `mirrors`, `unpack`, `into`, `strip_components`),
  each with its own sha256 and cache entry, plus `workdir` and a new
  `%{srcroot}` placeholder. The new form cannot be mixed with the legacy
  `source` / `sha256` / `mirrors` keys.
- Recipes can ship files next to the recipe with `[assets] include = [...]`.
  Only declared paths are staged, read-only, at `%{assetsdir}`
  (`NAWA_ASSETSDIR`); symlinks and special files are refused, and the assets
  are part of the build id.
- `[fetch.go]` and `[fetch.cargo]` (`dir = "."`) fetch language dependencies
  from the lockfile: `go mod download && go mod verify`, and
  `cargo fetch --locked`. The fetch runs in the sandbox with the network open;
  the build that follows is offline. Entries are cached under
  `NAWA_SRCCACHE`, keyed by the lockfile bytes (and the Cargo version for
  Cargo). A missing `go.sum` / `Cargo.lock` is a hard error; an already
  vendored Go project skips the fetch. `NAWA_GOPROXY` (`https://` or
  `file://`) overrides the Go proxy and `NAWA_FETCH_TIMEOUT` the 1800 s limit.
- The source cache now lives at `<srccache>/sha256/<digest>` with per-digest
  locks; entries from the old layout are reused when their bytes verify.

## Unreleased — fix: Ctrl+C and crashes can no longer corrupt an install

- SIGINT / SIGTERM / SIGHUP are deferred while an install or removal changes
  the system instead of killing nawa on the spot. Before the db entry is
  written the step is rolled back (files and db, nothing half-installed);
  after it, the current package is simply finished. Between packages nawa
  stops and exits with status 130. Pressing Ctrl+C three times forces an
  immediate exit, which is safe because of the next point.
- Every file move is written to an on-disk journal first
  (`<root>/.nawa/bak-*.journal`). The next nawa that takes the exclusive
  database lock replays it: an unfinished transaction is rolled back
  (including a db entry that was only partly written), a finished one is
  cleaned up, and leftover staging directories are removed. This also covers
  SIGKILL, an OOM kill and power loss (the journal is fsynced every 32
  entries and before the db is touched).
- A `Tx` that is dropped without commit (early return, panic) rolls back.

## Unreleased — fix: runit service control
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- `nawa service start|stop|restart <name>` control a running service through
## Unreleased — fix: runit service control  `sv` using the full runsvdir path (a bare name resolves against `$SVDIR`,
## Unreleased — fix: runit service control  which does not exist here, so the old `sv up <name>` hint never worked).
## Unreleased — fix: runit service control- `enable --now` waits for runsv to pick the service up and starts it;
## Unreleased — fix: runit service control  `disable --now` stops it first, then removes the link.
## Unreleased — fix: runit service control- `disable` records the choice in `/etc/runit/disabled/<n>`; package installs
## Unreleased — fix: runit service control  and upgrades no longer recreate a runsvdir link the admin disabled
## Unreleased — fix: runit service control  (`enable` clears the marker).
## Unreleased — fix: runit service control- `status` no longer reports "down" for services that are merely disabled or
## Unreleased — fix: runit service control  not yet picked up by runsvdir, and shows `sv status` output as-is.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — feat: update from binary repositories
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- `nawa update` keeps building from source by default; choose `--from repo`
## Unreleased — fix: runit service control  (or `--from-repo` / `-r`) for repository-only updates, `--from auto` to use
## Unreleased — fix: runit service control  repository archives when they are at least as new as the local recipes and
## Unreleased — fix: runit service control  otherwise build, or `--from ask` to choose interactively. `--build` / `-b`
## Unreleased — fix: runit service control  explicitly keeps source builds. `--repo DIR_OR_URL` selects the repository
## Unreleased — fix: runit service control  for one run; without it, the configured repository is used.
## Unreleased — fix: runit service control- Repository-only updates preflight every package in the complete plan and
## Unreleased — fix: runit service control  reject missing, wrong-architecture, or stale entries before installing
## Unreleased — fix: runit service control  anything. `nawa upgrade` remains the direct index-based binary updater.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — ux: sizes in the install plan, readable download output
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- `nawa install` / `update` print a table (package, version, source, download
## Unreleased — fix: runit service control  size, installed size, reason) and the totals, like `dnf`.
## Unreleased — fix: runit service control- Repository indexes record each archive's `size` (bytes) and `installed_size`;
## Unreleased — fix: runit service control  older indexes still parse (sizes show as `-`, the totals say how many are unknown).
## Unreleased — fix: runit service control- Downloads on a terminal show `name  12.0 MiB / 48.0 MiB  25%` on one updating
## Unreleased — fix: runit service control  line; small files (index, signatures) stay silent.
## Unreleased — fix: runit service control- Redirect hops are no longer printed (the signed CDN URLs were hundreds of
## Unreleased — fix: runit service control  characters); `NAWA_VERBOSE=1` prints host and path.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — feat: automatic package slimming (strip, cleanup, `-doc`/`-lang` split)
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- New `src/slim.rs`, run by the build engine between `install` and the
## Unreleased — fix: runit service control  archive: removes junk files (libtool `*.la`, `share/info/dir`,
## Unreleased — fix: runit service control  `charset.alias`, `perllocal.pod`, `.packlist`), splits man/info/doc into
## Unreleased — fix: runit service control  `<name>-doc` and gettext catalogs into `<name>-lang` (separate
## Unreleased — fix: runit service control  `.nawa.tar.zst`, same version/rel, `rundeps = <name>`), and strips ELF
## Unreleased — fix: runit service control  files (`--strip-unneeded`; `--strip-debug` for `*.a`, `.o`, unsigned `.ko`).
## Unreleased — fix: runit service control  A split that would empty the main package is skipped; `rustlib`, `/boot`,
## Unreleased — fix: runit service control  `/usr/lib/debug` and `/usr/src` are never stripped.
## Unreleased — fix: runit service control- Opt-outs: recipe `[slim] strip|cleanup|split = false`, profile
## Unreleased — fix: runit service control  `[build] slim = false`, environment `NAWA_SLIM=0|1` (wins).
## Unreleased — fix: runit service control- The slimming rules version is part of the build id: `--use-cache` rebuilds
## Unreleased — fix: runit service control  archives made before this change.
## Unreleased — fix: runit service control- `repo-create` / `sign` need no change: sub-packages are ordinary archives.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — fix: `nawa deps <pkg>` ignored the dependencies of an installed package
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- `nawa deps <name>` seeded the resolver with the installed set, the requested
## Unreleased — fix: runit service control  package included, so for an installed package it printed nothing and never
## Unreleased — fix: runit service control  looked at its dependencies. A package that ships in the rootfs but is being
## Unreleased — fix: runit service control  rebuilt (glibc in the min build rootfs) therefore never got its build tools
## Unreleased — fix: runit service control  (bison) installed, and its configure failed with "critical programs are
## Unreleased — fix: runit service control  missing". The requested package is now always walked: its missing
## Unreleased — fix: runit service control  dependencies are listed first and the package itself last, as for an
## Unreleased — fix: runit service control  uninstalled one. `deps-all` is unchanged (it still skips what is installed).
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — feat: `nawa revdep` — find broken shared-library links
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- New `nawa revdep [name...] [--lib-dir DIR]... [--ignore GLOB]... [--no-symlinks]
## Unreleased — fix: runit service control  [--unowned] [--scan-dir DIR]...`: reads every ELF file owned by the installed
## Unreleased — fix: runit service control  packages (db file lists) with the existing goblin reader (`lint::parse_elf`)
## Unreleased — fix: runit service control  and checks each `DT_NEEDED` the way the loader resolves it: RPATH/RUNPATH with
## Unreleased — fix: runit service control  `$ORIGIN`, `--lib-dir`, `/etc/ld.so.conf` + includes, `/etc/ld-musl-*.path`,
## Unreleased — fix: runit service control  then `/lib /usr/lib /lib64 /usr/lib64 /usr/local/lib`. A candidate must have
## Unreleased — fix: runit service control  the same ELF class and machine as the file needing it (a 32-bit library does
## Unreleased — fix: runit service control  not satisfy a 64-bit program; reported as "wrong architecture"). Also checks
## Unreleased — fix: runit service control  the ELF interpreter and dangling symlinks.
## Unreleased — fix: runit service control- `--unowned` (or `--scan-dir`) additionally walks `/usr /bin /sbin /lib /lib64
## Unreleased — fix: runit service control  /opt` (or the given dirs) with `walkdir` and checks files no package owns,
## Unreleased — fix: runit service control  reported under "(not owned by any package)".
## Unreleased — fix: runit service control- Output is grouped by package and ends with the list of packages to rebuild;
## Unreleased — fix: runit service control  exit status 1 when anything is broken. Takes the shared db lock; all lookups
## Unreleased — fix: runit service control  happen inside `--root`. No new dependencies.
## Unreleased — fix: runit service control- Automatic check: after `install`, `update` and `remove` nawa re-checks what
## Unreleased — fix: runit service control  the change can have broken (the changed packages plus every installed
## Unreleased — fix: runit service control  package that lists them in its run-dependencies) and prints a warning on
## Unreleased — fix: runit service control  stderr; the command's exit status is unaffected. `NAWA_NO_REVDEP=1` turns it
## Unreleased — fix: runit service control  off.
## Unreleased — fix: runit service control- `--json` prints one JSON document (`ok`, `scanned`, `packages` with the
## Unreleased — fix: runit service control  missing libraries and their providers, `unowned`, `rebuild`); the exit
## Unreleased — fix: runit service control  status is still 1 when something is broken.
## Unreleased — fix: runit service control- Missing libraries come with suggestions (disable with `--no-suggest`): the
## Unreleased — fix: runit service control  packages whose locally built archive in `<builddir>/out` ships the file
## Unreleased — fix: runit service control  (read from the archive's file list), else a recipe named like the library
## Unreleased — fix: runit service control  (`libpng16.so.16` → `png`/`libpng`), with the `nawa install` hint, or
## Unreleased — fix: runit service control  "installed but lacks the file" when the provider is already installed.
## Unreleased — fix: runit service control- `scripts/check-rootfs.sh ROOTFS` runs `nawa --root ROOTFS revdep --unowned`
## Unreleased — fix: runit service control  as a last gate for bootstrap/rootfs builds (exit 1 on broken links).
## Unreleased — fix: runit service control- `lint::ElfInfo` gained `interp`, `is_64` and `machine`; `install` gained
## Unreleased — fix: runit service control  `resolve_in_root` (`exists_in_root` is now built on it, both `pub(crate)`).
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — feat: `nawa sync` — signed recipe tree from the repository
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- New `nawa sync [--repo <dir-or-url>] [--force] [--allow-unsigned]`: fetches
## Unreleased — fix: runit service control  the newest recipe tree published by the repository and installs it at the
## Unreleased — fix: runit service control  configured `recipes` path. Replaces shipping the tree as the `nawa-recipes`
## Unreleased — fix: runit service control  package, whose pinned source always lagged the code it was cut from.
## Unreleased — fix: runit service control- The tree is carried by the signed `index.toml` (`[recipes]` rev / file /
## Unreleased — fix: runit service control  sha256), so it gets the same checks as `nawa upgrade`: trusted key,
## Unreleased — fix: runit service control  signature, rollback (`index.seen`) and freeze (`expires`) protection; the
## Unreleased — fix: runit service control  tarball must match the sha256 in the authenticated index.
## Unreleased — fix: runit service control- The tree is unpacked beside the target and swapped in with two renames
## Unreleased — fix: runit service control  (an interrupted sync never leaves a mixed tree); an archive without any
## Unreleased — fix: runit service control  `recipe.toml` never replaces a working tree. The synced revision is kept in
## Unreleased — fix: runit service control  `<root>/var/lib/nawa/recipes.rev`; syncing the same revision is a no-op.
## Unreleased — fix: runit service control- `nawa repo-create --recipes <dir> --recipes-rev <rev>` packs the tree
## Unreleased — fix: runit service control  (`recipes-<rev>.recipes.tar.zst`, deterministic, `.git` skipped, symlinks
## Unreleased — fix: runit service control  refused) and records it in the index; `--sign` signs it as before.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## Unreleased — feat: system config `/etc/nawa/nawa.toml`, default repository, install from repo or build
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- New config layer `/etc/nawa/nawa.toml` (dir overridable with
## Unreleased — fix: runit service control  `NAWA_SYSCONFDIR`) below the project `nawa.toml`: precedence is now
## Unreleased — fix: runit service control  flag > env > nawa.toml > /etc/nawa/nawa.toml > default. `nawa init --system`
## Unreleased — fix: runit service control  writes it; the `nawa` package ships it (`dist/nawa.toml`, kept in sync
## Unreleased — fix: runit service control  with `config::system_template()` by a unit test; recipe rel 2).
## Unreleased — fix: runit service control- Built-in default repository
## Unreleased — fix: runit service control  `https://raw.githubusercontent.com/mrdevmohamed/faseela-repo/main`
## Unreleased — fix: runit service control  (`repo = ""` / `none` switches it off). `nawa upgrade` uses it too.
## Unreleased — fix: runit service control- New key `install_mode` (`NAWA_INSTALL_MODE`): `auto` (default) | `repo` |
## Unreleased — fix: runit service control  `build` | `ask`. `nawa install <name>` takes each package from the
## Unreleased — fix: runit service control  repository when it has it (at least as new as the recipe, right
## Unreleased — fix: runit service control  architecture), otherwise builds it. Flags: `--build/-b`, `--from-repo/-r`,
## Unreleased — fix: runit service control  `--from <mode>`, `--repo <dir-or-url>`. The plan lists `[repo ]`/`[build]` per
## Unreleased — fix: runit service control  package; `repo` mode fails before touching anything; `auto` falls back to a
## Unreleased — fix: runit service control  build when the repository is unusable or a download/verification fails.
## Unreleased — fix: runit service control  Repository-only packages (no recipe) install as binary packages.
## Unreleased — fix: runit service control- `upgrade::open_repo` / `fetch_entry` extracted from `upgrade` (same
## Unreleased — fix: runit service control  trust, signature, freshness and archive checks) and shared with `install`.
## Unreleased — fix: runit service control- `nawa update` and `bootstrap/build-packages-inner.sh` always build
## Unreleased — fix: runit service control  (`--build`), so stage builds stay reproducible source builds.
## Unreleased — fix: runit service control- Tests: config layering/defaults (config.rs), `choose` prompt (confirm.rs),
## Unreleased — fix: runit service control  `tests/repo_install.rs` (auto / --build / --from-repo / binary-only /
## Unreleased — fix: runit service control  fallback / system-config precedence). Existing integration test helpers pin
## Unreleased — fix: runit service control  `NAWA_SYSCONFDIR` and `NAWA_INSTALL_MODE=build`.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control## 1.0.0
## Unreleased — fix: runit service control
## Unreleased — fix: runit service controlRenamed from `dpm` to **nawa** (Arabic نواة, "the date stone a seedling grows
## Unreleased — fix: runit service controlfrom"). This is a clean break, not a drop-in upgrade:
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control| was | now |
## Unreleased — fix: runit service control|---|---|
## Unreleased — fix: runit service control| binary / crate `dpm` | `nawa` |
## Unreleased — fix: runit service control| `dpm.toml`, `DPM_*` env vars | `nawa.toml`, `NAWA_*` |
## Unreleased — fix: runit service control| `*.dpm.tar.zst` archives | `*.nawa.tar.zst` |
## Unreleased — fix: runit service control| `/var/lib/dpm`, `/var/cache/dpm`, `/etc/dpm` | `/var/lib/nawa`, `/var/cache/nawa`, `/etc/nawa` |
## Unreleased — fix: runit service control| signed message `dpm-v1:sha256:<hash>` | `nawa-v1:sha256:<hash>` (old signatures do not verify) |
## Unreleased — fix: runit service control
## Unreleased — fix: runit service controlFirst release as a standalone project. The code is the `pkgmgr` 0.8.0 tree of the
## Unreleased — fix: runit service controlmydistro repository (history before this release lives there), with the version
## Unreleased — fix: runit service controlbumped to 1.0.0.
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- Own repository: sources, tests, vendored crates, smoke scripts, CI.
## Unreleased — fix: runit service control- `scripts/smoke-v06.sh` no longer needs a distribution recipe tree
## Unreleased — fix: runit service control  (`RECIPES=` overrides it; default is an empty fixture directory).
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control### Known issues
## Unreleased — fix: runit service control
## Unreleased — fix: runit service control- `ureq` 2.x (`proxy-from-env`) ignores `NO_PROXY`: with `HTTP(S)_PROXY` set,
## Unreleased — fix: runit service control  requests to `127.0.0.1` also go through the proxy. Four `fetch::` unit tests and
## Unreleased — fix: runit service control  `scripts/smoke-v06.sh` fail on such hosts; unset the proxy variables to run them.
## Unreleased — fix: runit service control