#!/usr/bin/env bash
# scripts/check-rootfs.sh ROOTFS [revdep args...] — final bootstrap gate: fail
# (exit 1) when anything under ROOTFS has a broken shared-library link, a
# missing ELF interpreter or a dangling symlink, including files that no
# package owns (`nawa revdep --unowned`). Extra arguments go to `nawa revdep`,
# e.g. `--ignore 'libnvidia-*' --lib-dir /opt/x/lib`, or `--json` for CI.
# NAWA picks the nawa binary (default: nawa from PATH).
set -euo pipefail
root="${1:?usage: check-rootfs.sh ROOTFS [revdep args...]}"
shift
[ -d "$root" ] || { echo "check-rootfs: $root is not a directory" >&2; exit 2; }
nawa="${NAWA:-nawa}"
echo "check-rootfs: scanning $root" >&2
if "$nawa" --root "$root" revdep --unowned --no-suggest "$@"; then
    echo "check-rootfs: OK" >&2
else
    echo "check-rootfs: FAIL: broken links in $root (see above)" >&2
    exit 1
fi
