use std::{collections::HashSet, path::PathBuf};

use cc::Build;

use crate::utils::cargo_handler::{CargoHandler, TargetArch};

use self::x86_64::X86_64CFilesArch;

pub mod loongarch64;
pub mod riscv64;
pub mod x86_64;

pub(super) trait CFilesArch {
    /// 设置架构相关的宏定义
    fn setup_defines(&self, c: &mut Build);
    /// 设置需要编译的架构相关的文件
    fn setup_files(&self, c: &mut Build, files: &mut HashSet<PathBuf>);
    /// 设置架构相关的全局编译标志
    fn setup_global_flags(&self, c: &mut Build);
}

/// 获取当前的架构;
pub(super) fn current_cfiles_arch() -> &'static dyn CFilesArch {
    let arch = CargoHandler::target_arch();
    match arch {
        TargetArch::X86_64 => &X86_64CFilesArch,
        TargetArch::Riscv64 => &riscv64::RiscV64CFilesArch,
        TargetArch::LoongArch64 => &loongarch64::LoongArch64CFilesArch,

        _ => panic!("Unsupported arch: {:?}", arch),
    }
}
