use anyhow::Result;
use dadk_user::dadk_user_main;

use crate::{console::user::UserCommand, context::DADKExecContext};

pub(super) fn run(ctx: &DADKExecContext, cmd: &UserCommand) -> Result<()> {
    let config_dir = ctx.user_config_dir()?;
    let cache_root_dir = ctx.cache_root_dir()?;
    let sysroot_dir = ctx.sysroot_dir()?;
    let dadk_user_action: dadk_user::context::Action = cmd.clone().into();

    // 获取应用程序黑名单配置
    let app_blocklist = ctx.app_blocklist().clone();

    let context = dadk_user::context::DadkUserExecuteContextBuilder::default()
        .sysroot_dir(sysroot_dir)
        .config_dir(config_dir)
        .action(dadk_user_action)
        .thread_num(1)
        .cache_dir(cache_root_dir)
        .target_arch(ctx.target_arch())
        .app_blocklist(app_blocklist)
        .build()
        .expect("Failed to build execute context");
    dadk_user_main(context);
    Ok(())
}
