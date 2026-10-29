#!/usr/bin/env bash
# scripts/vendor-nawa.sh — (re)generate the vendored crates for nawa.
#
# Run on a developer machine WITH network and cargo, then commit
# vendor and .cargo/config.toml together. Re-run whenever
# Cargo.lock changes. The distribution build (stage 2 and the `nawa` recipe) also
# build offline from these files, so no build step ever needs crates.io.
set -euo pipefail
cd "$(dirname "$0")/.."

# a config that already points at vendor/ would break the fetch below
rm -rf vendor .cargo/config.toml
mkdir -p .cargo
cargo vendor --locked vendor > .cargo/config.toml

# prove the result really is self-contained
cargo build --release --offline --locked
echo "vendored $(find vendor -mindepth 1 -maxdepth 1 -type d | wc -l) crates into vendor"
echo "now commit: vendor .cargo/config.toml"
