# Package Metadata Chain: Recipe → Archive → Signed Index (B3 Proposal)

**Status:** Implemented (B3 wiring landed on `feature/system-intelligence-migration`); the pure chain model is in `src/domain/package/metadata.rs`.
**Project:** Nawa
**Scope:** Capability and runtime-dependency metadata across the recipe, `.PKGINFO`, and the repository index
**Answers:** Design §16.1 (the recommended metadata chain and its exact spellings) and §16.2 (what binary-only solving needs, and how the index upgrades without breaking v1/v2/v3 readers)
**Compatibility:** Until this proposal is agreed and wired, no recipe, archive, index, or CLI behavior changes

---

## 1. Summary

Migration execution now retains the authenticated index proof through the
existing package installer, including the narrowly typed legacy-init handoff.
The private staged copy is checked against both the signed archive identity and
the duplicated solving metadata. Durable plan evidence stores source/location
fingerprints rather than secret-bearing repository URLs. Schema v4 remains
unchanged; explicit package relations use the additive v5 contract in §11.

The design's capability model (§6) needs one place where each package declares
*what it provides* (`init-system=runit`) and *what it needs at runtime*
(`rundeps`). This proposal fixes that metadata as a single chain of three
spellings of the same value, and specifies each spelling exactly:

1. **Recipe** — the declaration as source input (§6.3(1)).
2. **Archive `.PKGINFO`** — the package-level source of truth, authenticated
   by the archive digest (§6.3(2)).
3. **Signed repository index schema v4 (capabilities/dependencies) and v5
   (package relations)** — authenticated duplications so
   Nawa can solve without downloading every archive (§6.3(3)).

Values flow strictly downward; no stage re-interprets or re-derives them
(§6.3: the three spellings must not become independent conflicting
authorities). A missing field means *unknown*, never "declares nothing"
(§6.3(4)–(5)); the normalized model in `src/domain/package/metadata.rs`
preserves that distinction and its tests pin every rule stated here.

## 2. The chain

```
recipe.toml  [capabilities] provides, depends.run      (declaration input)
      │  build normalizes once (trim + lowercase)
      ▼
archive .PKGINFO  provides = … / rundeps = …            (package source of truth;
      │  authenticated by the archive digest, which the        §6.3(2))
      │  signed index entry carries
      ▼
index.toml (schema 4/5)  [[package]] solving metadata    (authenticated duplication
      signed with the existing nawa-sig-v1 signature          for solving, §6.3(3))
```

- **Recipe** is where a maintainer declares capabilities. It is the only
  stage a human edits.
- **Archive `.PKGINFO`** records the normalized declaration at build time. It
  is the authority for what the *installed* package provides, and it is what
  the database can read from a cached archive without any index.
- **Index v4/v5** duplicates the archive fields so binary-only solving works
  before download. The index signature authenticates the duplication; the
  chain-comparison rule (§6 below) keeps the two from drifting.

## 3. Recipe spelling

Additive table; recipes without it build exactly as today:

```toml
[capabilities]
provides = ["init-system=runit"]
```

Rules:

- Entries are `domain=identity`, normalized by `Capability::parse`
  (`src/domain/system/capability.rs`): surrounding whitespace trimmed, domain
  and identity lowercased, exactly one `=`, no internal whitespace.
- **An entry may not contain a comma.** The archive spelling of a list is one
  comma-joined value (§4), so a comma inside an entry would be ambiguous or
  split into a different entry on round-trip. The rule applies to every
  spelling of a list in this chain.
- An entry is declared at most once; duplicates are an error.
- The only capability domain in scope for this migration is `init-system`,
  and provider selection is limited to the exact identities
  `init-system=runit` and `init-system=systemd` (§6). The *spelling* is
  domain-generic; adding another domain is a separate, justified design
  decision, not a schema change.
- Runtime dependencies keep their existing source: `depends.run` (recipe
  `Depends`). Schema v5 adds an explicit grammar for versioned runtime package
  requirements plus conflicts/replacements (§11). Versioned capability
  provides remain out of scope.

## 4. Archive spelling (`.PKGINFO`)

