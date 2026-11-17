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
- First-party Nawa code must not call `sv`, `systemctl`, or equivalent
  commands to control services. Repository package hooks remain a separate
  legacy script mechanism and are governed by Section 9.
- Package-install behavior that automatically enables or starts services.

Package contents may still include service-related files. Systemd unit files,
systemd preset files, runit service run scripts, and related configuration are
ordinary package payload files: Nawa may install, upgrade, verify, and remove
them through its normal file-management path. Their activation and runtime
management belong to the selected init/service manager, distribution
provisioning, or the administrator—not to Nawa.

The implementation audit must identify and transfer ownership of the state
currently represented by `crate::service::keep_disabled` and
`/etc/runit/disabled`. The existing runit implementation documents that
packages ship default runsvdir symlinks and that these markers prevent a package
upgrade from silently recreating a link an administrator disabled. Removing
this logic without changing the Faseela package/provisioning model can
re-enable services during an upgrade.

Nawa must no longer own that service state. Before removing the marker handling,
Faseela must move enablement links out of ordinary package payloads and define
how its distribution-owned service configuration preserves administrator
choices. The migration must account for existing disabled markers. Do not
replace the Nawa code with a second hidden service manager or assume that
deleting the function has no behavioral effect.

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
- Using repository-supplied package hooks as migration handlers or as a
  substitute for built-in, reviewed migration logic. Existing package
  `.HOOKS` remain a separate legacy execution mechanism whose trust policy is
  explicitly reviewed in Section 9.
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
| Application ports | Define the external capabilities a use case needs through narrow interfaces, including plan confirmation and authorization. | Contain adapter implementations or become a generic interface for every helper function. |
| Adapters | Implement ports using concrete storage, network, archive, filesystem, signing, process, and sandbox facilities. | Decide user intent or package/provider policy. |
| Presentation | Render plans, diagnostics, confirmations, and human/machine-readable output. | Own package-management decisions. |
| Bootstrap | Construct concrete adapters, connect them to use cases, and start the selected command. | Become another place for business logic. |

The dependency direction is inward:

    CLI ----------------------> Application Features
      |                               |
      |                               v
      |                             Domain
      |                               ^
      |                               |
      +----> Presentation              |
                 |                    |
                 +---- implements ----+---- Application Ports
                                      ^
                                      |
                                  Adapters

    Bootstrap wires CLI + Application + Ports + Adapters + Presentation together.

Application defines the ports it needs. Infrastructure adapters implement
those ports and may depend on the port contracts and domain types. The
Presentation adapter implements the user-interaction port (plan display,
confirmation, and authorization prompts). Domain remains independent of
Application, CLI, Presentation, and Adapters. CLI must not call adapter
implementations directly. No layer may create a circular dependency.

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
          user_interaction.rs
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
        recipe_source/
        configuration/
        host_inspection/

      presentation/
        output.rs
        diagnostics.rs
        confirmation.rs
        formatting.rs

The tree may retain cohesive existing subsystems during early stages. For
example, the build pipeline, sandbox implementation, archive format, and
repository-index implementation do not need to be flattened or rewritten merely
to match this diagram. The Presentation adapter may implement the interaction
port without making Application depend on terminal libraries or output
formatting modules. Move or split a module only when its current ownership
crosses a boundary or makes a use case difficult to test.

The exact names and placement of modules should be settled by the Stage 0
source audit. The target tree is a guide, not a license for a mass file move.
The first implementation should also extract the presentation interaction
port: Application requests plan display/confirmation/authorization through
that port; it must not depend directly on terminal output or prompting code.

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
5. **Presentation adapter:** renders the complete plan and implements the
   application's user-interaction/authorization port. The application receives
   an explicit approval result; it does not write directly to a terminal.
   Dry-run ends here without mutation.
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
| `db.rs` | Database adapter and persistence mapping. Convert persisted `db::Entry` values into domain `InstalledPackage` values at the port boundary; never make the on-disk record the domain model. |
| `repo/`, `repositories.rs`, `fetch/` | Repository and fetch adapters. Convert signed `RepoEntry` transport/index records into normalized domain package candidates while preserving authentication provenance; do not expose transport structs to the solver. |
| `recipe/model.rs`, `validate.rs`, `expand.rs` | Split the pure recipe definition/validation rules into Domain from file loading and build expansion. Keep source path and raw recipe bytes in a loader-owned `RecipeDocument`/source envelope, not in the Domain `Recipe`; move `ExpandCtx` and filesystem-bound expansion into the build feature. |
| `build/`, `buildenv.rs`, `fetchrun/`, `sandbox/` | Build use case and cohesive build subsystem, with process, fetch, and sandbox effects behind adapter boundaries where needed. |
| `transaction/`, `recovery.rs`, `lock.rs`, `undo.rs` | Separate transaction domain/state, application lifecycle coordination, and durable journal/lock adapters. |
| `config.rs`, `profile.rs`, `sys.rs` | Split configuration loading and host observations from validated domain configuration/profile models. |
| `groups/` and `commands/groups.rs` | Group membership domain plus group application use cases; keep catalog loading in its repository adapter. |
| `plugins/` and `commands/plugins.rs` | Plugin management use cases plus isolated discovery/runner adapters; plugins remain outside the trusted package/migration core. |
| `artifact.rs`, `sign.rs`, `checksum.rs` | Artifact digest/provenance values may be Domain types; content-addressed storage, signature verification, and file hashing implementations belong in adapters. |
| `sources.rs`, `assets.rs`, `fetchrun/`, `slim.rs`, `closure.rs` | Keep with the build feature/subsystems they serve; isolate network and filesystem effects behind adapter boundaries where needed. |
| `output.rs`, `diag.rs`, `color.rs`, `confirm.rs`, `humanize.rs` | Presentation and user-interaction responsibilities. |
| `service.rs`, `commands/service.rs` | Delete as part of the accepted removal of service management from Nawa. |

