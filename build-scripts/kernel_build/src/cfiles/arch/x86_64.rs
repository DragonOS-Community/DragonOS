use std::path::PathBuf;

use cc::Build;

use crate::utils::FileUtils;

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
        files.push(PathBuf::from("src/arch/x86_64/driver/hpet.c"));
        // 获取`kernel/src/arch/x86_64/driver/apic`下的所有C文件
        files.append(&mut FileUtils::list_all_files(
            &PathBuf::from("src/arch/x86_64/driver/apic"),
            Some("c"),
            true,
        ));
    }
}