The existing flat `key = value` format gains one new key; lists are single
comma-joined values because `parse_pkginfo` collapses repeated keys (last
wins) — a list can only be spelled as one line:

```
pkgname = runit
pkgver = 2.1.2
pkgrel = 2
arch = x86_64
rundeps = glibc,mesa
size = 67890
provides = init-system=runit
```

Rules:

- `provides = init-system=runit` — the normalized recipe entries,
  comma-joined. A value may list several identities:
  `provides = init-system=runit,session=foo` (the second entry only
  illustrates the format; `session-manager` is out of scope, §6).
- `provides = ` (empty value) is an **explicit empty declaration**: the
  package declares no capabilities. A missing `provides` line is a **legacy
  archive** written before this proposal: its provides are *unknown*.
- `rundeps = glibc,mesa` — the existing `depends.run`, comma-joined. The
  empty value (`rundeps = `, as written today for packages without runtime
  dependencies) is the explicit empty list. `rundeps` is already written by
  every build, so no archive-side change is needed for it.
- Entries after the comma split are trimmed; an empty slot or a duplicate
  entry is an error in the normalized reader (strict, fail closed).
- Normalization happens once at build time (recipe → `.PKGINFO`); the
  archive reader trims only what the flat format requires. Name-like values
  (`rundeps`) are case-sensitive and are never lowercased — only capability
  identities are normalized.

Wiring notes for the build (`build/package.rs package_archives`):

- The main archive gains `provides = ` from the recipe's `[capabilities]`,
  joined the same way `rundeps` already is.
- **Sub-packages must not inherit the main package's capabilities.**
  `slim::derive_pkginfo` copies every non-rewritten line verbatim, so a
  naive wiring would give `foo-doc` the main package's
  `init-system=runit`. Wiring must clear (or explicitly override) `provides`
  for sub-packages; in this pass a sub-package always declares nothing
  (`provides = `). Its `rundeps` already points at the main package, which
  is exactly the chain the solver needs.
- Today's archives (no `provides` line) must keep working everywhere. The
  reference model's test `real_build_output_parses_with_provides_absent`
  pins that they normalize to "rundeps declared, provides unknown".
- The current install-time reader (`Pkginfo::from_map` in
  `install/payload.rs`) is lenient: an absent `rundeps` reads as `[]` and
  empty slots are silently filtered. The normalized reader is stricter and
  preserves the unknown/empty distinction. When the solver migrates onto
  the normalized model, that lenient read is replaced on the new path only;
  existing behavior is unchanged until then (and documented as an
  intentional normalization when it happens).

## 5. Signed index schema v4 (capabilities/dependencies)

Same `[repository]` block (its structure is unchanged, `schema = 4`), and the
`[[package]]` entry gains two required fields:

```toml
[repository]
schema = 4
archive_auth = "signed-index"
hash_algorithm = "sha256"
index_signature = "nawa-sig-v1"
archive_location = "per-package-url"

[[package]]
name = "runit"
version = "2.1.2"
rel = 2
arch = "x86_64"
file = "runit-2.1.2-2-x86_64.nawa.tar.zst"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 12345
installed_size = 67890
download_url = "https://mirror.example/nawa/runit-2.1.2-2-x86_64.nawa.tar.zst"
rundeps = ["glibc", "mesa"]
provides = ["init-system=runit"]
```

Rules:

- **v4 entries are the v3 entry plus required `rundeps` and `provides`.**
  Both are string arrays. An entry with no runtime dependencies or no
  capabilities writes the explicit empty list (`rundeps = []`,
  `provides = []`). A v4 entry *missing* a field is a parse error — never
  silently `unknown` (§6.3(5): in v4, solving fields must be present for
  every package). Legacy v1–v3 entries (fields absent) map to `unknown`
  instead, which is what keeps legacy records readable.
- Entry serialization follows the v3 model exactly:
  `download_url` present only for `archive_location = "per-package-url"`,
  `installed_size` omitted when 0 (unknown).
- **Entries reject unknown fields** (`deny_unknown_fields`), like the
  `[repository]` block already does. This is a deliberate tightening versus
  the v3 entry parser: a future schema extension must bump the schema
  version, and old readers fail closed with the existing unknown-schema
  message rather than ignoring new fields.
