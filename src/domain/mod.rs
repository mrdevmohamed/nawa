//! Pure domain types and rules (design §4.3 `domain/`): no presentation, no
//! I/O side effects. The domain layer sits below application; it imports
//! nothing from the layers above it.

pub(crate) mod system;
