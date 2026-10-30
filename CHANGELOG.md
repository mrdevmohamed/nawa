# Changelog

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
