# Transactions, services, and plugins

This page covers package lifecycle and system integration commands.

## Package transaction history

Nawa records package-changing operations in the target root:

~~~text
/var/lib/nawa/history/transactions/
~~~

Each transaction records its command, status, timing, requested packages, and package-level changes. The journal supports recovery after interrupted operations; Nawa does not blindly repeat package hooks during recovery.

List recent operations:

~~~sh
nawa history
~~~

Inspect a transaction:

~~~sh
nawa history info 42
~~~

Undo a transaction:

~~~sh
nawa history undo 42
~~~

Undo is a new compensating transaction, not deletion of the original history record. Nawa checks that the current package state has not been superseded and verifies the artifacts needed for the operation before changing the system. An undo can be refused when its preconditions are no longer true.

The separate nawa rollback NAME command restores a package version recorded by its package rollback state.

## runit services

Nawa's service integration targets **runit**. It reads service definitions from:

~~~text
/etc/sv/
/etc/runit/runsvdir/default/
~~~

List available services and their enabled state:

~~~sh
nawa service list
nawa service status
~~~

Enable and immediately start a service:

~~~sh
nawa service enable dbus --now
~~~

Stop a service but leave it enabled for the next supervision start:

~~~sh
nawa service stop dbus
~~~

Disable and stop a service:

~~~sh
nawa service disable dbus --now
~~~

The --root option scopes service paths to another root filesystem. Live status is available when the sv command is present and the service is actually supervised; otherwise Nawa reports the static enabled state.

## Reverse shared-library dependencies

Use nawa revdep to inspect installed ELF files for missing shared libraries, wrong-architecture libraries, missing interpreters, and broken symbolic links.

~~~sh
nawa revdep
nawa revdep --json
nawa revdep --unowned --scan-dir /opt
~~~

The scanner considers each file's RPATH/RUNPATH, the target root's configured library directories, musl path files, and standard library directories. Use --lib-dir DIR to add an earlier search directory.

## Plugins

Plugins are separate executables discovered in configured plugin locations, including system plugin directories and NAWA_PLUGIN_PATH. Nawa reads plugin manifests and communicates with plugin processes using a bounded JSON Lines protocol.

Common commands:

~~~sh
nawa plugin list
nawa plugin info example
nawa plugin enable example
nawa plugin disable example
nawa plugin run example command --argument
~~~

Capability grants and revocations are administrator policy operations:

~~~sh
nawa plugin grant example capability
nawa plugin revoke example capability
~~~

Plugins run out of process with a cleaned environment and bounded execution time. Review plugin executables and manifests before enabling them. See [Plugin API v1](design/plugin-api-v1.md) for the protocol and manifest contract.

## Package groups

The current implementation supports listing, inspecting, and removing installed group membership:

~~~sh
nawa group list
nawa group info desktop
nawa group remove desktop
~~~

Group schema and local membership storage are implemented. **Group installation is not yet available**: Nawa refuses it until signed group metadata is published in a repository. Check the [implementation status](design/IMPLEMENTATION-STATUS.md) for the latest state.