Other small root modules also need explicit owners during Stage 0:

| Current modules | Target ownership |
|---|---|
| `archive.rs`, `fswalk.rs`, `glob.rs`, `pathutil.rs`, `atomic.rs`, `unique.rs` | Archive/filesystem/path adapters or narrowly scoped infrastructure primitives; keep pure path/name rules in Domain where they express package invariants. |
| `version.rs`, `suggest.rs`, `humanize.rs` | Version comparison and suggestion rules in Domain where pure; human-readable formatting in Presentation. |
| `lint/` | Build/recipe validation feature, separated into pure recipe checks and filesystem/ELF inspection adapters. |
| `revdep.rs`, `undo.rs`, `recovery.rs` | Reverse-dependency analysis and undo decisions in Domain; filesystem restoration and crash reconciliation in adapters, coordinated by Application. |
| `sync.rs`, `repositories.rs` | Repository/sync application use cases plus repository adapters; do not mix remote transport with policy. |
| `config.rs`, `profile.rs`, `sys.rs` | Configuration/profile loading and host observation adapters; validated profile values and init identity remain Domain types. |
| `interrupt.rs`, `lock.rs` | Process cancellation and OS lock adapters, coordinated by Application lifecycle. |
| `error.rs`, `testutil.rs` | Shared error taxonomy only where stable across layers; test fixtures remain test support, not production Domain modules. |

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
  Domain planners receive domain package/recipe types, never `db::Entry`,
  `repo::RepoEntry`, file paths, or repository transport records.
- Keep installer internals behind a narrow application-facing API. Command
  handlers must not reach into payload, journal, rollback, or database internals.
- Define ports around meaningful external boundaries, including user
  confirmation/authorization. Avoid unnecessary traits, generic repositories,
  service locators, global mutable state, and a large context object passed
  everywhere.
- Domain modules may depend on other domain modules, but not on CLI,
  Application, or Adapters. Move shared pure identity/version/name validation
  rules out of `db.rs` before recipe/archive adapters depend on them; the
  database adapter must not become the owner of domain invariants.
- Application may depend on Domain and its own port contracts.
- Adapters may depend on port contracts and domain data types, but never on
  CLI or presentation code.
- No adapter may silently bypass signature verification, package conflict
  checks, transaction recording, or migration authorization.
- No first-party Nawa service-management APIs, providers, service-state
  database, or service-control adapter is permitted. First-party package and
  migration workflows must not invoke service-manager commands. Existing
  package hooks are a separate privileged script mechanism and are governed by
  the explicit policy in Section 9. Service definitions and presets remain
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
- **Application tests:** use small fake/in-memory port implementations,
  including a user-interaction port, to verify use-case orchestration,
  confirmation gates, failure handling, and transaction boundaries.
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

### 5.5 Detection contract by execution context

The detector must return separate fields for execution context, observed
runtime init, installed/configured init candidate, and desired init profile.
It must never collapse these into one `init-system` value.

| Context | Allowed evidence | Result and policy |
|---|---|---|
| Verified live host | Read-only PID 1 evidence such as `/proc/1/exe` and `/proc/1/comm`, plus checks that the process is not operating inside an unsupported container/chroot context. | May report the observed running init with its evidence and confidence. |
| Target root passed through `--root` | Target-root package records, target-root init symlinks, and target-root boot configuration. | Reports installed/configured candidates only. Host `/proc` must never be presented as the target's runtime state. |
| Chroot, container, or ambiguous environment | Only evidence that can be attributed unambiguously to that environment. | Runtime init is `unknown` unless the detector can prove the identity and context. |
| Offline root | Static target-root evidence only. | Planning and inspection are allowed; live migration apply is forbidden. |

