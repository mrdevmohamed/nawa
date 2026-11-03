# Nawa Signed Repository Index v2

**Status:** Proposed — design review  
**Project:** Nawa  
**Scope:** Repository metadata, archive authentication, compatibility, and publisher migration  
**Target format:** TOML  
**Cryptography:** Existing Ed25519 detached index signature (nawa-sig-v1)  
**Compatibility target:** Read legacy index v1 without changing its security policy

---

## 1. Summary

Nawa currently signs index.toml and separately signs each package archive:

- index.toml + index.toml.sig
- package.nawa.tar.zst + package.nawa.tar.zst.sig

A repository with hundreds of packages therefore needs roughly two GitHub Release assets per package. That makes GitHub Releases' 1,000-asset-per-release limit a practical repository ceiling.

Index v2 makes the **trusted, signed index the authentication authority for repository archives**. Each package entry already contains a SHA-256 digest. Once Nawa verifies the index signature against a configured trusted public key, the signed package digest authenticates the exact archive bytes. Per-package .sig assets are no longer required for repository-based installs or upgrades.

This is a metadata and trust-policy change, not a new cryptographic algorithm:

- The index remains signed with the existing Ed25519 nawa-sig-v1 format.
- Package bytes remain checked using SHA-256.
- Archive .PKGINFO remains checked against the package entry.
- Direct installation of an archive remains governed by the existing detached-signature policy.
- Legacy index v1 continues to require the existing per-archive signature behavior.

## 2. Goals

1. Remove the requirement to publish one .sig asset for every package in a v2 repository.
2. Authenticate archive bytes transitively through a signed index and its SHA-256 entries.
3. Preserve the current signature policy for legacy indexes.
4. Fail closed for unknown schema versions, unknown authentication modes, malformed hashes, and invalid signatures.
5. Reuse the current SHA-256 download/cache verification path.
6. Keep nawa upgrade, repository-backed nawa install, and nawa sync on one shared authenticated-index path.
7. Allow old and new clients to coexist during a deliberate migration window when publishers continue supplying legacy sidecars.

## 3. Non-goals

- Replacing Ed25519, changing key formats, or changing the detached .sig file format.
- Removing signatures from locally supplied archives.
- Trusting an embedded public key as proof of repository identity.
- Changing package archive layout or .PKGINFO.
- Introducing a new package transport or requiring a particular object-storage provider.
- Making an old Nawa binary understand v2 authentication when it has no v2 support.

## 4. Current behavior and design constraint

The current implementation already has the two necessary primitives:

1. src/repo.rs writes sha256 for every indexed package and for the optional recipe-tree archive.
2. src/upgrade.rs::open_repo verifies the index signature and freshness. fetch_entry downloads an archive using the index digest, verifies that digest, applies sign::verify_for_install to the archive sidecar, and compares .PKGINFO with the index entry.

Repository-backed package installation also calls upgrade::open_repo and upgrade::fetch_entry, so changing the shared policy there covers both install and upgrade. sync already relies on the signed index and the recipe-tree digest.

The important security distinction is that a signature which verifies only against the public key embedded in its own .sig proves self-consistency, not publisher identity. **Only an index signature matching a key in the configured trust store may authorize the v2 index to replace per-archive signatures.**

## 5. Index v2 format

### 5.1 New repository metadata

V2 adds one top-level [repository] table. Existing generated, expires, recipes, and [[package]] fields retain their current meaning.

~~~toml
generated = 1793678000
expires = 1801454000

[repository]
schema = 2
archive_auth = "signed-index"
hash_algorithm = "sha256"
index_signature = "nawa-sig-v1"

[recipes]
rev = "a1b2c3d4"
file = "recipes-a1b2c3d4.recipes.tar.zst"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[package]]
name = "bash"
version = "5.2"
rel = 1
arch = "x86_64"
file = "bash-5.2-1.nawa.tar.zst"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 1234567
installed_size = 5432100
~~~

### 5.2 Field definitions

