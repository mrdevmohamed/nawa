# Nawa package-management roadmap: detailed architecture and implementation design

> **STATUS: PROPOSED — DESIGN ONLY — NOT IMPLEMENTED**
>
> This document describes a future direction for Nawa. It is not a statement
> that any of the features below already exist, and it does not change the
> current package format, repository format, database format, CLI behavior,
> signing policy, cache policy, or installation semantics.
>
> The current stable implementation remains authoritative. Each implementation
> step must be reviewed, tested, and merged independently. Do not combine this
> proposal with unrelated package, recipe, bootstrap, or CI changes.
>
> **Design baseline:** Nawa 1.0.0, the repository state inspected for this
> proposal. Current behavior is described as it exists in the source tree;
> proposed APIs and file formats are illustrative until approved.

## 1. Executive summary

Nawa already has substantial package-management foundations:

- Source builds from declarative TOML recipes.
- Dependency planning across build and runtime dependencies.
- Optional repository-backed installation and upgrades.
- Repository index schemas v1, v2, and v3.
- Ed25519 signatures, trusted keys, and SHA-256 verification.
- Staged package extraction, file-conflict detection, configuration-file
  protection, and reversible file moves during an individual package change.
- A package-local rollback mechanism backed by cached archives.
- Shared/exclusive database locking with flock.
- Build isolation levels, including environment isolation, namespaces, and
  dependency-closure roots.
- Rootless builds, build profiles, layer caching, and build-output caching.
- Integration tests and CI coverage for builds, installation, repositories,
  locking, isolation, and static releases.

The next step is not to replace these systems. It is to add a coherent
management layer above them.

This proposal introduces six implementation phases:

| Phase | Name | Main result |
|---|---|---|
| 0 | Shared foundations and security corrections | Reliable artifact identity, provenance, and a common operation context |
| 1 | Transaction history and historical undo | Inspectable operation history and safe compensating undo |
| 2 | Plugin system | Versioned, capability-limited extensions outside the core process |
| 3 | Parallel downloads | Bounded concurrent fetching without changing installation ordering |
| 4 | Repository management | Multiple repositories, metadata queries, explicit cache refresh, safe cleanup |
| 5 | Package groups | Signed groups, group membership tracking, and group-aware removal |

The desired result is a package manager that is easier to administer at the
level of DNF while retaining Nawa's identity:

1. Source-based builds remain a first-class path.
2. Binary repositories remain an optional distribution and acceleration path.
3. The existing installer remains responsible for package file placement.
4. Archive and repository authentication remain mandatory according to the
   existing trust policy.
5. Build isolation remains independent from package-management features.
6. History, plugins, repositories, and groups must not create alternate paths
   that bypass the core safety checks.

## 2. Goals and non-goals

### 2.1 Goals

The completed design should provide:

- A durable record of package-changing commands and the packages they changed.
- A way to inspect a transaction and understand its exact package-level effects.
- A way to undo a previous transaction when its resulting state has not since
  been superseded.
- Reliable recovery metadata for interrupted operations.
- A stable plugin protocol for third-party commands and integrations.
- Bounded package and repository-metadata downloads.
- Multiple configured repositories with deterministic package selection.
- Repository queries that do not need to download every package archive.
- Safe, explicit cache maintenance.
- Package groups whose membership does not override explicit user intent.
- Backward compatibility for existing Nawa roots, archives, recipes, and
  single-repository configurations.
- Tests that verify failure behavior, not only successful command output.

### 2.2 Non-goals

This roadmap does not propose:

- Replacing Nawa's source build engine.
- Replacing the current local package database with RPM or dpkg.
- Replacing TOML recipes with a new recipe language.
- Replacing Ed25519, the existing signature message format, or SHA-256.
- Making plugins part of the trusted package manager core.
- Allowing repository plugins to decide whether a package signature is valid.
- Making all package operations globally atomic across the whole root filesystem.
- Automatically reverting administrator data, local configuration edits, user
  files, databases, or arbitrary side effects from package hooks.
- Deleting all cached archives merely because a user runs a generic cleanup
  command.
- Making a group a special package or changing the archive format to represent
  groups.
- Requiring an asynchronous runtime solely to make downloads concurrent.
- Changing Nawa's default behavior in a stable release before compatibility
  testing and explicit approval.

### 2.3 Architectural invariants

The following invariants apply to every phase.

**Core owns package state.** Only Nawa core code may update the local package
database, move package files into the target root, or commit a package
installation/removal.

**Verify before use.** An archive is not trusted because it is present in a
cache, because its filename looks correct, because a plugin supplied it, or
because a repository declared it. The core must validate the exact bytes using
the selected authentication policy before extraction.

**Plan before mutation.** Commands that can change package state must resolve
their package plan and show it before the first package mutation. Strict
repository mode must fail before mutation if the repository cannot supply the
required package set.

**Preserve per-package atomicity.** The existing installer stages one archive,
checks conflicts, journals file moves, updates the package database, and
restores the old file state on an installation failure. New code must use this
path rather than writing files directly.

**Do not overstate transaction atomicity.** A command that changes ten packages
can still finish after six packages and fail on the seventh. History must
record that as a partial operation. It must not label the entire system change
atomic.

**Keep slow work outside the database lock.** Compilation and network transfers
must not hold the package database lock for their full duration. Short locked
sections are used to reserve IDs, update transaction records, and commit each
package mutation.

**Keep the current formats readable.** New optional fields must have defaults
or be stored in separate versioned files. Old entries must remain readable.
New required metadata must not be silently assumed for legacy records.

**No silent security downgrade.** A missing signature, invalid signature,
wrong digest, untrusted key, malformed plugin response, or invalid repository
metadata must not be converted into success by fallback logic.

## 3. Current implementation baseline

This section records the extension points and limitations found in the
current code. It is included so future implementation work can be reviewed
against the actual architecture rather than an abstract package manager.

### 3.1 CLI and dispatch

Current main components:

| File | Current responsibility | Proposed extension |
|---|---|---|
| src/main.rs | Application setup and command dispatch | Construct an operation context and dispatch new subsystems |
| src/cli.rs | Clap command definitions, lock policy, completions | Add history, plugin, repository, and group commands |
| src/commands/installing.rs | Name-based install, update, autoremove, remove | Start/finish operation records and connect package steps |
| src/commands/querying.rs | Search, info, why, deps, list, files, owner, revdep | Add repository query and group inspection handlers |
| src/commands/configuring.rs | Configuration display and initialization | Display repositories and download/plugin settings |
| src/config.rs | Configuration discovery and precedence | Add validated settings while preserving old config |
| src/humanize.rs | Dates, durations, and sizes | Format transaction timestamps and history output |

The CLI currently uses a Clap derive-based Cmd enum. Dynamic plugin command
registration therefore needs deliberate CLI-dispatch work; it cannot simply
append arbitrary enum variants to a compiled enum at runtime.

### 3.2 Package database

src/db.rs stores one directory per installed package under:

~~~text
<root>/var/lib/nawa/local/<package>/
    desc
    files
    hashes
~~~

The package description is TOML and contains the name, version, release,
architecture, install reason, runtime dependencies, install date, and size.
Package file ownership is reconstructed from the per-package file lists.

This is an intentionally inspectable database. The proposal keeps it.

New artifact identity fields may be added to the package description only as
optional, defaulted fields. Larger provenance records should be stored in
separate versioned records and referenced by an identifier.

### 3.3 Installation, removal, and rollback

src/install.rs is the authoritative package mutation layer.

An archive installation currently:

1. Takes or creates a verified snapshot of the input archive.
2. Applies the selected detached-signature or signed-index authentication
   policy.
3. Extracts the archive into a temporary staging directory.
4. Reads .PKGINFO.
5. Checks existing package state and file ownership.
6. Protects locally modified configuration files under /etc.
7. Saves a package-local undo record when replacing an installed version.
8. Caches the archive.
9. Moves files into the target root through the internal Tx journal.
10. Updates the local package database using temporary files and renames.
11. Commits the file-move journal.
12. Runs the package hook.

Removal follows a similar reversible file-move process and drops the local
database entry after the files have been moved into its removal journal.

The current rollback command, nawa rollback <name>, restores the one package
version stored in the package's undo directory. That record is replaced by
later changes to the same package. It is not a chronological system-wide
transaction history.

The Tx structure provides a temporary in-process rollback mechanism for one
package operation. It is not a persistent, multi-package transaction journal.

### 3.4 Locking

src/lock.rs uses flock on:

~~~text
<root>/var/lib/nawa/lock
~~~

Readers use shared locking and writers use exclusive locking. The CLI lock
policy deliberately avoids holding the lock across a long source build or
repository download. Name-based installation and updates lock around each
package installation.

This is an important constraint for transaction history. An operation context
may span a long command, but its metadata updates must be short and protected.
The design must also tolerate more than one Nawa process running at once.

### 3.5 Repository and fetch path

src/repo.rs defines the repository index and validates its metadata.
src/upgrade.rs opens repositories, verifies their indexes, checks
freshness/rollback protection, fetches package entries, validates package
identity, and applies upgrades.
src/fetch.rs implements HTTP/local fetching, caching, retry/backoff, redirect
policy, mirror fallback, SHA-256 verification, and temporary-file activation.
src/sign.rs owns Ed25519 signing, verification, and trusted-key policy.

The current repository metadata supports legacy v1, signed-index v2, and
distributed per-package URLs in v3. The current configuration exposes one
scalar repository setting, with a local path or HTTP(S) URL.

The existing downloader is synchronous. Repository packages in the normal
installation plan are fetched and installed in plan order rather than through
a bounded concurrent download pool.

### 3.6 Build subsystem

src/build.rs, src/buildenv.rs, src/closure.rs, and src/sandbox/ implement
source builds and isolation. The levels are none, env, sandbox, and closure.
The current build profile controls compiler flags and build jobs. Build
dependencies are resolved separately from runtime dependencies.

The roadmap must not make the build engine depend on the plugin host or
repository query engine. It may consume the new downloader through a narrow
source-fetch API if that is useful, but the build isolation boundary must stay
unchanged.

### 3.7 Existing tests and CI

The repository already contains unit tests and integration suites for:

