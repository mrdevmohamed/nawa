# Nawa Plugin API v1

This document describes the enforced plugin API v1 behavior. Plugins remain
separate executables; they are never loaded into the Nawa process as dynamic
Rust libraries.

## Manifest

A plugin directory contains nawa-plugin.toml and its executable:

    [plugin]
    id = "audit"
    name = "Nawa audit plugin"
    version = "1.0.0"
    api_version = "1"
    executable = "/usr/lib/nawa/plugins/audit/nawa-plugin-audit"

    [commands]
    names = ["audit-report"]

    [capabilities]
    requested = ["command.register", "transaction.observe", "history.read",
      "repository.metadata.read", "package.plan.inspect"]

    [events]
    subscribe = ["transaction.completed", "transaction.partial",
      "package.installed", "package.upgraded", "package.removed"]

Unknown fields, unsupported event names, duplicate command names, reserved
built-in command names, invalid API versions, executable symlinks, and
executables outside the plugin directory are rejected.

## Administrator grants

The manifest only requests permissions. It never grants them. Actual grants
are read only from a secure system configuration file (/etc/nawa/nawa.toml
or the directory selected by NAWA_SYSCONFDIR):

    [plugins]
    enabled = ["audit"]

    [plugins.grants]
    audit = ["command.register", "transaction.observe", "history.read",
      "repository.metadata.read", "package.plan.inspect"]

Project configuration may enable or disable a plugin, but cannot grant
capabilities. A system allow-list and system disable always take precedence
over project preferences. Administrator grants are accepted only when the
system config and every parent directory are root-owned and not writable by
group or other users.

Commands:

    sudo nawa plugin grant audit command.register
    sudo nawa plugin grant audit transaction.observe
    sudo nawa plugin grant audit history.read
    sudo nawa plugin info audit
    sudo nawa plugin revoke audit transaction.observe

notification.send remains reserved and cannot be granted in API v1 because
there is no notification broker endpoint yet.

Enforced v1 meanings:

- command.register allows manifest commands to be dispatched as top-level
  nawa COMMAND [ARGS...]. Ambiguous duplicate registrations are rejected.
  Without this grant, use nawa plugin run ID COMMAND [ARGS...].
- transaction.observe is required before subscribed transaction events are sent.
- history.read adds timestamps, initiator uid, and undo linkage to event payloads.
- repository.metadata.read adds repository identifiers associated with a transaction.
- package.plan.inspect adds requested package names and committed package changes.

The host intersects administrator grants with capabilities requested by the
manifest and sends the effective list in the hello request. No plugin
capability grants write access to the Nawa package database, signing keys,
archive caches, or target root.

## Transaction events

Nawa writes a finalized transaction record to a filesystem outbox while the
database lock is held. It drains the outbox only after the command returns
and the database lock has been released. API v1 event names are:

- transaction.completed
- transaction.failed
- transaction.partial
- package.installed
- package.upgraded
- package.removed

Events use JSON Lines with method = event and the same response envelope and
request-id validation as hello and run. Subscribers must also declare
transaction.observe in their manifest. Payload fields are redacted according
to the effective grants.

Observer failure never rolls back an installed package. Failures are logged
as warnings and stored under <root>/var/lib/nawa/plugins/failures/. A malformed
outbox record remains available for diagnosis instead of being silently deleted.

## Process sandbox

Every plugin invocation runs through the existing Nawa namespace sandbox.
There is no fallback to direct execution when a kernel feature is unavailable.
The sandbox uses user, mount, PID, IPC, UTS, and network namespaces; a private
mount tree; no external network; a read-only root including separate
sub-mounts; masked Nawa state, target-root paths, home directories, session
sockets, repository caches, and common credential locations; and a private
writable /tmp and temporary plugin HOME. The private /dev comes from existing
sandbox device setup.

When Nawa runs as root, the plugin drops supplementary groups and switches
to subordinate uid/gid 65534 before exec. Every plugin gets NO_NEW_PRIVS, an
empty capability set, a 30-second timeout, and resource limits (1 GiB address
space, 32 processes, 64 MiB per file, and 64 open file descriptors). Standard
output is reserved for JSONL; stderr is drained continuously and only a
bounded diagnostic tail is retained.

The existing build sandbox remains unchanged by default: its optional uid,
resource, and strict read-only fields default off. Plugins fail closed if
namespace, mount, identity, or resource restrictions cannot be established.

This is a defense-in-depth Linux process sandbox, not a formal guarantee
against kernel vulnerabilities or every side channel. API v1 does not provide
a seccomp syscall allow-list or a network capability.