A migration plan may be generated for an offline root, but `migrate apply`
must require the actual Faseela live host, a canonical target root of `/`,
effective root privileges, and a positively identified runtime init. It must
refuse chroots, containers, remote roots, and unknown contexts. A binary or
symlink named `init` inside a target root is evidence of installed intent,
not proof of the running PID 1.

The initial detector should use read-only kernel/process/filesystem evidence.
It must not call `systemctl`, `sv`, or another service-management command.

## 6. Capabilities and provider selection

Package names are implementation details. System requirements should be
expressed as capabilities.

For the initial migration design, the only system capability in scope is
`init-system`. Do not add `service-manager`, `session-manager`, or other
provider domains to Nawa's capability resolver as part of this work. Additional
capability domains require a separate, justified design decision.

For this initial migration, provider selection is limited to the exact
`init-system=runit` and `init-system=systemd` identities. Do not implement
general version-range negotiation, capability expressions, or arbitrary
provider plugins in the first pass. A system profile selects the desired
provider for this one exclusive role.

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

The eventual schema must define normalized capability identifiers and
architecture constraints. General capability version-range negotiation is
deferred; the first implementation supports only the exact init identities
`init-system=runit` and `init-system=systemd`.

### 6.2 Capability semantics

Before implementation, define these rules precisely:

- A capability requirement is not necessarily a package dependency.
- More than one package may provide a capability, but an exclusive system
  role has one selected provider in the desired profile.
- Two init providers may be installed side by side during a staged transition
  only when their package payloads do not conflict and installing the new
  provider does not replace the active boot/init selector. The migration
  handler must prove this from the actual package manifests and target layout;
  the solver must not assume coexistence is possible.
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

Recommended metadata contract:

1. Recipe metadata declares the package's capabilities and runtime dependency
   information as source input.
2. The built archive records the normalized package metadata in `.PKGINFO`;
   this is the package-level source of truth. Its digest must be authenticated
   by a verified detached signature or a signed repository index. SHA-256
   alone proves byte integrity, not publisher identity.
3. A future signed repository-index schema (v4 or later) may duplicate the
   normalized `provides` and runtime-dependency fields so Nawa can solve
   without downloading every archive. The index signature authenticates these
   fields and the referenced archive digest. Nawa must compare the index fields
   with the verified archive metadata when the archive is fetched. A local
   unsigned archive or an unsigned sidecar must not provide trusted init
   capability metadata for migration.
4. Legacy v1/v2/v3 repositories and archives without these fields remain
   readable, but missing critical init-provider metadata means migration is
   unsupported. Never infer `init-system` from package names.
5. In v4, metadata fields required for solving must be present for every
   package, with an explicit empty list when the package has no dependencies
   or capabilities. A missing field in a legacy record means `unknown`, not
   `[]`. The normalized candidate model must preserve this distinction.

The precise TOML spelling and schema version remain to be specified before
implementation. Recipe, archive, and signed-index values must not become
independent conflicting authorities.

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
desktop package must not silently rewrite the system's init provider. Likewise,
`nawa install systemd` by itself installs a package and its declared
dependencies; it must not silently switch the boot/init provider, prepare a
new initramfs, or trigger a migration. Only an explicit target-profile change
and separately authorized migration may do that.

The system profile is not the existing build profile selected through
`--profiles`. Those are different concepts and must use separate models and
configuration paths. A recommended layout is a distribution-owned default
under `/usr/share/nawa/` and a local administrator override under
`/etc/nawa/`; the exact filenames and merge contract must be agreed with
Faseela. Administrator overrides take precedence over distribution defaults,
invalid overrides fail closed for migration, and package updates must never
silently overwrite local policy.

## 8. Dependency and capability solver

The planner should extend the current dependency-resolution facilities rather
than become a second independent package resolver. In the first implementation
it should handle the dependency forms Nawa actually supports plus the exact
init-provider constraint. It is not a general SAT solver. Before planning,
Application assembles normalized candidates from trusted recipe and repository
inputs; the Domain planner receives those values and performs no I/O.

### 8.1 Inputs

A solve request should include:

- Requested package operations and user intent.
- Installed package records and install reasons.
- Trusted repository candidates and their metadata.
- Package build/runtime dependencies, init-provider metadata, conflicts, and
  replacements. For binary-only packages, these inputs must come from
  authenticated metadata; absent fields are unknown, not empty.
