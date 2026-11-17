#!/usr/bin/env bash
# e2e CLI smoke test for nawa v0.6 features: remote http(s) repositories
# (index + archive fetch, signature check, cache reuse, offline fallback).
set -euo pipefail

# locate the repo relative to this script so it works from any checkout
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
# override via env: nawa=... MKFAKE=... ./smoke-v06.sh
NAWA="${NAWA:-$REPO_ROOT/target/release/nawa}"
MKFAKE="${MKFAKE:-$REPO_ROOT/target/release/examples/mkfake}"
RECIPES="${RECIPES:-$REPO_ROOT/tests/fixtures/recipes}"
WORK=$(mktemp -d /tmp/nawa-smoke06-XXXX)
ROOT=$WORK/root
REPO=$WORK/repo
KEYS=$WORK/keys
PORT=$((20000 + RANDOM % 20000))
SRV_PID=""

cleanup() {
    [[ -n $SRV_PID ]] && kill "$SRV_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "== 1. build a signed local repo =="
$NAWA keygen --out "$KEYS" >/dev/null
$MKFAKE demo 1.0 1 "$WORK/demo-1.0-1.nawa.tar.zst" "usr/bin/demo=v1" >/dev/null
$MKFAKE demo 2.0 1 "$WORK/demo-2.0-1.nawa.tar.zst" "usr/bin/demo=v2" >/dev/null
$NAWA install --root "$ROOT" "$WORK/demo-1.0-1.nawa.tar.zst"
grep -q v1 "$ROOT/usr/bin/demo"
$NAWA sign "$WORK/demo-2.0-1.nawa.tar.zst" --key "$KEYS/nawa.sec" >/dev/null
mkdir -p "$REPO" && cp "$WORK/demo-2.0-1.nawa.tar.zst" "$WORK/demo-2.0-1.nawa.tar.zst.sig" "$REPO/"
$NAWA repo-create "$REPO" --sign --key "$KEYS/nawa.sec" >/dev/null
test -f "$REPO/index.toml.sig"
# remote repos are refused without a trust store: trust the test key
$NAWA trust --root "$ROOT" "$KEYS/nawa.pub" >/dev/null
if $NAWA upgrade --yes --root "$ROOT/../untrusted-root" --recipes "$RECIPES" --repo "http://127.0.0.1:1" >/dev/null 2>&1; then
    echo "FAIL: remote repo accepted without trusted keys"; exit 1
fi

echo "== 2. serve over HTTP; upgrade from the remote URL =="
( cd "$REPO" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
SRV_PID=$!
for i in $(seq 1 50); do
    curl -fs -o /dev/null "http://127.0.0.1:$PORT/index.toml" 2>/dev/null && break
    sleep 0.1
done
$NAWA upgrade --yes --root "$ROOT" --recipes "$RECIPES" --repo "http://127.0.0.1:$PORT"
grep -q v2 "$ROOT/usr/bin/demo"
FNAME=$(awk '/^file/{gsub(/"/,"");print $3}' "$REPO/index.toml")
CACHED=$(echo "$ROOT"/var/cache/nawa/remote/*/"$FNAME")
test -f "$CACHED" && echo "   (archive cached under var/cache/nawa/remote: OK)"

echo "== 3. verify the cached archive byte-for-byte against the index sha256 =="
IDX_SHA=$(awk '/^sha256/{gsub(/"/,"");print $3}' "$REPO/index.toml")
echo "$IDX_SHA  $CACHED" | sha256sum -c --quiet

echo "== 4. mirror down: cached index reuse, plan is empty =="
kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null || true; SRV_PID=""
OUT=$($NAWA upgrade --yes --root "$ROOT" --recipes "$RECIPES" --repo "http://127.0.0.1:$PORT" 2>&1)
grep -q "using cached index" <<< "$OUT" && echo "   (offline fallback to cached index: OK)"
grep -q "up to date" <<< "$OUT"

echo "== 5. dry-run against a fresh remote version over HTTP =="
( cd "$REPO" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
SRV_PID=$!
for i in $(seq 1 50); do
    curl -fs -o /dev/null "http://127.0.0.1:$PORT/index.toml" 2>/dev/null && break
    sleep 0.1
done
$MKFAKE demo 3.0 1 "$WORK/demo-3.0-1.nawa.tar.zst" "usr/bin/demo=v3" >/dev/null
$NAWA sign "$WORK/demo-3.0-1.nawa.tar.zst" --key "$KEYS/nawa.sec" >/dev/null
cp "$WORK/demo-3.0-1.nawa.tar.zst" "$WORK/demo-3.0-1.nawa.tar.zst.sig" "$REPO/"
$NAWA repo-create "$REPO" --sign --key "$KEYS/nawa.sec" >/dev/null
$NAWA upgrade --yes --root "$ROOT" --recipes "$RECIPES" --repo "http://127.0.0.1:$PORT" --dry-run | grep -q "dry run"
grep -q v2 "$ROOT/usr/bin/demo"
echo "   (dry run made no changes: OK)"

echo "== 6. apply the remote upgrade =="
$NAWA upgrade --yes --root "$ROOT" --recipes "$RECIPES" --repo "http://127.0.0.1:$PORT" >/dev/null
grep -q v3 "$ROOT/usr/bin/demo"
$NAWA rollback --root "$ROOT" demo >/dev/null
grep -q v2 "$ROOT/usr/bin/demo"
echo "   (remote upgrade + rollback: OK)"

echo
echo "ALL V0.6 SMOKE TESTS PASSED"
