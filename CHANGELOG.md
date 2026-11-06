# Changelog

## Unreleased — feat: content-addressed artifact identity and verified rollback

- Every installed archive is now identified by the SHA-256 of its exact bytes.
  The bytes are published to a content-addressed object store
  (`<root>/var/cache/nawa/objects/sha256/<xx>/<digest>.nawa.tar.zst`) and the
  database entry records them as `artifact_sha256` — package file names stop
  being the identity.
- A provenance record (`<digest>.artifact.toml`) is stored next to every
  object: package identity, origin (source build, repository, standalone
  archive), and how the bytes were authenticated (detached signature,
  trusted signed index, or explicitly unsigned), including the signer's
  key fingerprint and whether that key was trusted at install time.
- For signed-index (schema v2) repositories, the exact index and signature
  bytes that authenticated the install are preserved with the artifact, so
  the evidence never depends on the live repository or its current state.
  Detached signatures are preserved the same way.
- Publication is fail-safe: bytes are hashed again while they are copied,
  a digest/size disagreement with the record is refused, an already-present
  object is re-verified before reuse, and a corrupt object is replaced by
  the freshly verified duplicate (self-healing, path is always the digest).
- `nawa rollback` re-resolves the previous artifact by its recorded digest
  (object store first, then the version-named cache only if it hashes to
  the same digest), re-verifies the bytes, and re-checks authentication
  under the recorded evidence: saved index + signature against the current
  trust store (with or without a currently-trusted key — never substituted
  by different or embedded-key evidence), preserved detached signature, or
  an explicit unsigned record. Missing, corrupt, or foreign-package bytes
  and invalid signatures are hard refusals — restore never weakens the
  install policy (`--allow-unsigned` cannot convert invalid signatures).
- Legacy databases without artifact digests keep the old rollback path; a
  legacy cache file is only accepted when it matches the recorded digest.

## Unreleased — fix: install dates display correctly again

- `Entry.install_date` is now written as Unix **seconds**. It used to be
  written in nanoseconds while every viewer (`nawa info`, `nawa query`,
  removal plans) interpreted it as seconds, which produced absurd dates.
- Existing databases are detected and migrated conservatively: a value that
  can only be a legacy nanosecond stamp (at or above 1e14) is converted to
  seconds on the next nawa run that takes the exclusive database lock; the
  original bytes are kept beside the entry as `desc.legacy-nanos` so the
  conversion is always recoverable. The repair is idempotent, and ambiguous
  small values are never rewritten.
- Temporary directory names no longer derive from the recorded install date;
  uniqueness comes from an internal nanosecond id, so a same-second install
  pair cannot collide on a backup/staging directory.

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

- `nawa service start|stop|restart <name>` control a running service through
  `sv` using the full runsvdir path (a bare name resolves against `$SVDIR`,
  which does not exist here, so the old `sv up <name>` hint never worked).
- `enable --now` waits for runsv to pick the service up and starts it;
  `disable --now` stops it first, then removes the link.
- `disable` records the choice in `/etc/runit/disabled/<n>`; package installs
  and upgrades no longer recreate a runsvdir link the admin disabled
  (`enable` clears the marker).
- `status` no longer reports "down" for services that are merely disabled or
  not yet picked up by runsvdir, and shows `sv status` output as-is.

## Unreleased — feat: update from binary repositories

- `nawa update` keeps building from source by default; choose `--from repo`
  (or `--from-repo` / `-r`) for repository-only updates, `--from auto` to use
  repository archives when they are at least as new as the local recipes and
  otherwise build, or `--from ask` to choose interactively. `--build` / `-b`
  explicitly keeps source builds. `--repo DIR_OR_URL` selects the repository
  for one run; without it, the configured repository is used.
- Repository-only updates preflight every package in the complete plan and
  reject missing, wrong-architecture, or stale entries before installing
  anything. `nawa upgrade` remains the direct index-based binary updater.

## Unreleased — ux: sizes in the install plan, readable download output

- `nawa install` / `update` print a table (package, version, source, download
  size, installed size, reason) and the totals, like `dnf`.
- Repository indexes record each archive's `size` (bytes) and `installed_size`;
  older indexes still parse (sizes show as `-`, the totals say how many are unknown).
- Downloads on a terminal show `name  12.0 MiB / 48.0 MiB  25%` on one updating
  line; small files (index, signatures) stay silent.
- Redirect hops are no longer printed (the signed CDN URLs were hundreds of
  characters); `NAWA_VERBOSE=1` prints host and path.

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

## Unreleased — feat: `nawa revdep` — find broken shared-library links

- New `nawa revdep [name...] [--lib-dir DIR]... [--ignore GLOB]... [--no-symlinks]
  [--unowned] [--scan-dir DIR]...`: reads every ELF file owned by the installed
  packages (db file lists) with the existing goblin reader (`lint::parse_elf`)
  and checks each `DT_NEEDED` the way the loader resolves it: RPATH/RUNPATH with
  `$ORIGIN`, `--lib-dir`, `/etc/ld.so.conf` + includes, `/etc/ld-musl-*.path`,
  then `/lib /usr/lib /lib64 /usr/lib64 /usr/local/lib`. A candidate must have
  the same ELF class and machine as the file needing it (a 32-bit library does
  not satisfy a 64-bit program; reported as "wrong architecture"). Also checks
  the ELF interpreter and dangling symlinks.
- `--unowned` (or `--scan-dir`) additionally walks `/usr /bin /sbin /lib /lib64
  /opt` (or the given dirs) with `walkdir` and checks files no package owns,
  reported under "(not owned by any package)".
- Output is grouped by package and ends with the list of packages to rebuild;
  exit status 1 when anything is broken. Takes the shared db lock; all lookups
  happen inside `--root`. No new dependencies.
- Automatic check: after `install`, `update` and `remove` nawa re-checks what
  the change can have broken (the changed packages plus every installed
  package that lists them in its run-dependencies) and prints a warning on
  stderr; the command's exit status is unaffected. `NAWA_NO_REVDEP=1` turns it
  off.
- `--json` prints one JSON document (`ok`, `scanned`, `packages` with the
  missing libraries and their providers, `unowned`, `rebuild`); the exit
  status is still 1 when something is broken.
- Missing libraries come with suggestions (disable with `--no-suggest`): the
  packages whose locally built archive in `<builddir>/out` ships the file
  (read from the archive's file list), else a recipe named like the library
  (`libpng16.so.16` → `png`/`libpng`), with the `nawa install` hint, or
  "installed but lacks the file" when the provider is already installed.
- `scripts/check-rootfs.sh ROOTFS` runs `nawa --root ROOTFS revdep --unowned`
  as a last gate for bootstrap/rootfs builds (exit 1 on broken links).
- `lint::ElfInfo` gained `interp`, `is_64` and `machine`; `install` gained
  `resolve_in_root` (`exists_in_root` is now built on it, both `pub(crate)`).

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
