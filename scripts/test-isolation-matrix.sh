#!/usr/bin/env bash
# scripts/test-isolation-matrix.sh — drive the real `nawa build` CLI at ONE
# isolation level (none | env | sandbox | closure) end to end.
#
# CI runs it once per level (a matrix job), so every level is exercised on the
# runner itself and a host that refuses a kernel feature FAILS instead of
# silently weakening the build (`--allow-degraded` is never passed unless
# NAWA_ALLOW_DEGRADED=1 is set).
#
# What is checked per level:
#   all levels : the archive is produced and `nawa` exits 0
#   env+       : `--rebuild-check` (two builds in different dirs, same hash)
#   sandbox+   : a recipe that reaches for the network cannot (netns)
#   closure    : the build sees ONLY its dependency closure (deptool's lib is
#                there, the host's gcc is not, bash comes from the closure)
#
# Usage: scripts/test-isolation-matrix.sh <level>
# Env:   NAWA=<path to nawa>   (default target/release/nawa)
#        WORK=<scratch dir>  (default target/isolation-matrix/<level>;
#                             NOT under /tmp — the sandbox mounts a fresh
#                             tmpfs over /tmp before staging)
set -euo pipefail

level="${1:-}"
case "$level" in
  none | env | sandbox | closure) ;;
  *) echo "usage: $0 none|env|sandbox|closure" >&2; exit 2 ;;
esac

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAWA="${NAWA:-$repo/target/release/nawa}"
WORK="${WORK:-$repo/target/isolation-matrix/$level}"
[[ -x $NAWA ]] || { echo "nawa binary not found: $NAWA (cargo build --release first)" >&2; exit 2; }

pass=0
ok() { printf '\033[32mPASS\033[0m %s\n' "$*"; pass=$((pass + 1)); }
die() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }

degraded=()
[[ ${NAWA_ALLOW_DEGRADED:-0} == 1 ]] && degraded=(--allow-degraded)

rm -rf "$WORK"
mkdir -p "$WORK"/recipes/{base-devel,deptool,probe,netprobe}

nawa() {
  env -u CFLAGS -u CXXFLAGS -u LDFLAGS "$NAWA" \
    --root "$WORK/target-root" --recipes "$WORK/recipes" \
    --builddir "$WORK/build" --profiles "$WORK/profiles" "$@"
}

echo "== isolation level: $level   nawa: $("$NAWA" --version 2>&1 | head -1)"
echo "== host probe (informational)"
"$NAWA" sandbox-probe 2>&1 || true

# ---- recipes -----------------------------------------------------------------
cat > "$WORK/recipes/base-devel/recipe.toml" <<'EOF'
[package]
name = "base-devel"
version = "1"
rel = 1

[build]
install = '''
mkdir -p %{pkgdir}/usr/bin
for tool in bash mkdir ls; do
    cp -L "/usr/bin/$tool" "%{pkgdir}/usr/bin/$tool"
    for lib in $(ldd "/usr/bin/$tool" | awk '$1 ~ /^\// {print $1} $3 ~ /^\// {print $3}'); do
        cp -L --parents "$lib" %{pkgdir}/
    done
done
'''
EOF

cat > "$WORK/recipes/deptool/recipe.toml" <<'EOF'
[package]
name = "deptool"
version = "1"
rel = 1

[build]
install = """
mkdir -p %{pkgdir}/usr/lib
printf 'fake lib\n' > %{pkgdir}/usr/lib/libdeptool.so
"""
EOF

if [[ $level == closure ]]; then
  cat > "$WORK/recipes/probe/recipe.toml" <<'EOF'
[package]
name = "probe"
version = "1.0"

[depends]
build = ["deptool"]

