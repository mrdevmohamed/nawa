//! System feature use cases (design §4.3 `application/features/system`):
//! detection, the profile loader and the migration record store;
//! planning/dry-run reporting and the migration coordinator in later
//! slices.

pub(crate) mod detect;
pub(crate) mod migrate;
pub(crate) mod profile;
