# System intelligence and migration engine

> **STATUS: PROPOSED — DESIGN FOR DISCUSSION — NOT IMPLEMENTED**
>
> This document is an architectural proposal. It does not describe shipped Nawa
> behavior and does not authorize a package-format, repository-index, database,
> CLI, or system-migration change by itself. Examples are illustrative.
>
> **Design baseline:** Nawa 1.0.0 source and documentation inspected on
> 2026-11-17. Re-check the implementation before turning any section into a
> task. The current source contains runit-specific service-management code, but
> the accepted target design explicitly removes service management from Nawa.

## 1. Purpose

Nawa should evolve from a package manager that primarily resolves package
names and package dependencies into a system-aware package manager that can
reason about the desired and actual state of a Linux installation.

The user should describe the outcome they want. Nawa should:

1. Inspect the target system and identify what is known, unknown, or
   inconsistent.
2. Resolve package dependencies and system capabilities.
3. Calculate the smallest safe set of package and system changes.
4. Explain why every change is necessary.
5. Present the complete plan before making mutations.
6. Execute the plan through the existing package installer and transaction
   infrastructure.
7. Verify the resulting state, including work that must wait for reboot.
8. Recover safely when a step fails or the machine restarts unexpectedly.

This is deterministic package-management logic, not an AI feature. The solver
must make decisions from explicit metadata, administrator policy, and measured
system state. It must not guess at critical system behavior.

The runit-to-systemd transition is the motivating example. The initial system
intelligence scope should stay narrow: identify the init system, plan a
supported init migration, and verify the booted result. Other provider domains
are separate future proposals, not implicit additions to this design.

## 2. Existing foundations and constraints

The current project already has important building blocks:

- Rust CLI and declarative TOML package recipes.
- Build and runtime dependency declarations.
- Binary repositories with authenticated metadata and package digests.
- Ed25519 repository trust and SHA-256 artifact verification.
- Package installation/removal staging and file-conflict checks.
- Package-level rollback and transaction history/recovery infrastructure.
- Package transaction hooks for shared cache/index refresh work.
- Existing runit-specific service-management code in the source tree, which
  is explicitly scheduled for removal rather than extension.

The proposal should extend these foundations rather than replace them.

### 2.1 Service management is explicitly outside Nawa

The current source contains a runit-specific `nawa service` command and
service-management implementation. The accepted design decision is to remove
this subsystem completely. Nawa is a package manager and migration planner; it
is not a service manager.

The target architecture must not add or retain:

- A `nawa service` command or service-management subcommands.
- A Service Provider trait or runit/systemd service backends.
- APIs to list, enable, disable, start, stop, restart, or query services.
- Service-manager or session-manager capabilities in the initial capability
  model.
- Calls to `sv`, `systemctl`, or equivalent commands to control services.
- Package-install behavior that automatically enables or starts services.

Package contents may still include service-related files. Systemd unit files,
systemd preset files, runit service run scripts, and related configuration are
ordinary package payload files: Nawa may install, upgrade, verify, and remove
them through its normal file-management path. Their activation and runtime
management belong to the selected init/service manager, distribution
provisioning, or the administrator—not to Nawa.

The implementation audit must also remove service-specific installer coupling,
including any call such as `crate::service::keep_disabled` and any dependency
on runit disabled markers. Do not replace this with systemd enablement tracking
or another service-manager integration. A genuinely generic user-configuration
preservation mechanism may be designed separately, but it must not manage
service state.

Distribution recipes and package payloads must not rely on Nawa implicitly
enabling or starting services after installation. Faseela's own system setup
and service-manager policy must handle that responsibility outside Nawa.

### 2.2 Existing transactions are not whole-system atomicity

Nawa's package installer and history journal provide valuable recovery
behavior, but a multi-package operation must not be described as globally
atomic unless the implementation can prove that property.

Changing PID 1, boot configuration, initramfs, and packages crosses several
independent system boundaries. Service enablement and runtime service control
are not migration-engine responsibilities in Nawa. A migration may need to pause
for a reboot and continue in a later process. It must have its own durable
state machine and use the existing transaction journal where appropriate.

Do not claim that package rollback alone can restore an arbitrary system-wide
migration.

### 2.3 Current source organization and architectural debt

A review of the current `master` source tree (2026-11-17) found a mixed
organization rather than a consistent module architecture:

- The source tree contains approximately 115 Rust source files, with about 40
  files directly under `src/`; some areas are grouped into directories while
  other domain modules remain flat at the crate root.
- There are useful existing boundaries such as `build/`, `commands/`,
  `fetch/`, `install/`, `recipe/`, `repo/`, `sandbox/`, `transaction/`,
  and `upgrade/`. These should be preserved where they represent cohesive
  subsystems.
- Large orchestration files still combine multiple responsibilities:
  `commands/installing.rs` handles several package lifecycle use cases and
  coordinates repository access, planning, installation, locking, recovery,
  and transaction history; `install/mod.rs` is a large public-in-crate
  installer facade and implementation file; `main.rs` contains command
  dispatch and package lifecycle coordination in addition to process startup;
  `cli.rs` combines argument definitions, completion behavior, and command
  lock policy.
- Some concerns are split across multiple locations while other unrelated
  concerns are coordinated directly by command handlers. This makes ownership,
  dependency direction, and the correct place for new system-migration logic
  harder to determine.

This is architectural debt accumulated through incremental feature growth. It
does not mean every existing module is wrong or that all files should be moved.
The goal is to establish explicit boundaries and dependency rules before
adding the capability solver and migration engine. The source audit must verify
the exact dependency graph rather than infer it from directory names.