- Effective target system profile.
- Architecture and version constraints.
- Administrator pins, exclusions, and provider preferences.
- Protected packages and unsupported/unknown system state.
- Whether the operation is a normal package transaction or a staged
  migration.

### 8.2 Output

The solver should produce a structured solution containing:

- Packages to install, upgrade, keep, or remove.
- The exact selected init provider for the target profile, or an explicit
  unresolved result when authenticated metadata is unavailable.
- Dependency and capability explanation chains.
- Conflicts and the reason each candidate was rejected.
- Package files and dependents affected by each operation, without interpreting
  service definitions or changing service state.
- A migration candidate and its prerequisites when the requested operation
  changes the target init provider; selecting a candidate does not execute it.
- Reboot requirement and post-reboot init-system checks.
- Any unresolved fact or unsafe condition that prevents execution.

The plan must be deterministic for identical normalized inputs and policy.
The same rules must apply to recipe-built and binary-only package candidates.
Do not choose an arbitrary provider when the signed candidate data is incomplete.

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
- Never infer that a service is enabled, started, healthy, or usable merely
  because a related file exists.

Nawa must not expose a service API, service CLI, provider interface, service
state database, or service-specific capability. Nawa's built-in install,
upgrade, removal, and migration workflows must not themselves invoke service
manager commands or automatically enable/start services.

There is an important existing compatibility boundary: current package
archives may contain a `.HOOKS` entry with `post_install`, `pre_remove`, and
`post_remove` shell scripts. The current installer executes these scripts
through a shell on the live root or inside a chroot. Distribution
post-transaction trigger scripts are also executed by the current application.
Consequently, the blanket claim that "Nawa can never invoke service-manager
commands during a package operation" would be false while these script
mechanisms remain available: a hook can itself call `systemctl`, `sv`, or
another command.

The initial scope therefore makes this distinction explicit:

- Nawa provides no service-management API and has no built-in service
  activation behavior.
- Faseela package and transaction-trigger scripts must not use hooks to enable,
  start, stop, or otherwise manage services. The distribution needs a
  documented recipe/repository review policy and CI checks for this rule.
- Static hook linting is a policy guard, not a security sandbox; it cannot
  prove that arbitrary shell code never performs service management.
- If the project requires a hard technical guarantee that package hooks cannot
  manage services, restricting or removing privileged package hooks and
  transaction triggers is a separate compatibility/security project. It must
  be designed and tested explicitly rather than claimed by this migration
  proposal.

Service unit files, presets, and runit run scripts remain ordinary payload
files. Installing one of those files is not the same as enabling or starting
the service.

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
  imports and tests only after Faseela has taken over the disabled-state
  compatibility behavior and stopped packaging default enablement symlinks.
- Do not replace this with systemd enablement tracking or another hidden
  service-management path. If generic configuration-file preservation is
  needed, design it independently from service state.

The removal is complete only when the crate builds and tests pass without a
service command, service-management module, first-party service-control
adapter, or runit-specific installer marker behavior. The marker behavior may
be removed only after Faseela's package payload/provisioning policy preserves
the prior disabled-service choice across package upgrades.

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

For the first implementation, privileged migration handlers must be built
into Nawa and selected only by an allowlisted, typed handler ID. Do not load
migration executables, scripts, plugins, or dynamic libraries from a
repository. A future authenticated extension mechanism would require a
separate security design and must not be assumed by this proposal.

A handler should expose small, typed operations with explicit inputs and
postconditions. It must not receive unrestricted authority to execute
repository-supplied scripts.

Migration metadata may describe which handler is needed, but metadata alone
must never grant new privilege.

### 10.3 Durable state machine

The migration should persist a versioned state record under Nawa's state
directory, for example:

    <target-root>/var/lib/nawa/migrations/<migration-id>.toml

Suggested durable states:

| State | Meaning |
|---|---|
| planned | Live-host plan passed preflight and explicit approval; its persistent record is created immediately before the mutation gate. Offline plans are transient and do not create this state. |
| preparing | The migration owns a persistent mutation gate while package and boot preparation runs. |
| awaiting-reboot | Preparation is committed; the new init has not yet been verified after reboot. |
| verifying | The live host has rebooted and init/boot postconditions are being checked. |
| completed | Target state is verified and finalization is complete. |
| failed | A step failed, compensation restored the known old bootable/package state, and no migration gate remains. |
| rollback-required | Verification failed, but a documented rollback action is available. |
| needs-recovery | State is ambiguous or automatic safe continuation/rollback is impossible. |

