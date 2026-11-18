# System Intelligence and Migration Architecture

**Status:** Accepted direction; implementation in progress  
**Decision date:** 2026-11-18  
**Canonical implementation tracker:** `docs/implementation-progress.md`  
**Architecture baseline:** Feature-Oriented Hexagonal Architecture with Functional Core / Imperative Shell

## 1. Product definition

System Intelligence is Nawa's system-aware decision and reconciliation layer. It observes a target system, interprets its installed and desired state, resolves package and provider choices against authenticated metadata, explains the resulting plan, and coordinates approved changes through Nawa's existing transaction machinery.

Migration is one high-risk feature built on these shared capabilities. The runit-to-systemd transition is the first migration vertical slice, not the definition of the product.

The platform must distinguish:
- facts observed from the live runtime;
- facts found in an offline target root;
- installed package and filesystem evidence;
- desired state declared by distribution and administrator profiles;
- repository metadata and its authentication provenance;
- inferences, unknowns, contradictions, and confidence.

Absence of evidence is not evidence of absence. A target profile is not proof of the running init. An installed provider is not proof that it is PID 1.

## 2. Architectural constraints

Keep the existing Feature-Oriented Hexagonal Architecture and Functional Core / Imperative Shell.

- **Domain:** pure models and deterministic decisions; no filesystem, process, network, CLI, or database access.
- **Application:** feature use cases and orchestration; depends on domain and declared ports.
- **Adapters:** filesystem, package database, repository, process observation, locks, snapshots, and transaction persistence.
- **Presentation:** CLI parsing and rendering; no policy decisions.

Do not start a second dependency solver, installer, transaction journal, lock manager, or generic shell-command migration framework. Reuse the existing package planner, authenticated repository path, installer, persistent lock, transaction history and write-ahead log.

A structural refactor is not a product milestone by itself. New abstractions must serve an implemented use case and have tests for their behavior.

## 3. Shared system model

### 3.1 Observation

The observation layer produces one immutable `ObservedSystem` snapshot per command. It must describe:

- selected target root and execution context;
- runtime PID 1 evidence, when attributable;
- installed provider candidates and their evidence;
- effective desired profile and source;
- package database observations;
- trust and repository availability;
- unresolved or contradictory facts.

Every fact must retain its source. Runtime observation is allowed only when the managed root is the current environment's root. For an offline root, host `/proc` must never be attributed to the target.

Observation is read-only. Planning must consume the same snapshot rather than independently re-reading selected system facts during one command.

### 3.2 Capabilities and providers

A capability is a normalized domain/identity pair, such as `init-system=systemd`. A provider is a package candidate that declares a capability in authenticated metadata.

Provider decisions must account for:
- metadata schema and provenance;
- whether metadata is authenticated;
- exclusivity and compatibility rules;
- ambiguity or conflicting declarations;
- desired profile policy;
- installed state and runtime evidence.

Unknown metadata remains Unknown. Legacy index entries without capability declarations must not be interpreted as declaring no capabilities.

The first supported capability domain is `init-system`. Do not claim support for service managers, network stacks, desktop environments, bootloaders, or other domains until their semantics, metadata, policies, and tests are specified.

### 3.3 Metadata chain

The provider declaration must remain consistent through the full trusted chain:

`recipe -> archive .PKGINFO -> signed repository index -> fetched metadata verification -> solver candidate`.

The current index v4 path carries runtime dependency names and capability declarations. It does not yet provide complete version constraints, conflicts, or replacements semantics. Those gaps must be modeled and tested before the platform claims a complete general-purpose solver.

## 4. Unified planning

The system plan is a deterministic result over:
- one immutable observation snapshot;
- the effective target profile;
- installed package state;
- authenticated repository candidates;
- explicit policy and user intent.

The plan must explain:
1. what was observed and where each fact came from;
2. what is desired and which profile supplied it;
3. which provider was selected and why;
4. exact package actions and dependency ordering;
5. constraints, conflicts, and metadata trust;
6. unresolved facts and ambiguous choices;
7. boot, recovery, and compatibility risks;
8. what approval is required.

The application boundary returns one immutable `SystemPlan`, assembled from a single
`SystemSnapshot` and one repository inspection. It records repository-source
fingerprint, exact signed-index SHA-256, trusted signer fingerprint, archive
authentication mode, typed transition outcome, deterministic plan identity,
and each selected package's source, version, archive filename, size and
SHA-256. Unknown inventory, invalid configuration, unavailable repositories,
untrusted metadata and unresolved provider selection remain explicit typed
states; they are never represented as an empty candidate set.