### 2.4 Current dependency and repository metadata limits

The current implementation is not a general package/provider solver:

- `plan.rs` builds a dependency-first graph from the local recipe tree. It
  follows declared build and runtime dependencies and treats already installed
  dependencies as satisfied.
- Binary repository entries in the current signed v2/v3 index contain package
  identity, archive location, digest, and size metadata. They do not currently
  expose a signed `provides` capability list or a complete runtime-dependency
  list for solving before archive download.
- Runtime dependencies are recorded in package archive `.PKGINFO` and in the
  installed package database. A binary-only package without a local recipe can
  currently be installed, but that path does not gain the same dependency
  planning as a recipe-backed install.
- The current repository upgrade path compares installed package versions and
  orders candidates using available recipe data, with an alphabetical fallback
  when that data is unavailable. It does not select alternative providers.

These limitations are part of the baseline, not behavior that this proposal
can assume already exists. The initial implementation must define one
authenticated candidate model that can represent both recipe-built and
binary-only packages. Until the signed metadata needed for a safe solve exists,
Nawa must report that the plan is incomplete and refuse a critical init
migration rather than assume missing dependencies or capabilities do not
exist.

## 3. Goals and non-goals

### 3.1 Goals

- Model system roles as capabilities rather than hard-coded package names.
- Distinguish the installed package set, the configured target profile, and
  the currently running system.
- Make dependency solving explainable and deterministic.
- Detect the current and target init system without controlling services.
- Describe init migrations as reviewed, versioned transitions.
- Keep service management outside Nawa, with no service-provider abstraction.
- Run only trusted, known migration handlers for privileged system changes.
- Make plans inspectable and dry-runnable.
- Reuse existing package installation, artifact verification, history, and
  recovery code.
- Preserve old recipes, archives, indexes, databases, and command behavior
  unless a separately reviewed compatibility change is approved.
- Fail closed when critical system facts or migration prerequisites cannot be
  established.

### 3.2 Non-goals

This proposal does not require:

- Rewriting Nawa or replacing its package database.
- Replacing the source build engine or TOML recipes.
- Replacing the existing repository signature and artifact-verification
  mechanisms.
- Adding an AI model or network service to the package manager.
- Running arbitrary scripts downloaded from repositories as root.
- Automatically changing a live system merely because a package name appears
  in an upgrade list.
- Treating every provider change as a simple package conflict.
- Managing service state or providing service-manager integrations.
- Claiming that all filesystem, bootloader, or user-data changes can be rolled
  back automatically.
- Supporting every Linux init system in the first implementation.

## 4. Target architecture: Feature-Oriented Hexagonal Architecture

The target architecture for Nawa is **Feature-Oriented Hexagonal Architecture
with a Functional Core / Imperative Shell**. Nawa remains one Rust application
(a modular monolith); this does not introduce microservices or require a
multi-crate Cargo workspace.

This combines three complementary ideas:

- **Feature-oriented organization:** user-visible use cases are grouped by
  capability, such as package installation, removal, build, query, repository,
  and history. A feature owns its application workflow instead of scattering
  that workflow across generic command and utility modules.
- **Hexagonal architecture:** application use cases depend on narrow ports.
  Concrete adapters implement those ports for the filesystem, package
  database, repositories, archive handling, signature verification, process
  execution, sandboxing, and transaction storage.
- **Functional Core / Imperative Shell:** package policy, dependency solving,
  conflict analysis, and plan construction are deterministic logic over
  explicit inputs. Network, filesystem, process, privilege, locking, and
  durable-state effects happen at the application and adapter boundaries.

This is the selected direction for the refactor. It supersedes a generic
"four folders only" interpretation of layered architecture. The layers still
exist as dependency boundaries, but the application layer is organized around
features and use cases.

### 4.1 Architectural responsibilities

| Area | Responsibility | Must not do |
|---|---|---|
| CLI | Parse arguments, validate command-line shape, and route a request to an application use case. | Resolve dependencies, modify package files, or own transaction policy. |
| Application features | Orchestrate one user operation: gather inputs, call domain logic, request confirmation, coordinate execution, and report the outcome. | Reimplement domain rules or contain low-level transport/filesystem details. |
| Domain | Own package, dependency, recipe, plan, transaction, system-profile, and migration models and deterministic rules. | Read environment variables, access the network, spawn processes, or mutate the filesystem. |
| Application ports | Define the external capabilities a use case needs through narrow interfaces. | Contain adapter implementations or become a generic interface for every helper function. |
| Adapters | Implement ports using concrete storage, network, archive, filesystem, signing, process, and sandbox facilities. | Decide user intent or package/provider policy. |
| Presentation | Render plans, diagnostics, confirmations, and human/machine-readable output. | Own package-management decisions. |
| Bootstrap | Construct concrete adapters, connect them to use cases, and start the selected command. | Become another place for business logic. |

The dependency direction is inward:

    CLI ----------------------> Application Features
                                      |
                                      v
                                  Domain
                                      ^
                                      |
    Adapters ----------------> Application Ports

    Bootstrap wires CLI + Application + Ports + Adapters together.

Application defines the ports it needs. Adapters implement those ports and may
depend on the port contracts and domain types. Domain remains independent of
Application, CLI, and Adapters. CLI must not call adapter implementations
directly. No layer may create a circular dependency.

Ports should be introduced where an external effect must be substituted,
isolated, or tested. Do not create a trait for every function or wrap simple
pure calculations in interfaces. Prefer concrete domain types and small
application-facing ports.

### 4.2 Functional Core and Imperative Shell

The **Functional Core** is primarily the `domain/` tree:

