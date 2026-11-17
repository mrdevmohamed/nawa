//! System feature use cases (design §4.3 `application/features/system`):
//! detection and the profile loader; migration planning/dry-run reporting
//! and the migration coordinator are later Phase 5 slices. The durable
//! migration record store is a top-level module (`migration_record`) —
//! see its header for the placement rationale.

pub(crate) mod detect;
pub(crate) mod profile;
