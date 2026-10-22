use serde::Deserialize;

/// Default time for GRUB to wait for user selection
const GRUB_DEFAULT_TIMEOUT: u32 = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct GrubConfig {
    /// Time to wait for user selection before booting
    #[serde(default = "default_timeout")]
    pub timeout: u32,

    #[serde(rename = "i386-legacy")]
    pub i386_legacy: Option<ArchConfig>,
    #[serde(rename = "i386-efi")]
    pub i386_efi: Option<ArchConfig>,
    #[serde(rename = "x86_64-efi")]
    pub x86_64_efi: Option<ArchConfig>,
}

const fn default_timeout() -> u32 {
    GRUB_DEFAULT_TIMEOUT
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchConfig {
    /// 指向grub-file的路径
    #[serde(rename = "grub-file")]
    pub grub_file: String,
    /// 指向grub-install的路径
    #[serde(rename = "grub-install")]
    pub grub_install: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test if the GRUB configuration parsing is correct for all architectures
    #[test]
    fn test_all_architectures() {
        let toml = r#"
        timeout = 15
        [i386-legacy]
        grub-file = "/opt/dragonos-grub/arch/i386/legacy/grub/bin/grub-file"
        grub-install = "/opt/dragonos-grub/arch/i386/legacy/grub/sbin/grub-install"
        [i386-efi]
        grub-file = "/opt/dragonos-grub/arch/i386/efi/grub/bin/grub-file"
        grub-install = "/opt/dragonos-grub/arch/i386/efi/grub/sbin/grub-install"
        [x86_64-efi]
        grub-file = "/opt/dragonos-grub/arch/x86_64/efi/grub/bin/grub-file"
        grub-install = "/opt/dragonos-grub/arch/x86_64/efi/grub/sbin/grub-install"
        "#;
        let config: GrubConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.timeout, 15);
        assert!(config.i386_legacy.is_some());
        assert!(config.i386_efi.is_some());
        assert!(config.x86_64_efi.is_some());
    }

    #[test]
    fn test_default_timeout() {
        let toml = r#"
        [i386-legacy]
        grub-file = "grub Legacy"
        grub-install = "/boot/grub/i386-legacy"
        "#;
        let config: GrubConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.timeout, GRUB_DEFAULT_TIMEOUT);
    }

    #[test]
    fn test_custom_timeout() {
        let toml = r#"
        timeout = 5
        [i386-efi]
        grub-file = "grub EFI"
        grub-install = "/boot/grub/i386-efi"
        "#;
        let config: GrubConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.timeout, 5);
    }

    #[test]
    fn test_no_architectures() {
        let toml = r#"
        timeout = 20
        "#;
        let config: GrubConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.timeout, 20);
        assert!(config.i386_legacy.is_none());
        assert!(config.i386_efi.is_none());
        assert!(config.x86_64_efi.is_none());
    }
}