- Normalize and compare package versions.
- Resolve package and capability requirements.
- Build install, update, upgrade, and removal plans.
- Detect dependency cycles and package conflicts.
- Calculate reverse dependencies and autoremove candidates.
- Validate recipe and migration definitions.
- Determine legal transaction and migration state transitions.
- Explain why a package or provider is included in a plan.

These operations receive their inputs explicitly and return values or
structured errors. The same inputs and policy must produce the same result.
They must not read the live package database, download metadata, inspect
`/proc`, invoke commands, or write files.

The **Imperative Shell** is the application workflow plus concrete adapters:

- Read installed state and trusted repository candidates through ports.
- Invoke domain planning functions with explicit snapshots.
- Present the plan and collect the required authorization.
- Acquire the appropriate lock and revalidate assumptions at the mutation
  boundary.
- Download and authenticate artifacts, apply payloads, update the database,
  and record transaction intents/outcomes through adapters.
- Verify results and report failures or recovery requirements.

Do not hold package database locks across long downloads or builds. Preserve
Nawa's existing lock and transaction guarantees while making their ownership
explicit. A plan built from an earlier snapshot must not be applied blindly if
the relevant installed state or repository inputs have changed before commit.

### 4.3 Practical Rust module organization

The following is the target grouping. It describes ownership, not a requirement
to create every file or directory immediately. Keep Nawa in one Cargo crate
during the refactor.

    src/
      main.rs                 # minimal process entry point
      bootstrap.rs            # composition root and concrete wiring

      cli/
        mod.rs
        args.rs
        commands/
          package.rs
          build.rs
          repository.rs
          history.rs
          system.rs

      application/
        mod.rs
        context.rs
        lifecycle.rs          # lock, recovery, post-transaction coordination
        ports/
          package_database.rs
          repository.rs
          artifact_store.rs
          filesystem.rs
          signature_verifier.rs
          process_runner.rs
          transaction_journal.rs
          init_system_probe.rs
        features/
          package/
            install.rs
            remove.rs
            update.rs
            upgrade.rs
            autoremove.rs
          build/
            build.rs
            cache.rs
          query/
            search.rs
            info.rs
            deps.rs
            why.rs
            files.rs
          repository/
            sync.rs
            create.rs
            verify.rs
          history/
            list.rs
            undo.rs
            rollback.rs
          groups/
            list.rs
            install.rs
            remove.rs
          plugins/
            list.rs
            manage.rs
            run.rs
          system/
            detect.rs
            profile.rs
            plan.rs
            migrate.rs

      domain/
        package/
          model.rs
          version.rs
        dependency/
          resolver.rs
          graph.rs
        planning/
          install.rs
          removal.rs
          conflicts.rs
        recipe/
          model.rs
          validation.rs
        transaction/
          model.rs
          state.rs
        groups/
          model.rs
        system/
          identity.rs
          profile.rs
          capability.rs
          migration.rs

      adapters/
        database/
        repository/
        archive/
        signing/
        filesystem/
        process/
        sandbox/
        transaction/
        plugins/

      presentation/
        output.rs
        diagnostics.rs
        confirmation.rs
        formatting.rs

The tree may retain cohesive existing subsystems during early stages. For
example, the build pipeline, sandbox implementation, archive format, and
repository-index implementation do not need to be flattened or rewritten merely
to match this diagram. Move or split a module only when its current ownership
crosses a boundary or makes a use case difficult to test.

The exact names and placement of modules should be settled by the Stage 0
source audit. The target tree is a guide, not a license for a mass file move.

### 4.4 Example: the `nawa install firefox` flow

The intended flow separates planning from effects:

1. **CLI adapter:** parses `nawa install firefox` and constructs an install
   request. It does not load the package database or invoke the installer.
2. **Install use case:** coordinates the operation and requests the required
   inputs through application ports.
3. **Input adapters:** obtain an installed-package snapshot and authenticated
   candidate metadata from recipes and/or the signed repository index. Both
   source-built and binary-only candidates must use the same normalized domain
   model; missing dependency or capability metadata must remain explicitly
   unknown, not be treated as an empty list.
4. **Functional Core:** the domain planner resolves declared dependencies,
   applies the target init-provider constraint, detects conflicts, determines
   install order, and returns an explainable plan. It performs no I/O. The
   initial implementation is not a general SAT solver and must not claim to be
   one.
5. **Presentation:** renders the complete plan and obtains the required
   confirmation. Dry-run ends here without mutation.
6. **Execution coordination:** checks cancellation and preconditions, acquires
   the required lock at the mutation boundary, re-reads the relevant installed
   state, and revalidates the approved plan. If inputs changed, it must stop
   and require a fresh plan/approval rather than execute a stale plan; it then
   records transaction intent.
7. **Effect adapters:** fetch and verify the selected artifact, apply the
   package payload, update installed state, and persist each transaction
   outcome. Existing archive authentication, file-conflict, journaling, and
   recovery guarantees must be preserved.
8. **Post-transaction lifecycle:** after releasing the package lock, dispatch
   eligible distribution maintenance triggers and observer events, then report
   the final outcome.

Conceptually:

    InstallRequest
        |
        v
    InstallUseCase
        |
        +----> PackageDatabasePort ----> DatabaseAdapter
        |
        +----> RepositoryPort ----------> RepositoryAdapter
        |
        v
    DomainPlanner (pure)
        |
        v
    Explainable InstallPlan
        |
        v
    Confirmation / Authorization
        |
        v
    Transaction Coordinator
        |
        +----> ArtifactStorePort -------> Fetch / Cache Adapter
        +----> SignatureVerifierPort --> Signing Adapter
        +----> FilesystemPort ----------> Payload Adapter
        +----> TransactionJournalPort -> Journal Adapter
        |
        v
    Install Result

