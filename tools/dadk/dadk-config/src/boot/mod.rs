use std::{fs, path::PathBuf};

use anyhow::Result;
use dragonstub::DragonStubConfig;
use grub::GrubConfig;
use hypervisor::qemu::QemuConfig;
use metadata::BootMetadata;
use serde::Deserialize;
use uboot::UbootConfig;

pub mod dragonstub;
pub mod grub;
pub mod hypervisor;
pub mod metadata;
pub mod uboot;

/// Boot configuration file
#[derive(Debug, Clone, Deserialize)]
pub struct BootConfigFile {
    /// Boot metadata
    pub metadata: BootMetadata,

    /// GRUB configuration
    pub grub: Option<GrubConfig>,
    /// DragonStub configuration
    pub dragonstub: Option<DragonStubConfig>,

    /// U-Boot configuration
    pub uboot: Option<UbootConfig>,

    /// QEMU configuration
    pub qemu: Option<QemuConfig>,
}

impl BootConfigFile {
    pub fn load(path: &PathBuf) -> Result<Self> {
        // 读取文件内容
        let content = fs::read_to_string(path)?;
        Self::load_from_str(&content)
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: BootConfigFile = toml::from_str(content)?;

        Ok(config)
    }
}