- Build engine behavior.
- Dependency planning.
- End-to-end package operations.
- Name-based and repository-based installation.
- Repository index v2.
- Upgrade regressions.
- Database locking.
- Rootless builds.
- Sandbox and isolation behavior.

The CI pipeline builds, runs Clippy with warnings denied, runs tests across
multiple Ubuntu versions and privilege contexts, checks isolation behavior,
runs CLI smoke tests, and builds the static musl binary.

Each phase must add tests to this existing framework. A documentation-only
proposal does not claim that the new behavior has been implemented or tested.

## 4. Phase 0 — Shared foundations and security corrections

### 4.1 Purpose

Phase 0 prepares the current installer and package metadata for future history.
It should be small, focused, and independently useful.

It must not implement the full history command yet.

Deliverables:

1. A common operation/transaction context passed through package-changing
   command paths.
2. Correct timestamp units and separate identifiers for time and temporary
   paths.
3. Immutable content-addressed storage for verified package archive bytes.
4. A provenance record that describes how an archive was authenticated.
5. A corrected package rollback path that verifies the exact original bytes.

### 4.2 Operation context

Introduce a type conceptually similar to:

~~~rust
pub struct OperationContext {
    pub id: Option<u64>,
    pub command: OperationCommand,
    pub started_at: u64,
    pub options: OperationOptions,
    pub recorder: Option<HistoryRecorder>,
}
~~~

The exact Rust API may change during implementation. The important
responsibility is to pass one operation identity through all package steps of
a command rather than independently inventing IDs inside the installer.

Suggested command kinds:

~~~rust
pub enum OperationCommand {
    Install,
    Update,
    Upgrade,
    Remove,
    Autoremove,
    BuildInstall,
    ArchiveInstall,
    GroupInstall,
    GroupRemove,
    HistoryUndo,
}
~~~

Do not serialize arbitrary shell arguments or secret environment variables.
History needs a normalized command kind and safe, useful parameters, not a dump
of the caller's environment.

An operation context should carry:

- A stable transaction ID once one is allocated.
- The normalized command name.
- Start time in Unix seconds.
- Optional end time.
- A status.
- Safe command parameters, such as requested package names and repository IDs.
- A history recorder or a no-history test implementation.
- A list or stream of package-step results.

build without installation does not change the target package database and
should not create a package transaction by default. build --install does.

### 4.3 Timestamp correctness

The current installer derives a nanosecond timestamp and stores it in
Entry.install_date. The human-readable date formatter interprets that field
as Unix seconds. These units do not agree.

New writes must store Unix seconds in install_date:

~~~rust
let install_date = SystemTime::now()
    .duration_since(UNIX_EPOCH)?
    .as_secs();
~~~

A separate identifier must be used for temporary directory names. Temporary
directory uniqueness must not be coupled to the package installation date.

Migration requirements:

- Old package descriptions must continue to deserialize.
- The migration must identify legacy nanosecond values using a conservative
  documented threshold or an explicit database-version marker.
- A legacy value that is clearly in nanoseconds must be converted to seconds.
- A value that is ambiguous must not be destructively rewritten without a
  recoverable backup.
- The migration must be idempotent.
- nawa info, nawa list/query, and removal-plan date output must use the
  corrected formatter consistently.

Do not change Entry.install_date to nanoseconds again. All persistent
timestamps in the new history format use Unix seconds unless a field name
explicitly states another unit.

### 4.4 Content-addressed archive objects

The current human-readable cache uses package-version filenames, for example:

~~~text
<root>/var/cache/nawa/gcc-15.1-1.nawa.tar.zst
~~~

That name is useful to users but is not a reliable identity for bytes. Two
different archives can have the same package filename across repository
publication, rebuild, or replacement.

Introduce an immutable object store:

~~~text
<root>/var/cache/nawa/
├── objects/
│   └── sha256/
│       ├── 12/
│       │   └── 12<full-digest>.nawa.tar.zst
│       └── ab/
│           └── ab<full-digest>.nawa.tar.zst
├── remote/
├── layers/
└── sources/
~~~

The path is derived only from the full SHA-256 digest. A package name or
version may be recorded in metadata, but it must not determine the object
identity.

Object publication rules:

1. Copy or download bytes into a unique temporary file in the target
   filesystem.
2. Flush the file and calculate SHA-256 from the bytes that will be stored.
3. If an expected digest exists, reject a mismatch.
4. Verify the required signature/authentication before the object becomes
   eligible for installation or historical restoration.
5. Publish the immutable object without overwriting a different object.
6. If another process already published the same digest, verify that object
   and discard the temporary duplicate.
7. Do not follow symlinks supplied by untrusted cache entries.
8. Do not trust a filename as evidence that the object contents match it.

Use a no-clobber publication strategy appropriate for the target filesystem.
A plain rename that can replace an existing file is not sufficient for
immutable-object publication unless the existing object is first verified
under a lock and the operation has no race.

Compatibility:

- Existing version-named archive cache entries must remain readable.
- Existing package rollback records must remain usable when their cached
  archives are valid.
- New installations should record a content-addressed artifact reference.
- A compatibility lookup may import an old version-named archive into the
  object store after verifying it.
- Do not remove legacy cache files during migration.
- Do not change .nawa.tar.zst archive contents or the package archive
  filename convention in repository indexes as part of this phase.

### 4.5 Artifact provenance and authentication evidence

A digest proves byte identity; it does not by itself prove publisher identity.
A transaction must retain both artifact identity and authentication
provenance.

Use a versioned sidecar or object metadata record. One conceptual TOML
representation is:

~~~toml
schema = 1
artifact_sha256 = "..."
archive_size = 1234567
authentication = "signed-index"
verified_at = 1793910000

[package]
name = "openssl"
version = "3.5.0"
rel = 1
arch = "x86_64"

[repository]
id = "faseela"
index_sha256 = "..."
index_signature_sha256 = "..."
signer_fingerprint = "..."
trusted_at_install = true
~~~

For detached-signature archives, the provenance record must refer to the
detached signature bytes and the key used to verify them.

For signed-index archives, the record must refer to the exact index bytes and
signature used at installation. The index digest, package entry digest, and
trusted signer fingerprint must be connected and verifiable.

A complete evidence bundle may contain:

~~~text
objects/sha256/<digest>/
    archive
    artifact.toml
    detached.sig                 # when detached-signature auth was used
    repository-proof/
        index.toml               # when signed-index auth was used
        index.toml.sig
        signer.pub
~~~

The implementation may store shared proof objects separately and reference
them by digest rather than duplicating the same index for every package.

Trust semantics:

- The public key must be identified by a stable fingerprint.
- The record must say whether the key was trusted by the target root at the
  time of installation.
- The proof bundle must preserve the exact bytes that were verified.
- Historical verification must recheck the signature and the package digest.
- A key removed from the current trust store must not be silently substituted
  by an unrelated key.
- A future explicit revocation mechanism must be able to reject a previously
  accepted key. Merely retaining a public key must not override an explicit
  revocation.
- The meaning of key removal versus key revocation must be documented before
  implementation. Removing a key from the active trust configuration and
  declaring a key compromised are not necessarily the same administrative
  action.

For unsigned bootstrap-mode archives, record that the artifact was accepted
without a trusted signature. Do not describe such an artifact as authenticated.
Historical restore must follow an explicit policy for unsigned artifacts and
must not silently upgrade their trust status.

### 4.6 Installed package artifact reference

The installed package database should be able to answer: which exact archive
was installed?

Possible optional fields in the package description:

~~~toml
artifact_sha256 = "..."
artifact_record = "sha256:..."
~~~

These fields must be optional and serde-defaulted. Old descriptions without
them remain valid.

The full authentication proof should not be copied into every package
description. The package entry should point to a durable artifact record.

For packages installed from a source build, record the produced archive digest
and a source/build provenance category. A local build is not automatically
signed merely because it has a digest.

For packages installed from a repository, record the authenticated repository
artifact identity. Keep the distinction between build identity and artifact
identity: a build ID describes inputs, while the archive SHA-256 describes
exact output bytes.

### 4.7 Repair package-level rollback

The current rollback implementation installs the cached previous archive
through the general standalone archive path and accepts unsigned content.
That does not reconstruct the original signed-index authentication proof.

New rollback behavior must:

1. Load the previous package record.
2. Resolve its immutable artifact object.
3. Verify the object SHA-256 against the recorded digest.
4. Verify detached signature evidence or signed-index proof, according to
   the original authentication method.
5. Validate archive package metadata against the expected package identity.
6. Confirm the target package state and ownership constraints.
7. Use the normal staged installer to restore the package.
8. Preserve the original install reason where appropriate.
9. Refuse restoration if the required archive or proof is missing or invalid.

A rollback must not treat a cached archive as trusted solely because Nawa
verified it once in the past. It must verify the exact cached bytes again.

Legacy undo records without a digest/provenance record require a documented
compatibility path. The implementation may permit the existing detached
signature policy where a usable signature exists. It must not claim strong
historical proof for legacy unsigned cache entries. Any weaker legacy behavior
must be explicit and must not be confused with the new guarantee.

### 4.8 Phase 0 implementation order

Recommended small changes:

1. Fix timestamp writes and add migration/compatibility tests.
2. Introduce artifact metadata types without changing install behavior.
3. Add content-addressed object publication and verification.
4. Make new installs retain artifact references.
5. Record authentication provenance for detached signatures.
6. Record signed-index proof persistence for trusted v2/v3 installations.
7. Repair package rollback to use the recorded artifact proof.
8. Add regression tests before enabling object-store cleanup.

Acceptance criteria:

- Existing local databases load.
- Existing archive files remain usable.
- New object writes are immutable and digest-verified.
- A corrupted cached archive cannot be restored.
- Signed-index and detached-signature archives retain distinct provenance.
- Timestamp display is correct for new entries and legacy entries.
- Existing archive format, repository schema, and CLI defaults remain unchanged.
- cargo test --release --locked and Clippy pass.

## 5. Phase 1 — Transaction history and advanced undo

### 5.1 Purpose and semantics

Add a durable operation journal similar in usability to dnf history, while
preserving Nawa's per-package installer model.

New commands:

~~~text
nawa history
nawa history info <ID>
nawa history undo <ID>
~~~

Optional later commands:

~~~text
nawa history list
nawa history undo <ID> --dry-run
nawa history undo <ID> --yes
nawa history export <ID>
~~~

