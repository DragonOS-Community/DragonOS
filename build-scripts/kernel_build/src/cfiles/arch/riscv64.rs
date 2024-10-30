use std::{collections::HashSet, path::PathBuf};

use crate::{constant::ARCH_DIR_RISCV64, utils::FileUtils};

use super::CFilesArch;

pub(super) struct RiscV64CFilesArch;

impl CFilesArch for RiscV64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__riscv64__", None);
        c.define("__riscv", None);
    }

    fn setup_global_include_dir(&self, include_dirs: &mut HashSet<PathBuf>) {
        include_dirs.insert("src/arch/riscv64/include".into());
    }

    fn setup_files(&self, _c: &mut cc::Build, files: &mut HashSet<PathBuf>) {
        files.insert(PathBuf::from("src/arch/riscv64/asm/head.S"));

        FileUtils::list_all_files(&arch_path("asm"), Some("c"), true)
            .into_iter()
            .for_each(|f| {
                files.insert(f);
            });
    }

    fn setup_global_flags(&self, c: &mut cc::Build) {
        // 在这里设置编译器，不然的话vscode的rust-analyzer会报错
        c.compiler("riscv64-unknown-elf-gcc");
        // // c.flag("-march=rv64imafdc");
        // c.no_default_flags(true);
        c.flag("-mcmodel=medany");

        c.flag("-mabi=lp64d");
        c.flag("-march=rv64gc");
    }
}

fn arch_path(relative_path: &str) -> PathBuf {
    PathBuf::from(format!("{}/{}", ARCH_DIR_RISCV64, relative_path))
}