The exact execution ordering remains governed by the transaction and
installer contracts. This diagram is an ownership model, not a replacement
transaction algorithm.

### 4.5 Mapping the current source tree

The first mapping from current `master` to the target architecture is:

| Current code | Target ownership |
|---|---|
| `main.rs` | Minimal process entry point; move command lifecycle coordination and concrete wiring into dedicated application/bootstrap modules. |
| `cli.rs` | `cli/`: argument definitions, command types, and shell completions. Lock policy moves out of CLI definitions. |
| `commands/mod.rs` and `commands/*.rs` | Split into thin CLI adapters and feature-specific application use cases. |
| `commands/installing.rs` | Split by install, remove, update, upgrade, and autoremove use cases; keep shared workflow coordination small and explicit. |
| `plan.rs` and `deps.rs` | `domain/planning/` and `domain/dependency/`: pure planning and graph rules. |
| `install/mod.rs` | Keep a narrow installer facade while separating payload application, authentication, journals, rollback, and persistence responsibilities. |
| `install/payload.rs` | Filesystem/package-payload adapter responsibilities. |
| `install/journal.rs`, `previous.rs`, `rollback.rs`, `step.rs` | Transaction and filesystem adapter implementations, with transaction state models separated from storage. |
| `db.rs` | Database adapter and persistence mapping; keep domain package/state types independent of file formats. |
| `repo/`, `repositories.rs`, `fetch/` | Repository and fetch adapters; keep authenticated index/package data models explicit and validated. |
| `recipe/model.rs`, `validate.rs`, `expand.rs` | Domain recipe model and validation/expansion rules; file discovery and loading remain an I/O boundary. |
| `build/`, `buildenv.rs`, `fetchrun/`, `sandbox/` | Build use case and cohesive build subsystem, with process, fetch, and sandbox effects behind adapter boundaries where needed. |
| `transaction/`, `recovery.rs`, `lock.rs`, `undo.rs` | Separate transaction domain/state, application lifecycle coordination, and durable journal/lock adapters. |
| `config.rs`, `profile.rs`, `sys.rs` | Split configuration loading and host observations from validated domain configuration/profile models. |
| `groups/` and `commands/groups.rs` | Group membership domain plus group application use cases; keep catalog loading in its repository adapter. |
| `plugins/` and `commands/plugins.rs` | Plugin management use cases plus isolated discovery/runner adapters; plugins remain outside the trusted package/migration core. |
| `artifact.rs`, `sign.rs`, `checksum.rs` | Artifact identity and verification values where pure; concrete cryptographic and file access in signing/artifact adapters. |
| `sources.rs`, `assets.rs`, `fetchrun/`, `slim.rs`, `closure.rs` | Keep with the build feature/subsystems they serve; isolate network and filesystem effects behind adapter boundaries where needed. |
| `output.rs`, `diag.rs`, `color.rs`, `confirm.rs`, `humanize.rs` | Presentation and user-interaction responsibilities. |
| `service.rs`, `commands/service.rs` | Delete as part of the accepted removal of service management from Nawa. |

This table is an initial ownership hypothesis. Stage 0 must inspect imports,
call sites, and tests before assigning the final destination of each module.
In particular, do not mechanically move every current file into the proposed
folder with the same name.

### 4.6 Refactoring and dependency rules

- Keep `main.rs` minimal. Use `bootstrap.rs` to construct adapters and connect
  them to application use cases.
- Keep CLI types and parsing separate from application orchestration and
  package policy.
- Make each feature use case responsible for one user operation. Shared
  behavior belongs in a named application service or domain function only
  when it is genuinely shared.
- Keep dependency resolution and plan construction pure and deterministic.
- Keep installer internals behind a narrow application-facing API. Command
  handlers must not reach into payload, journal, rollback, or database internals.
- Define ports around meaningful external boundaries. Avoid unnecessary traits,
  generic repositories, service locators, global mutable state, and a large
  context object passed everywhere.
- Domain modules may depend on other domain modules, but not on CLI,
  Application, or Adapters.
- Application may depend on Domain and its own port contracts.
- Adapters may depend on port contracts and domain data types, but never on
  CLI or presentation code.
- No adapter may silently bypass signature verification, package conflict
  checks, transaction recording, or migration authorization.
- No service-management APIs, providers, service state, or service-manager
  invocations are permitted in Nawa. Service definitions and presets remain
  ordinary package payload files.
- Do not change recipe syntax, archive format, repository index format,
  installed database format, CLI behavior, or recovery semantics as an
  incidental result of structural refactoring.
- Do not split into multiple Cargo crates or introduce plugin interfaces
  without a demonstrated need and a separately reviewed boundary.

A new module must have one clear owner, a narrow API, and tests at the layer
where its rules belong. Enforce dependency direction through module visibility,
code review, and focused architecture checks; do not rely on folder names alone.

### 4.7 Testing strategy for this architecture

- **Domain unit tests:** run without filesystem, network, subprocess, root
  privileges, or a temporary installed system. Cover dependency resolution,
  version selection, conflicts, planning, and state transitions.
- **Application tests:** use small fake/in-memory port implementations to
  verify use-case orchestration, confirmation gates, failure handling, and
  transaction boundaries.
- **Adapter tests:** exercise real filesystem/database/archive behavior with
  temporary roots, plus controlled local HTTP fixtures for repository and
  fetch behavior.
- **Integration tests:** verify that adapters are wired correctly and that
  existing package commands preserve their observable behavior.
