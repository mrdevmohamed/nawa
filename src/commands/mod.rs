//! Command handlers grouped by domain; `main.rs` only dispatches.
mod building;
mod configuring;
mod fetching;
mod groups;
mod installing;
mod plugins;
mod querying;
mod recipes;

pub(crate) use building::*;
pub(crate) use configuring::*;
pub(crate) use fetching::*;
pub(crate) use groups::*;
pub(crate) use installing::*;
pub(crate) use plugins::*;
pub(crate) use querying::*;
pub(crate) use recipes::*;
