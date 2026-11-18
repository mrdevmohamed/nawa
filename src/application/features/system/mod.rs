//! System feature use cases (design §4.3 `application/features/system`):
//! detection, the profile loader and the read-only migration-record
//! inspection (`migrate_inspect`, §12.2). The migration coordinator is a
//! later Phase 5 slice. The durable migration record store itself is a
//! top-level module (`migration_record`) — see its header for the
//! placement rationale.

pub(crate) mod detect;
pub(crate) mod migrate_inspect;
pub(crate) mod profile;
