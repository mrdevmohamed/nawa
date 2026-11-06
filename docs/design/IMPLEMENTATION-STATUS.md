# Roadmap implementation status

Committing the implementation of `docs/design/package-management-roadmap.md`
against Nawa 1.0.0, phase by phase. Each phase builds, tests green, clippy
clean at the time it landed.

## Phase 0 — Shared foundations and security corrections ✅
- `install_date` is now written as Unix SECONDS; conservative one-shot
  migration of legacy nanosecond values (`desc.pre-seconds.bak` backups,
  idempotent) runs on exclusive-lock commands.
- Temporary directory names use their own nanosecond+pid uniqueness,
  decoupled from the recorded install date.
- `src/artifact.rs`: immutable content-addressed object store
  (`var/cache/nawa/objects/sha256/<xx>/<digest>.nawa.tar.zst`) with no-clobber
  hardlink publication, digest validation, symlink refusal, duplicate-race
  verification; versioned provenance records (`*.artifact.toml`) with four
  authentication kinds (signed-index / detached-signature / unsigned-bootstrap
  / local-build); proof bundles retain the detached signature or the exact
  index bytes + signature + signer key; legacy cache import after verify.
- `db::Entry.artifact_sha256` (optional, serde-defaulted, legacy shape
  preserved on write).
- `nawa rollback` re-verifies the exact cached bytes (digest + proof) and
  refuses corrupted/missing objects; bootstrap artifacts are never reported
  as authenticated.
- Tests: object store immutability/races, provenance roundtrip, timestamp
  migration, verified rollback (corrupt/missing/ping-pong), signed-index
  proof retention end-to-end.

## Phase 1 — Transaction history and historical undo ✅
- `src/transaction/`: immutable TOML transaction records
  (`var/lib/nawa/history/transactions/<id>.toml`, schema 1, monotonic ids
  under the existing db lock), write-ahead step-intent journal, crash
  reconciliation (committed / failed / needs-recovery decided by comparing
  intents with the db; never re-runs hooks), and compensating undo.
- Every package-changing command (install / update / upgrade / remove /
  autoremove / build --install / archive install) opens one operation and
  records each package step around the real installer call.
- `nawa history` (newest-first table), `nawa history info <ID>`,
  `nawa history undo <ID> [--yes]`.
- Undo preflights EVERY required artifact and state conflict (identity +
  artifact digest) before the first mutation, refuses superseded packages,
  creates a new linked transaction (`undone_by`/`undoes`).
- Statuses: preparing/running/completed/partial/failed/interrupted/
  needs-recovery/undone.
- Fixed a latent deadlock (autoremove re-locking the db it already holds).

## Phase 2 — Plugin system ✅
- `src/plugins/`: manifests (deny-unknown-fields, conservative ids, absolute
  non-symlink executables inside the approved dir, world-writable refusal,
  API-version gate, built-in command shadowing refused), discovery
  (/usr/lib, /usr/local/lib, $NAWA_PLUGIN_PATH; user dirs never executed by
  root; duplicate ids reported), JSON Lines protocol (request ids, 1 MiB
  bound, UTF-8/envelope validation), one-shot runner (clean environment —
  no secrets/keys/NAWA_*, bounded timeout, crash/hang/garbage isolation).
- `nawa plugin list|info|enable|disable|run`; state persisted in the
  `[plugins]` table of nawa.toml (other keys preserved byte-for-byte).
- New dependency: serde_json (pure Rust, musl-safe).

## Phase 3 — Parallel downloads (configuration layer) ◐
- `downloads` setting implemented end-to-end: `--` precedence flag >
  NAWA_DOWNLOADS > nawa.toml > default 4, clamped 1..=64, zero rejected.
- NOT yet implemented: the bounded worker pool itself and strict-mode
  prefetch (the roadmap's §7.3–7.5 runtime work). The synchronous downloader
  remains authoritative; no behavior regressed.

## Phase 4 — Repository management ✅
- `[[repositories]]` multi-repo configuration with validated ids and
  deterministic legacy migration (explicit entries win, warning emitted).
- `nawa repolist [--all]` (configuration + cached state, never downloads),
  `nawa makecache` (refresh through the existing authentication/freshness
  path; a failed refresh never replaces a valid cache),
  `nawa repoquery <name> [--repo-id ID] [--info]` (cached-metadata queries,
  never downloads archives, documented nonzero exit when nothing matches),
  `nawa clean <metadata|packages|sources|layers|all> [-n]` with mark-and-sweep
  retention (installed entries + history records + legacy undo) and
  live-worker `.part` protection.

## Phase 5 — Package groups ◐
- Group definition schema + validation (`src/groups/mod.rs`) and the local
  membership store (`var/lib/nawa/groups/installed/<id>.toml`, atomic
  writes, path-safety) are implemented and unit-tested.
- `nawa group list|info|remove` work against the membership store; group
  remove recomputes ownership from remaining group/explicit roots, orders
  removals dependents-first, rechecks reverse dependencies, and records one
  history transaction.
- `nawa group install` refuses until signed group metadata is published in
  a repository (the documented prerequisite from §9.2) instead of guessing
  from unsigned data.

## Test totals
283 unit tests + integration suites (by_name, repo_install, e2e, lock,
sandbox, isolation, plugins, ...) green at the final commit.