| Field | Type | Required | Meaning |
|---|---|---:|---|
| repository.schema | integer | yes | Repository index schema. Must equal 2. |
| repository.archive_auth | string | yes | Must equal signed-index. |
| repository.hash_algorithm | string | yes | Must equal sha256. |
| repository.index_signature | string | yes | Must equal nawa-sig-v1; documents the existing signature envelope/domain. |
| generated | integer | yes in v2 | Existing Unix timestamp; used for rollback/freshness protection. |
| expires | integer | yes in v2 | Existing expiry timestamp; must be later than generated. |
| package[].sha256 | hex string | yes | SHA-256 of the exact archive bytes. |
| package[].size | integer | yes in v2 | Exact archive size in bytes; must be greater than zero. |
| recipes.sha256 | hex string | when recipes exist | SHA-256 of the recipe tarball; already covered by the index signature. |

V2 validation rules:

- SHA-256 digests must be exactly 64 hexadecimal characters. Normalize generated values to lowercase; readers may accept uppercase hex but compare decoded digest bytes.
- Package filenames must pass the existing safe-filename validation and must be unique within the index.
- Package names must be unique: an index continues to expose one selected version per package.
- Every v2 package must have a non-empty name/version, a valid architecture string, a positive rel, a valid digest, and a positive archive size.
- expires must be greater than generated.
- The recipes filename and digest must be validated with the same path and hash rules.
- Duplicate or contradictory entries are rejected rather than resolved by "first match".
- Unknown v2 authentication values are rejected. They must never silently fall back to a weaker interpretation.
- Unknown schema versions are rejected with an explicit unsupported-schema error.

### 5.3 Why the index signature is unchanged

The v2 schema does not introduce a new signature algorithm. index.toml.sig continues to sign the SHA-256 digest of the exact raw index.toml bytes through the current signed message:

~~~text
nawa-v1:sha256:<hex-sha256-of-index.toml>
~~~

Nawa must verify the signature over the downloaded bytes, not a parsed-and-reserialized TOML representation. This avoids canonicalization ambiguity and preserves current tooling.

The term "Index v2" refers to the TOML schema and repository archive-authentication policy. It does **not** mean a new nawa-sig-v2 cryptographic format.

## 6. Authentication model

### 6.1 Verification result

open_repo should retain the result of index verification rather than reducing it to "signature file exists".

A small internal result type is recommended:

~~~rust
enum IndexVerification {
    TrustedKey { key_name: String },
    EmbeddedKeyOnly,
    UnsignedAllowed,
}

enum ArchiveAuth {
    DetachedSignature, // legacy policy
    SignedIndexSha256, // v2, only after trusted-key verification
}
~~~

Exact names may follow the codebase's conventions. The important invariant is:

> ArchiveAuth::SignedIndexSha256 can be selected only when a v2 index signature verifies against at least one configured trusted key.

Do not derive archive authentication from the index's declared archive_auth string alone, from the existence of index.toml.sig, or from a key embedded in an untrusted signature file.

### 6.2 V2 trusted-index path

When all the following are true:

1. repository.schema == 2;
2. archive_auth == signed-index;
3. hash_algorithm == sha256;
4. index_signature == nawa-sig-v1;
5. index.toml.sig is valid against a key in the configured trust store;

then Nawa selects ArchiveAuth::SignedIndexSha256.

For each package:

1. Validate the filename and v2 entry fields.
2. Download the archive to a .part file or locate it in the local repository.
3. Verify the exact archive SHA-256 against the signed index entry.
4. Verify the actual archive size against package[].size.
5. Check .PKGINFO name, version, release, and architecture against the entry.
6. Install through the normal atomic installer.

No per-package .sig download or verification is required in this path. If a sidecar happens to exist, it is ignored for repository authentication: the trusted index and matching digest are authoritative. A detached sidecar remains relevant when the archive is installed directly as a local file.

A hash mismatch, missing archive, bad archive metadata, or invalid index signature is always fatal. --allow-unsigned must never override a digest mismatch or a present-but-invalid signature.

### 6.3 V2 index without trusted authentication

A v2 declaration is not sufficient to grant index-based archive authentication.

- If a signature exists and trusted keys are configured, it must match a configured key. A signature from an unknown key is rejected, just as it is today.
- If no trusted key is configured, a valid signature checked only against its embedded key is classified as EmbeddedKeyOnly. It does not activate SignedIndexSha256.
- If the index is unsigned and the current CLI policy allows it, classify it as UnsignedAllowed. It does not activate SignedIndexSha256.
- In either of the last two cases, repository archives use the existing detached archive-signature policy.