The first release should keep the interface small. nawa history can be the
default list command; history list may be an alias.

A transaction is the logical user command. A package change is one step within
that transaction.

Examples:

- One nawa install firefox command is one transaction, even if it installs
  Firefox and several dependencies.
- One nawa update command is one transaction, even if it rebuilds several
  packages.
- One nawa remove firefox command is one transaction.
- One nawa group install desktop command is one transaction and may include
  multiple package changes plus a group-membership record.
- nawa history undo 12 is a new transaction that compensates for transaction
  12. It must not rewrite the old transaction as though it never happened.

A build without installation is not a package database transaction. A
build-with-install operation is.

### 5.2 Storage layout

Suggested layout:

~~~text
<root>/var/lib/nawa/
├── local/
├── undo/                         # legacy package-level rollback compatibility
├── history/
│   ├── schema-version
│   ├── last-id
│   ├── transactions/
│   │   ├── 0000000001.toml
│   │   ├── 0000000002.toml
│   │   └── ...
│   ├── journal/
│   │   ├── 0000000010/
│   │   │   ├── state.toml
│   │   │   └── steps/
│   │   └── ...
│   └── locks/                    # only if a separate lock is proven necessary
└── lock
~~~

The exact journal representation may be simplified, but the design must
support write-ahead intent and crash reconciliation.

Transaction records are immutable after completion except for narrowly defined
administrative annotations such as an undone_by reference or a schema
migration. Prefer append-only events or a new linked record over rewriting
historical facts.

last-id is updated while holding the exclusive database lock. IDs are
monotonic and never reused. Gaps are acceptable after crashes; reusing an ID
is not.

A history directory must not be writable by untrusted package build scripts.
Build isolation already masks Nawa state in sandboxed builds; preserve that
boundary.

### 5.3 Transaction record

A representative completed transaction:

~~~toml
schema = 1
id = 15
command = "update"
status = "completed"
started_at = 1793910000
finished_at = 1793910320
duration_seconds = 320
initiated_by = "uid:0"
root = "/"
requested_packages = ["curl", "openssl"]
repository_ids = ["faseela"]

[[package_changes]]
name = "openssl"
action = "upgrade"
status = "committed"

[package_changes.before]
version = "3.4.0"
rel = 1
arch = "x86_64"
reason = "dependency"
artifact_sha256 = "..."

[package_changes.after]
version = "3.5.0"
rel = 1
arch = "x86_64"
reason = "dependency"
artifact_sha256 = "..."

[[package_changes]]
name = "curl"
action = "upgrade"
status = "committed"

[package_changes.before]
version = "8.9.0"
rel = 1
arch = "x86_64"
reason = "explicit"
artifact_sha256 = "..."

[package_changes.after]
version = "8.10.0"
rel = 1
arch = "x86_64"
reason = "explicit"
artifact_sha256 = "..."
~~~

This example illustrates the information, not a finalized serde layout. The
implementation must validate transaction records and reject path values,
package names, or artifact references that could escape their intended roots.

Each package change should record:

- Package name and validated package identity.
- Action: install, upgrade, downgrade, reinstall, remove, or restore.
- Step status.
- Before-state, if one existed.
- After-state, if one exists.
- Previous and resulting artifact references.
- Install reason before and after.
- Architecture.
- Repository/source category.
- Authentication/provenance reference.
- A safe failure summary when a step failed.
- Whether package hooks completed or failed after commit.

Avoid recording secrets, full environment variables, signing private keys,
credentials embedded in URLs, or arbitrary hook output without an explicit
redaction policy.

### 5.4 Transaction lifecycle

Suggested states:

| State | Meaning |
|---|---|
| preparing | ID allocated and command plan being assembled |
| running | At least one operation step may execute |
| completed | All planned package and metadata steps completed |
| partial | Some state changes committed, then a later step failed |
| failed | The operation failed before any package state changed |
| interrupted | A process ended while the transaction was active |
| needs-recovery | Reconciliation cannot safely infer the result |
| undone | A separate compensating operation completed; retain the original record |

The original transaction should not change from completed to undone in a way
that erases its history. Record a reference such as undone_by = 27, and make
transaction 27 an ordinary history entry with command = history-undo and
undoes = 15.

The precise state machine must be documented and tested. In particular,
interrupted is not synonymous with failed: package state may have changed
before the process died.

### 5.5 Write-ahead package-step journal

A package install can commit its files and database entry, then the process may
crash before a separate transaction-record write completes. A plain
after-the-fact history write would lose that change.

Use a write-ahead step record:

1. Before a package mutation, persist a step intent containing the transaction
   ID, package name, expected operation, current package identity, desired
   package identity, and artifact reference.
2. Flush and atomically publish the intent.
3. Call the existing installer/remover.
4. After the package database commit, persist the resulting package identity
   and step result.
5. Mark the step committed.
6. Continue to the next package.

The installer should expose a narrow transaction-recorder integration point
or callback at the actual package commit boundary. Do not add a second
installer implementation just for history.

Because the package database and history journal are separate files, a normal
rename cannot make both records atomic as one filesystem operation. The
write-ahead intent and recovery algorithm are therefore required.

Recovery should inspect:

- The intent.
- The package database entry.
- The installed artifact reference, if present.
- The archive object and digest.
- The package file list where needed.

If the package database matches the intended after-state, the recovery code can
record the step as committed. If it still matches the before-state, it can
record that the mutation did not commit. If it matches neither, mark the
transaction needs-recovery and refuse automatic guessing.

Recovery must not run arbitrary package hooks a second time merely because a
history event was missing. Hook execution is an external side effect and needs
its own recorded status.

### 5.6 Do not hold the global lock through builds and downloads

The existing lock policy is intentional: builds and downloads may be long, so
they do not hold the database lock for the full command.

Preserve that behavior.

Recommended lifecycle:

1. Read recipes and prepare the plan using existing logic.
2. Acquire the exclusive lock briefly; allocate the ID and write the
   preparing transaction.
3. Release the lock.
4. Fetch repository archives and build source packages without the database
   lock.
5. Before each package mutation, acquire the exclusive lock.
6. Revalidate the current package state against the step's recorded
   precondition.
7. Commit the package through the existing installer.
8. Persist the corresponding history result before releasing the lock.
9. Release the lock and continue.

This permits concurrent Nawa commands to perform slow preparation. It also
means plans can become stale while another command changes the same root.
Therefore every package step must revalidate its preconditions under the
exclusive lock. A stale plan must stop or be recomputed; it must not overwrite
a newer package operation silently.

History ID allocation and history-file mutation must be serialized. Use the
existing database lock initially. Introduce a second history lock only if a
measured need exists and its lock ordering is specified.

### 5.7 Multi-package commands and partial completion

Do not promise a globally atomic update.

Example:

~~~text
Transaction #22: update
  openssl: committed
  curl: committed
  firefox: fetch failed
Result: partial
~~~

The command may report that two package changes committed before failure.
The original transaction remains in history.

A subsequent invocation is a new transaction, even if it resumes the same
package goal. It must not append unrelated later changes to the original
transaction.

If an operation fails before any package state changes, mark it failed.
If one or more package steps committed before failure, mark it partial.
If the process disappears unexpectedly, use interrupted until reconciliation
determines whether the operation is completed, partial, or needs manual
recovery.

A post-install hook can fail after the installer has committed the package.
That does not mean the package files were rolled back. Record the package step
as committed with a hook-failure detail and report the transaction as partial
or completed-with-warnings according to the agreed status model. Do not report
a package rollback that did not occur.

### 5.8 History listing and inspection

nawa history should be script-friendly when output is redirected and
human-friendly on a terminal.

Suggested columns:

~~~text
ID    Date                 Command       Packages   Result
-----------------------------------------------------------
27    2026-11-05 22:10     history-undo  3          completed
26    2026-11-05 21:55     update        8          partial
25    2026-11-05 20:03     install       4          completed
24    2026-11-04 18:30     remove        1          completed
~~~

Requirements:

- Newest transactions first by ID.
- Stable numeric IDs, not timestamps as identifiers.
- Clear distinction among completed, partial, failed, interrupted, and
  needs-recovery.
- A documented default display limit and --limit.
- Optional filters such as --package, --command, --since, and --until
  may be added after the first implementation.
- No color in non-terminal output, consistent with current Nawa behavior.
- A future --json mode should emit one valid JSON document and stable schema.

nawa history info <ID> should show:

- Command and safe parameters.
- Start/end time and duration.
- Result state.
- Packages planned, changed, skipped, and failed.
- Before and after package identity.
- Source and repository.
- Artifact digest and provenance reference.
- Hook outcomes.
- Undo status and links to the undoing transaction, if applicable.

### 5.9 Historical undo semantics

nawa history undo <ID> is a compensating transaction, not time travel.

The command must:

1. Load and validate the target transaction.
2. Refuse unsupported transaction schema versions.
3. Refuse an operation that has no committed package changes to compensate.
4. Determine the package steps that actually committed.
5. Compare the current package state with the after-state recorded by the target
   transaction.
6. Detect packages changed by later operations.
7. Resolve all required previous archive objects and authentication proof.
8. Verify every required archive before changing any package.
9. Build a complete undo plan.
10. Validate dependencies, file conflicts, architecture, and package hooks.
11. Ask for confirmation unless --yes is explicitly given.
12. Create a new history transaction linked to the original ID.
13. Apply the compensation through the ordinary installer/remover.
14. Record every committed step and finish the new transaction.

The undo plan must be all-preflighted before the first package mutation. It
cannot guarantee that every later filesystem operation succeeds, but it can
avoid discovering a missing old archive after already restoring half the
packages.

#### Optimistic concurrency / superseded package protection

A transaction should only be undone automatically when the current package
state still corresponds to the after-state it created.

Example:

- Transaction 10 upgrades OpenSSL from 3.4 to 3.5.
- Transaction 11 upgrades OpenSSL from 3.5 to 3.6.
- Undo transaction 10 must not downgrade OpenSSL from 3.6 to 3.4.

The comparison should use package identity plus artifact digest when available,
not only name and version. A package may have the same version string but
different archive bytes.

If a package was changed by a later transaction, report a conflict and refuse
that part of automatic undo. The first release should fail the complete undo
preflight rather than silently perform a partial undo. A later, explicit
conflict-resolution interface may be considered.