The legal forward path is
`planned -> preparing -> awaiting-reboot -> verifying -> completed`.
A failure in `preparing` may become `failed` only after all committed
package/boot changes have been safely compensated; otherwise it becomes
`needs-recovery`. A failed post-reboot verification becomes
`rollback-required` only when a documented rollback action is available.
After rollback, Nawa must verify the old init/boot state before recording
`failed` and clearing the gate. Any ambiguous outcome becomes
`needs-recovery` and keeps the gate active.

An interrupted process is not a stable state by itself. On restart, an intent
without a corresponding result is reconciled into one of the durable states
after inspecting actual system state.

Every external migration step must have a write-ahead intent persisted before
the side effect and a result persisted after it. Writes must use atomic
publication, file and directory fsync, and restrictive ownership/permissions.
The migration record is rooted at the selected target root; it must never
accidentally write to the host when Nawa is planning an offline root.
Planning against an offline root is read-only: it produces an in-memory or
user-exported plan and does not create a persistent `planned` migration record
inside the target. The durable `planned` record is created only after the
live-host apply preflight and explicit authorization succeed.

A **persistent migration mutation gate** is required because the ordinary
`flock` lock disappears when the process exits or the machine reboots.

The gate check must run before any process-local lock-bypass shortcut. In the
current source, `lock::acquire` returns early when `NAWA_LOCK_HELD` is set so
a nested package hook can avoid deadlocking on its parent's lock. This
environment variable is forgeable and is not proof that a parent process
actually holds the lock. It must never authorize a package mutation or skip
the persistent migration-gate check. A nested package-mutating CLI command
must fail closed when the marker is present rather than proceed without a
lock. A nested read-only command may bypass reacquiring the lock only where
Stage 0 proves its read snapshot is consistent; otherwise it must fail fast
with a clear diagnostic. The refactor must replace the generic
environment-based mutation bypass with explicit in-process lock ownership
where application use cases share a lock guard.

While a migration is in `preparing`, `awaiting-reboot`, `verifying`,
`rollback-required`, or `needs-recovery`, all Nawa-owned package-mutating
commands must stop before changing the package set. Read-only inspection and
explicit migration recovery commands remain allowed. The gate is checked
against the selected root before honoring `NAWA_LOCK_HELD`, and under the
exclusive package database lock (or the already-held parent lock for a nested
read-only check).

The migration coordinator may perform only the package transactions in its
approved plan through a private, typed, unforgeable in-process permit created
by the migration use case. The permit must not be represented by an environment
variable, CLI flag, plugin capability, or inherited subprocess environment.
It is distinct from lock ownership: the coordinator must hold the appropriate
exclusive lock and pass both the lock guard and permit through the internal
application call chain without reacquiring the same non-reentrant lock. A
nested `nawa` process launched by a package hook receives no permit and must
not be able to mutate packages while the gate is pending. The migration
coordinator must invoke package use cases inside the same Nawa process rather
than spawn a nested CLI process.

This gate serializes Nawa-owned package mutations only. It cannot prevent an
administrator or another privileged program from modifying the target root
outside Nawa; the migration preflight must warn about concurrent external
package managers or manual changes. The gate is cleared only after verified
completion or a completed, verified rollback.

### 10.4 Migration stages

1. **Detect:** compare observed PID 1, installed init candidates, and the
   effective target profile as separate facts.
2. **Preflight:** require the verified live Faseela host, root privileges,
   supported architecture/release, authenticated packages and metadata, disk
   space, known boot layout, compatible initramfs/boot tooling, and an external
   recovery route.
3. **Plan:** calculate package changes and boot/init selection changes; show
   all actions, conflicts, retained packages, and rollback limits.
4. **Stage artifacts:** download/build and verify all required package
   artifacts before entering the mutation phase. Do not hold the package lock
   across downloads or builds.
5. **Prepare:** acquire the exclusive package lock, revalidate the approved
   plan against current installed state, persist the migration gate and
   write-ahead record, then install the required packages through the existing
   verified installer. Prepare boot/initramfs integration only through the
   built-in trusted handler. Do not activate services.
6. **Commit pending state:** persist `awaiting-reboot` only after every
   preparation postcondition is checked and the previous boot/recovery path is
   retained. Nawa exits with a clear pending-migration status; it does not
   reboot automatically.
7. **Resume and verify:** after reboot, an explicit
   `nawa system migrate resume` (or equivalent documented command) checks the
   live PID 1 identity and boot/init postconditions only. It must not inspect
   or manage individual services.
8. **Finalize:** only after successful verification and rollback-policy checks,
   retire old init packages/artifacts as permitted, record completion, and
   clear the persistent mutation gate.

