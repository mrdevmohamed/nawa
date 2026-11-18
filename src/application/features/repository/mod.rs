//! Repository feature use cases (design §4.3): operations a user can ask for
//! against repositories. The index format and fetch mechanisms stay in their
//! own cohesive subsystems (`repo`, `adapters::archive`, `fetch`); this module
//! owns the orchestration decisions only.
pub(crate) mod sync;
