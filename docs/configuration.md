# Configuration

Nawa combines command-line options, environment variables, project configuration, and system-wide defaults.

## Configuration files

Create a project configuration:

~~~sh
nawa init
~~~

Nawa searches for the nearest nawa.toml in the current directory or one of its parents. Set NAWA_CONFIG to choose a specific file.

The system-wide configuration is /etc/nawa/nawa.toml. Create its template with:

~~~sh
nawa init --system
~~~

Use --force only when you intend to replace an existing template.

## Precedence

For settings that support all layers, higher entries override lower ones:

1. Command-line option.
2. Environment variable.
3. Project nawa.toml.
4. System /etc/nawa/nawa.toml.
5. Built-in default.

Relative paths in a configuration file are resolved relative to the directory containing that file.

Inspect the effective configuration and its source:

~~~sh
nawa config show
~~~

## Supported settings

| Setting | Environment variable | Default / behavior |
|---|---|---|
| root | NAWA_ROOT | / |
| recipes | NAWA_RECIPES | recipes |
| builddir | NAWA_BUILDDIR | build |
| profiles | NAWA_PROFILES | profiles |
| jobs | NAWA_JOBS | Profile value, otherwise available CPU count. |
| repo | NAWA_REPO | The Faseela stable repository URL; set to an empty string, none, off, or disabled to turn it off. |
| install_mode | NAWA_INSTALL_MODE | auto; accepts auto, repo, build, or ask. |
| downloads | NAWA_DOWNLOADS | Defaults to 4; accepted range is 1–64. |

The project file can also contain a [plugins] table and a [[repositories]] list. Plugin state and repository entries are described in their respective guides.

## Example project configuration

~~~toml
# nawa.toml
recipes = "recipes"
builddir = "build"
profiles = "profiles"
jobs = 8
install_mode = "auto"

# Set to an empty string to disable the default repository.
# repo = ""

[[repositories]]
id = "stable"
name = "Distribution stable"
url = "https://packages.example.org/stable"
enabled = true
priority = 50

[plugins]
enabled = []
disabled = []
~~~

The example repository URL is illustrative; replace it with a repository you trust. When [[repositories]] entries are present, they take precedence over the legacy single repo setting.

## Install source policy

The install_mode setting controls nawa install NAME:

| Mode | Behavior |
|---|---|
| auto | Prefer a compatible repository package when it is at least as new as the recipe; otherwise build from source. |
| repo | Use the repository only. Fail if it cannot provide the package for this architecture. |
| build | Build from the configured recipe tree. |
| ask | Like auto, but ask an interactive terminal to choose repository or build. |

Command-line options such as --build, --from-repo, and --from MODE override the configured mode for that invocation.

The update command defaults to rebuilding outdated installed packages from recipes. Select a repository explicitly with nawa update --from repo or choose auto to prefer a suitable repository package and fall back to a build.