- **End-to-end tests:** use disposable VMs for boot-critical init migration;
  unit tests alone cannot establish that a machine reboots successfully.

Do not mock the domain planner to test its own rules. Do not use real root
filesystem mutations or live network dependencies in ordinary domain tests.


## 5. System state: observed, installed, and desired

A central rule is to keep three kinds of state separate. Nawa observes init
identity only; it does not inspect or manage service enablement or runtime
service state.

### 5.1 Observed runtime state

What is actually running now: PID 1, init-system identity, mounted
filesystems, and other boot-related facts needed by the approved migration.

Examples:

- PID 1 is systemd.
- PID 1 is runit.
- A command is running inside a container or chroot.
- The init-system identity cannot be established.

An observation must include its source and confidence. Unknown is a valid
result; it must not silently become a guessed value.

### 5.2 Installed state

What is present in the target root: package database entries, files,
service-unit files, initramfs images, and boot configuration.

When Nawa is invoked with --root, observations of the host's /proc/1 must not
be mistaken for observations of the target root. In a chroot or offline root,
the runtime state may be unknown while the installed state remains readable.

### 5.3 Desired state

What the distribution or administrator has declared the system should use.
This is expressed by a versioned system profile and local administrator policy.

A desired profile does not prove the migration has completed. For example,
the target profile may select systemd while the running PID 1 is still runit
because a reboot is pending.

### 5.4 Detection rules

The detector should:

- Prefer structured, target-root-aware sources.
- Use /proc only when it refers to the environment Nawa is managing; do not
  query a service manager.
- Identify chroot, container, offline-root, and live-host contexts where
  possible.
- Report unavailable or contradictory observations explicitly.
- Never infer successful migration from the presence of a binary or unit file.
- Avoid writing files or changing packages; service activation is outside
  detector and migration responsibilities.

## 6. Capabilities and provider selection

Package names are implementation details. System requirements should be
expressed as capabilities.

For the initial migration design, the only system capability in scope is
`init-system`. Do not add `service-manager`, `session-manager`, or other
provider domains to Nawa's capability resolver as part of this work. Additional
capability domains require a separate, justified design decision.

A package may provide one or more capabilities. A system profile selects the
provider wanted for each exclusive role. Applications can require a capability
without requiring a particular implementation package.

### 6.1 Illustrative metadata

The following is a possible model, not the current recipe or repository schema:

    [capabilities]
    provides = [
      "init-system=runit"
    ]

A systemd package could provide:

    [capabilities]
    provides = [
      "init-system=systemd"
    ]

The final schema must define normalized capability identifiers, optional
provider versions, architecture constraints, and how a requirement can express
a compatible range or a specific provider.

### 6.2 Capability semantics

Before implementation, define these rules precisely:

- A capability requirement is not necessarily a package dependency.
- More than one package may provide a capability, but an exclusive system
  role has one selected provider in the desired profile.
- Two providers may be installed side by side during a staged transition if
  their files and package contents allow it.
- Selecting a new provider does not immediately mean the old provider package
  can be removed.
- Package-level Conflicts should mean an actual package/file/runtime
  incompatibility, not merely that two packages provide alternative choices.
- The solver must distinguish a provider candidate from the active provider.
- Administrator pins and distribution policy must influence provider choice.
- Ambiguous or equally valid critical providers should be reported, not chosen
  arbitrarily.

Do not model runit and systemd as ordinary package conflicts if that prevents
the migration planner from preparing the new provider before reboot. Exclusive
selection belongs to the target system profile; package conflicts are a
separate concept.

### 6.3 Backward compatibility

Legacy packages without capability metadata must remain readable. The solver
must not invent capabilities for them.

For critical roles such as init-system, a migration must require explicit,
authenticated provider metadata and a supported migration definition. Missing
metadata means the transition cannot be planned safely.

The exact location of capability declarations remains an open schema decision.
Any repository-supplied metadata that affects solving must be covered by the
existing authenticated repository data or a separately reviewed, authenticated
format. Unsigned side data must not influence privileged changes.

## 7. System profiles

A system profile describes a distribution's intended composition. It should
not be confused with a snapshot of the packages currently installed.

Illustrative profile:

    schema = 1
    distribution = "faseela"
    profile = "desktop"
    architecture = "x86_64"

    [providers]
    init-system = "systemd"

This is a discussion example only. It does not imply that all listed
capabilities or providers are currently supported by Nawa or Faseela.

The design should distinguish:

- Distribution defaults, shipped and versioned by the distribution.
- Local administrator overrides, stored separately and never overwritten
  silently by a package update.
- Effective target profile, produced by merging defaults and valid overrides.
- Observed runtime state, reported independently by the detector.

A profile change should be an explicit input to plan generation. Installing a
desktop package must not silently rewrite the system's init provider.

The profile file location, ownership, merge rules, and update policy need to be
decided with Faseela before implementation.

## 8. Dependency and capability solver

The solver should extend the current dependency-resolution facilities rather
than become a second independent package resolver.

### 8.1 Inputs

A solve request should include:

- Requested package operations and user intent.
- Installed package records and install reasons.
- Trusted repository candidates and their metadata.
- Package dependencies, provides, conflicts, and replacements.
- Effective target system profile.
- Architecture and version constraints.
- Administrator pins, exclusions, and provider preferences.
- Protected packages and unsupported/unknown system state.
- Whether the operation is a normal package transaction or a staged
  migration.

### 8.2 Output

The solver should produce a structured solution containing:

