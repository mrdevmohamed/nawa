# Nawa documentation

The official documentation is published as a searchable static site:

**[Nawa documentation](https://mrdevmohamed.github.io/nawa/)**

## User guide

- [Getting started](getting-started.md)
- [Command reference](commands.md)
- [Configuration](configuration.md)
- [Package recipes](recipes.md)
- [Build and fetch](build-and-fetch.md)
- [Repositories and security](repositories-and-security.md)
- [Transactions, services, and plugins](transactions-services-plugins.md)
- [Development](development.md)

## Design and engineering records

These documents describe architecture, implementation constraints, proposals, and review evidence. They are not all end-user guarantees.

- [Build isolation](build-isolation.md)
- [Implementation status](design/IMPLEMENTATION-STATUS.md)
- [Fetch phase design](design/fetch-phase.md)
- [Signed repository index design](design/signed-repository-index-v2.md)
- [Plugin API v1](design/plugin-api-v1.md)
- [Package management roadmap](design/package-management-roadmap.md)
- [Dependency-aware rebuild proposal](future-dependency-aware-rebuild.md)
- [PR #15 review](reviews/PR-15-REVIEW.md)
- [PR #16 review](reviews/PR-16-REVIEW.md)

## Documentation conventions

- The user guide describes behavior implemented in the current source tree.
- Design documents explain architecture and compatibility requirements.
- Proposals and incomplete work are labeled as such.
- CI evidence is tied to the exact commit tested; a later documentation commit does not inherit an earlier test result.
