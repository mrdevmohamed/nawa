# Nawa

Nawa (نواة) is a source-based package manager for Linux distributions, written in Rust. It builds packages from TOML recipes, manages binary repositories, and installs packages transactionally with rollback and Ed25519 signature verification.

## Features

- Declarative package recipes and dependency resolution.
- Multiple SHA-256-verified sources and recipe-local assets.
- Lockfile-driven Go Modules and Rust/Cargo dependency fetching, followed by offline builds.
- Build isolation: none, env, sandbox, and closure.
- Binary repositories, signed indexes, package caching, and reproducible builds.
- Atomic installation, upgrade, removal, and rollback.
- Transaction history and recovery.
- runit service management and external plugins.

Nawa does not include a package recipe collection. Configure a recipe tree and build profiles with --recipes and --profiles, or in nawa.toml.

## Documentation

**[Read the Nawa documentation](https://mrdevmohamed.github.io/nawa/)**

The documentation includes installation, command reference, configuration, recipe writing, build isolation, source fetching, repositories, signing, transaction history, services, plugins, and development guidance.

The Markdown source is maintained in [docs/](docs/README.md). Design specifications and historical review records are kept alongside the user guide.

## Install

Download a release binary from [GitHub Releases](https://github.com/mrdevmohamed/nawa/releases). Static musl builds are provided for x86_64 Linux.

## Build and test

Dependencies are resolved from Cargo.lock. Fetch them once, then build and test offline:

~~~sh
cargo fetch --locked
cargo build --release --offline --locked
cargo test --release --offline --locked
~~~

For package recipes, declare [fetch.cargo] so Nawa fetches the locked crates into its content-addressed cache before the offline build.

## Quick start

~~~sh
nawa init
nawa config show
nawa search <term>
nawa install <package>
nawa build <package>
nawa update
nawa list
~~~

Use nawa --help and nawa COMMAND --help for command options. Commands accept --root to work against a test root instead of the live system.

## Project layout

- src/ — package manager implementation
- tests/ — unit and integration tests
- scripts/ — CI helpers and smoke tests
- docs/ — user guide, design documents, and review records
