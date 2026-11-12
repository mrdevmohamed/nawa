# Development

Nawa is written in Rust. The project uses Cargo, a locked dependency graph, and GitHub Actions for CI and release builds.

## Requirements

- Rust toolchain compatible with the project's declared MSRV (Rust 1.82).
- Cargo.
- A Linux environment for the full integration and isolation test suite.
- musl-tools when building the static x86_64 musl release locally.

## Clone and build

~~~sh
git clone https://github.com/mrdevmohamed/nawa.git
cd nawa

cargo fetch --locked
cargo build --release --offline --locked
~~~

After fetching dependencies once, Cargo build and test commands can run offline:

~~~sh
cargo test --release --offline --locked
cargo clippy --release --all-targets --offline --locked -- -D warnings
~~~

## Test and CI

The main workflow is .github/workflows/ci.yml. It runs:

- A module-layering check (`scripts/check-layering.py`): no dependency cycles between top-level modules and no `use crate::*`. Lower layers (`atomic`, `checksum`, `db`, `transaction`, ...) never import higher ones (`install`, `plugins`, `commands`, `cli`); when two modules need the same code, move it down into a shared module. Run it locally with `python3 scripts/check-layering.py`.
- Release-mode build, Clippy, and the Rust test suite.
- Tests as normal user and root across supported Ubuntu runner versions.
- Real CLI builds at none, env, sandbox, and closure isolation levels.
- CLI smoke tests for package build, signing, upgrade, rollback, remote repositories, and services.
- A static x86_64 musl build check.
- A final CI passed job that requires all preceding jobs to succeed.

The isolation suite deliberately treats unexpected skips as failures. A green test run should be tied to the exact commit it tested; later commits do not inherit earlier CI evidence automatically.

## Release process

The release workflow is .github/workflows/release.yml. A release tag must match the version in Cargo.toml (for example, v1.0.0). The workflow waits for CI, builds the static musl binary, verifies that it is static, generates a SHA-256 checksum, and publishes the release assets.

## Documentation site

The documentation is authored in Markdown under docs/ and configured by zensical.toml.

Install the documentation builder and preview the site:

~~~sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install "zensical==0.1.0"
zensical serve
~~~

Build the static site:

~~~sh
zensical build --clean
~~~

The GitHub Actions workflow in .github/workflows/docs.yml builds the site for pull requests and deploys it from master. Repository Pages must be configured to use **GitHub Actions** as its build and deployment source.

## Contribution guidelines

- Keep user documentation in English to match the project's existing README and design documents.
- Document behavior that exists in the current code. Label proposals and incomplete features clearly.
- Keep examples reproducible and mark illustrative values such as URLs and checksums.
- Update command and configuration pages when CLI behavior changes.
- Preserve design documents and review records; they provide important engineering history.
