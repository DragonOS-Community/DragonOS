use std::{cell::OnceCell, path::PathBuf};

use anyhow::Result;
use clap::Parser;
use dadk_config::{
    app_blocklist::AppBlocklistConfigFile, common::target_arch::TargetArch,
    manifest::DadkManifestFile, rootfs::RootFSConfigFile,
};
use derive_builder::Builder;
use log::warn;
use manifest::parse_manifest;

use crate::{
    console::CommandLineArgs,
    utils::{abs_path, check_dir_exists},
};

mod manifest;

/// DADK的执行上下文
#[derive(Debug, Clone, Builder)]
pub struct DADKExecContext {
    pub command: CommandLineArgs,
    /// DADK manifest file
    manifest: Option<DadkManifestFile>,

    /// RootFS config file
    rootfs: OnceCell<RootFSConfigFile>,

    /// Application blocklist config file
    #[builder(setter(skip))]
    app_blocklist: OnceCell<AppBlocklistConfigFile>,
}

pub fn build_exec_context() -> Result<DADKExecContext> {
    let mut builder = DADKExecContextBuilder::create_empty();
    builder.command(CommandLineArgs::parse());
    builder.rootfs(OnceCell::new());
    if builder.command.as_ref().unwrap().action.needs_manifest() {
        parse_manifest(&mut builder).expect("Failed to parse manifest");
    } else {
        builder.manifest(None);
    }
    let ctx: DADKExecContext = builder.build()?;
    ctx.setup_workdir().expect("Failed to setup workdir");
    Ok(ctx)
}

impl DADKExecContext {
    /// 获取工作目录的绝对路径
    pub fn workdir(&self) -> PathBuf {
        abs_path(&PathBuf::from(&self.command.workdir))
    }

    /// 设置进程的工作目录
    fn setup_workdir(&self) -> Result<()> {
        std::env::set_current_dir(&self.workdir()).expect("Failed to set current directory");
        Ok(())
    }
    /// Get rootfs configuration
    pub fn rootfs(&self) -> &RootFSConfigFile {
        self.rootfs.get_or_init(|| {
            RootFSConfigFile::load(&self.manifest().metadata.rootfs_config)
                .expect("Failed to load rootfs config")
        })
    }

    pub fn manifest(&self) -> &DadkManifestFile {
        self.manifest.as_ref().unwrap()
    }

    /// Get sysroot directory
    ///
    /// If the directory does not exist, or the path is not a folder, an error is returned
    pub fn sysroot_dir(&self) -> Result<PathBuf> {
        check_dir_exists(&self.manifest().metadata.sysroot_dir)
            .map(|p| p.clone())
            .map_err(|e| anyhow::anyhow!("Failed to get sysroot dir: {}", e))
    }

    /// Get cache root directory
    ///
    /// If the directory does not exist, or the path is not a folder, an error is returned
    pub fn cache_root_dir(&self) -> Result<PathBuf> {
        check_dir_exists(&self.manifest().metadata.cache_root_dir)
            .map(|p| p.clone())
            .map_err(|e| anyhow::anyhow!("Failed to get cache root dir: {}", e))
    }

    #[deprecated]
    pub fn user_config_dir(&self) -> Result<PathBuf> {
        check_dir_exists(&self.manifest().metadata.user_config_dir)
            .map(|p| p.clone())
            .map_err(|e| anyhow::anyhow!("Failed to get user config dir: {}", e))
    }

    pub fn target_arch(&self) -> TargetArch {
        self.manifest().metadata.arch
    }

    /// 获取应用程序黑名单配置
    pub fn app_blocklist(&self) -> &AppBlocklistConfigFile {
        self.app_blocklist.get_or_init(|| {
            let manifest = self.manifest();
            let config_path = &manifest.metadata.app_blocklist_config;

            // 转换为绝对路径
            let abs_config_path = if config_path.is_absolute() {
                config_path.clone()
            } else {
                self.workdir().join(config_path)
            };

            AppBlocklistConfigFile::load(&abs_config_path).unwrap_or_else(|e| {
                warn!(
                    "Failed to load app blocklist config: {}, using empty config",
                    e
                );
                AppBlocklistConfigFile::load_from_str(
                    r#"
                        blocked_apps = []
                        strict = true
                        log_skipped = true
                        "#,
                )
                .unwrap_or_default()
            })
        })
    }

    /// 获取磁盘镜像的路径，路径由工作目录、架构和固定文件名组成
    pub fn disk_image_path(&self) -> PathBuf {
        self.workdir()
            .join(format!("bin/{}.img", self.disk_image_basename()))
    }

    /// 获取磁盘挂载路径
    pub fn disk_mount_path(&self) -> PathBuf {
        self.workdir()
            .join(format!("bin/mnt/{}", self.disk_image_basename()))
    }

    fn disk_image_basename(&self) -> String {
        let arch: String = self.target_arch().into();
        format!("disk-image-{}", arch)
    }

    /// 获取磁盘镜像大小
    pub fn disk_image_size(&self) -> usize {
        self.rootfs().metadata.size
    }
}
