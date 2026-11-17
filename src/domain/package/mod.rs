//! Package domain model (design §4.3 `domain/package/`). The target tree
//! also names `model.rs` and `version.rs`: version comparison already lives
//! in `src/version.rs`, and package records are introduced by the use cases,
//! so this submodule starts with the metadata-chain model (the B3 schema
//! proposal, docs/design/package-metadata-chain.md).

pub(crate) mod metadata;
