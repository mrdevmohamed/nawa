//! `nawa service`: runit service management.

use anyhow::Result;

use crate::cli::ServiceCmd;
use crate::ctx::Ctx;
use crate::service;

pub(crate) fn cmd_service(ctx: &Ctx, action: ServiceCmd) -> Result<()> {
    match action {
        ServiceCmd::List => service::print_list(&ctx.root),
        ServiceCmd::Enable { name, now } => {
            if now {
                service::enable_now(&ctx.root, &name)?;
            } else {
                service::enable(&ctx.root, &name)?;
                println!(
                    "nawa: enabled {name} — runsvdir picks it up within ~5 s \
                     (or: `nawa service start {name}`)"
                );
            }
            Ok(())
        }
        ServiceCmd::Disable { name, now } => {
            if now {
                service::disable_now(&ctx.root, &name)?;
                crate::nawa_info!("stopped and disabled {name}");
            } else {
                service::disable(&ctx.root, &name)?;
                println!(
                    "nawa: disabled {name} — runsvdir stops it within ~5 s \
                     (or use `nawa service disable --now {name}`)"
                );
            }
            Ok(())
        }
        ServiceCmd::Start { name } => service::start(&ctx.root, &name),
        ServiceCmd::Stop { name } => service::stop(&ctx.root, &name),
        ServiceCmd::Restart { name } => service::restart(&ctx.root, &name),
        ServiceCmd::Status { name } => service::print_status(&ctx.root, name.as_deref()),
    }
}
