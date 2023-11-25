use std::path::PathBuf;

use cc::Build;

use crate::utils::cargo_handler::CargoHandler;

use self::arch::current_cfiles_arch;

mod arch;

/// 构建项目的c文件
pub struct CFilesBuilder;

impl CFilesBuilder {
    pub fn build() {
        let mut c = cc::Build::new();

        Self::setup_global_flags(&mut c);
        Self::setup_defines(&mut c);
        Self::setup_global_include_dir(&mut c);
        Self::setup_files(&mut c);
        c.compile("dragonos_kernel_cfiles");
    }

    fn setup_global_flags(c: &mut Build) {
        c.flag("-fno-builtin")
            .flag("-nostdlib")
            .flag("-fno-stack-protector")
            .flag("-fno-pie")
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
        c.include("src/include");
        c.include("src");
        c.include(".");

        current_cfiles_arch().setup_global_include_dir(c);
    }

    /// 设置需要编译的文件
    fn setup_files(c: &mut Build) {
        let mut files: Vec<PathBuf> = Vec::new();

        current_cfiles_arch().setup_files(c, &mut files);

        Self::set_rerun_if_files_changed(&files);
        c.files(files.as_slice());
    }

    /// 设置Cargo对文件更改的监听
    fn set_rerun_if_files_changed(files: &Vec<PathBuf>) {
        CargoHandler::emit_rerun_if_files_changed(files.as_slice());
    }
}
