use serde::Deserialize;

use crate::common::target_arch::TargetArch;

#[derive(Debug, Clone, Deserialize)]
pub struct UbootConfig {
    /// URL to download U-Boot binary file
    ///
    /// If the URL is `https://mirrors.dragonos.org.cn/pub/third_party/u-boot`,
    /// then the final download URL will be `https://mirrors.dragonos.org.cn/pub/third_party/u-boot/u-boot-{version}-{arch}.tar.xz`
    #[serde(rename = "download-url", default = "default_download_url")]
    pub download_url: String,

    /// Version of U-Boot
    #[serde(rename = "version", default = "default_version")]
    pub version: String,

    /// Prefix directory for U-Boot binary file
    ///
    /// Example:
    /// If the current architecture is `riscv64` and the version is `v2023.10`,
    /// `path_prefix` is `bin/uboot/`,
    /// then the path to locate the U-Boot binary file would be: `bin/uboot/riscv64/v2023.10/uboot.bin`
    #[serde(rename = "path-prefix", default = "default_path_prefix")]
    pub path_prefix: String,
}

impl Default for UbootConfig {
    fn default() -> Self {
        Self {
            download_url: Self::DEFAULT_DOWNLOAD_URL.to_string(),
            version: Self::DEFAULT_VERSION.to_string(),
            path_prefix: Self::DEFAULT_PATH_PREFIX.to_string(),
        }
    }
}

impl UbootConfig {
    const DEFAULT_DOWNLOAD_URL: &'static str =
        "https://mirrors.dragonos.org.cn/pub/third_party/u-boot";

    const DEFAULT_VERSION: &'static str = "v2023.10";

    const DEFAULT_PATH_PREFIX: &'static str = "bin/uboot/";
    /// Get the full download URL for the U-Boot binary file archive
    pub fn full_download_url(&self, target_arch: TargetArch) -> String {
        let arch_str: &str = target_arch.into();
        format!(
            "{}/u-boot-{}-{}.tar.xz",
            self.download_url, self.version, arch_str
        )
    }
}

fn default_download_url() -> String {
    UbootConfig::DEFAULT_DOWNLOAD_URL.to_string()
}

fn default_version() -> String {
    UbootConfig::DEFAULT_VERSION.to_string()
}

fn default_path_prefix() -> String {
    UbootConfig::DEFAULT_PATH_PREFIX.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_uboot_config() {
        let config = UbootConfig::default();
        assert_eq!(config.download_url, UbootConfig::DEFAULT_DOWNLOAD_URL);
        assert_eq!(config.version, "v2023.10");
        assert_eq!(config.path_prefix, "bin/uboot/");
    }

    #[test]
    fn test_full_download_url_riscv64() {
        let config = UbootConfig::default();
        let url = config.full_download_url(TargetArch::RiscV64);
        assert_eq!(
            url,
            "https://mirrors.dragonos.org.cn/pub/third_party/u-boot/u-boot-v2023.10-riscv64.tar.xz"
        );
    }

    #[test]
    fn test_empty_toml_deserialization() {
        let toml_content = "";
        let config: UbootConfig = toml::from_str(toml_content).unwrap();

        // Verify that the default values are set
        assert_eq!(config.download_url, UbootConfig::DEFAULT_DOWNLOAD_URL);
        assert_eq!(config.version, "v2023.10");
        assert_eq!(config.path_prefix, "bin/uboot/");
    }
}