- Packages to install, upgrade, keep, or remove.
- Provider selections for requested system capabilities.
- Dependency and capability explanation chains.
- Conflicts and the reason each candidate was rejected.
- Package files and dependents affected by each operation, without interpreting
  service definitions or changing service state.
- Required migration handlers and their prerequisites.
- Reboot requirement and post-reboot init-system checks.
- Any unresolved fact or unsafe condition that prevents execution.

The plan must be deterministic for identical inputs and policy.

### 8.3 Selection policy

Prefer the smallest valid change set. Preserve explicit user-installed roots
where possible. Respect configured provider preferences and repository
priorities. Never remove a protected system package merely to make a solve
succeed.

When no valid solution exists, return an actionable explanation. Do not
silently discard a requirement, downgrade repository authentication, or select
an untrusted package.

### 8.4 Explanation

Every planned action should have a reason that can be traced to a request,
dependency, capability requirement, profile change, or migration step.

For example:

    systemd
      required by: target profile
      capability: init-system=systemd
      replaces active provider selection: init-system=runit

    runit
      retained until: systemd boot verification succeeds
      removal reason: old provider cleanup after migration

The explanation must distinguish package removal from deactivating a provider.

## 9. Service-management boundary and package payloads

Service management is not a Nawa subsystem. This is an explicit architecture
boundary, not a deferred provider implementation.

Nawa may handle service-related files only as ordinary package payload:

- Install and remove unit files, run scripts, presets, and configuration files
  according to the package manifest.
- Apply normal package ownership, permissions, conflict checks, verification,
  and transaction recovery to those files.
- Never infer that a service is enabled, started, healthy, or even usable
  merely because a related file exists.

Nawa must not expose a service API, service CLI, provider interface, service
state database, or service-specific capability. It must not invoke service
manager commands as part of package installation, upgrade, removal, or init
migration. This includes implicit post-install activation through recipe
hooks.

The distribution's service manager, distribution provisioning tools, and the
administrator are responsible for deciding which services are enabled and for
starting, stopping, or supervising them. Faseela must provide the appropriate
systemd presets/units or runit service definitions and handle activation
outside Nawa.

The migration engine may detect and verify the init system itself (for example,
whether systemd is PID 1 after reboot). It must not enable, disable, start,
stop, restart, list, or query individual services. A migration definition must
not contain service-management steps.

When removing the existing runit-specific implementation, use this source
cleanup checklist:

- Delete `src/service.rs` and `src/commands/service.rs`.
- Remove the service module declarations and command registration from
  `src/main.rs` and `src/commands/mod.rs`.
- Remove `ServiceCmd`, `Cmd::Service`, dispatch, completion definitions, and
  the service lock-mode exception from `src/cli.rs` and related CLI code.
- Remove service-specific tests, help text, documentation, and shell
  completions.
- Remove installer calls to `crate::service::keep_disabled` (including the
  payload/install path), the runit disabled-marker path, and all related
  imports and tests.
- Do not replace this with systemd enablement tracking or another hidden
  service-management path. If generic configuration-file preservation is
  needed, design it independently from service state.

The removal is complete only when the crate builds and tests pass without a
service command, service-management module, service-manager invocation, or
runit-specific installer marker behavior.

## 10. Migration engine

A migration is a named, versioned transition from a known system state to a
known target state. It is not an arbitrary package hook.

Example transition:

- From: active and configured provider is runit.
- To: target provider is systemd.
- Preconditions: supported Faseela release, known boot layout, verified
  packages, supported architecture, and a recoverable boot path.
- Package plan: install required systemd packages and their ordinary package payloads,
  including unit and preset files.
- System steps: prepare boot/initramfs integration using trusted handlers.
- Reboot: required.
- Verification: check that systemd is PID 1 and required boot postconditions
  are satisfied. Nawa does not inspect or manage individual services.
- Finalization: remove or retire old runit components only after verification
  and after the rollback policy allows it.

### 10.1 Migration definition

An illustrative declaration:

    id = "faseela-runit-to-systemd-v1"
    from = "init-system=runit"
    to = "init-system=systemd"
    handler = "faseela.init.runit_to_systemd"
    requires_reboot = true

This is conceptual metadata. The actual contract must be typed, versioned,
validated, and bound to a trusted built-in handler identifier. A repository
must not be able to provide an arbitrary executable path or shell command for
root to run.

### 10.2 Trusted handlers

Privileged migration handlers should be built into Nawa or delivered through a
separately reviewed and authenticated extension mechanism with a strict
capability boundary. The first implementation should prefer built-in
allowlisted handler IDs.

A handler should expose small, typed operations with explicit inputs and
postconditions. It must not receive unrestricted authority to execute
repository-supplied scripts.

Migration metadata may describe which handler is needed, but metadata alone
must never grant new privilege.

### 10.3 Durable state machine

The migration should persist a versioned state record under Nawa's state
directory, for example:

    /var/lib/nawa/migrations/<migration-id>.toml

Suggested states:

| State | Meaning |
|---|---|
| planned | Plan created; no mutation has begun. |
| preparing | Preconditions checked and preparation is running. |
| awaiting-reboot | Packages and boot preparation are committed; reboot is required. |
| verifying | The machine has rebooted and postconditions are being checked. |
| completed | Target state has been verified and finalization is complete. |
| failed | A step failed and the failure is recorded. |
| interrupted | Process ended before a step outcome was committed. |
| needs-recovery | Automatic safe continuation is not possible. |
| rollback-required | The target state failed verification and recovery action is required. |

Every state transition must be durably recorded. Writes should use the project's
atomic publication and fsync conventions. Recovery must inspect recorded
intents and actual system state; it must not blindly repeat non-idempotent
steps.

