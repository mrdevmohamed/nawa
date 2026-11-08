# Getting started

This guide takes you from a Nawa release binary to a first package operation.

## 1. Install Nawa

Download the appropriate release asset from [GitHub Releases](https://github.com/mrdevmohamed/nawa/releases).

The project currently publishes a statically linked **x86_64 Linux musl** binary. Verify the downloaded file with the checksum published beside it:

~~~sh
sha256sum -c nawa-1.0.0-x86_64-linux-musl.sha256
~~~

Use the exact asset name shown on the release page; the example uses the current naming pattern.

Place the binary somewhere in your PATH, for example:

~~~sh
install -Dm755 nawa-1.0.0-x86_64-linux-musl ~/.local/bin/nawa
~~~

Check that it runs:

~~~sh
nawa --version
nawa --help
~~~

## 2. Prepare a recipe tree

Nawa is a package manager and build engine, not a recipe repository. It expects a directory containing one subdirectory per package, with a recipe.toml file in each package directory.

~~~text
recipes/
├── hello/
│   └── recipe.toml
└── zlib/
    └── recipe.toml
~~~

The --recipes option selects the tree for one invocation. You can also set it in nawa.toml or with NAWA_RECIPES.

## 3. Create a configuration file

In the directory where you want to work:

~~~sh
nawa init
~~~

This writes a commented nawa.toml template. Edit only the settings you need, then inspect the resolved values:

~~~sh
nawa config show
~~~

The [configuration guide](configuration.md) explains precedence, paths, repositories, and environment variables.

## 4. Inspect recipes and packages

~~~sh
nawa search hello
nawa info hello
nawa recipe check hello
nawa deps hello
~~~

search and info inspect the configured recipe tree. recipe check performs local, source-independent checks and reports warnings. deps prints the dependency resolution order.

## 5. Build or install a package

Build from a recipe:

~~~sh
nawa build hello
~~~

Preview an installation plan without changing the target root:

~~~sh
nawa install --dry-run hello
~~~

Install by name using the configured source policy:

~~~sh
nawa install hello
~~~

The default auto install mode prefers a repository package when it is suitable for the machine and at least as new as the local recipe; otherwise it builds from the recipe. Use --build to force a source build or --from-repo to require a repository package.

Package-changing operations normally ask for confirmation. Use --yes for deliberate non-interactive operation.

## 6. Test against a separate root

Use --root to target a disposable root filesystem instead of the live system:

~~~sh
mkdir -p /tmp/nawa-root
nawa --root /tmp/nawa-root install --dry-run hello
~~~

Do not experiment on a production root. Read the command reference before running operations that replace files, remove packages, or change services.

## Next steps

- [Command reference](commands.md)
- [Configuration](configuration.md)
- [Recipe format](recipes.md)
- [Build isolation and fetching](build-and-fetch.md)