This preserves the current bootstrap/override behavior without silently treating a self-signed index as a trusted publisher.

For remote repositories, the current require_trust_for_remote rule remains in force: a configured trusted key is required unless the user explicitly passes --allow-unsigned.

### 6.4 Legacy v1 path

An index with no [repository] table is legacy v1. Nawa must preserve today's behavior:

- Verify index.toml.sig using the current trust policy.
- Require and verify each package's detached .sig when trusted keys are configured, unless the user explicitly passes --allow-unsigned.
- Preserve current bootstrap behavior when no trusted keys are configured.
- Preserve the existing invalid-signature behavior: a present but invalid signature is rejected, including when --allow-unsigned is supplied.
- Keep the existing timestamp, expiry, cache, archive path, package metadata, and install policies.

V1 must not silently inherit v2's "signed index authenticates archive" rule.

### 6.5 Direct archive install

nawa install ./package.nawa.tar.zst, nawa verify, and other operations on a standalone archive keep the current detached-signature policy. A repository index is not in scope for a standalone file and must not be used to authorize it.

## 7. Download and cache behavior

V2 reuses the current download_cached(url, dest, Some(expected_sha256)) contract:

- A cached file is reused only after its digest is rechecked.
- A corrupt cache entry is removed/replaced through the existing .part + atomic rename flow.
- A downloaded archive is digest-checked before it becomes the active cache entry.
- The package is checked against .PKGINFO after the hash passes.

For ArchiveAuth::SignedIndexSha256, do not issue a request for <archive>.sig. This avoids a redundant network request and removes the per-package sidecar requirement.

For ArchiveAuth::DetachedSignature, retain today's sidecar download behavior, including temporary-file handling and cached-sidecar recovery rules.

The repository URL cache remains per URL. index.seen is updated only after the index was authenticated by a configured trusted key, preserving current rollback protection.

## 8. Publisher and CLI behavior

### 8.1 repo-create

Add an explicit opt-in option:

~~~text
nawa repo-create DIR --out index.toml --sign --index-version 2
~~~

Proposed CLI contract:

- --index-version 1|2, default 1 during the rollout.
- --index-version 2 requires --sign and a usable signing key. An unsigned v2 index must not be generated by the normal publisher path.
- The v2 generator writes the [repository] table and validates all package metadata before writing the final index.
- The index signature is created only after the final index bytes have been written.
- --recipes continues to work unchanged; its digest is already in the signed index.
- V1 remains byte/behavior compatible with the current repo-create output.

Do not make v2 the default until the v2-capable Nawa binary has been deployed to the Faseela build root and the repository migration has been tested.

### 8.2 Publisher workflow

For a v2 publication:

1. Assemble the complete repository directory and all package archives.
2. Generate a v2 index and validate every entry.
3. Sign index.toml.
4. Upload all newly referenced package archives.
5. Upload the recipe archive, if present.
6. Upload/replace index.toml and index.toml.sig only after every referenced data asset is available.
7. Verify the remotely published index and signature, then verify that each referenced archive asset exists.
8. Only after the new index is live and verified may stale assets be considered for deletion.

The workflow must not generate or upload per-package .sig files for a v2-only publication. A temporary dual-publish mode may create both .sig files and a v2 index to support older clients during migration.

The publisher must count existing release assets before upload and fail early with a clear diagnostic when the planned publication would exceed GitHub's asset limit. It must not retry a permanent 1,000-asset-limit error as if it were a network failure.

### 8.3 Signing key rotation

V2 uses the existing trust store with multiple public keys:

- An index signed by any configured trusted key is accepted.
- Adding a new public key before switching the publisher permits a controlled key rotation.
- Removing the old trusted key happens only after all publishers and supported indexes have moved to the new key.
- An index signed only by an embedded, untrusted key never activates v2 archive authentication.

## 9. Compatibility and migration

### 9.1 Compatibility matrix

