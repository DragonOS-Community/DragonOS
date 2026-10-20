use anyhow::{anyhow, Result};

use crate::context::DADKExecContext;

pub(super) fn delete(ctx: &DADKExecContext) -> Result<()> {
    let sysroot_dir = ctx.sysroot_dir()?;
    // 检查 sysroot_dir 是否存在
    if !sysroot_dir.exists() {
        return Err(anyhow!("Sysroot directory does not exist"));
    }

    // 检查 sysroot_dir 是否是一个目录
    if !sysroot_dir.is_dir() {
        return Err(anyhow!("Sysroot path is not a directory"));
    }

    // 检查 sysroot_dir 是否是当前工作目录的子目录
    if !sysroot_dir.starts_with(&ctx.workdir()) {
        return Err(anyhow!(
            "Sysroot directory must be a subdirectory of the current working directory"
        ));
    }

    std::fs::remove_dir_all(sysroot_dir)?;
    Ok(())
}