#### Install, upgrade, remove cases

| Original change | Compensation |
|---|---|
| New package installed | Remove it, but only if no remaining package requires it and current state still matches |
| Package upgraded | Restore the exact previous artifact |
| Package downgraded | Restore the previous artifact from before that downgrade |
| Package removed | Reinstall the removed artifact |
| Package reinstalled | Restore the previous artifact and prior reason |
| Package step not committed | No compensation required |
| Failed hook after package commit | Compensate the package only if explicitly requested; do not claim hook side effects are reversible |

Dependency order must be respected. Restoring removed dependencies should
generally happen before restoring packages that require them. Removing packages
installed by an old transaction should happen in dependent-first order and
must recheck current reverse dependencies.

#### Configuration files and external side effects

Historical undo must preserve Nawa's existing configuration-file protection.

It must not blindly overwrite locally modified files under /etc. The normal
installer should continue to place changed configurations beside local edits
as .nawa-new according to its existing rules.

Package hooks may alter service state, databases, users, caches, or external
files. Those side effects are not necessarily reversible. History should
record hook outcomes and the undo command should explain that package archive
restoration cannot promise to reverse arbitrary hook behavior.

Do not describe historical undo as restoring a byte-for-byte snapshot of the
entire root filesystem.

### 5.10 Retention and garbage collection

A completed transaction may reference an archive that is no longer installed
but is essential for undo.

The object store must retain an archive while it is referenced by:

- An installed package entry.
- An undo/history package before-state.
- An undo/history package after-state when needed to reapply or inspect.
- A pending or interrupted transaction.
- A compatibility rollback record.
- A group or other future state record that explicitly requires it.

A transaction-retention policy may eventually prune old history. Pruning must
be explicit, must show which undo capabilities will be lost, and must not
leave dangling artifact references.

Do not implement automatic history pruning in the first release. Start with
no automatic deletion and add measured retention behavior later.

### 5.11 Phase 1 tests

At minimum:

- Install transaction with one package.
- Install transaction with dependencies.
- Upgrade transaction with before/after versions.
- Remove transaction and restoration.
- Autoremove transaction with dependent-first order.
- Build-with-install transaction.
- A command that fails before any mutation.
- A command that fails after one or more committed package steps.
- Crash after write-ahead intent but before install.
- Crash after package database commit but before history completion.
- Corrupt or truncated transaction record.
- Unsupported transaction schema.
- Two concurrent processes allocating IDs.
- Two concurrent processes trying to change the same package.
- Historical undo of install, upgrade, removal, and partial transaction.
- Refusal when a package was changed by a later transaction.
- Refusal when an archive object or proof bundle is missing or corrupt.
- Signed-index and detached-signature archive restoration.
- Local /etc modifications remain protected.
- Hook failure is recorded without claiming that the package was rolled back.
- Legacy package rollback still works.

Acceptance criteria:

- IDs are unique and monotonic.
- Every package mutation is associated with an operation record.
- Crashes leave enough data for safe reconciliation.
- Partial operations are accurately described.
- Undo preflights all required artifacts and state conflicts.
- Undo itself creates a new transaction.
- No existing package mutation bypasses the central installer.
- History operations respect the existing database lock.

## 6. Phase 2 — Plugin system

### 6.1 Purpose

Plugins allow Nawa to gain integrations without adding every optional feature to
the core binary.

The first implementation should favor a small, versioned, out-of-process API.
Do not load arbitrary Rust dynamic libraries into the Nawa process.

A process boundary improves crash isolation and versioning. It is not, by
itself, a security sandbox: a child process launched by a root Nawa process
normally inherits the parent's privileges unless the host deliberately drops
or constrains them.

### 6.2 Plugin categories

The API may support these extension categories:

| Category | Examples | Risk level |
|---|---|---|
| External command | Audit report, package dashboard, maintenance command | Low to medium |
| Read-only query provider | Additional metadata lookup | Medium |
| Repository transport/adapter | Fetch metadata from a nonstandard service | High |
| Pre-operation validator | Policy checks before a package change | High |
| Post-operation observer | Notifications, metrics, audit event | Medium |
| Integration | Desktop notifications, service manager integrations | Medium to high |

The first release should implement command extensions and post-operation
observers. Repository adapters and privileged hooks should come later, after
the protocol and capability model have been reviewed.

### 6.3 Discovery locations

Suggested locations:

~~~text
/usr/lib/nawa/plugins/              # distribution-managed plugins
/usr/local/lib/nawa/plugins/        # locally administered plugins
~/.local/lib/nawa/plugins/          # per-user, unprivileged plugins
~~~

Root Nawa must not automatically execute a user-writable plugin merely because
it is discoverable in a user's home directory.

Discovery policy must distinguish:

- System plugins approved by the administrator.
- User plugins available only to that user.
- Enabled plugins.
- Disabled plugins.
- Invalid or incompatible plugins.

For privileged commands, only administrator-approved system plugins may be
eligible, and only with explicitly granted capabilities.

### 6.4 Manifest format

An illustrative manifest:

~~~toml
[plugin]
id = "notify"
name = "Nawa notification plugin"
version = "1.2.0"
api_version = "1"
executable = "/usr/lib/nawa/plugins/nawa-plugin-notify"
description = "Send desktop notifications after package operations"

[commands]
names = ["notify-test"]

[capabilities]
requested = ["history.read", "transaction.observe"]
~~~

Manifest rules:

- Plugin IDs are lowercase, stable identifiers with a conservative character
  set.
- IDs must be unique across enabled discovery paths.
- Version uses a documented semantic-version policy.
- API version is separate from plugin version.
- Executable paths must be absolute after resolution and must not escape the
  approved plugin directory for system plugins.
- The executable must be a regular file and must not be writable by
  unprivileged users when it is approved for privileged use.
- Unknown manifest fields should be rejected unless a schema explicitly
  allows forward-compatible extension fields.
- The manifest must not grant capabilities merely by listing them. Requested
  and administrator-granted capabilities are separate concepts.

### 6.5 Protocol

Use JSON Lines over standard input and standard output for the initial
protocol. Standard error is reserved for diagnostics. Standard output must
contain protocol messages only.

A handshake request:

