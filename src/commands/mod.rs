//! Command handlers grouped by domain; `main.rs` only dispatches.
mod building;
mod configuring;
mod installing;
mod querying;

pub(crate) use building::*;
pub(crate) use configuring::*;
pub(crate) use installing::*;
pub(crate) use querying::*;
