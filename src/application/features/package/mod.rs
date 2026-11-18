//! Package lifecycle use cases (design §4.3: `application/features/package`).

pub(crate) mod apply_transaction;
pub(crate) mod plan;
pub(crate) mod plan_install;
pub(crate) mod query;
pub(crate) mod revdep;

pub(crate) use apply_transaction::{install_for_migration, MigrationInstall};