~~~json
{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","method":"hello","params":{"nawa_version":"1.0.0","plugin_api":"1"}}
~~~

Example response:

~~~json
{"protocol":"nawa-plugin","protocol_version":1,"request_id":"1","ok":true,"result":{"plugin_id":"notify","plugin_version":"1.2.0","api_version":"1","commands":["notify-test"],"capabilities":["history.read","transaction.observe"]}}
~~~

Protocol requirements:

- One request per JSON line and one response per JSON line.
- Every request has a request ID.
- Responses must match the request ID.
- Maximum message size is bounded.
- Invalid UTF-8, malformed JSON, unexpected fields, duplicate request IDs,
  protocol-version mismatch, and oversized messages are errors.
- The host validates all fields as untrusted input.
- Plugin output is never interpreted as shell code.
- The host does not pass secrets or private signing keys to a plugin.
- The protocol must define whether a plugin is one-shot or remains alive for a
  session. Start with one-shot commands and bounded hook invocations for
  simpler lifecycle and cleanup.

A future protocol may use a Unix socket or a structured RPC transport, but
that is not required for version 1.

### 6.6 Command registration and CLI behavior

Clap's current derive-based static command enum cannot dynamically add
arbitrary enum variants after compilation.

Two implementation options exist:

1. Namespaced first version: expose plugin commands as
   nawa plugin run <plugin-id> <command> [args...].
2. Direct command dispatch later: add a carefully tested unknown-command
   fallback so registered plugin command names can be routed to the plugin
   host while built-in Nawa commands remain reserved.

Recommended rollout:

- First ship nawa plugin list, info, enable, disable, and plugin run.
- Define command metadata and completion output.
- Only then consider direct top-level plugin commands.
- Built-in names always take precedence and cannot be shadowed.
- Duplicate plugin command names are rejected deterministically.
- Shell completion must not execute plugins just to discover their names.

The plugin host must preserve argument boundaries. It should pass an argv-like
array, not concatenate untrusted arguments into a shell command.

### 6.7 Enable/disable configuration

Suggested configuration:

~~~toml
[plugins]
enabled = ["notify", "audit"]
disabled = ["experimental-repo"]
~~~

A configuration format may later support per-plugin settings:

~~~toml
[plugins.notify]
enabled = true
timeout_seconds = 5
~~~

The first version should avoid a large arbitrary configuration language.

Precedence should be documented. An explicit administrator disable must not
be overridden by a per-user enable for privileged execution.

### 6.8 Capability model

Capabilities are named, narrow permissions. Examples:

- history.read
- repository.metadata.read
- transaction.observe
- command.register
- notification.send
- package.plan.inspect

Avoid broad capabilities such as root, filesystem.write, or package.install
for general plugins.

A plugin must not receive direct write access to:

- /var/lib/nawa/local
- /var/lib/nawa/history
- /var/cache/nawa/objects
- The trusted-key directory
- The target root package paths

A repository plugin may propose metadata or return downloaded bytes, but Nawa
core must validate the signed metadata, digest, package identity, and signature
policy before installation.

A pre-transaction plugin should be a validator/observer, not an alternate
installer. It may return allow/deny plus a bounded explanation. It must not be
able to edit the already-approved package plan without forcing the user to
review a new plan.

Privileged actions, if ever supported, must be brokered by Nawa core through a
small explicit API. The plugin itself should not inherit unrestricted root
privileges.

### 6.9 Process isolation, errors, and timeouts

For each plugin execution:

- Set a timeout.
- Bound stdout and stderr.
- Bound memory and process count where the host platform supports it.
- Close unused file descriptors.
- Use a controlled environment.
- Avoid passing the signing-key directory or arbitrary credentials.
- Run unprivileged by default.
- For privileged plugin functionality, require an administrator-approved
  execution identity and narrowly brokered capabilities.
- Do not silently run a plugin with weaker restrictions if a requested
  restriction cannot be enforced.

Errors must be isolated:

- A failed notification must not undo an installed package.
- A failed post-transaction observer must be reported as a warning and stored
  as an observer failure.
- A pre-transaction validator timeout must fail closed when that validator
  is a required policy gate.
- An optional observer timeout may be reported and skipped.
- Malformed protocol output must never be treated as approval.

### 6.10 Plugin lifecycle commands

Initial command set:

~~~text
nawa plugin list
nawa plugin info <ID>
nawa plugin enable <ID>
nawa plugin disable <ID>
nawa plugin run <ID> <COMMAND> [ARGS...]
~~~

Possible later commands:

~~~text
nawa plugin validate <ID>
nawa plugin doctor
~~~

Plugin management must not download or install arbitrary executable code
without a separate, explicit package installation and trust decision. A plugin
being present on disk does not imply it is trusted or enabled.

### 6.11 Repository adapter contract

Do not let a repository plugin return an already-trusted package object.

The adapter should provide transport/translation only:

1. The plugin returns a normalized metadata location and/or bytes.
2. Nawa core validates URL and path rules.
3. Nawa core parses the supported metadata schema.
4. Nawa core verifies the repository signature against configured trust.
5. Nawa core verifies artifact digests and package identity.
6. Nawa core passes a verified artifact to the existing installer.

If a repository type cannot produce evidence compatible with Nawa's trust
model, it must not be allowed to install packages through privileged mode.

### 6.12 Plugin API versioning

Use a protocol version independent from Nawa's application version.

Compatibility policy:

- A plugin declares the API version it implements.
- Nawa declares the API versions it supports.
- Incompatible major API versions are refused before execution.
- Backward-compatible fields are optional and defaulted.
- Unknown response fields are ignored only where the protocol explicitly
  permits them.
- Breaking protocol changes require a new API version.
- Plugins must not infer capabilities from the Nawa binary version alone.

### 6.13 Phase 2 tests

Test:

- Manifest discovery and deterministic ordering.
- Duplicate IDs and duplicate command names.
- Invalid executable paths and symlink escapes.
- Unsafe ownership and writable executable checks.
- Disabled plugin is never started.
- Incompatible API version is rejected.
- Malformed JSON and wrong request IDs are rejected.
- Oversized output is rejected.
- Hung plugin is terminated after timeout.
- Plugin crash does not crash Nawa.
- A post-transaction plugin failure does not undo package state.
- A required pre-operation policy plugin fails closed.
- User plugin cannot run with privileged capabilities.
- Plugin cannot write Nawa database records through the protocol.
- Repository plugin cannot bypass core signature and digest checks.
- Built-in CLI command names cannot be shadowed.

## 7. Phase 3 — Parallel downloads

### 7.1 Purpose

Reduce wall-clock time when a package plan contains several repository
archives, without changing dependency resolution or package installation
safety.

The current fetch implementation is synchronous and already includes retry,
backoff, redirect policy, mirrors, cache verification, and temporary-file
activation. Preserve and reuse that implementation.

### 7.2 Configuration

Add a download concurrency setting independent from build jobs.

Suggested configuration:

~~~toml
jobs = 8
downloads = 6
~~~

Suggested environment variable:

~~~sh
NAWA_DOWNLOADS=6
~~~

Suggested optional CLI override:

~~~text
nawa install --max-downloads 4 package-a package-b
nawa update --max-downloads 4
nawa upgrade --max-downloads 4
nawa makecache --max-downloads 4
~~~

The exact option name must be consistent across commands. Avoid adding both
--downloads and --max-downloads as permanent competing spellings.

Configuration precedence should match Nawa's existing policy:

1. Command-line option.
2. Environment variable.
3. Nearest project nawa.toml.
4. System configuration.
5. Built-in default.

The default should be conservative and configurable. A value such as 4 or 6
is a reasonable initial proposal, but must be measured on target systems.
Clamp the value to a safe positive range and reject zero rather than silently
spawning no workers.

### 7.3 Worker model

The current code uses synchronous ureq. A bounded worker pool using the
standard library is sufficient for the first version.

Do not add Tokio or another large async runtime solely for downloads unless
benchmarking shows a concrete need.

Conceptual work queue:

~~~text
Repository package entries
          |
      Work queue
          |
   +------+------+------+
   |             |      |
 Worker 1     Worker 2 Worker N
   |             |      |
 independent temporary files
   |             |      |
 retries / mirrors / digest verification
   +------+------+------+
          |
 immutable verified artifact objects
~~~

Each worker must have:

- A bounded number of active transfers.
- An independent HTTP agent or safely shared client instance.
- Its own temporary destination.
- Existing retry/backoff behavior.
- Existing redirect validation.
- Mirror fallback.
- Expected SHA-256 validation.
- Size validation when available.
- Clear error context tied to the package and repository.

The worker pool must not create unbounded threads from the number of packages
in the plan. The configured limit is a hard ceiling.

### 7.4 Separate fetch from install

This is the most important behavioral decision.

Downloads may be concurrent. Package installation remains dependency ordered
and uses the current installer.

For a repository-backed plan:

1. Resolve the dependency plan.
2. Decide each package's source using the existing auto/repo/build policy.
3. Validate the entire strict repository plan.
4. Start bounded downloads for repository-selected archives.
5. Verify every downloaded archive and repository package identity.
6. Store verified archives as immutable objects.
7. Process package operations in dependency order.
8. Build source packages where selected.
9. Install each ready archive through the existing installer.
10. Record each committed package step in history.

This design can fetch later repository packages while an earlier package is
being prepared. It must not install a dependent package before its dependency
has been committed.

Strict repo mode should prefetch and verify all required repository archives
before the first package mutation. If a required download fails, no package
from that command should have been installed. This is stronger preflight
behavior than fetching one package immediately before installing it.

In auto mode, a failed repository fetch may fall back to the source build
for that recipe where the current policy permits it. The fallback must be
reported clearly. A package that exists only in the repository and has no
recipe cannot be built and must fail.

### 7.5 Cache publication and races

The downloader must not let two workers write to the same .part path.

Use unique temporary names incorporating a transaction/worker identifier or
random exclusive file creation. Publish by artifact digest after validation.

When two workers fetch identical bytes:

- Both may download to separate temporary files.
- Both calculate and verify the digest.
- One publishes the immutable object.
- The other verifies the already-published object and removes its temporary
  duplicate.
- Neither may overwrite an existing object with different bytes.

When the expected digest is not yet known, the bytes are not eligible for
installation. They must first be tied to authenticated repository metadata and
then validated.

### 7.6 Failure and cancellation

A worker failure must not hide the package name, repository, or root cause.

On a permanent failure:

- Stop scheduling additional work when appropriate.
- Allow already-running transfers to finish or cancel them safely.
- Delete incomplete temporary files.
- Preserve already-verified immutable objects for reuse.
- Do not convert a 403 or arbitrary 4xx into package absence.
- Continue to retry only errors classified as transient by the existing
  downloader.
- Keep the current distinction between optional 404 sidecars and real
  download failures.

Error output should be deterministic. When several workers fail, report all
relevant failures in plan order rather than whichever thread completes first.

### 7.7 Repository metadata downloads

The same bounded infrastructure may later fetch metadata for multiple
configured repositories. Metadata downloads must use the same signature,
freshness, and digest validation as normal repository access.

A metadata download is not trusted merely because it came from a configured
URL.

### 7.8 Progress output

Interactive output may show per-package progress and a combined summary.
Non-terminal output must remain deterministic and script-friendly.

Avoid multiple workers writing uncontrolled progress lines to stderr. Use a
single progress reporter or serialized output channel.

Do not require a terminal for parallel downloads. Piped and CI invocations
must work.

### 7.9 Phase 3 tests and measurements

Tests:

- Maximum concurrent worker count is never exceeded.
- One package failure does not corrupt another worker's temporary file.
- Retry and backoff still occur.
- Mirrors still work.
- Wrong digest is rejected.
- Wrong size is rejected.
- Existing verified cache object is reused.
- Concurrent publication of the same digest is safe.
- Concurrent publication of different digests cannot overwrite.
- Strict repository mode fetches all required packages before mutation.
- Auto mode falls back to source builds only where allowed.
- Installation order remains dependency-first.
- A failed dependent fetch leaves no partial archive in the object store.
- Progress output does not interleave unpredictably.
- downloads = 1 behaves like sequential downloading.
- Build jobs remains independent from download concurrency.

Record benchmarks for a small plan and a large plan. Do not make performance
claims without measurements on a defined network and repository fixture.

## 8. Phase 4 — Advanced repository management

### 8.1 Purpose

Make repositories first-class administrative objects rather than a single URL
passed into install/upgrade behavior.

The first repository-management release should add:

- Multiple configured repositories.
- repolist.
- repoquery.
- makecache.
- clean.

Keep current local and HTTP(S) repository support. Preserve existing signed
index verification and repository freshness checks.

### 8.2 Multiple repository configuration

Current configuration supports a scalar repo. Preserve it as a legacy
default.

Proposed new form:

~~~toml
[[repositories]]
id = "faseela"
name = "Faseela Packages"
url = "https://packages.example.com/stable"
enabled = true
priority = 10

[[repositories]]
id = "community"
name = "Community Packages"
url = "https://community.example.com"
enabled = true
priority = 20

[[repositories]]
id = "testing"
name = "Testing"
url = "/srv/repositories/testing"
enabled = false
priority = 50
~~~

A repository definition should include:

- Stable ID.
- Human-readable name.
- Local path or HTTP(S) base URL.
- Enabled state.
- Numeric priority.
- Optional architecture restrictions.
- Optional cache policy.
- Optional repository-specific metadata settings.

Do not put trusted signing keys in the repository file as a shortcut. Trust
anchors remain in the existing trusted-key store or a separately reviewed
trust configuration.

### 8.3 Configuration compatibility and precedence

src/config.rs currently validates configuration fields and rejects unknown
keys in its file-level configuration structure. Adding repositories must
update that structure deliberately.

Migration rules:

1. Existing repo = "URL" configuration remains valid.
2. If no repositories entries exist, synthesize one legacy default
   repository from repo.
3. If both legacy repo and repository entries exist, define deterministic
   precedence rather than silently dropping one.
4. Recommended rule: explicit repositories entries form the enabled
   repository set; the legacy repo remains a fallback default only when no
   explicit repository list exists. Emit a clear warning if both forms are
   present and the legacy value is ignored.
5. An empty legacy repo = "" continues to disable the legacy repository.
6. Relative local repository paths resolve relative to the config file that
   contains them, following the existing path anchoring rules.
7. Environment and command-line overrides must be documented and tested.
8. config show must display each repository and the source of its settings.

Do not silently merge duplicate repository IDs. Reject them with a useful
error.

### 8.4 Repository selection

When the same package exists in multiple enabled repositories, the selection
policy must be deterministic and visible.

Candidate selection should account for:

1. Repository enabled state.
2. Architecture compatibility.
3. Repository priority.
4. Version/release comparison.
5. Explicit repository selection, if the user supplied --repo or --repo-id.
6. Package source policy: auto, repo, build, ask.

Define priority direction explicitly. Recommended convention: lower numeric
priority means higher precedence, with 10 preferred over 20.

A priority must not silently force an older package when a newer compatible
candidate exists unless the configured policy explicitly says priority wins.
The design must choose and test whether repository priority wins before
version, or whether the newest version wins within an allowed priority band.
Do not leave this to iteration order.

For the first implementation, prefer explicit behavior:

- An explicit --repo-id restricts lookup to that repository.
- Otherwise choose the highest-priority eligible repository.
- Within the same priority, choose the highest compatible version.
- Break remaining ties by stable repository ID ordering.
- Show the selected repository in install/update plans.

If a different policy is chosen, document it and add tests before release.

### 8.5 Repository IDs and cache isolation

Repository IDs are identifiers, not filesystem paths. Validate them with a
strict allowed character set.

The remote cache must remain URL-scoped, as it is today, and should additionally
associate cached metadata with the repository ID/configuration. Two repository
IDs pointing to different URLs must not share metadata accidentally.

Changing a repository URL must not cause Nawa to trust the old cache as if it
belonged to the new URL.

### 8.6 nawa repolist

Initial syntax:

~~~text
nawa repolist
nawa repolist --all
nawa repolist --enabled
~~~

Output should include:

- Repository ID.
- Name.
- Enabled/disabled state.
- Priority.
- URL/path with credentials redacted.
- Cached metadata generation or last refresh time when available.
- Trust state of the latest cached index.
- Any current cache error or expiration warning.

repolist should not download metadata by default. It reports configuration
and locally known cache state. repolist --refresh may be considered later,
but makecache is the explicit refresh command.

### 8.7 Repository query metadata

The current package index can search package names and contains archive
identity/digest information. It does not provide a complete, indexed file
provider database.

A useful repoquery needs more structured package metadata:

- Name, version, release, architecture.
- Summary and description.
- Runtime dependencies.
- Build dependencies where published and appropriate.
- Provides.
- Conflicts.
- Replaces.
- Download size and installed size.
- Repository ID.
- Archive filename or immutable download location.
- SHA-256 and authentication metadata.
- Optional package file-list reference.

Optional package metadata fields can be added to the package entry where
appropriate. File lists should be stored separately for large repositories.

### 8.8 Backward-compatible metadata extension

The current repository index schemas are v1, v2, and v3. Do not casually
change the meaning of those schemas or make existing clients believe that a
new field is required.

A candidate approach is a signed-index extension:

~~~toml
[repodata]
schema = 1
filelists_file = "filelists-v1.toml.zst"
filelists_sha256 = "..."
filelists_size = 987654
~~~

The repodata reference is part of the exact signed index bytes. A new
client verifies the index signature first, then checks the file-list artifact
digest and size against the signed reference.

The current serde structures appear to tolerate unknown top-level fields, but
this must be confirmed by a compatibility test against actual supported
released clients before publication. Do not assume this behavior without
testing.

If the extension cannot be safely ignored by older clients, use a new
repository schema or a separately published metadata channel. In either case:

- Old clients must either ignore the optional extension safely or reject the
  new schema clearly.
- New clients must not treat untrusted supplemental metadata as authoritative.
- The package archive's digest and signature verification remain unchanged.
- The file-list metadata must be authenticated by the signed index reference
  or its own explicit trusted signature.

A file-list artifact may contain package-to-path mappings:

~~~toml
schema = 1

[[package]]
name = "firefox"
version = "..."
rel = 1
arch = "x86_64"
files = [
  "/usr/bin/firefox",
  "/usr/lib/firefox/libxul.so"
]
~~~

The actual format should be optimized for size and query performance. The
example is not a required serialization layout.

### 8.9 nawa repoquery

Proposed commands:

~~~text
nawa repoquery firefox
nawa repoquery --available
nawa repoquery --repo-id community firefox
nawa repoquery --arch x86_64 firefox
nawa repoquery --whatprovides /usr/bin/firefox
nawa repoquery --whatprovides libssl.so.3
nawa repoquery --requires curl
nawa repoquery --info openssl
~~~

The first release should prioritize package-name queries and package info.
Provider and file-list queries depend on authenticated metadata and should be
enabled only when that metadata is available.

Behavior:

- Search enabled repositories by default.
- Permit explicit repository filtering.
- Respect architecture filters.
- Show repository ID for every result.
- Distinguish cached/offline results from freshly refreshed results.
- Never download package archives just to answer a metadata query.
- Return a documented nonzero exit code when no matches exist.
- Offer machine-readable output after the basic output format is stable.

whatprovides semantics must be clear. A file path lookup uses the
authenticated file-list metadata. A shared-library SONAME lookup uses
published provides metadata or a clearly defined ELF metadata index. Do not
claim to resolve arbitrary file ownership from name-only package indexes.

### 8.10 nawa makecache

Proposed syntax:

~~~text
nawa makecache
nawa makecache --repo-id community
nawa makecache --refresh
nawa makecache --metadata-only
~~~

The first version can simply refresh enabled repository metadata; avoid
prematurely adding overlapping flags.

Responsibilities:

1. Read enabled repository configuration.
2. Fetch index metadata into temporary files.
3. Validate HTTP result and redirects.
4. Parse the index.
5. Verify the index signature and trust policy.
6. Validate repository schema and entries.
7. Validate generated/expiry timestamps and rollback/freeze protection.
8. Atomically activate the verified cached metadata.
9. Fetch supplemental metadata only when its authenticated reference is valid.
10. Record refresh timestamps and errors.
11. Report each repository independently.

A failed refresh must not replace a working verified cache with incomplete or
unverified data.

The existing open_repo behavior refreshes remote index data during repository
opening and may use a cached index when the network is unavailable. The
implementation should consolidate this behavior so makecache, repoquery,
install, and upgrade share one repository-cache policy rather than
implementing different signature and freshness rules.

Clarify offline behavior:

- A verified, unexpired cache may be used according to the configured policy.
- An expired cache must be rejected unless the existing explicit force policy
  permits it.
- A failed refresh must be reported even if an older cache remains available.
- A cached invalid signature must never become acceptable because the network
  is unavailable.

### 8.11 nawa clean

Proposed syntax:

~~~text
nawa clean metadata
nawa clean packages
nawa clean sources
nawa clean layers
nawa clean all
nawa clean --dry-run all
~~~

Initial cleanup categories:

| Category | Contents |
|---|---|
| metadata | Repository indexes and supplemental metadata caches |
| packages | Downloaded package objects not referenced by installed state or history |
| sources | Source tarballs that are not needed by active builds or configured retention |
| layers | Build-closure layers not referenced by current build-cache policy |
| all | All eligible categories, not protected state |

The command must:

- Show the amount of data that would be removed.
- Support dry-run.
- Avoid deleting active .part files from live workers.
- Avoid deleting objects referenced by installed packages or history.
- Avoid deleting archives required by a pending/interrupted transaction.
- Avoid deleting the only valid rollback artifact.
- Avoid following symlinks out of cache roots.
- Use an exclusive or dedicated garbage-collection lock while deleting.
- Recheck references immediately before removal.
- Report retained objects and why they are retained.
- Never remove signing keys or trust-store contents.
- Never remove package database entries.

clean all must not mean "delete everything under /var/cache/nawa". It means
garbage-collect everything that is eligible under the documented retention
rules.

For package objects, use mark-and-sweep:

1. Mark all artifact objects referenced by installed package entries.
2. Mark all objects referenced by retained transaction records.
3. Mark objects referenced by legacy undo records.
4. Mark objects referenced by active transactions.
5. Mark objects pinned by another documented subsystem.
6. Sweep only objects that are not marked and are not active.

For build layers, the reference model must account for archives needed by
future closure builds and build-cache records. A layer must not be deleted
while a build is using it. Use a dedicated lock or an atomic lease/reference
mechanism if required.

History retention and cache retention must be coordinated. If history is later
pruned, the user must be told which historical undo operations will become
unavailable. The first release should not prune history automatically.

### 8.12 Phase 4 tests

Test:

- Legacy scalar repo configuration.
- New multi-repository configuration.
- Relative local repository paths.
- Duplicate IDs and invalid IDs.
- Disabled repositories are not queried or selected.
- Repository priority and tie-breaking are deterministic.
- Explicit repository filtering is honored.
- URL credentials are redacted in output.
- Trust and expiry policy applies to every repository.
- A failed refresh does not replace a valid cache.
- Invalid index signatures are rejected.
- File-list digest mismatch is rejected.
- Old clients safely ignore or reject the metadata extension as designed.
- Package-name query does not fetch archives.
- File-provider query uses authenticated file-list metadata.
- clean --dry-run removes nothing.
- clean packages retains installed and historical artifacts.
- clean all preserves active downloads and builds.
- A concurrent install cannot lose an artifact to garbage collection.

## 9. Phase 5 — Package groups

### 9.1 Purpose

Groups provide a convenient way to install a coherent set of packages without
changing the package archive format.

Examples:

- Development Tools.
- Desktop Environment.
- Multimedia.
- Network Utilities.
- System Administration.
- Virtualization Host.

A group is metadata that resolves to ordinary package names. Every package
still goes through the ordinary dependency planner, source selection, fetch,
signature verification, and installer.

A group is not a special package archive and must not bypass dependency
resolution.

### 9.2 Group definitions

Groups should be published in authenticated repository metadata or a signed
supplemental metadata artifact referenced by the signed index.

Illustrative definition:

~~~toml
schema = 1

[[group]]
id = "development"
name = "Development Tools"
description = "Compilers, build tools, and debugging utilities"

mandatory = [
  "gcc",
  "make"
]

default = [
  "clang",
  "cmake",
  "git"
]

optional = [
  "gdb",
  "ninja",
  "lldb"
]
~~~

Categories:

- Mandatory: required by the group definition.
- Default: installed by default when installing the group.
- Optional: available for explicit selection.

The first release may implement only a flat package list. If mandatory/default/
optional categories are included initially, their semantics must be documented
and tested. Do not introduce profile-like expressions or conditional package
selectors until the basic group model works.

A group definition must have:

- Stable group ID.
- Display name.
- Description.
- Schema version.
- Package lists.
- Optional metadata revision.
- Repository provenance.
- Digest/authentication reference through the signed repository metadata.

Validate every package name with the same rules used for package identities.
Duplicate package names within a group must be normalized or rejected.

### 9.3 Local group membership

The system must distinguish the reason a package is installed from the groups
that currently reference it.

Suggested storage:

~~~text
<root>/var/lib/nawa/groups/
├── schema-version
└── installed/
    ├── development.toml
    └── desktop.toml
~~~

Example membership record:

~~~toml
schema = 1
group_id = "development"
definition_revision = "..."
installed_at = 1793910000
source_repository = "faseela"

packages = [
  "gcc",
  "make",
  "clang",
  "cmake",
  "git"
]
~~~

The membership file records the package set requested by that group install.
The local group record must not become a second package database; package
identity and file ownership still come from local/<name>/desc and files.

The local membership record must be updated as part of the same logical
history transaction as package installation/removal. A crash between group
membership and package changes must be recoverable through the transaction
journal.

### 9.4 Install reasons and explicit user intent

Current package entries use reasons such as explicit and dependency.
Group support must not turn every group package into an ordinary explicit
package, because that would prevent removal when the group is removed.

Recommended semantics:

- explicit: the user explicitly requested the package.
- dependency: installed to satisfy another package.
- group: installed as a direct member of one or more installed groups.

Group membership is authoritative for group references. The entry's reason
is a summary of why the package remains installed, not the complete membership
database.

Rules:

1. Installing a group records the group membership.
2. A package newly installed only because it is a direct group member may use
   reason group.
3. Dependencies pulled in by group packages may use reason dependency.
4. A package that was already explicit remains explicit.
5. A package that was already needed by another installed package must not be
   removed merely because one group is removed.
6. A package referenced by two groups remains group-owned after one group is
   removed.
7. If a package is explicitly requested later, its reason must be promoted to
   explicit through a deliberate, tested rule.
8. Removing a group only removes its membership record first; it does not
   blindly remove all package names from the system.

The current planner's orphan logic recognizes existing reason categories and
dependency graphs. It must be extended deliberately so group packages are
treated as roots while at least one installed group references them. Do not
allow group-owned packages to be removed by autoremove while their group
membership exists.

### 9.5 Group commands

Initial CLI:

~~~text
nawa group list
nawa group info <GROUP>
nawa group install <GROUP>
nawa group remove <GROUP>
~~~

Useful later additions:

~~~text
nawa group install <GROUP> --with-optional
nawa group install <GROUP> --exclude <PACKAGE>
nawa group info <GROUP> --installed
~~~

Keep the first interface small. If optional package selection is supported,
the selected package set must be recorded in the local membership record so a
future group metadata change does not retroactively alter what the user chose.

group list should distinguish groups known in enabled repositories from
groups installed locally.

group info should show:

- Description.
- Repository/provenance.
- Mandatory/default/optional members.
- Installed membership state.
- Which packages are installed.
- Which are missing.
- Which are installed for another reason.

group install should:

1. Resolve the authenticated group definition.
2. Validate all package names.
3. Resolve package dependencies.
4. Choose repository/build sources using the normal policy.
5. Show the complete package plan.
6. Confirm.
7. Install packages in dependency order through the normal plan runner.
8. Commit group membership only with a recorded transaction result.
9. Preserve packages that were already installed and preserve their explicit
   reasons.

group remove should:

1. Load local membership, not a potentially changed remote definition.
2. Remove the group's membership from the candidate state.
3. Identify packages no longer owned by another group or explicit request.
4. Recompute dependency reachability from remaining explicit and group roots.
5. Propose only packages that are no longer required.
6. Order removals dependent-first.
7. Respect reverse dependencies and the normal force policy.
8. Show the full plan and ask for confirmation.
9. Record membership removal and package removals in one history transaction.

Do not use the latest remote group definition to decide what an already
installed group owns. The local membership record is the record of the user's
actual installation choice.

### 9.6 Group updates

Group definitions may change after installation. The first group release
should not silently add or remove packages when repository metadata changes.

A later group upgrade or group sync command may compare:

- Current local membership.
- Newly published group definition.
- Explicitly selected optional packages.
- Current explicit package reasons.
- Dependencies still needed by the system.

The user must see additions and removals before they are applied.

### 9.7 Group and autoremove interaction

autoremove must treat installed group members as requested roots.

Example:

~~~text
Group: desktop
  firefox
  pipewire

Group: multimedia
  pipewire
  ffmpeg
~~~

Removing desktop must not remove pipewire, because multimedia still
references it.

If firefox depends on a package shared with another explicit application,
that dependency must remain.

When a group is removed, packages no longer needed may become ordinary
dependency orphans. The group removal operation may propose removing them
immediately, or leave them for a later autoremove; the chosen policy must be
consistent and documented. The first implementation should prefer an explicit
group-removal plan that reports exactly which packages will be removed and
which will remain.

### 9.8 Phase 5 tests

Test:

- Group metadata signature and digest validation.
- Invalid and duplicate group IDs.
- Invalid package names.
- Group with missing package in all repositories and recipes.
- Group installation from repository.
- Group installation by source build.
- Group with mixed repository/build sources.
- Existing explicit package remains explicit.
- Existing dependency package is not needlessly reinstalled.
- Overlapping groups preserve shared packages.
- Removing a group preserves packages used by another group.
- Removing a group preserves explicit packages.
- Runtime dependents prevent unsafe removal.
- autoremove retains active group members.
- Group membership is recovered after interrupted operations.
- Group install/remove appears in transaction history.
- Group metadata update does not silently change local membership.
- Optional package selections persist locally.

## 10. Cross-cutting design rules

### 10.1 One package mutation path

All features must end at the same package mutation functions:

- install_archive and its verified variants.
- remove_with.
- The dependency planner and plan runner.
- The existing package-level rollback path, upgraded to use artifact proof.

Plugins, groups, repository adapters, and history undo must not implement their
own file extraction or package database writes.

### 10.2 Transaction manager boundary

A future transaction module may expose operations conceptually like:

~~~rust
pub fn begin_operation(...) -> Result<OperationHandle>;
pub fn record_step_intent(...) -> Result<()>;
pub fn record_step_commit(...) -> Result<()>;
pub fn finish_operation(...) -> Result<()>;
pub fn reconcile_incomplete(...) -> Result<()>;
pub fn plan_undo(...) -> Result<UndoPlan>;
pub fn apply_undo(...) -> Result<()>;
~~~

The real API must fit Nawa's existing error-handling and module conventions.

The transaction manager is not a replacement for the per-package Tx file
journal. They solve different problems:

- The existing Tx protects one package file operation while it is being
  applied.
- The transaction manager records the user's multi-package command and its
  package-level outcomes over time.

### 10.3 Error semantics

Every new command needs documented exit behavior.

At minimum distinguish:

- Invalid command/configuration.
- No matching package/group/repository.
- Authentication failure.
- Network failure.
- Package plan conflict.
- Package mutation failure.
- Partial transaction.
- Recovery required.
- Plugin failure.
- Cache cleanup partial failure.

Do not use success exit status for a command that claims completion while
required package steps failed.

### 10.4 Human and machine output

Human output should follow Nawa's existing terminal-aware color policy.

Machine output must be:

- Stable.
- Documented.
- Free of progress messages on stdout.
- Valid JSON or a clearly documented line protocol.
- Consistent about exit codes.

Do not make scripts parse decorative table formatting when a machine-readable
mode is available.

### 10.5 Logging and privacy

History is an audit record, not a place to dump every process detail.

Do not record:

- Signing private key contents or paths that reveal secret locations beyond
  what is strictly necessary.
- Authentication tokens.
- Proxy credentials.
- Full environment variables.
- Secrets embedded in repository URLs.
- Arbitrary shell command output by default.
- User file contents.

Record normalized package and repository identities, artifact hashes, safe
error summaries, and operation outcomes.

### 10.6 Offline bootstrap

Nawa is intended for from-scratch distributions and is built from vendored
dependencies for offline bootstrap.

Every phase must consider bootstrap constraints:

- Avoid introducing a large runtime dependency without a clear need.
- Prefer standard-library threads for the first parallel downloader.
- Keep plugin execution optional; the base package manager must work with no
  plugins installed.
- Keep repository/group features optional; source builds must remain usable
  without a configured binary repository.
- Add new crates only after checking MSRV, offline vendoring, musl support,
  static build behavior, and license compatibility.
- Update scripts/vendor-nawa.sh and vendor/ only when a dependency is
  intentionally approved.

### 10.7 Configuration migration

All new configuration settings must follow the existing precedence model:
command-line flag, environment variable, project config, system config, and
built-in default.

New settings must appear in:

- nawa init template.
- System configuration template.
- nawa config show.
- Documentation.
- Configuration unit tests.
- Distribution package defaults where applicable.

Do not silently change the meaning of existing jobs, repo, install_mode,
root, recipes, builddir, or profiles.

### 10.8 Lock ordering and concurrency

The existing database lock remains the primary lock for package state.

Any future object-store or garbage-collection lock must document lock order.
Avoid a cycle in which one process acquires the database lock and waits for a
cache lock while another process holds the cache lock and waits for the
database lock.

Network transfers and source builds should not hold the global database lock.
Garbage collection must coordinate with active downloads/builds through
explicit temporary-file ownership, leases, or dedicated locks.

### 10.9 Filesystem safety

All new path handling must preserve existing protections:

- Validate package and group IDs before using them in path components.
- Reject archive paths that escape staging.
- Avoid following cache symlinks.
- Use temporary files in the destination filesystem before rename.
- Do not write outside the configured root when operating on --root.
- Keep plugin executable resolution separate from package archive paths.
- Treat repository metadata and plugin responses as untrusted input until
  validated.

## 11. Suggested source layout after implementation

This is an organizational proposal, not a requirement to move existing files.

~~~text
src/
├── transaction/
│   ├── mod.rs
│   ├── context.rs
│   ├── history.rs
│   ├── journal.rs
│   ├── recovery.rs
│   └── undo.rs
├── plugins/
│   ├── mod.rs
│   ├── manifest.rs
│   ├── discovery.rs
│   ├── protocol.rs
│   ├── runner.rs
│   └── capabilities.rs
├── downloads/
│   ├── mod.rs
│   ├── pool.rs
│   ├── task.rs
│   └── progress.rs
├── repositories/
│   ├── mod.rs
│   ├── config.rs
│   ├── manager.rs
│   ├── cache.rs
│   ├── query.rs
│   └── metadata.rs
├── groups/
│   ├── mod.rs
│   ├── metadata.rs
│   ├── membership.rs
│   └── planner.rs
└── ...
~~~

Existing install.rs, upgrade.rs, repo.rs, fetch.rs, and
commands/installing.rs should remain in place initially. Extract modules only
when a concrete boundary requires it. Avoid a large file-move refactor mixed
with feature implementation.

## 12. CLI proposal summary

The intended command surface after all phases:

~~~text
# Existing package/build commands
nawa install <package>...
nawa update [package...]
nawa upgrade
nawa remove <package>
nawa autoremove
nawa build <package>
nawa rollback <package>

# Transaction history
nawa history
nawa history info <ID>
nawa history undo <ID>

# Plugins
nawa plugin list
nawa plugin info <ID>
nawa plugin enable <ID>
nawa plugin disable <ID>
nawa plugin run <ID> <COMMAND> [ARGS...]

# Repository administration
nawa repolist
nawa repoquery <query>
nawa makecache
nawa clean <metadata|packages|sources|layers|all>

# Groups
nawa group list
nawa group info <GROUP>
nawa group install <GROUP>
nawa group remove <GROUP>
~~~

Global root handling remains unchanged. Every command that touches package
state must use the same lock policy as the equivalent existing package
operation. Read-only commands should use shared locking where they read the
local database. Repository-only metadata queries may not need the package DB
lock unless they also inspect installed state.

## 13. Implementation sequencing and review boundaries

Each phase should be split into small pull requests. Do not merge all phases as
one branch.

### Phase 0 PR sequence

1. Timestamp unit fix and legacy-read compatibility.
2. Artifact metadata data types and serialization tests.
3. Immutable object-store implementation and unit tests.
4. New install path writes object references.
5. Detached-signature provenance.
6. Signed-index proof persistence.
7. Package rollback uses recorded artifact verification.
8. Documentation and migration notes.

### Phase 1 PR sequence

1. Transaction record schema and ID allocator.
2. Operation context passed through command handlers.
3. Write-ahead step intent integrated with installer commit.
4. History finalization and list/info CLI.
5. Interrupted transaction reconciliation.
6. Undo planning and conflict detection.
7. Undo executor using the existing package installer/remover.
8. History-aware object retention.
9. Full end-to-end regression tests.

### Phase 2 PR sequence

1. Manifest schema and validation.
2. Discovery and enable/disable configuration.
3. JSONL handshake and subprocess lifecycle.
4. Plugin list/info/run commands.
5. Command registration and collision policy.
6. Read-only history and transaction-observer events.
7. Capability broker and security review.
8. Repository adapter only after the core verification contract is complete.

### Phase 3 PR sequence

1. Download concurrency configuration.
2. Unique temporary download paths and safe object publication.
3. Bounded worker pool.
4. Parallel repository package prefetch.
5. Strict repository-mode preflight.
6. Auto-mode source-build fallback integration.
7. Metadata-download concurrency.
8. Progress reporting and benchmarks.

### Phase 4 PR sequence

1. Repository config structures and legacy scalar migration.
2. Repository manager and deterministic selection.
3. repolist.
4. makecache using existing authentication/freshness code.
5. Signed supplemental metadata extension.
6. repoquery package-name and info queries.
7. File/provides metadata and whatprovides.
8. Safe clean --dry-run.
9. Reference-aware package-object garbage collection.
10. Source/layer cleanup only after their references are defined.

### Phase 5 PR sequence

1. Group metadata schema and validation.
2. Signed group metadata loading.
3. Local group membership database.
4. group list and group info.
5. Group install using the existing dependency planner.
6. Group-aware install reason and autoremove logic.
7. Group removal planning and execution.
8. History/recovery integration.
9. Optional members and group definition updates, if approved.

Each PR should include:

- A focused description.
- Relevant unit tests.
- Relevant integration tests.
- Documentation for user-visible behavior.
- No unrelated recipe updates.
- No unrelated bootstrap or workflow changes.
- No unreviewed format changes.
- CI result and any platform-specific limitations.

## 14. Acceptance criteria for the complete roadmap

The roadmap is complete only when all of the following are true.

### Transaction history

- Every package-changing command creates a transaction record.
- Package-level outcomes are recorded accurately.
- Partial and interrupted operations are distinguishable.
- Historical undo preflights all required artifacts and state conflicts.
- Undo creates a new transaction.
- Superseded package changes are not overwritten blindly.
- Legacy package rollback remains supported.

### Plugins

- Plugins have a versioned manifest and protocol.
- Disabled plugins do not execute.
- Incompatible plugins are rejected.
- Plugin crashes and malformed messages cannot crash core Nawa.
- Privileged capability use is explicit and restricted.
- Plugins cannot bypass archive or repository verification.

### Downloads

- Configured concurrency is bounded.
- Retries, backoff, mirror fallback, and SHA-256 verification remain intact.
- Strict repository mode verifies the full required archive set before mutation.
- Package installation remains dependency ordered.
- Concurrent downloads cannot corrupt or overwrite cache objects.

### Repositories

- Existing single-repository configuration remains valid.
- Multiple repositories are deterministic and inspectable.
- repolist, repoquery, makecache, and clean have documented behavior.
- Supplemental metadata is authenticated.
- File-provider queries do not require downloading all package archives.
- Cache cleanup retains installed and historically referenced artifacts.

### Groups

- Group metadata is authenticated.
- Group membership is stored locally.
- Explicit user installs are preserved.
- Overlapping groups are handled correctly.
- Group removal respects dependencies.
- autoremove preserves active group members.
- Group operations are visible in history.

### Overall

- Existing source builds continue to work without repositories or plugins.
- Existing archive format remains readable.
- Existing signing format remains valid.
- Existing build isolation behavior is unchanged.
- Existing configuration remains readable.
- Offline vendored builds remain supported.
- The normal test suite and Clippy pass.
- Static musl release builds still work.
- No phase is considered complete based only on a successful happy-path test.

## 15. Risks and decisions that must be resolved

These points require explicit review before implementation.

### 15.1 Historical trust and key revocation

If a publisher key is removed or revoked after an archive was installed, should
historical undo still be allowed using the archived original proof?

The implementation must distinguish ordinary trust-store maintenance from
explicit key compromise/revocation. The policy must be documented. A stored
public key must never silently override an explicit revocation.

### 15.2 Root filesystem side effects

Package hooks may change state outside the package file list. History can
record that hooks ran and whether they succeeded, but it cannot infer or undo
arbitrary side effects. The user interface must not promise full-system
snapshot restoration.

### 15.3 Multi-process operations

Because builds and downloads occur outside the DB lock, another process can
change a package after a plan was calculated. Every package step must recheck
preconditions before commit. A stale plan must stop safely.

### 15.4 Group membership versus install reason

The local group membership database is the source of truth for group
references. The package entry reason remains useful for why, list, and
autoremove, but it cannot encode all overlapping ownership by itself.

### 15.5 Dynamic CLI compatibility

Dynamic plugin commands need an explicit solution with Clap. Do not implement
command injection by parsing a shell string or allowing plugin commands to
shadow built-in commands.

### 15.6 Repository metadata compatibility

Before extending current indexes, test the actual behavior of old clients
against the new optional fields. If old clients cannot safely ignore them,
publish a new schema or separate metadata channel. Never assume compatibility
from serde defaults alone.

### 15.7 Cache retention

The more history is retained, the more rollback artifacts must remain
available. Cleanup and history pruning need a single reference model. A cache
command must not silently invalidate an undo operation that Nawa still claims
is available.

### 15.8 Dependency footprint

A new crate may affect offline vendoring, MSRV, musl linking, and the static
binary. Prefer existing dependencies or the standard library when they are
sufficient. Any new dependency requires a separate justification and CI check.

## 16. Explicit out-of-scope items for the first implementation cycle

The following items may be useful later, but should not be added to the first
implementation cycle:

- Full filesystem snapshots or Btrfs/ZFS snapshot orchestration.
- Automatic rollback of arbitrary package hook side effects.
- Automatic plugin download/install from an untrusted plugin marketplace.
- In-process Rust dynamic-library plugins.
- Arbitrary plugin access to root privileges.
- A new asynchronous runtime without a measured requirement.
- Repository federation or peer-to-peer package distribution.
- Complex group conditions based on hardware or arbitrary expressions.
- Automatic group upgrades without a reviewable plan.
- Automatic pruning of history.
- Automatic removal of every old artifact not currently installed.
- Changes to the .nawa.tar.zst payload format.
- Changes to the Ed25519 signature message format.
- A new build system or recipe language.

## 17. Final design position

Nawa should become more capable by composing its existing strong systems, not
by replacing them with a broad framework.

The recommended implementation order is:

1. **Phase 0:** make package artifact identity and provenance durable, correct
   timestamp handling, and repair historical package-level restoration.
2. **Phase 1:** add a transaction journal and safe compensating undo.
3. **Phase 2:** add an out-of-process, versioned, capability-limited plugin
   interface.
4. **Phase 3:** add bounded parallel downloads while keeping ordered installs.
5. **Phase 4:** add multiple repositories, authenticated metadata queries,
   explicit cache refresh, and reference-aware cleanup.
6. **Phase 5:** add signed package groups and group-aware ownership.

The core rule is unchanged throughout: source builds, repository packages,
plugins, groups, and historical undo all converge on the same verified package
installation/removal path.

This document is a design proposal only. Implementation should begin with
Phase 0 as small, independently tested changes. No later phase should be
started by weakening the security or compatibility guarantees established by
the earlier phases.
