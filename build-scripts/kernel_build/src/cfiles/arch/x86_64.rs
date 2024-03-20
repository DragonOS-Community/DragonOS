use std::path::PathBuf;

use cc::Build;

use crate::{constant::ARCH_DIR_X86_64, utils::FileUtils};

use super::CFilesArch;

pub(super) struct X86_64CFilesArch;

impl CFilesArch for X86_64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__x86_64__", None);
    }

    fn setup_global_include_dir(&self, c: &mut cc::Build) {
        c.include("src/arch/x86_64/include");
    }

    fn setup_files(&self, _c: &mut Build, files: &mut Vec<PathBuf>) {
        // 获取`kernel/src/arch/x86_64/driver/apic`下的所有C文件
        files.append(&mut FileUtils::list_all_files(
            &arch_path("driver/apic"),
            Some("c"),
            true,
        ));

        files.append(&mut FileUtils::list_all_files(
            &arch_path("init"),
            Some("c"),
            true,
        ));
        files.append(&mut FileUtils::list_all_files(
            &arch_path("asm"),
            Some("c"),
            true,
        ));
        files.append(&mut FileUtils::list_all_files(
            &arch_path("interrupt"),
            Some("c"),
            true,
        ));

        // setup asm files
        files.push(PathBuf::from("src/arch/x86_64/asm/head.S"));
        files.push(PathBuf::from("src/arch/x86_64/asm/entry.S"));
        files.push(PathBuf::from("src/arch/x86_64/asm/apu_boot.S"));
    }

    fn setup_global_flags(&self, c: &mut Build) {
        c.asm_flag("-m64");
        c.flag("-mcmodel=large").flag("-m64");
    }
}

fn arch_path(relative_path: &str) -> PathBuf {
    PathBuf::from(format!("{}/{}", ARCH_DIR_X86_64, relative_path))
}