- Entry validation = the v2/v3 entry rules (non-empty name/version/arch,
  `rel != 0`, archive filename, 64-hex-char SHA-256, non-zero `size`, unique
  names and filenames, `download_url` per `archive_location`) plus the
  metadata rules (entries non-empty, no commas, no duplicates, and every
  `provides` entry parses as `domain=identity`).
- Nothing else in the index changes: the groups catalog stays schema 1, the
  signature format stays `nawa-sig-v1`. The signature covers the whole index
  document, so the new fields are authenticated with no new crypto and no
  new trust policy.

## 6. Chain rules: one value, three spellings

1. **One normalized value.** The build normalizes recipe declarations once
   (trim, lowercase) and writes them to `.PKGINFO` verbatim;
   `repo-create` copies them from its `.PKGINFO` parse into the index. No
   stage re-derives capabilities from package names, file lists, or heuristics
   (§6.3(4): never infer).
2. **Authentication rule (who may assert metadata).** Solving inputs must be
   authenticated (§8.1). Index v4 fields are authenticated by the index
   signature; archive `.PKGINFO` is authenticated by the archive digest the
   signed index carries. A local or unsigned archive — an archive installed
   outside any authenticated repository path — must not provide trusted
   capability metadata for migration (§6.3(3)), regardless of what its
   `.PKGINFO` says.
3. **Comparison rule (defense in depth on fetch).** When an archive is
   fetched from a v4 index and its digest verifies, every field the index
   declares is compared with the archive `.PKGINFO` exactly: the sorted entry
   lists must be equal, per field. A mismatch means repository tooling is
   broken or the metadata was tampered with; Nawa refuses the operation with
   an error naming the field and both lists — it never silently prefers one
   side. The reference model's `check_chain` implements this.
4. **Legacy rule.** A legacy index (v1–v3) duplicates no fields, so there is
   nothing to compare; the archive's own `.PKGINFO` is still authenticated
   by its verified digest. That is enough to *read* metadata, but it is not
   enough to upgrade the index: see §9.

## 7. Unknown vs empty

The distinction the solver depends on (§6.3(5)):

| State            | Recipe                        | `.PKGINFO`           | Index v4 entry     | Meaning for the solver                                    |
| ---------------- | ----------------------------- | -------------------- | ------------------ | --------------------------------------------------------- |
| unknown          | `[capabilities]` absent       | `provides` line absent | impossible (required) | report `unknown`; init migration unsupported (§6.3(4)); never inferred from names |
| declared none    | `provides = []`               | `provides = ` (empty value) | `provides = []` | explicitly declares no capabilities                    |
| declared entries | `provides = ["init-system=runit"]` | `provides = init-system=runit` | `provides = ["init-system=runit"]` | the normalized identities, in declaration order |

`rundeps` mirrors the same three states (its flat empty value is what every
build already writes for "no runtime dependencies").

Publication rule that keeps the table honest: **`repo-create --index-version
4` refuses an archive whose `.PKGINFO` has no `provides` line.** Writing
`provides = []` into the index for such an archive would fabricate an
authenticated "declares nothing" claim about a package that might provide
`init-system=runit`. Rebuilding the package against a recipe that declares
its capabilities (possibly an empty `[capabilities]` table) is the fix.
`rundeps` needs no such rule — every build already writes it.

## 8. What binary-only solving requires

Answering §16.2's first half — the exact per-package fields the solver needs
to plan an install or upgrade without reading recipes or downloading every
archive (§8.1):

**Required, and therefore required in v4/v5 entries:**

| Field      | Why                                                                      |
| ---------- | ------------------------------------------------------------------------ |
| `name`     | the solve target and identity                                            |
| `version`, `rel` | candidate ordering (`vercmp`), upgrade decisions                    |
| `arch`     | architecture constraint (§8.1)                                           |
| `file`     | which archive to fetch                                                   |
| `sha256`   | authenticates the exact archive bytes, hence its `.PKGINFO`              |
| `size`     | download accounting; also the v2/v3 non-zero rule                        |
| `rundeps`  | the dependency closure for binary-only packages                          |
| `provides` | provider selection for the init-system role (§6.2)                       |

