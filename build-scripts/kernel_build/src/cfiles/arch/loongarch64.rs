use std::{collections::HashSet, path::PathBuf};

use crate::constant::ARCH_DIR_LOONGARCH64;

use super::CFilesArch;

pub(super) struct LoongArch64CFilesArch;

impl CFilesArch for LoongArch64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__loongarch64__", None);
        c.define("__loongarch", None);
    }

    fn setup_files(&self, _c: &mut cc::Build, _files: &mut HashSet<PathBuf>) {}

    fn setup_global_flags(&self, c: &mut cc::Build) {
        // 在这里设置编译器，不然的话vscode的rust-analyzer会报错
        c.compiler("loongarch64-unknown-linux-gnu-gcc");
        c.flag("-mcmodel=normal");

        c.flag("-march=loongarch64");
    }
}

#[allow(dead_code)]
fn arch_path(relative_path: &str) -> PathBuf {
    PathBuf::from(format!("{}/{}", ARCH_DIR_LOONGARCH64, relative_path))
}