Faseela's initramfs generator and boot configuration are part of this
integration boundary. The migration must verify that the generated initramfs
selects the requested systemd init explicitly. It must not rely on a fallback
ordering that prefers an installed `/usr/bin/runit` merely because that file
exists. The Faseela `saq` init-selection contract and BIOS/UEFI boot paths
must be audited and tested before the migration handler is considered safe.

A migration may be refused if its handler cannot prove a safe preparation and
verification path for the target machine.

### 10.5 Rollback policy

The migration definition must state what can be restored, how, and at which
stage. At minimum, it should preserve the old boot entry or another documented
recovery route when the platform supports it. It must also define which exact
kernel, initramfs, modules, and boot configuration artifacts are retained;
keeping only a symlink to an old image is not a rollback strategy.

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
that prepared it, but it must not reuse the package WAL as if it covered boot
side effects. Package transactions continue to journal package/filesystem and
database steps; the migration journal separately records boot/initramfs
handler intents and outcomes. It should record:

- Migration ID and definition version.
- Source and target profile/provider.
- Requested and selected packages.
- Handler ID and handler version.
- Preconditions and their results.
- Ordered write-ahead step intents and outcomes, including the durable point
  before and after every boot/initramfs side effect.
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

The persistent migration gate must be checked by every Nawa-owned
package-mutating entry point after process restart and after reboot, before
honoring `NAWA_LOCK_HELD` or another process-local lock-bypass marker. The
check is performed against the selected root while the exclusive package
database lock is held, or under the already-held parent lock for a nested
read-only gate check. The only bypass is the private, typed in-process permit
held by the active migration coordinator; it cannot be forged with an
environment variable, CLI argument, or plugin. The migration coordinator
executes its approved package operations through application use cases in the
same process, not a nested CLI invocation.

This gate serializes Nawa-managed package mutations only. It cannot prevent an
administrator or an unrelated privileged program from modifying the target
root outside Nawa; that limitation must be documented and the migration
preflight should warn about concurrent external package managers or manual
changes.

## 12. CLI and user experience

The commands below are proposals, not current commands.

### 12.1 Inspection and planning

    nawa system status
    nawa system profile show
    nawa system plan
    nawa system explain init-system
    nawa why runit

The output should state whether information describes the running host, an
offline target root, or a configured target profile.

### 12.2 Migration management

    nawa system migrate list
    nawa system migrate show faseela-runit-to-systemd-v1
    nawa system migrate plan faseela-runit-to-systemd-v1
    nawa system migrate apply faseela-runit-to-systemd-v1
    nawa system migrate resume
    nawa system migrate status

Use the single `nawa system migrate` namespace for migration lifecycle
commands. Planning may target an offline root; `apply` and `resume` are live
host operations only and must reject `--root` targets other than the verified
live Faseela root. The final flag spelling should be reviewed during
implementation, but do not introduce a second top-level `migrate` namespace.

### 12.3 Upgrade behavior

A normal upgrade should remain a package upgrade unless trusted target
metadata and the active distribution policy declare a system transition.

When a transition is discovered, Nawa may show it as a separate,
high-impact migration candidate in the upgrade preview. A normal package
upgrade must not silently switch the target init provider, remove the current
init package, or alter boot configuration. Applying the migration requires its
own explicit migration authorization and durable migration record.

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

The actual package names and actions must come from authenticated repository
metadata and the reviewed Faseela migration definition, not from hard-coded
display text. The preview must distinguish package-upgrade actions from
boot/init migration actions.

### 12.4 Non-interactive operation

High-impact system transitions must not run merely because a caller supplied
a generic auto-confirm flag.

Unattended migration requires a separate explicit policy/authorization that
names the exact migration ID, source/target init, allowed package set, handler
version, and recovery requirements. A generic `--yes` flag is insufficient.
Without that policy, Nawa must stop before mutation and explain how to review
the plan.

Provide machine-readable plan output only after a stable schema is approved.
JSON output must not omit warnings, unresolved state, or required user actions.

## 13. Security requirements

- Continue to verify repository signatures and package digests before use.
- Ensure capability and migration metadata is covered by authenticated data.
- Treat local profile overrides as administrator-controlled input and validate
  them strictly.
- Do not execute repository-supplied shell text as a migration handler.
  Existing archive `.HOOKS` are a separate legacy mechanism: they are stored
  in package archives and the current installer runs them through a shell as
  root on the live root or inside a chroot. Their trust policy must account for
  the current unsigned-package policy as well as signed repositories.
- Use allowlisted, built-in migration-handler IDs and typed arguments. A
  migration handler must never be implemented as an archive hook or a
  post-transaction trigger.
- Validate every path relative to the target root; reject path traversal and
  unexpected symlinks.
