//! Command handlers grouped by domain; `dispatch` owns the top-level command
//! orchestration (lock, recovery, snapshots, the `Cmd` → handler match);
//! `main.rs` only bootstraps.
mod building;
mod configuring;
mod fetching;
mod groups;
mod history;
mod installing;
mod plugins;
mod querying;
mod recipes;
mod sandbox;
mod signing;
mod system;

pub(crate) use building::*;
pub(crate) use configuring::*;
pub(crate) use fetching::*;
pub(crate) use groups::*;
pub(crate) use history::*;
pub(crate) use installing::{cmd_autoremove, cmd_install, cmd_outdated, cmd_remove, cmd_update};
pub(crate) use plugins::*;
pub(crate) use querying::*;
pub(crate) use recipes::*;
pub(crate) use sandbox::*;
pub(crate) use signing::*;
pub(crate) use system::*;

mod dispatch;
pub(crate) use dispatch::*;