A plan identity is a content-derived identifier over the observed facts,
installed inventory, repository evidence and ordered artifact pins. It is an
inspection identity only at this stage: approval-to-execution validation must
still bind and re-check that identity before any mutation.

The planner is read-only. It must not create migration records, change profiles, modify boot artifacts, or install packages.

A normal `install`, `update`, or `upgrade` must not silently switch the running init. An init-provider transition requires a distinct migration plan and explicit approval.

## 5. Profiles and policy

Distribution defaults live in the distribution-owned profile path, currently:

`/usr/share/nawa/system-profile.toml`

Administrator overrides live in:

`/etc/nawa/system-profile.toml`

Profile schema 1 remains readable. Its legacy `[providers]` entries are
interpreted as required-provider shorthand; for example,
`init-system = "systemd"` means that the profile requires that exact provider.
The shorthand is retained for compatibility with existing Faseela images.

Schema 2 adds explicit provider constraints grouped by capability domain:

```toml
schema = 2

[policy.init-system]
required = ["systemd"]
preferred = []
forbidden = ["runit"]
```

The three policy classes have intentionally different semantics:

- **Required:** the exact provider must be selected. For the exclusive
  `init-system` role, at most one identity may be required. Missing,
  ambiguous, untrusted or unsatisfiable required providers leave the plan
  unresolved; Nawa must not substitute a different provider.
- **Preferred:** an ordered, advisory list. The order expresses preference,
  but it does not authorize an automatic package or init transition. If the
  preference is unavailable or not active, the plan explains the mismatch
  and leaves the decision to the administrator.
- **Forbidden:** provider identities that must not be selected. A forbidden
  active provider is reported as a policy violation. A prohibition by itself
  never chooses a replacement; a separate required target is needed before
  a migration can be planned.

Policy validation is strict. An identity cannot appear in more than one
classification for the same role; duplicates are rejected; and the exclusive
`init-system` role cannot require multiple providers. A role cannot combine
required and preferred constraints. Preferred order is preserved. Unknown
capability domains may be parsed and displayed, but remain non-actionable
until their own solver and compatibility semantics are specified.

Merge precedence is per capability domain. If an administrator override
declares `[policy.init-system]`, that complete policy replaces the
distribution's `init-system` policy; policies for other domains remain.
An explicitly empty domain table clears that domain's inherited policy.
A role omitted from the override inherits the distribution value. The
effective profile reports the source file for each policy domain and each
required provider. Invalid or unsupported schemas fail closed; there is no
fallback to guessed defaults.


## 6. Execution and reconciliation

Approved package actions must run through Nawa's existing installer and transaction path. The system-intelligence layer must not become a second package manager.

Required invariants:
- acquire the existing exclusive database lock;
- check the persistent migration mutation gate before mutation;
- pin the approved plan to exact package and artifact identities;
- stage and verify artifacts before the mutation window;
- persist write-ahead intent before every side effect;
- record the observed outcome after verifying its postcondition;
- never infer success from a command exit code alone;
- classify ambiguous or interrupted outcomes as `needs-recovery`;
- keep the mutation gate active while the system cannot be proven consistent.

The migration permit is an in-process capability minted only inside the coordinator's authorized path. Environment variables and generic force flags are not authorization.

## 7. Migration as a high-risk feature

The migration state machine is a domain model. The durable record is stored below the selected target root:

`var/lib/nawa/migrations/<migration-id>.toml`

Gate-keeping states include preparation, awaiting reboot, verification, rollback-required, and needs-recovery states. Terminal completion is valid only after post-boot verification; package installation alone is not completion.

The current coordinator can prepare approved package steps and persist lifecycle outcomes. The actual boot-changing handler remains fail-closed until its external boot and rescue contracts are complete.

### 7.1 Snapshot contract

A snapshot must:
- be bound to one migration identity and target root;
- retain the old boot configuration, init aliases, kernel/initramfs targets, fstab and required legacy-provider artifacts;
- validate a manifest and every payload hash before changing any target;
- refuse path traversal, symlinked ancestors and out-of-allowlist paths;
- preserve the required runit service tree, including `/etc/runit/runsvdir/default`, recursively recording regular files, symlinks and directory modes; never traverse symlinked directories, reject special files and out-of-root symlink targets, and cap the manifest at 999 entries;
- be idempotently restorable;
- be integrated into coordinator and recovery, not merely exist as an isolated adapter.

BIOS raw sectors and EFI NVRAM are not ordinary files. Their backup/restore and rescue procedures must be specified separately. A documented and tested Live-ISO rescue route is mandatory before enabling boot-changing execution.

### 7.2 Post-boot verification and finalization

