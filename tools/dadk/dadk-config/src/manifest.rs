use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::common::target_arch::TargetArch;

use std::fs;
use toml;

/// The main configuration file for DADK
#[derive(Debug, Clone, Deserialize)]
pub struct DadkManifestFile {
    pub metadata: Metadata,

    /// A flag variable used to indicate whether
    /// the default value function was called during deserialization.
    #[serde(skip)]
    pub used_default: bool,
}

impl DadkManifestFile {
    pub fn load(path: &PathBuf) -> Result<Self> {
        // 读取文件内容
        let content = fs::read_to_string(path)?;
        Self::load_from_str(&content)
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        // Parse TOML content
        let mut manifest_toml: DadkManifestFile = toml::from_str(content)?;

        manifest_toml.used_default = check_used_default();

        Ok(manifest_toml)
    }
}

thread_local! {
    /// Global variable to track if default values were used during deserialization.
    static USED_DEFAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Call this function to set a flag when
/// default values are used during DADK manifest parsing
fn set_used_default() {
    USED_DEFAULT.with(|used_default| {
        used_default.set(true);
    });
}

/// Check if default values were used during deserialization.
fn check_used_default() -> bool {
    USED_DEFAULT.with(|used_default| used_default.get())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// Target processor architecture
    pub arch: TargetArch,
    /// Rootfs configuration file path
    #[serde(default = "default_rootfs_config_path", rename = "rootfs-config")]
    pub rootfs_config: PathBuf,

    /// Hypervisor configuration file path
    #[serde(
        default = "default_hypervisor_config_path",
        rename = "hypervisor-config"
    )]
    pub hypervisor_config: PathBuf,

    /// Boot configuration file path
    #[serde(default = "default_boot_config_path", rename = "boot-config")]
    pub boot_config: PathBuf,

    /// Sysroot directory path
    #[serde(default = "default_sysroot_dir", rename = "sysroot-dir")]
    pub sysroot_dir: PathBuf,

    /// Root Cache directory path
    #[serde(default = "default_cache_root_dir", rename = "cache-root-dir")]
    pub cache_root_dir: PathBuf,

    /// User configuration directory path
    /// 这个字段只是临时用于兼容旧版本，v1.0版本重构完成后会删除
    #[deprecated(note = "This field is deprecated and will be removed in DADK 1.0")]
    #[serde(default = "default_user_config_dir", rename = "user-config-dir")]
    pub user_config_dir: PathBuf,

    /// Application blocklist configuration file path
    #[serde(
        default = "default_app_blocklist_config_path",
        rename = "app-blocklist-config"
    )]
    pub app_blocklist_config: PathBuf,
}

/// Returns the default path for the rootfs configuration file.
fn default_rootfs_config_path() -> PathBuf {
    set_used_default();
    "config/rootfs.toml".into()
}

/// Returns the default path for the hypervisor configuration file.
fn default_hypervisor_config_path() -> PathBuf {
    set_used_default();
    "config/hypervisor.toml".into()
}

/// Returns the default path for the boot configuration file.
fn default_boot_config_path() -> PathBuf {
    set_used_default();
    "config/boot.toml".into()
}

/// Returns the default path for the sysroot directory.
fn default_sysroot_dir() -> PathBuf {
    set_used_default();
    "bin/sysroot".into()
}

/// Returns the default path for the cache directory.
fn default_cache_root_dir() -> PathBuf {
    set_used_default();
    "bin/dadk_cache".into()
}

fn default_user_config_dir() -> PathBuf {
    set_used_default();
    "user/dadk/config".into()
}

/// Returns the default path for the application blocklist configuration file.
fn default_app_blocklist_config_path() -> PathBuf {
    set_used_default();
    "config/app-blocklist.toml".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Test loading a complete configuration file
    #[test]
    fn test_full_load_success() -> Result<()> {
        let toml_content = r#"
            [metadata]
            arch = "x86_64"
            rootfs-config = "config/rootfs-x86_64.toml"
            hypervisor-config = "config/hypervisor-x86_64.toml"
            boot-config = "config/boot-x86_64.toml"
            sysroot-dir = "bin/sysroot"
            cache-root-dir = "bin/dadk_cache"
            user-config-dir = "user/dadk/config"
            app-blocklist-config = "config/app-blocklist.toml"
        "#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(toml_content.as_bytes())?;

        let path = temp_file.path().to_path_buf();
        let manifest = DadkManifestFile::load(&path)?;

        assert_eq!(manifest.metadata.arch, TargetArch::X86_64);
        assert_eq!(
            manifest.metadata.rootfs_config,
            PathBuf::from("config/rootfs-x86_64.toml")
        );
        assert_eq!(
            manifest.metadata.hypervisor_config,
            PathBuf::from("config/hypervisor-x86_64.toml")
        );
        assert_eq!(
            manifest.metadata.boot_config,
            PathBuf::from("config/boot-x86_64.toml")
        );
        assert_eq!(manifest.metadata.sysroot_dir, PathBuf::from("bin/sysroot"));
        assert!(!manifest.used_default);

        Ok(())
    }

    /// Test whether an error is reported when the file does not exist.
    #[test]
    fn test_load_file_not_found() {
        let path = PathBuf::from("non_existent_file.toml");
        let result = DadkManifestFile::load(&path);

        assert!(result.is_err());
    }

    /// Test whether an error is reported when the TOML content is invalid
    #[test]
    fn test_load_invalid_toml() -> Result<()> {
        let invalid_toml_content = r#"
            [metadata
            arch = "x86_64"
        "#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(invalid_toml_content.as_bytes())?;

        let path = temp_file.path().to_path_buf();
        let result = DadkManifestFile::load(&path);

        assert!(result.is_err());

        Ok(())
    }

    /// Test whether an error is reported when the arch field is invalid
    #[test]
    fn test_load_invalid_arch_toml() -> Result<()> {
        // Invalid arch value
        let invalid_toml_content = r#"
            [metadata]
            arch = "abcde"
        "#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(invalid_toml_content.as_bytes())?;

        let path = temp_file.path().to_path_buf();
        let result = DadkManifestFile::load(&path);

        assert!(result.is_err());

        Ok(())
    }

    /// Test whether an error is reported when a required field is missing
    #[test]
    fn test_load_missing_required_fields() -> Result<()> {
        let toml_content = r#"
            [metadata]
            # arch field is missing
        "#;

        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(toml_content.as_bytes())?;

        let path = temp_file.path().to_path_buf();
        let result = DadkManifestFile::load(&path);

        assert!(result.is_err());

        Ok(())
    }

    /// Test whether default values are used
    /// when the rootfs_config and other configuration file path fields are not set
    #[test]
    fn test_load_default_config_path_value() -> Result<()> {
        let toml_content = r#"
            [metadata]
            arch = "x86_64"
        "#;
        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(toml_content.as_bytes())?;
        let path = temp_file.path().to_path_buf();
        let manifest = DadkManifestFile::load(&path)?;
        assert_eq!(manifest.used_default, true);
        assert_eq!(
            manifest.metadata.rootfs_config,
            PathBuf::from("config/rootfs.toml")
        );
        assert_eq!(
            manifest.metadata.hypervisor_config,
            PathBuf::from("config/hypervisor.toml")
        );
        assert_eq!(
            manifest.metadata.boot_config,
            PathBuf::from("config/boot.toml")
        );
        Ok(())
    }
}