- Require root privileges and explicit authority for boot, initramfs, and
  init-provider changes; refuse apply in chroots, containers, and offline roots.
- Persist and enforce the migration mutation gate under the package database
  lock for every package-mutating command.
- Keep migration state and journal paths rooted under the selected target root,
  with restrictive ownership, permissions, no-follow path handling, and
  atomic/fsynced publication.
- Keep third-party plugins outside the trusted package-manager and migration
  core. A plugin must not register a privileged migration handler or bypass
  package verification, transactions, or migration authorization.
- Treat existing archive `.HOOKS` and local post-transaction trigger scripts
  as privileged code-execution mechanisms; document their trust boundary and
  do not reuse them as migration handlers.
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
- Durable state-record recovery, legal transition checks, and idempotent step
  handling at every write-ahead boundary.
- No domain module imports CLI, Application, or adapter modules; architecture
  checks fail on forbidden dependency directions.
- The pure planner produces identical results for identical normalized
  source-built and binary-only inputs.
- Persistent gate checks run before the `NAWA_LOCK_HELD` shortcut. The
  environment marker cannot authorize mutation; nested package-mutating
  commands fail closed even when it is set.
- No environment variable, CLI flag, inherited hook environment, or plugin
  can construct or impersonate the private in-process migration permit. A
  forged `NAWA_LOCK_HELD` marker cannot bypass package mutation locking.

### 14.2 Integration tests

- The `nawa service` command and all service-management APIs are absent.
- Package unit, preset, and run-script files install as ordinary payload files.
- Faseela's out-of-Nawa provisioning preserves existing user-disabled runit
  services across package upgrades and imports existing disabled markers
  before the Nawa marker logic is removed.
- Built-in package installation and migration paths do not implement or call
  service-management operations. Faseela recipe and transaction-trigger
  policies reject service activation in package hooks; tests also document
  that shell hooks are privileged code and cannot be treated as a sandbox.
- Mutating package commands are blocked while a migration mutation gate is
  pending; read-only commands remain available where snapshot consistency is
  proven. `NAWA_LOCK_HELD` is not proof of lock ownership and cannot authorize
  mutation or bypass the gate. Nested package-mutating commands fail closed
  even when that marker is set. Only the private in-process permit, together
  with the coordinator's lock ownership, may continue approved package steps.
- `nawa system migrate apply` refuses offline roots, chroots, containers,
  and unknown runtime init states; offline-root planning remains read-only
  and does not persist migration state under the target root.
- A runit-to-systemd plan is generated without mutating the machine.
- The plan does not remove runit during preparation.
- Failed package installation leaves the current init bootable.
- Failed initramfs or boot preparation stops before the reboot boundary.
- Reboot pending state and every interrupted handler step are recovered after
  process termination or power loss.
- A stale plan is rejected and must be regenerated/reapproved after relevant
  installed state changes.
- Successful reboot verifies systemd as PID 1 and the expected boot/initramfs
  postconditions before old-provider cleanup; no service state is queried.
- Failed verification enters rollback-required or needs-recovery.
- Chroot, container, and offline-root detection never reports host PID 1 as
  the target's init, and migration apply is rejected for those contexts.
- Unsigned or malformed capability/migration metadata is rejected.
- Existing repositories, recipes, package records, and history remain readable.

### 14.3 End-to-end migration tests

The first real migration must run in disposable virtual machines with
snapshots or an equivalent externally controlled recovery mechanism. Test both
BIOS and UEFI paths supported by Faseela, and use the real `saq` init-selection
implementation so the test proves which init the generated initramfs will
actually launch. Separately verify that Faseela's service-policy/provisioning
migration preserves administrator enable/disable choices without adding
service-management behavior to Nawa.

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
| 0. Architecture and behavior audit | Map the source/module dependency graph, command-to-installer flows, database and transaction boundaries, root handling, service-management references, package `.HOOKS` and transaction triggers, `NAWA_LOCK_HELD` behavior and nested-hook call paths, runit default enablement links/disabled-marker semantics, binary-only package limitations, `saq` init-selection, and current tests. Record the existing CLI and package behavior as the compatibility baseline. | A source-backed module ownership map, dependency rules, service-removal checklist, package-hook policy gap, service-state compatibility plan, lock-ownership contract, and behavior-focused test baseline are reviewed. |
| 1. Structural refactor and service removal | Establish CLI, feature-oriented Application, Domain, Ports, Adapters, Presentation, and Bootstrap boundaries. Move code incrementally; reduce orchestration in `main.rs`; separate package lifecycle use cases; narrow the installer facade; remove the service command/backend; transfer runit enablement-link and disabled-state ownership to Faseela before deleting `keep_disabled`. | Package operations preserve their documented behavior; the service command and first-party service-control code are gone; disabled services stay disabled across upgrades under Faseela-owned policy; package-hook trust/service policy is explicit; tests and checks pass. |
| 2. Init identity and capability foundation | Add read-only init-system detection and the authenticated metadata needed to identify init providers and resolve binary-only runtime dependencies. | Legacy metadata remains readable; live, chroot, container, and offline-root states are distinguished or reported unknown; no critical solve treats missing metadata as an empty dependency/provider list. |
| 3. Profile and planning | Add the validated target init profile and deterministic package planning. | Dry-run plans explain every init-provider and package decision without managing services. |
| 4. Migration framework | Add versioned migration records, a write-ahead handler journal, persistent mutation gate, built-in trusted handlers, and recovery. | Failure/restart tests prove safe continuation or an explicit recovery-required status; unrelated package mutation is blocked across process exit and reboot while migration is pending. |
| 5. Faseela transition | Add the distribution-specific runit-to-systemd definition and handlers; service activation and service-state conversion remain outside Nawa. | Disposable VM migration passes boot, init verification, recovery, and Faseela-owned preservation of administrator service choices on supported BIOS/UEFI paths. |

