# Package recipes

Nawa reads package definitions from a recipe tree supplied by the distribution or administrator. It does not include a recipe collection.

## Directory layout

Each package lives in its own directory and has a recipe.toml file:

~~~text
recipes/
└── hello/
    ├── recipe.toml
    └── optional-local-assets/
~~~

## Minimal legacy recipe

The following is an illustrative template. Replace the source URL, checksum, source directory, and build commands with values appropriate for the upstream project.

~~~toml
[package]
name = "hello"
version = "1.0"
rel = 1
arch = ["x86_64"]
summary = "Example package"
license = ["MIT"]
source = "https://example.org/hello-1.0.tar.xz"
sha256 = "REPLACE_WITH_64_LOWERCASE_HEX_CHARACTERS"
source_dir = "hello-1.0"

[depends]
build = ["base-devel"]
run = []
check = []

[build]
prepare = ""
configure = "./configure --prefix=/usr"
compile = "make -j%{jobs}"
check = "make check"
install = "make DESTDIR=%{pkgdir} install"

[hooks]
post_install = ""
pre_remove = ""
post_remove = ""
~~~

The checksum shown is intentionally a placeholder and is not accepted by Nawa. Downloaded source archives must have a valid pinned SHA-256 digest.

## Package metadata

The [package] table supports:

| Key | Meaning |
|---|---|
| name | Package name. |
| version | Upstream/package version. |
| rel | Distribution release number; defaults to 1. |
| arch | Supported architecture names. |
| summary | Short package description. |
| license | License identifiers. |
| source | Source URL, local:<path>, or empty for a package without a downloadable archive. |
| sha256 | Pinned source archive digest. Required for downloadable legacy sources. |
| mirrors | Fallback URLs whose contents must match the same SHA-256 digest. |
| source_dir | Directory inside the extracted source archive. |
| source_date_epoch | Optional fixed timestamp for reproducible archive metadata. |

A recipe with an empty or local source may be appropriate for a meta-package or a package whose build steps operate on locally available inputs. New-schema recipes have additional timestamp requirements; see the fetch-phase design.

## Dependencies

[depends] separates dependency purposes:

- build: packages required to prepare and compile the package.
- run: dependencies required by the installed package.
- check: packages required only when the recipe's test/check section is run.

At the closure isolation level, build dependencies also define what is made available inside the isolated build root, beyond the base build toolchain.

## Build steps

The [build] table has five shell-script sections. Empty sections are skipped.

| Section | Typical use |
|---|---|
| prepare | Apply patches or prepare generated files. |
| configure | Configure the upstream build system. |
| compile | Compile the source. |
| check | Run upstream tests when requested. |
| install | Install staged files into the package directory. |

Use Nawa's recipe placeholders for paths and build settings. The commonly used placeholders in the example are %{jobs} and %{pkgdir}. Do not assume arbitrary placeholders are expanded; nawa recipe check warns about unknown placeholders.

## Hooks

The [hooks] table supports post_install, pre_remove, and post_remove. Hooks execute as part of package operations. Keep them small, predictable, and safe to retry. Package transaction records distinguish a committed package step from a hook failure.

## Recipe-local assets

Use [assets] to include files stored beside the recipe, such as patches or configuration fragments:

~~~toml
[assets]
include = ["patches/fix-build.patch", "config/hello.conf"]
~~~

Assets are snapshotted into a read-only assets directory and made available as %{assetsdir}. Asset paths are relative to the recipe directory and may not escape it.

Using [assets] activates the new recipe schema. Add the required schema marker to package.source as described below.

## Multiple sources and ecosystem fetches

The new schema supports:

- [[package.sources]] for multiple pinned source artifacts.
- [fetch.go] for Go Modules.
- [fetch.cargo] for Cargo dependencies.
- package.workdir to select the source workspace directory.

New-schema recipes must set:

~~~toml
[package]
source = "local:/nawa-schema-2-requires-a-newer-nawa"
~~~

This is a fail-closed compatibility marker, not a real local path. Do not combine the new [[package.sources]] form with legacy source, sha256, mirrors, or source_dir fields. Each new source has a unique name, URL, and SHA-256 digest.

For exact fields, validation rules, cache identity, and offline behavior, read [Fetch phase design](design/fetch-phase.md).

## Validate recipes

Run checks without downloading source archives:

~~~sh
nawa recipe check hello
nawa recipe check --all
~~~

The checker reports warnings for issues such as missing declared assets, unknown tables or keys, unknown placeholders, and recipe-side Go/Cargo download commands. Treat warnings as actionable review items before publishing recipes.
