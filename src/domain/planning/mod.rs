//! Planning domain (design §8): deterministic package planning over
//! normalized candidate inputs. The planner receives assembled candidate
//! values and performs no I/O; the application layer assembles candidates
//! from recipes, repository indexes and the local database.
//!
//! `install.rs` owns the install/update resolution core (§8.2's package
//! decisions); `removal.rs` owns autoremove/removal planning. Conflict
//! modeling moves here when its use case is extracted (`conflicts.rs` in
//! the §4.3 tree).

pub(crate) mod install;
pub(crate) mod removal;