### 10.4 Migration stages

1. **Detect:** compare observed state, installed state, and target profile.
2. **Preflight:** verify privileges, packages, disk space, architecture, boot
   layout, required tools, repository trust, and recovery prerequisites.
3. **Plan:** solve package and provider changes; display all actions.
4. **Prepare:** install required packages and service definitions using the
   existing verified installer; prepare boot integration through the trusted
   handler; preserve the previous boot path where supported.
5. **Commit pending state:** durably record that the new provider is prepared
   and a reboot is required.
6. **Reboot boundary:** Nawa exits with a clear pending-migration status. The
   system must not pretend the new init is active before reboot.
7. **Verify:** on the next boot, inspect PID 1 and required system services.
8. **Finalize:** only after verification and rollback-policy checks, retire
   old provider components and mark the migration complete.

A migration may be refused if its handler cannot prove a safe preparation and
verification path for the target machine.

### 10.5 Rollback policy

The migration definition must state what can be restored, how, and at which
stage. At minimum, it should preserve the old boot entry or another documented
recovery route when the platform supports it.

Do not promise automatic rollback of arbitrary root filesystem changes.
Package-level rollback, saved configuration copies, boot-entry restoration,
filesystem snapshots, and manual recovery are different mechanisms and must be
reported honestly.

Do not remove the old init provider before the new provider has booted and
passed verification. Even then, retain any old boot/initramfs artifacts needed
by the configured rollback window.

If a machine fails to boot, Nawa cannot assume it will be running to repair
itself. The distribution must document an external rescue path.

## 11. Transaction integration

The migration engine must reuse the existing installer and transaction
history/journal instead of creating a second package installation path.

A migration record should link to the package transaction or transactions
that prepared it. It should record:

- Migration ID and definition version.
- Source and target profile/provider.
- Requested and selected packages.
- Handler ID and handler version.
- Preconditions and their results.
- Ordered step intents and outcomes.
- Files or boot artifacts changed by each handler.
- Reboot boundary and verification results.
- Recovery instructions and final status.

Package-level operations continue to use the current artifact verification,
file conflict checks, staging, and database update path.

A multi-stage migration is a long-running logical operation, not necessarily
one filesystem-atomic transaction. The CLI and history must show this
distinction.

Shared cache refresh triggers are not suitable for critical migration steps:
they are designed for retryable refresh work after package transactions, not
for boot-critical changes. Migration steps need explicit preconditions,
idempotency rules, durable intent, and postcondition checks.

## 12. CLI and user experience

The commands below are proposals, not current commands.

### 12.1 Inspection and planning

    nawa system status
    nawa system profile show
    nawa plan
    nawa plan --dry-run
    nawa explain systemd
    nawa why runit

The output should state whether information describes the running host, an
offline target root, or a configured target profile.

### 12.2 Migration management

    nawa migrate list
    nawa migrate show faseela-runit-to-systemd-v1
    nawa migrate plan faseela-runit-to-systemd-v1
    nawa migrate apply faseela-runit-to-systemd-v1
    nawa migrate status

The final command names should be reviewed against Nawa's current CLI
conventions before implementation. Avoid adding aliases that duplicate
existing package commands without a clear need.

### 12.3 Upgrade behavior

A normal upgrade should remain a package upgrade unless trusted target
metadata and the active distribution policy declare a system transition.

When a transition is discovered, Nawa should show it as a separate,
high-impact part of the proposed plan. It must not silently remove the current
init package or alter boot configuration.

Example preview:

    System transition detected

    Running provider:
      init-system = runit

    Target provider:
      init-system = systemd

    Package changes:
      + systemd
      + required systemd integration packages
      ~ affected package payloads, as reported by the package solver

    Deferred actions:
      keep runit until the new system has booted and passed verification
      prepare initramfs and boot configuration
      reboot required
      finalize old-provider cleanup after verification

    Safety:
      current init remains active until reboot
      previous boot path is retained where supported
      migration has a durable recovery record

    Apply this system migration? [y/N]

The actual package names and actions must come from the trusted repository
solution and Faseela migration definition, not from hard-coded display text.

### 12.4 Non-interactive operation

High-impact system transitions must not run merely because a caller supplied
a generic auto-confirm flag.

Unattended migration requires a separate explicit policy/authorization that
names the permitted migration and its recovery requirements. Without that
policy, Nawa should stop before mutation and explain how to review the plan.

Provide machine-readable plan output only after a stable schema is approved.
JSON output must not omit warnings, unresolved state, or required user actions.

## 13. Security requirements

- Continue to verify repository signatures and package digests before use.
- Ensure capability and migration metadata is covered by authenticated data.
- Treat local profile overrides as administrator-controlled input and validate
  them strictly.
- Never execute shell text supplied by a repository as root.
- Use allowlisted migration-handler IDs and typed arguments.
- Validate every path relative to the target root; reject path traversal and
  unexpected symlinks.
- Require explicit authority for boot, initramfs, and init-provider changes.
- Keep plugins outside the trusted package-manager core; a plugin must not
  bypass package verification, transactions, or migration authorization.
- Log the reason for every package/provider choice and every privileged step.
- Fail closed when boot layout, recovery path, or provider identity is unknown.
- Do not downgrade repository trust or accept unsigned metadata as a fallback.

## 14. Testing strategy

Testing must cover both the solver and real system transition boundaries.

### 14.1 Unit tests

- Capability parsing, normalization, version matching, and invalid metadata.
- Multiple providers and exclusive target-profile selection.
- Deterministic solving and minimal-change preference.
- Conflict explanations and reverse dependency impact.
- Legacy package records without capability metadata.
- Unknown and contradictory detector observations.
- Init-system detection behavior for live hosts, chroots, containers, and
  offline roots.