| Client | Index v1 | Index v2 with package .sig sidecars | Index v2 without package .sig sidecars |
|---|---|---|---|
| Existing Nawa (no v2 support) | Existing behavior | Existing behavior, assuming signatures are valid | Not supported; with trusted keys it refuses unsigned archives |
| Nawa with v2 support | Legacy detached-signature policy | Uses trusted index + hashes; sidecars are optional/redundant in the trusted v2 path | Uses trusted index + hashes |
| Nawa with v2 support but no trusted signer configured | Existing legacy fallback | Uses legacy package-signature policy | Same bootstrap/override behavior as legacy; not authenticated by the v2 declaration |

Backward compatibility means **new Nawa keeps reading v1 repositories with the exact old policy**. It does not mean an old Nawa binary can safely consume a v2 repository after per-package signatures have been removed.

For a mixed-client migration, the publisher may dual-publish package sidecars while the repository remains in v2 mode. Remove them only after the supported client floor has moved to a v2-capable build.

### 9.2 Recommended rollout

**Phase 0 — repair current stable release state**

The previous experimental publication uploaded seven package archives without their detached signatures, then failed before publishing the recipe bundle and refreshed index. Before any cleanup:

- Download and verify the currently published index.toml and index.toml.sig.
- Confirm that none of those seven new archives is referenced by the currently signed index.
- If they are unreferenced, classify them as orphan assets and remove them only as a targeted cleanup.
- Do not delete any archive or recipe tarball referenced by the current signed index.
- Reconcile the index with the release asset list and ensure every current indexed package has its required v1 signature.

This is a recovery action, not part of the v2 format.

**Phase 1 — implement and ship v2-capable Nawa**

- Implement parsing, validation, trusted index verification result, and the v2 archive-auth path.
- Keep v1 tests and behavior unchanged.
- Release the updated Nawa binary.
- Update the Nawa binary included in the Faseela build root/rootfs before asking the package publisher to generate v2.

**Phase 2 — test on an isolated repository**

- Create a small v2 repository with at least two packages and a recipe archive.
- Publish only the index signature; omit package sidecars.
- Test upgrade, repository-backed install, and sync with a configured trusted key.
- Test all negative cases in the test matrix below.
- Optionally run a dual-publish test to prove old clients still work while sidecars remain.

**Phase 3 — migrate the production repository**

- Free required GitHub Release slots by removing only verified orphan or superseded assets, after checking index references and the retention policy.
- Publish data assets first and the signed v2 index last.
- Confirm the public index.toml and .sig match and that every referenced archive is available.
- Keep legacy package sidecars for the agreed compatibility window if the asset budget allows it.

**Phase 4 — stop publishing package sidecars**

Once the minimum supported installed Nawa version understands v2 and the repository operator has accepted the client compatibility boundary:

- Stop producing package .sig Release assets for new v2 archives.
- Keep signing the index on every publication.
- Keep standalone archive signing available.
- Retain v1 reading code indefinitely or until a separately announced breaking release.

## 10. Security properties

| Threat | Required result |
|---|---|
| Archive replaced but signed index unchanged | SHA-256 mismatch; reject before extraction/install. |
| Index and archive both replaced, signer is not trusted | Index signature verification fails; no v2 archive authentication. |
| Index signed only by its embedded key | Treat as self-signed/legacy fallback; never grant v2 trusted-index mode. |
| Package .sig removed from a trusted v2 repository | Install remains valid after signed-index verification and SHA-256 match. |
| Package .sig malformed while trusted v2 index is valid | Ignore it in repository mode; it is not part of v2 repository authentication. Standalone verification still rejects it. |
| Index signature is present but invalid | Reject, even with --allow-unsigned. |
| Package digest is wrong or archive bytes are tampered | Reject, even with --allow-unsigned. |
| Archive contents disagree with indexed name/version/rel/arch | Reject. |
| Replay of an older signed index | Existing index.seen rollback protection rejects it. |
| Expired repository snapshot | Existing expiry policy remains in force. |
| Unknown schema/authentication mode | Fail closed with an actionable unsupported-format error. |
| Cache entry corrupted or swapped | Recompute SHA-256; discard and fetch the expected bytes again. |

A compromised trusted signing key or a malicious authorized publisher can still sign a malicious index and package digest. V2 does not change the trust boundary; it removes redundant per-archive signatures underneath that boundary.

## 11. Test plan

