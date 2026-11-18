//! System-level domain types (design §4.3 `domain/system`): init identity
//! and capabilities, the system profile, and the migration model
//! (definition, durable state machine, record).

pub(crate) mod capability;
pub(crate) mod identity;
pub(crate) mod migration;
pub(crate) mod observation;
pub(crate) mod plan;
pub(crate) mod profile;
