use std::{collections::HashSet, path::PathBuf};

use cc::Build;

use crate::{constant::ARCH_DIR_X86_64, utils::FileUtils};

use super::CFilesArch;

pub(super) struct X86_64CFilesArch;

impl CFilesArch for X86_64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__x86_64__", None);
    }

    fn setup_global_include_dir(&self, include_dirs: &mut HashSet<PathBuf>) {
        include_dirs.insert("src/arch/x86_64/include".into());
    }

    fn setup_files(&self, _c: &mut Build, files: &mut HashSet<PathBuf>) {
        const DIRS: [&str; 4] = ["driver/apic", "init", "asm", "interrupt"];
        DIRS.iter().for_each(|dir| {
            FileUtils::list_all_files(&arch_path(dir), Some("c"), true)
                .into_iter()
                .for_each(|f| {
                    files.insert(f);
                });
        });

        // setup asm files
        files.insert(PathBuf::from("src/arch/x86_64/asm/head.S"));
        files.insert(PathBuf::from("src/arch/x86_64/asm/entry.S"));
        files.insert(PathBuf::from("src/arch/x86_64/asm/apu_boot.S"));
    }

    fn setup_global_flags(&self, c: &mut Build) {
        c.asm_flag("-m64");
        c.flag("-mcmodel=large").flag("-m64");
    }
}

fn arch_path(relative_path: &str) -> PathBuf {
    PathBuf::from(format!("{}/{}", ARCH_DIR_X86_64, relative_path))
}
