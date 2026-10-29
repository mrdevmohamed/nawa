#!/usr/bin/env bash
# scripts/ci-no-skips.sh — fail CI when a nawa test SKIPPED an isolation check.
#
# The sandbox/isolation tests probe the kernel and print `SKIP: ...` instead of
# failing when the host refuses a feature (restricted containers must not turn
# the suite red). On CI that is exactly what we must NOT let through silently:
# a green run in which the sandbox tests all skipped proves nothing.
#
# Usage: cargo test ... -- --nocapture 2>&1 | tee test.log
#        scripts/ci-no-skips.sh test.log
#
# Skips that are by design (the complementary pair in rootless.rs, which runs
# one half as root and the other half as a normal user) are allowed.
set -euo pipefail

log="${1:?usage: ci-no-skips.sh <test-log>}"
[[ -r $log ]] || { echo "ci-no-skips: cannot read $log" >&2; exit 2; }

allowed=(
  'SKIP: test runner is root'
  'SKIP: test runner is not root'
  'SKIP: this host allows every isolation feature'
)

pattern=$(printf '%s\n' "${allowed[@]}" | paste -sd'|')

skips=$(grep -E '(^|[[:space:]])(SKIP:|skipped:)' "$log" | grep -Ev "$pattern" || true)

if [[ -n $skips ]]; then
  echo "::error::nawa tests skipped isolation checks (the host refused a kernel feature):"
  echo "$skips" | sort | uniq -c | sort -rn
  exit 1
fi

n=$(grep -cE "$pattern" "$log" || true)
echo "ci-no-skips: no unexpected skips ($n by-design skip(s) ignored)"