**Not required for solving:** `installed_size` (disk accounting only; may be
0 = unknown), `download_url` (repository-level `archive_location` decides the
URL scheme), and recipe availability (that is what binary-only means).
Conflicts/replacements remain unknown for legacy archives/indexes; they are
not inferred as empty.

Everything else the solver needs comes from the installed-package database
and the detected system state, not from this chain.

## 9. Upgrade and compatibility story

Answering §16.2's second half — how v4 lands without breaking v1/v2/v3
readers:

- **v4 is opt-in per repository.** `nawa repo-create --index-version 4`
  (the flag currently accepts 1/2/3) emits the new schema after validating
  every archive's metadata (§7 publication rule). A repository that is not
  ready for v4 keeps publishing v3; nothing about v1–v3 changes.
- **Old readers fail closed.** `repo/validate.rs` already rejects unknown
  schema versions ("unsupported repository index schema {}; this Nawa
  supports schemas 2 and 3"). An old binary asked to sync a v4 repository
  refuses with exactly that existing message — no silent misparse, no
  metadata loss. When v4 is wired, *new* binaries extend the message to
  "2, 3 and 4"; old binaries are not changed, which is the point.
- **No downgrade path.** A v4 entry cannot be read as v3: `rundeps` and
  `provides` are required in v4, so a v3 *reader* would either reject the
  entry (if it adopted the same strictness) or ignore the unknown fields
  (today's entry parser has no `deny_unknown_fields`). Publishers that must
  serve old and new clients simultaneously publish a v3 and a v4 index side
  by side as separate files during the migration window; each client reads
  the newest schema it supports, and the two indexes are generated from the
  same archives so their digests agree.
- **No recipe-reader breakage.** `[capabilities]` is an additive recipe
  table with a serde default; recipes without it are byte-identical in
  behavior to today.
- **No archive-reader breakage.** The `provides` key is additive to
  `.PKGINFO`; every existing reader ignores keys it does not know. Legacy
  archives remain readable by new code and normalize to "provides unknown"
  (§4, §7).
- **Migration support, not automatic adoption.** Missing critical
  init-provider metadata (a legacy archive, or a legacy index over a legacy
  archive) means init-system migration stays unsupported for that package —
  reported as `unknown`, never resolved by guessing (§6.3(4), §8.2).

## 10. Wiring status (implemented)

The proposal is wired end to end; the model stays pure (`std`/`serde`/`toml`
plus `domain/system/capability.rs`) and its `#![allow(dead_code)]` is gone:

1. **Recipe** (`recipe/model.rs`, `recipe/validate.rs`): the additive
   `[capabilities]` table is `Option<Capabilities>` so an absent table stays
   *unknown* (§7); entries are normalized once by
   `domain::package::metadata::normalize_provides` (parseable, no commas, no
   duplicates). Unknown keys are an error.
2. **Build** (`build/package.rs`, `slim.rs`): `pkginfo_header` writes
   `provides` only for a declared table (empty value = explicit empty);
   `derive_pkginfo` clears `provides` for every sub-package, which keeps it
   from inheriting the main package's `init-system=runit`.
3. **`repo-create`** (`repo/create.rs`): `--index-version 4` is accepted,
   requires the locations manifest, reads/normalizes both fields from
   `.PKGINFO`, refuses an archive with no `provides` line (§7), and
   round-trips each entry through the strict reader before signing.
4. **`repo/validate.rs`**: schema 4 is accepted; entries are parsed through
   the domain `IndexEntryV4` (`deny_unknown_fields`) so unknown fields and
   missing/duplicate/malformed solving fields fail closed; the
   unknown-schema message now names 2, 3 and 4.
5. **Fetch/install** (`install/mod.rs`, `apply_transaction.rs`):
   `IndexProof` carries the signed entry's normalized metadata and
   `check_chain` compares it with the `.PKGINFO` of the *verified snapshot*
   after the digest check, before any payload is touched.
6. **Solver** (`plan.rs`, `plan_install.rs`, `upgrade/source.rs`): source
   candidates carry normalized `provides`; `OpenRepo::metadata_for` builds
   `PkgMetadata` from authenticated index fields (legacy → unknown); and
   `provider_note` explains which planned package provides the profile's
   selected init, preserving the authentication provenance.

## 11. Package relations schema 1 and signed index v5

The existing shared resolver handles version-constrained runtime package
requirements, conflicts and explicit package replacements. Missing legacy
metadata remains *unknown*, not an empty relation set.

### Recipe contract

A relation-bearing recipe uses this guarded source marker (it may also use
fetch-schema tables):

```toml
[package]
source = "local:/nawa-package-relations-1-requires-a-newer-nawa"
source_date_epoch = 1 # required for relation-only/meta recipes

[depends]
run = ["libc >= 2.40-1", "crypto >= 3.0-2"]

[relations]
schema = 1
conflicts = ["old-crypto < 4.0-1"]
replaces = ["old-crypto < 4.0-1"]
```

`[relations]` accepts exactly `schema`, `conflicts`, and `replaces`; schema
must be 1 and both lists are required (`[]` means explicitly empty). The
relation marker is required when the table exists; relation-only recipes do
not enter the fetch-source staging path. Versioned `depends.run` entries
without `[relations]` are rejected. Schema 1 permits constraints on runtime
dependencies only; build/check dependencies remain package names.

### Exact requirement syntax and normalization

One list entry is exactly `NAME` or `NAME OP VERSION-REL`. `OP` is one of
`=`, `!=`, `<`, `<=`, `>`, `>=`. Package names use the existing package-name
alphabet. `VERSION-REL` uses the package version alphabet followed by a final
hyphen and a positive decimal release. Splitting occurs at the final hyphen,
so hyphens in the version remain valid. Outer/name/operator/version whitespace
is trimmed; internal whitespace, commas, empty entries, missing/invalid rels,
unknown operators, extra tokens, and duplicates after normalization fail
closed. `vercmp` is the single ordering used for every operator.

### Archive/index representation and compatibility

`.PKGINFO` carries all three fields together:

```text
relations_schema = 1
conflicts = old-crypto<4.0-1
replaces = old-crypto<4.0-1
```

Flat lists are comma-joined; commas inside entries are forbidden. Signed
index v5 carries the complete tuple on every package entry:

```toml
[package.relations]
schema = 1
conflicts = ["old-crypto<4.0-1"]
replaces = ["old-crypto<4.0-1"]
```

v5 unknown fields, missing tuples, and unsupported relation schemas are
errors. `repo-create --index-version 5` refuses archives with unknown
relations and self-validates its serialized v5 index. Relation-bearing
archives cannot be published as v1-v4: metadata is never silently stripped.
`repo-update` preserves and compares relations with the immutable archive
digest/size. The signed index authenticates this tuple; fetch compares it with
`.PKGINFO` after archive digest verification. Index v1-v4 remains
backward-compatible and absent relations stay unknown; v4 still supports only
`rundeps` and `provides`.

### Deterministic resolver behavior

The shared resolver propagates runtime requirements through the full selected
graph before emitting actions. It rejects unsatisfied range intersections,
unavailable constrained candidates, unknown relations for versioned edges,
and implicit downgrades. A matching installed version satisfies a non-target
edge; otherwise the selected source candidate must be newer and satisfy every
constraint. No alternative source/archive is substituted silently. Build-root
closure uses the same grammar and checks candidate versions.

Conflicts are checked against the projected installed state: selected candidate
versions plus explicit replacement removals. Declarations from either side
are considered when candidate metadata is known. A replacement is considered
only for a requested replacing package and an installed target matching the
declared range. Ambiguous replacers, still-installed dependents,
target-and-replacement collisions and init-provider/boot-entry replacement
fail closed. Removals are dependent-first and share the existing package
transaction history, file WAL and installer path. A partial failure remains a
recorded partial transaction, never an inferred success.

Normal package plans preserve legacy v1-v4 behavior while displaying unknown
conflict/replacement metadata. Migration approval requires a trusted schema-v5
index and refuses every removal action, so generic relations cannot retire
runit/systemd during migration. Versioned virtual capabilities and
multi-source candidate backtracking are not part of relations schema 1.
