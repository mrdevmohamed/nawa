# Changelog

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
