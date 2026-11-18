//! System feature use cases (design §4.3 `application/features/system`):
//! detection, the profile loader, the read-only migration-record
//! inspection (`migrate_inspect`, §12.2), the migration coordinator
//! (`migrate_coordinator`, §10.3–§10.4) and crash recovery / post-reboot
//! resume (`migrate_recover`, §10.3–§10.5). The durable migration record
//! store itself is a top-level module (`migration_record`) — see its
//! header for the placement rationale.

pub(crate) mod detect;
pub(crate) mod migrate_coordinator;
pub(crate) mod migrate_inspect;
pub(crate) mod migrate_recover;
pub(crate) mod profile;
