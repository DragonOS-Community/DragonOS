use crate::context::DADKExecContext;

pub mod profile;
pub mod rootfs;
pub mod user;

pub fn run(ctx: DADKExecContext) {
    match &ctx.command.action {
        crate::console::Action::Kernel => {
            unimplemented!("kernel command has not implemented for run yet.")
        }
        crate::console::Action::Rootfs(rootfs_command) => {
            rootfs::run(&ctx, rootfs_command).expect("Run rootfs action error.")
        }
        crate::console::Action::User(user_command) => {
            user::run(&ctx, user_command).expect("Run user action error.")
        }
        crate::console::Action::Profile(profile_command) => {
            profile::run(&ctx, profile_command).expect("Run profile action error.")
        }
    }
}
