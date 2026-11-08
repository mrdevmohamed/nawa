# Command reference

Run nawa --help or nawa COMMAND --help for the exact options supported by the installed binary. Global options can be placed before or after the subcommand.

## Global options

| Option | Purpose |
|---|---|
| --root PATH | Target root filesystem. Defaults to /. |
| --recipes PATH | Recipe tree. Defaults to recipes. |
| --builddir PATH | Scratch and build directory. Defaults to build. |
| --profiles PATH | Architecture-specific profile directory. Defaults to profiles. |
| --jobs N | Build parallelism; otherwise use the profile setting or CPU count. |

The corresponding environment variables and configuration-file settings are documented in [Configuration](configuration.md).

## Build and install

| Command | Purpose |
|---|---|
| nawa build NAME | Build a package from its recipe. |
| nawa install NAME... | Install packages by name, resolving dependencies by default. |
| nawa install ARCHIVE... | Install one or more .nawa.tar.zst archives. Do not mix names and archives in one invocation. |
| nawa update [NAME...] | Update installed packages whose recipe versions are newer. Defaults to building from source. |
| nawa outdated | List installed packages with newer local recipes. |
| nawa upgrade | Upgrade installed packages from a configured binary repository. |
| nawa remove NAME | Remove an installed package. |
| nawa autoremove | Remove dependencies that are no longer needed. |
| nawa rollback NAME | Restore a package version recorded in its rollback state. |

Useful build options:

- --check: run the recipe's check section; failure stops the build.
- --check-nonfatal: run checks but report failures as warnings.
- --isolation LEVEL: select none, env, sandbox, or closure.
- --offline: refuse network access and use verified cached inputs only.
- --use-cache: reuse a matching package archive when supported by the isolation level.
- --rebuild-check: build twice in separate work directories and compare archive hashes.
- --install: install the archive produced by nawa build.
- --allow-network: permit a recipe that explicitly requests build-time network access.
- --allow-degraded: permit a build to continue when the host cannot provide a requested isolation feature.

Useful install/update options:

- --dry-run or -n: display the plan without building or installing.
- --yes or -y: suppress confirmation prompts.
- --no-deps: install only the named packages, without dependency resolution.
- --from auto|repo|build|ask: choose the package source.
- --build: force source builds.
- --from-repo: require packages from the binary repository.
- --repo DIR_OR_URL: override the repository for the command.
- --force: allow replacement of files owned by another package where supported.
- --allow-unsigned: relax signature requirements only under the command's documented trust policy.

## Query and dependency inspection

| Command | Purpose |
|---|---|
| nawa search TERM | Search recipe names and summaries. |
| nawa info NAME | Show recipe details and installed state. |
| nawa query [PATTERN] | List installed packages; nawa list is an alias. |
| nawa query --explicit | Show explicitly installed packages. |
| nawa query --deps | Show packages installed as dependencies. |
| nawa deps NAME | Print dependency order for one recipe. |
| nawa deps-all | Print the global build order for the recipe tree. |
| nawa why NAME | Show installed reverse dependencies. |
| nawa why --tree NAME | Also scan recipe dependents. |
| nawa files NAME | List files owned by an installed package. |
| nawa owner PATH | Find the installed package that owns a file. |
| nawa revdep | Scan installed ELF files for missing libraries and interpreters. |

For machine-readable reverse-dependency output, use nawa revdep --json. The command also accepts --lib-dir, --ignore, --unowned, and repeated --scan-dir options.

## Sources and recipes

| Command | Purpose |
|---|---|
| nawa fetch NAME... | Download and verify declared sources and run declared ecosystem fetch backends. |
| nawa fetch --all | Fetch every recipe with a remote source or fetch declaration. |
| nawa fetch --sources-only NAME... | Fetch source artifacts without running Go/Cargo fetch backends. |
| nawa recipe check NAME | Run local recipe checks. |
| nawa recipe check --all | Check all recipes in the configured tree. |

See [Build and fetch](build-and-fetch.md) for offline workflows.

## Repository and signing commands

| Command | Purpose |
|---|---|
| nawa repolist [--all] | List configured repositories and their enabled state. |
| nawa makecache | Refresh repository metadata through signature and freshness checks. |
| nawa repoquery NAME [--info] | Query cached repository metadata without downloading an archive. |
| nawa repo-create DIR | Generate a repository index from package archives. |
| nawa repo-update DIR | Incrementally update a signed repository index. |
| nawa sync | Download the configured repository's recipe snapshot after verification. |
| nawa keygen | Generate an Ed25519 signing key pair. |
| nawa sign FILE | Sign a package archive or repository index. |
| nawa verify FILE | Verify a package archive or repository index signature. |
| nawa trust KEYFILE | Add a public key to the target root's trust store. |
| nawa clean CATEGORY | Remove selected cache data; categories are metadata, packages, sources, layers, and all. |

Repository creation has schema-specific requirements. Index schema v2 and later must be signed; schema v3 also requires a locations manifest. Read [Repositories and security](repositories-and-security.md) before publishing an index.

## System and maintenance commands

| Command | Purpose |
|---|---|
| nawa history | List recent package transactions. |
| nawa history info ID | Show a transaction record. |
| nawa history undo ID | Create a compensating transaction for a prior transaction. |
| nawa service list | List available runit services and enabled state. |
| nawa service status [NAME] | Show static and, when available, live service status. |
| nawa service enable NAME [--now] | Enable a runit service; optionally start it immediately. |
| nawa service disable NAME [--now] | Disable a service; optionally stop it first. |
| nawa service start|stop|restart NAME | Control an enabled service. |
| nawa plugin list | List discovered plugins. |
| nawa plugin info ID | Show plugin manifest details. |
| nawa plugin enable|disable ID | Change plugin state. |
| nawa plugin grant|revoke ID CAPABILITY | Change an administrator capability grant. |
| nawa plugin run ID COMMAND [ARGS...] | Run a plugin command. |
| nawa group list | List installed groups. |
| nawa group info ID | Show a group's recorded membership. |
| nawa group remove ID | Remove group membership and packages no longer needed. |
| nawa config show | Show effective configuration and where each value came from. |
| nawa init | Write a project configuration template. |
| nawa init --system | Write the system-wide configuration template. |
| nawa completions bash|zsh|fish | Print shell completion scripts. |

**Group installation status:** the command exists, but currently refuses to install a group until signed group metadata is published by a repository. See the [implementation status](design/IMPLEMENTATION-STATUS.md).

## Common examples

Preview an install plan:

~~~sh
nawa install --dry-run curl
~~~

Build and install from source:

~~~sh
nawa build --install --check curl
~~~

Require a repository package:

~~~sh
nawa install --from repo curl
~~~

Update selected packages from the recipe tree:

~~~sh
nawa update curl openssl
~~~

Check all installed packages for broken ELF dependencies:

~~~sh
nawa revdep
~~~
