# Nawa

**A source-based package manager for Linux distributions, written in Rust.**

Nawa builds packages from declarative TOML recipes, can install prebuilt packages from binary repositories, and manages installed files and package changes transactionally.

## Start here

- **[Getting started](getting-started.md)** — install Nawa, configure a recipe tree, and run your first package command.
- **[Command reference](commands.md)** — package, repository, build, and system-management commands.
- **[Write recipes](recipes.md)** — recipe format, dependencies, build steps, and source declarations.
- **[Repositories and security](repositories-and-security.md)** — repositories, signatures, and trusted keys.

## What Nawa provides

- **Source builds:** build packages from TOML recipes with declared build and runtime dependencies.
- **Binary repositories:** install package archives from local or remote repositories, with signed repository metadata.
- **Package planning:** one deterministic solver handles authenticated runtime
  package-version requirements and v5 conflict/replacement declarations;
  legacy indexes remain readable with missing relations marked unknown.
- **Controlled source fetching:** download SHA-256-verified sources and prepare Go Modules or Cargo dependencies before offline builds.
- **Build isolation:** choose none, env, sandbox, or closure isolation.
- **Package management:** install, update, remove, query, and inspect packages and their files.
- **Recovery:** record package-changing operations, recover interrupted work, and undo supported transactions.
- **System integration:** inspect broken ELF shared-library dependencies.
- **System intelligence:** inspect attributable runtime/installed/profile state
  and review immutable signed package plans. Migration transaction and snapshot
  integration is tested in disposable roots; production boot migration remains
  blocked pending installed-disk BIOS/UEFI and rescue acceptance. See the
  [implementation tracker](implementation-progress.md).
- **Extensibility:** discover and run external plugins through Nawa's plugin interface.

Nawa does **not** ship a package recipe collection. A distribution or administrator supplies a recipe tree and, optionally, architecture-specific build profiles.

## Project links

- [Source code](https://github.com/mrdevmohamed/nawa)
- [Releases](https://github.com/mrdevmohamed/nawa/releases)
- [Issue tracker](https://github.com/mrdevmohamed/nawa/issues)
- [Changelog](https://github.com/mrdevmohamed/nawa/blob/master/CHANGELOG.md)

## Documentation scope

The user guide describes commands and behavior available in the current source tree. Design documents and review records are included separately and are labeled as design or implementation status; proposals are not presented as shipped features.