[build]
compile = """
echo "bash=$(command -v bash)"
echo "uid-inside=$UID"
echo "deptool-lib=$(ls /usr/lib/libdeptool.so 2>/dev/null || echo MISSING)"
if command -v gcc >/dev/null 2>&1; then
    echo "GCCC-PRESENT-IN-CLOSURE <- must not happen"
    exit 3
fi
"""
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/probe
"""
EOF
else
  cat > "$WORK/recipes/probe/recipe.toml" <<'EOF'
[package]
name = "probe"
version = "1.0"

[build]
compile = "echo building"
install = """
mkdir -p %{pkgdir}/usr/bin %{pkgdir}/etc
echo x > %{pkgdir}/usr/bin/probe
echo cfg > %{pkgdir}/etc/probe.conf
"""
EOF
fi

# a build that must NOT be able to reach the network inside a sandbox
cat > "$WORK/recipes/netprobe/recipe.toml" <<'EOF'
[package]
name = "netprobe"
version = "1.0"

[build]
compile = """
bash -c 'if (exec 3<>/dev/tcp/1.1.1.1/53) 2>/dev/null; then exit 4; fi'
rc=$?
if [ "$rc" = 4 ]; then
    echo "NETWORK-REACHABLE <- must not happen inside a sandbox"
    exit 4
fi
echo "network-blocked"
"""
install = """
mkdir -p %{pkgdir}/usr/bin
echo x > %{pkgdir}/usr/bin/netprobe
"""
EOF

# ---- 1. build at the requested level ----------------------------------------
if [[ $level == closure ]]; then
  # the closure is assembled from the dependency archives, so build those first
  for dep in base-devel deptool; do
    nawa build "$dep" --isolation env > "$WORK/$dep.log" 2>&1 \
      || { cat "$WORK/$dep.log"; die "closure prerequisite $dep failed to build"; }
  done
  ok "closure prerequisites (base-devel, deptool) built"
fi

nawa build probe --isolation "$level" "${degraded[@]}" > "$WORK/probe.log" 2>&1 \
  || { cat "$WORK/probe.log"; die "nawa build probe --isolation $level failed"; }
cat "$WORK/probe.log"
[[ -s $WORK/build/out/probe-1.0-1.nawa.tar.zst ]] \
  || die "no archive produced at isolation=$level"
ok "probe built with --isolation $level"

# ---- 2. level-specific assertions -------------------------------------------
if [[ $level == closure ]]; then
  grep -q 'bash=/usr/bin/bash' "$WORK/probe.log"          || die "closure: bash is not the closure's bash"
  grep -q 'deptool-lib=/usr/lib/libdeptool.so' "$WORK/probe.log" || die "closure: dependency lib missing inside the build"
  grep -q 'GCCC-PRESENT-IN-CLOSURE' "$WORK/probe.log"     && die "closure: the host toolchain leaked into the build"
  ok "closure: only the dependency closure is visible (no host gcc)"
fi

if [[ $level == sandbox || $level == closure ]]; then
  # netprobe has no build deps, so build it at the same level it is testing
  # (closure needs base-devel in the closure, which is already built above)
  nawa build netprobe --isolation "$level" "${degraded[@]}" > "$WORK/netprobe.log" 2>&1 \
    || { cat "$WORK/netprobe.log"; die "$level: network is reachable from inside the build"; }
  grep -q 'network-blocked' "$WORK/netprobe.log" \
    || die "$level: netprobe did not report network-blocked"
  ok "$level: no network inside the build"
fi

# ---- 3. reproducibility ------------------------------------------------------
if [[ $level != none ]]; then
  # same builddir as above: the closure level needs the dependency archives
  # in <builddir>/out, and rebuild-check builds its two copies below it
  nawa build probe --isolation "$level" "${degraded[@]}" --rebuild-check \
    > "$WORK/rebuild.log" 2>&1 \
    || { cat "$WORK/rebuild.log"; die "$level: --rebuild-check failed"; }
  grep -q 'rebuild-check: OK' "$WORK/rebuild.log" \
    || { cat "$WORK/rebuild.log"; die "$level: --rebuild-check did not report OK"; }
  ok "$level: reproducible (rebuild-check OK)"
fi

echo
echo "isolation=$level: $pass check(s) passed"
