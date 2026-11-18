//! Package domain model (design §4.3 `domain/package/`). The target tree
//! also names `model.rs` and `version.rs`: version comparison already lives
//! in `domain/package/version.rs`, and package records are introduced by the
//! use cases. Alongside the metadata-chain model (the B3 schema proposal,
//! docs/design/package-metadata-chain.md), this module owns pure version
//! comparison.

pub(crate) mod metadata;
pub(crate) mod version;
