# Repositories and security

Nawa can install package archives from local or remote binary repositories. Repository metadata is cached and checked through Nawa's freshness and authentication path before it is used.

## Configure repositories

A single repository can be configured with repo. For multiple repositories, use [[repositories]] entries in nawa.toml:

~~~toml
[[repositories]]
id = "stable"
name = "Stable"
url = "https://packages.example.org/stable"
enabled = true
priority = 50

[[repositories]]
id = "testing"
name = "Testing"
url = "https://packages.example.org/testing"
enabled = false
priority = 80
~~~

Repository IDs must be valid package-style identifiers and unique. Explicit [[repositories]] entries take precedence over the legacy scalar repo setting.

Inspect configuration without network access:

~~~sh
nawa repolist
nawa repolist --all
~~~

Refresh metadata:

~~~sh
nawa makecache
~~~

Query cached metadata without downloading package archives:

~~~sh
nawa repoquery openssl
nawa repoquery openssl --info
~~~

## Repository index schemas

Nawa understands legacy repository index schema v1 and authenticated schemas v2 and v3.

| Schema | Archive location | Authentication |
|---|---|---|
| v1 | Legacy repository layout | Legacy compatibility behavior. |
| v2 | Shared base URL | Signed index with SHA-256 archive digests. |
| v3 | Per-package HTTPS download URLs | Signed index with SHA-256 archive digests. |

For schema v2 and v3, repository metadata declares the signed-index authentication format, SHA-256 hashes, and Nawa's supported signature format. Schema v3 requires an absolute HTTPS URL for each package archive and for a published recipe snapshot.

Schema v2 and v3 indexes must be signed. Schema v3 also requires a locations manifest when creating the index. Consult [Signed repository index design](design/signed-repository-index-v2.md) for the wire format and validation rules.

## Create signing keys

Generate an Ed25519 signing key pair:

~~~sh
nawa keygen
~~~

The default key directory depends on whether Nawa is running system-wide or as a user. You can choose a directory with --out. Keep the secret key private; distribute only the public key.

Sign and verify files:

~~~sh
nawa sign index.toml
nawa verify index.toml
~~~

To trust a repository signing key on a target root:

~~~sh
nawa trust nawa.pub
~~~

The trust store is scoped to the target root, under /etc/nawa/trusted-keys. Use --root when preparing another root filesystem.

## Create a repository

A signed schema-v2 index can be generated from a directory of package archives:

~~~sh
nawa repo-create ./repo --index-version 2 --sign --key ./keys/nawa.sec
~~~

Schema v3 requires a locations manifest:

~~~sh
nawa repo-create ./repo \
  --index-version 3 \
  --locations ./locations.toml \
  --sign --key ./keys/nawa.sec
~~~

The locations manifest supplies the per-package archive URLs and recipe snapshot URL information expected by schema v3. Use nawa repo-update to update an existing signed index incrementally. It verifies the trusted base index before applying additions or removals and signs the new index.

## Trust and unsigned content

Treat package repositories and signing keys as security boundaries. A valid SHA-256 digest detects archive corruption, while a trusted signature authenticates the metadata that contains the digest.

Do not use --allow-unsigned as a routine workaround. The precise behavior depends on whether trusted keys are configured and on the command being run. A repository index that is expired is rejected unless an explicit force option is used.

## Recipe synchronization

If the repository publishes a recipe snapshot in its signed index, use:

~~~sh
nawa sync
~~~

Nawa verifies repository metadata before accepting the snapshot. sync supports explicit repository selection and options for accepting an expired index or, under the command's trust policy, an unsigned index.
