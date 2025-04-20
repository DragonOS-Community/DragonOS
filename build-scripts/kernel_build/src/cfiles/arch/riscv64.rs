use super::CFilesArch;
use std::{collections::HashSet, path::PathBuf};

pub(super) struct RiscV64CFilesArch;

impl CFilesArch for RiscV64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__riscv64__", None);
        c.define("__riscv", None);
    }

    fn setup_files(&self, _c: &mut cc::Build, files: &mut HashSet<PathBuf>) {
        files.insert(PathBuf::from("src/arch/riscv64/asm/head.S"));
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
