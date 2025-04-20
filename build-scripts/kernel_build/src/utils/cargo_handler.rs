use std::{env, path::PathBuf};

use crate::kconfig::Feature;

lazy_static! {
    static ref CARGO_HANDLER_DATA: CargoHandlerData = CargoHandlerData::new();
}

struct CargoHandlerData {
    target_arch: TargetArch,
}

impl CargoHandlerData {
    fn new() -> Self {
        CargoHandlerData {
            target_arch: TargetArch::new(),
        }
    }
}

#[derive(Debug)]
pub struct CargoHandler;

impl CargoHandler {
    pub fn readenv(key: &str) -> Option<String> {
        if let Ok(value) = env::var(key) {
            Some(value)
        } else {
            None
        }
    }

    /// 获取当前编译的目标架构
    pub fn target_arch() -> TargetArch {
        CARGO_HANDLER_DATA.target_arch
    }

    /// 设置Cargo对文件更改的监听
    ///
    /// ## Parameters
    ///
    /// - `files` - The files to set rerun build
    pub fn emit_rerun_if_files_changed(files: &[PathBuf]) {
        for f in files {
            println!("cargo:rerun-if-changed={}", f.to_str().unwrap());
        }
    }

    /// 添加features
    ///
    /// ## Parameters
    ///
    /// - `features` - The features to be set
    pub fn emit_features(features: &[Feature]) {
        for f in features.iter() {
            if f.enable() {
                println!("cargo:rustc-cfg=feature=\"{}\"", f.name());
            }
        }
    }
}

/// 目标架构
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
    Riscv64,
    Mips64,
    Powerpc64,
    LoongArch64,
    S390x,
    Sparc64,
    Unknown,
}

impl TargetArch {
    pub fn new() -> Self {
        let data = CargoHandler::readenv("CARGO_CFG_TARGET_ARCH")
            .expect("CARGO_CFG_TARGET_ARCH is not set")
            .to_ascii_lowercase();

        match data.as_str() {
            "x86_64" => TargetArch::X86_64,
            "aarch64" => TargetArch::Aarch64,
            "riscv64" => TargetArch::Riscv64,
            "mips64" => TargetArch::Mips64,
            "powerpc64" => TargetArch::Powerpc64,
            "loongarch64" => TargetArch::LoongArch64,
            "s390x" => TargetArch::S390x,
            "sparc64" => TargetArch::Sparc64,
            _ => TargetArch::Unknown,
        }
    }
}
