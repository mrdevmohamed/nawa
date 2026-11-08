# Build and fetch

Nawa separates source acquisition from package compilation. This makes it possible to verify and cache source inputs first, then run the build without network access.

## Build a package

~~~sh
nawa build hello
~~~

A build reads the recipe, resolves build dependencies, prepares the declared inputs, executes the configured build sections, stages installed files, and creates a .nawa.tar.zst package archive.

Run the recipe's test section and fail the build if it fails:

~~~sh
nawa build --check hello
~~~

Use --check-nonfatal when test failures should be reported without failing the package build.

## Build isolation levels

Nawa supports four isolation levels:

| Level | Summary |
|---|---|
| none | Build without an additional isolation environment. |
| env | Use a controlled build environment. |
| sandbox | Run build sections in a filesystem/process sandbox. |
| closure | Assemble a build root from the recipe's declared dependency closure. |

Select the level for one build:

~~~sh
nawa build --isolation sandbox hello
~~~

A default can be set in the architecture profile's [build] table. See [Build isolation](build-isolation.md) for the detailed behavior, filesystem model, and limitations.

Isolation depends on host kernel features. Nawa fails rather than silently weakening a requested isolation level. --allow-degraded explicitly permits continuing when a required host feature is unavailable; use it only when you understand the reduced isolation.

## Fetch sources before building

Fetch and verify sources for selected recipes:

~~~sh
nawa fetch hello
~~~

Fetch every recipe with a remote source or ecosystem fetch declaration:

~~~sh
nawa fetch --all
~~~

Use --sources-only to download and verify declared source artifacts without running Go or Cargo dependency fetch backends:

~~~sh
nawa fetch --all --sources-only
~~~

Nawa verifies downloaded source files against declared SHA-256 digests. Recipe-local assets are staged from the recipe tree and are covered by the recipe snapshot.

## Go and Cargo dependencies

Recipes can declare [fetch.go] and/or [fetch.cargo] to let Nawa prepare ecosystem dependencies before compilation. The fetch phase uses the ecosystem lockfile inputs, stores fetched dependencies in Nawa's cache, and allows the following build to run offline.

The toolchain dependencies are implicit for these fetch backends: golang for Go and rust for Cargo. The toolchain recipes themselves must not declare their own matching fetch table.

Do not run go mod download or cargo fetch manually in a normal build section when Nawa's [fetch.*] backend is responsible for that work. See [Fetch phase design](design/fetch-phase.md) for the complete schema and compatibility contract.

## Offline build workflow

A practical CI sequence is:

~~~sh
# Online preparation step
nawa fetch --all

# Offline build step
nawa build --offline hello
~~~

For a stricter preflight, Nawa's offline fetch mode verifies that the required cache entries already exist and refuses downloads:

~~~sh
nawa fetch --all --offline
~~~

An offline build fails when a required source or ecosystem cache entry is missing. Run the online fetch step first.

## Build profiles

Nawa loads an optional profile from profiles/<arch>.toml. A profile can set:

~~~toml
[profile]
arch = "x86_64"
cflags = "-O2 -pipe"
cxxflags = "-O2 -pipe"
ldflags = ""
jobs = 4

[build]
isolation = "sandbox"
timeout = 14400
slim = true
~~~

The [profile] table supplies compiler/linker flags and a default job count. Existing CFLAGS, CXXFLAGS, or LDFLAGS already present in Nawa's environment take precedence over profile values. The [build] table controls the default isolation, per-section timeout, and post-install slimming policy.

## Reproducibility and cache reuse

Use --rebuild-check to build twice in separate directories and compare package archive hashes:

~~~sh
nawa build --rebuild-check hello
~~~

This check requires at least env isolation. Use --use-cache to reuse an existing archive when its recorded build identity matches the current inputs. Cache identity includes recipe bytes, source digests, dependency closure, and profile flags.