### Legacy regression tests

1. V1 index with trusted index key and valid package sidecar installs.
2. V1 index with trusted keys and missing package sidecar is rejected.
3. V1 package sidecar signed by an unknown key is rejected.
4. V1 present-but-invalid index signature is rejected even with --allow-unsigned.
5. V1 with no trusted keys preserves the current embedded-key/bootstrap warnings and behavior.
6. Existing local repository and remote repository tests continue to pass.

### V2 positive tests

1. Trusted v2 index + package archive + correct SHA-256 + no package .sig installs.
2. The same repository upgrades an installed older package.
3. Repository-backed nawa install <name> uses v2 archive authentication.
4. nawa sync uses the authenticated index and checks the recipe archive digest.
5. Cached valid archive is reused after rechecking the expected digest.
6. A v2 package with an optional valid or invalid detached sidecar is treated according to the rule that repository mode uses the trusted signed index; standalone nawa verify continues to check the sidecar.
7. A trusted key added during key rotation verifies the new index.

### V2 negative and downgrade tests

1. Index signed only by an embedded non-trusted key never activates v2 archive authentication.
2. With trusted keys configured, a missing v2 index signature fails unless --allow-unsigned is explicit.
3. With --allow-unsigned, the repository falls back to legacy archive-signature policy; it must not silently claim trusted-index authentication.
4. A present invalid/unknown-key index signature is rejected; no downgrade to archive sidecars.
5. Tampered archive fails SHA-256 even when --allow-unsigned is set.
6. Wrong .PKGINFO fields fail even when the archive digest matches a deliberately constructed test index.
7. Missing, malformed, uppercase/invalid-length, or non-hex digests follow the documented validation rule.
8. Duplicate package names, duplicate filenames, unsafe filenames, and zero-size v2 entries are rejected.
9. Unsupported schema, hash algorithm, archive-auth mode, or signature scheme is rejected.
10. A stale sidecar already present in the remote cache is not accidentally used to change v2's selected authentication mode.
11. A network failure while refreshing the index signature never turns a stale or missing signature into an unsigned success.
12. A failed or partial archive transfer never replaces a previously verified cache entry.

### Publisher tests

1. V2 repo-create refuses to run without --sign.
2. V2 output contains correct metadata and validates every archive hash/size.
3. Generated index signature verifies against the configured public key.
4. V2 publisher does not upload per-package .sig assets in v2-only mode.
5. Publisher uploads package and recipe data before replacing the index.
6. Publisher preflight detects an asset-limit violation before making any release changes.
7. A failed publication leaves the previous signed index usable and does not delete referenced assets.

## 12. Implementation sequence

Keep implementation split into reviewable changes:

1. **repo.rs:** Add RepositoryMeta, serialize/parse v2, validate schema and v2 entries. Preserve absent metadata as v1.
2. **sign.rs:** Expose the index-verification result (trusted key vs embedded-only/unsigned) without weakening existing verify_for_install or verify_standalone.
3. **upgrade.rs:** Store selected archive-auth policy in OpenRepo; select v2 only after trusted-key index verification; branch fetch_entry to skip sidecar fetch/verification only in that mode.
4. **sync.rs:** Reuse the same authenticated OpenRepo; validate recipe metadata and digest under v2. No second trust implementation.
5. **CLI/repository generation:** Add repo-create --index-version 2; require signing; preserve v1 default during initial rollout.
6. **Tests:** Add unit, integration, remote HTTP fixture, trust/downgrade, and publisher tests.
7. **Docs:** Update nawa-usage.md, command help, and release/publisher instructions.
8. **Faseela integration:** Ship v2-capable Nawa in the build root before switching faseela-repo's production index.

## 13. Acceptance criteria

The design is ready for implementation when maintainers agree that:

- v2 trust is granted only by a signature verified against a configured trusted key;
- package SHA-256 values in that trusted index are authoritative for repository archive bytes;
- v1 remains on the exact current detached archive-signature policy;
- standalone archive verification is unchanged;
- --allow-unsigned never overrides a present invalid signature or a hash mismatch;
- old clients are not promised support for v2 repositories after package sidecars are removed;
- publishers have a safe transition plan and never delete assets referenced by the active signed index.
