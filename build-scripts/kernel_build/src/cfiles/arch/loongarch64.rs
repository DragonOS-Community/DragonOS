use std::{collections::HashSet, path::PathBuf};

use crate::{constant::ARCH_DIR_LOONGARCH64, utils::FileUtils};

use super::CFilesArch;

pub(super) struct LoongArch64CFilesArch;

impl CFilesArch for LoongArch64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__loongarch64__", None);
        c.define("__loongarch", None);
    }

    fn setup_global_include_dir(&self, include_dirs: &mut HashSet<PathBuf>) {
        include_dirs.insert("src/arch/loongarch64/include".into());
    }

    fn setup_files(&self, _c: &mut cc::Build, files: &mut HashSet<PathBuf>) {
        files.insert(PathBuf::from("src/arch/loongarch64/asm/head.S"));

        FileUtils::list_all_files(&arch_path("asm"), Some("c"), true)
            .into_iter()
            .for_each(|f| {
                files.insert(f);
            });
    }

    fn setup_global_flags(&self, c: &mut cc::Build) {
        // 在这里设置编译器，不然的话vscode的rust-analyzer会报错
        c.compiler("loongarch64-linux-gnu-gcc");
        c.flag("-mcmodel=large");

        c.flag("-march=loongarch64");
    }
}

fn arch_path(relative_path: &str) -> PathBuf {
    PathBuf::from(format!("{}/{}", ARCH_DIR_LOONGARCH64, relative_path))
}
