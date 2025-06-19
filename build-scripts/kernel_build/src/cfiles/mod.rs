use std::{collections::HashSet, path::PathBuf};

use cc::Build;

use crate::utils::cargo_handler::CargoHandler;

use self::arch::current_cfiles_arch;

mod arch;
mod common;

/// 构建项目的c文件
pub struct CFilesBuilder;

impl CFilesBuilder {
    pub fn build() {
        let mut c = cc::Build::new();

        Self::setup_global_flags(&mut c);
        Self::setup_defines(&mut c);
        Self::setup_global_include_dir(&mut c);
        Self::setup_files(&mut c);
        if c.get_files().count() == 0 {
            return;
        }
        c.compile("dragonos_kernel_cfiles");
    }

    fn setup_global_flags(c: &mut Build) {
        c.flag("-fno-builtin")
            .flag("-nostdlib")
            .flag("-fno-stack-protector")
            .flag("-static-pie")
            .flag("-Wno-expansion-to-defined")
            .flag("-Wno-unused-parameter")
            .flag("-O1");

        // set Arch-specific flags
        current_cfiles_arch().setup_global_flags(c);
    }

    fn setup_defines(c: &mut Build) {
        if let Ok(k) = std::env::var("EMULATOR") {
            c.define("EMULATOR", Some(k.as_str()));
        } else {
            c.define("EMULATOR", "__NO_EMULATION__");
        }

        current_cfiles_arch().setup_defines(c);
    }

    fn setup_global_include_dir(c: &mut Build) {
        let mut include_dirs = HashSet::new();

        c.include(".");

        common::setup_common_include_dir(&mut include_dirs);

        let include_dirs: Vec<PathBuf> = include_dirs.into_iter().collect();
        Self::set_rerun_if_files_changed(&include_dirs);

        include_dirs.into_iter().for_each(|dir| {
            c.include(dir);
        });
    }

    /// 设置需要编译的文件
    fn setup_files(c: &mut Build) {
        let mut files: HashSet<PathBuf> = HashSet::new();
        current_cfiles_arch().setup_files(c, &mut files);
        // 去重
        let files: Vec<PathBuf> = files.into_iter().collect();
        Self::set_rerun_if_files_changed(&files);
        c.files(files.as_slice());
    }

    /// 设置Cargo对文件更改的监听
    fn set_rerun_if_files_changed(files: &Vec<PathBuf>) {
        CargoHandler::emit_rerun_if_files_changed(files.as_slice());
    }
}