- Migration definition validation and handler allowlisting.
- Migration state-machine transition legality.
- Durable state-record recovery and idempotent step handling.

### 14.2 Integration tests

- The `nawa service` command and all service-management APIs are absent.
- Package unit, preset, and run-script files install as ordinary payload files.
- Package installation and migration never invoke `sv` or `systemctl` to
  change service state.
- A runit-to-systemd plan is generated without mutating the machine.
- The plan does not remove runit during preparation.
- Failed package installation leaves the current init bootable.
- Failed initramfs or boot preparation stops before the reboot boundary.
- Reboot pending state is recovered after process termination.
- Successful reboot verifies systemd before old-provider cleanup.
- Failed verification enters rollback-required or needs-recovery.
- Chroot, container, and offline-root detection never reports host PID 1 as
  the target's init.
- Unsigned or malformed capability/migration metadata is rejected.
- Existing repositories, recipes, package records, and history remain readable.

### 14.3 End-to-end migration tests

The first real migration must run in disposable virtual machines with
snapshots or an equivalent externally controlled recovery mechanism.

Test at least:

1. Successful runit-to-systemd transition.
2. Interruption before package preparation.
3. Interruption after packages are installed but before reboot.
4. Successful reboot with verification.
5. Boot failure and recovery through the documented rescue path.
6. Verification failure after boot.
7. Finalization and cleanup only after the rollback policy permits them.

A green unit suite is not evidence that a machine can reboot successfully.
Record the exact VM image, commit, boot configuration, and test result.

## 15. Incremental implementation roadmap

Implementation should follow the Feature-Oriented Hexagonal Architecture
with a Functional Core / Imperative Shell defined in Section 4. Split work
into reviewable changes. Do not combine this proposal with unrelated recipe,
repository, package, or distribution changes.

| Stage | Scope | Exit condition |
|---|---|---|
| 0. Architecture and behavior audit | Map the source/module dependency graph, command-to-installer flows, database and transaction boundaries, root handling, service-management references, and current tests. Record the existing CLI and package behavior as the compatibility baseline. | A source-backed module ownership map, dependency rules, service-removal checklist, and behavior-focused test baseline are reviewed. |
| 1. Structural refactor and service removal | Establish CLI, feature-oriented Application, Domain, Ports, Adapters, Presentation, and Bootstrap boundaries. Move code incrementally; reduce orchestration in `main.rs`; separate package lifecycle use cases; narrow the installer facade; remove the complete service subsystem and `keep_disabled` coupling. | No behavior change in package operations; no service command, provider, service invocation, or runit-specific installer state remains; tests and checks pass. |
| 2. Init identity and capability foundation | Add read-only init-system detection and only the minimal authenticated metadata needed to identify init providers. | Legacy metadata remains readable; live, chroot, container, and offline-root states are distinguished or reported unknown. |
| 3. Profile and planning | Add the validated target init profile and deterministic package planning. | Dry-run plans explain every init-provider and package decision without managing services. |
| 4. Migration framework | Add versioned migration records, state machine, trusted handlers, and recovery. | Failure/restart tests prove safe continuation or an explicit recovery-required status. |
| 5. Faseela transition | Add the distribution-specific runit-to-systemd definition and handlers; service activation remains outside Nawa. | Disposable VM migration passes boot, init verification, and recovery tests. |

The first implementation step is Stage 0, not a rewrite of the dependency
solver and not a mass file move. Stage 0 records the actual dependency graph and
compatibility baseline. Stage 1 then creates clean boundaries with small,
reviewable, behavior-preserving changes. Only after that foundation is stable
should new capability and migration behavior be added.

## 16. Open design decisions for discussion

These questions should be resolved before changing package or repository
schemas:

1. Should capabilities be declared in recipe metadata, archive metadata, the
   signed repository index, or a combination with one canonical source?
2. How should capability requirements express version ranges and provider
   preferences?
3. Where should Faseela's distribution profile live, and how should local
   administrator overrides be merged?
4. What bootloader and initramfs configurations must the first Faseela
   migration support?
5. What recovery mechanism is guaranteed on supported Faseela installations?
6. How long should the previous init provider and boot artifacts be retained?
7. Should migration authorization be a dedicated prompt, a policy file, or
   both?
8. How should offline upgrades and chroot package installation represent a
   migration that cannot be verified until the target machine boots?
9. Which CLI command names best fit the current Nawa command structure?

## 17. Acceptance criteria

The proposal is ready to become an implementation plan when:

- The current source extension points, module ownership, dependency graph, and
  compatibility constraints have been audited.
- The Feature-Oriented Hexagonal Architecture with Functional Core /
  Imperative Shell is documented and its dependency direction is enforced.
- Core planning rules are testable without filesystem, network, or process I/O.
- Application use cases are testable through fake ports, and adapters are
  tested independently.
- Capability and profile schemas have been reviewed and versioned.
- The solver can explain init-system selection and every planned package
  action.
- All service-management code and interfaces are removed from Nawa, including
  runit-specific installer marker handling; service-related package files
  remain ordinary payload files.
- Migration handlers are trusted, allowlisted, and narrowly scoped.
- The transaction/recovery design covers interruption before and after reboot.
- The Faseela migration has a documented boot and rescue strategy.
- Tests prove that Nawa refuses unsafe or ambiguous transitions and never
  attempts to control services.
- Documentation clearly distinguishes implemented behavior from proposals.

Until those criteria are met, this document is a discussion proposal only.