After reboot, verify the actual runtime PID 1 using attributable evidence, the installed system profile, boot path and required package/provider state. Only after all required postconditions pass may the old provider package be removed and the migration record become terminal.

If verification fails or evidence is ambiguous, preserve the gate and recovery artifacts. Do not remove the old provider or discard the snapshot.

## 8. Security boundaries

- Never follow symlinks in migration state paths or boot snapshot paths.
- Validate every ancestor below the selected target root, not only the final directory.
- Restrict migration records and manifests to root-only permissions.
- Fail closed on corrupt, unreadable, unsupported, or ambiguous state.
- No generic `--force` path may override migration ownership handoff or the mutation gate.
- Ordinary package hooks are not trusted as a system-service policy mechanism. Static recipe checks are not a shell sandbox.
- Never perform a live migration against the developer's working machine during tests. Use disposable VMs and throwaway target roots.
- Never claim BIOS/UEFI acceptance without logs from a current ISO and a successful installed-disk boot in each firmware mode.

## 9. Delivery sequence

The sequence is dependency-driven; a later stage must not be marked complete merely because its type or adapter exists.

| Stage | Deliverable | Exit condition |
|---|---|---|
| A — Security foundation | Safe record-store path walk, restrictive permissions, mutation gate tests | Ancestor symlink, corrupt state, unreadable state and forged-marker tests pass |
| B — Observation contract | One immutable observation snapshot with evidence/context/unknown handling | Live, container, chroot and offline-root tests pass without host-state leakage |
| C — Capability metadata | Trusted metadata chain and provider ambiguity rules | Recipe-to-index-to-solver provenance tests pass |
| D — Unified planning | Deterministic package/provider/system plan and explanations | Same inputs produce same plan; no writes; trust and unknowns are visible |
| E — Profiles and policy | Distribution/admin merge and explicit precedence | Invalid profile fails closed; each effective field reports provenance |
| F — Transaction integration | Existing installer, lock and WAL used by approved system actions | Interruption and reconciliation tests prove no unjournaled mutation |
| G — Migration recovery | Snapshot, coordinator wiring, recovery and post-boot finalizer | Interrupted, failed-boot and restore scenarios pass in disposable VMs |
| H — Distribution acceptance | Faseela/SAQ boot, initramfs, BIOS/UEFI and rescue integration | Current release ISO boots installed disk in BIOS and UEFI; rescue/rollback evidence retained |

## 10. Current implementation boundary (2026-11-18)

Implemented or substantially present:
- Feature-Oriented Hexagonal Architecture and Functional Core / Imperative Shell baseline;
- init-system observation, profile loading, capability metadata and provider explanation;
- schema-1 profile compatibility plus schema-2 required/preferred/forbidden provider policy parsing, validation, domain-level merge precedence and source provenance;
- source-aware package resolution for recipe and authenticated binary candidates on install/update, with a single mixed topological action order and cross-source dependency tests;
- explicit reporting for legacy repository metadata whose runtime dependency closure is unknown, while migration candidate selection remains fail-closed;
- read-only system status/profile/plan/explain CLI;
- authenticated repository transition preview integration;
- migration domain state machine, durable journal, mutation gate, coordinator preparation path and recovery inspection;
- typed legacy init ownership handoff;
- durable boot snapshot adapter with manifest/hash validation and initial restore tests.

Still incomplete:
- a fully generic capability model beyond the init-system vertical slice;
- complete dependency solver semantics (version ranges, conflicts, replacements);
- exact artifact/source pinning across system-plan approval and execution; normal package auto/ask source pinning has landed, but system-plan identity must be carried through approval;
- a single fully connected observation-to-policy-to-solver-to-typed-SystemPlan application flow;
- actionable policy semantics for capability domains beyond the init-system vertical slice;
- migration record store ancestor path hardening and race-resistant filesystem access;
- snapshot integration with coordinator and recovery;
- safe runit service-tree capture and restore;
- BIOS raw-sector and EFI NVRAM recovery contracts;
- post-boot verification/finalization and old-provider removal sequencing;
- interrupted-install, failed-boot, rescue and rollback VM acceptance;
- real current-ISO BIOS and UEFI installed-disk evidence.

## 11. Definition of done

System Intelligence is complete only when a user can inspect the system, understand the evidence and uncertainty, preview a deterministic provider/package plan, see the trust and risk reasons, explicitly approve a permitted change, and have Nawa execute it through its existing transaction system with verified postconditions and recoverable interruption.

Migration is complete only when the old and new boot routes are both recoverable, the actual post-reboot system is verified, and failure can be handled without losing the mutation gate or the old boot path.