The first implementation step is Stage 0, not a rewrite of the dependency
solver and not a mass file move. Stage 0 records the actual dependency graph,
privileged-hook behavior, service-state compatibility, and boot/initramfs
contract. Stage 1 creates clean boundaries and removes the service API only
after Faseela's replacement ownership is in place. Only after that foundation
is stable should capability and migration behavior be added.

## 16. Open design decisions for discussion

These questions should be resolved before changing package or repository
schemas:

1. Confirm the recommended metadata chain: recipe declaration, canonical
   archive `.PKGINFO`, and authenticated duplication in a future signed index
   schema. Define the exact recipe and archive field syntax.
2. What exact package metadata is required for binary-only dependency solving,
   and how will the signed index be upgraded without breaking v1/v2/v3 readers?
3. Confirm the distribution profile location and precedence between Faseela
   defaults and administrator overrides, keeping it separate from the existing
   build profiles selected by `--profiles`.
4. What bootloader and initramfs configurations must the first Faseela
   migration support?
5. What recovery mechanism is guaranteed on supported Faseela installations?
6. How long should the previous init provider and boot artifacts be retained?
7. Should migration authorization be a dedicated prompt, a policy file, or
   both?
8. How should offline upgrades and chroot package installation represent a
   migration that cannot be verified until the target machine boots?
9. What is the enforceable Faseela policy for privileged package `.HOOKS` and
   post-transaction triggers, given that current Nawa executes these scripts?
10. Which exact `saq` interface selects systemd in the generated initramfs,
    and how is that behavior tested for each supported boot mode?
11. Which Faseela-owned component translates legacy runit enabled/disabled
    choices into the systemd preset/provisioning policy, without Nawa querying
    or managing individual services?

## 17. Acceptance criteria

The proposal is ready to become an implementation plan when:

- The current source extension points, module ownership, dependency graph,
  binary-only package limitations, hook execution paths, and compatibility
  constraints have been audited.
- The Feature-Oriented Hexagonal Architecture with Functional Core /
  Imperative Shell is documented and its dependency direction is enforced.
- Core planning rules are testable without filesystem, network, or process I/O.
- Application use cases are testable through fake ports, and adapters are
  tested independently.
- Capability and profile schemas have been reviewed and versioned.
- The solver can explain init-system selection and every planned package
  action.
- All first-party service-management code and interfaces are removed from
  Nawa. Runit-specific installer marker handling is removed only after Faseela
  has moved default enablement links and administrator disabled-state
  preservation into distribution-owned configuration; service-related files
  remain ordinary payload files.
- Migration handlers are trusted, allowlisted, and narrowly scoped.
- The migration journal records write-ahead intents/outcomes for boot side
  effects, and the persistent mutation gate blocks unrelated package changes
  until verified completion or rollback.
- Migration apply is limited to a verified live Faseela host; offline roots
  are planning-only.
- The Faseela migration has a documented boot and rescue strategy, including
  a tested `saq` init-selection contract for supported BIOS/UEFI modes.
- The privileged package-hook and post-transaction-trigger trust policy is
  explicit and compatible with the no-service-management boundary.
- Tests prove that built-in Nawa features do not manage or control services,
  and that the Faseela hook policy is explicit about the limits of static
  checks on privileged shell scripts.
- Documentation clearly distinguishes implemented behavior from proposals.

Until those criteria are met, this document is a discussion proposal only.
