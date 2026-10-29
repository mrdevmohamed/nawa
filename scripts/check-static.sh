#!/usr/bin/env bash
# scripts/check-static.sh BINARY — fail unless BINARY is a fully static ELF:
# no PT_INTERP and no DT_NEEDED entries. A release binary that needs any
# shared library cannot be called portable.
set -euo pipefail
bin="${1:?usage: check-static.sh BINARY}"
[ -f "$bin" ] || { echo "check-static: $bin not found" >&2; exit 2; }
out=$(readelf -l -d "$bin" 2>&1)
if printf '%s\n' "$out" | grep -q 'Requesting program interpreter'; then
    echo "check-static: FAIL: $bin has a dynamic loader:" >&2
    printf '%s\n' "$out" | grep 'Requesting program interpreter' >&2
    exit 1
fi
if printf '%s\n' "$out" | grep -q '(NEEDED)'; then
    echo "check-static: FAIL: $bin needs shared libraries:" >&2
    printf '%s\n' "$out" | grep '(NEEDED)' >&2
    exit 1
fi
echo "check-static: ok: $bin is static"
