use serde::Deserialize;

use super::hypervisor::hyp_type::HypervisorType;

#[derive(Debug, Clone, Deserialize)]
pub struct BootMetadata {
    /// The boot protocol used during startup
    #[serde(rename = "boot-protocol")]
    pub boot_protocol: BootProtocol,
    /// The mode of booting
    #[serde(rename = "boot-mode")]
    pub boot_mode: BootMode,
    /// The hypervisor used during startup
    pub hypervisor: HypervisorType,

    /// Kernel command-line arguments
    #[serde(rename = "kcmd-args", default = "default_empty_vec")]
    pub kcmd_args: Vec<String>,
    /// Arguments passed to the init process
    #[serde(rename = "init-args", default = "default_empty_vec")]
    pub init_args: Vec<String>,
}

fn default_empty_vec() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum BootProtocol {
    /// BIOS Bootloader
    #[serde(rename = "grub-legacy")]
    GrubLegacy,
    /// UEFI Bootloader (Grub)
    #[serde(rename = "grub-efi")]
    GrubEFI,
    /// Direct Linux Boot (with `-kernel` options)
    #[serde(rename = "direct")]
    Direct,
    /// Dragon Stub Bootloader (riscv only)
    #[serde(rename = "dragon-stub")]
    DragonStub,
}

/// The mode of booting
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum BootMode {
    /// Graphic mode
    #[serde(rename = "graphic")]
    Graphic,
    /// Graphic mode with VNC
    #[serde(rename = "graphic-vnc")]
    GraphicVnc,
    /// No graphic mode
    #[serde(rename = "no-graphic")]
    NoGraphic,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper function to parse TOML string into BootMetadata
    fn parse_boot_metadata(toml_str: &str) -> Result<BootMetadata, toml::de::Error> {
        toml::from_str(toml_str)
    }

    fn assert_missing_field(err_str: &str, field: &str) {
        assert!(err_str.contains(&format!("missing field `{field}`")));
    }

    fn assert_unknown_variant(err_str: &str, variant: &str) {
        assert!(err_str.contains(&format!("unknown variant `{variant}`")));
    }

    #[test]
    fn test_parse_grub_legacy_graphic() {
        let toml_str = r#"
        boot-protocol = "grub-legacy"
        boot-mode = "graphic"
        hypervisor = "qemu"
        "#;

        let result = parse_boot_metadata(toml_str).unwrap();
        assert_eq!(result.boot_protocol, BootProtocol::GrubLegacy);
        assert_eq!(result.boot_mode, BootMode::Graphic);
    }

    #[test]
    fn test_parse_grub_efi_graphic_vnc() {
        let toml_str = r#"
        boot-protocol = "grub-efi"
        boot-mode = "graphic-vnc"
        hypervisor = "qemu"
        "#;

        let result = parse_boot_metadata(toml_str).unwrap();
        assert_eq!(result.boot_protocol, BootProtocol::GrubEFI);
        assert_eq!(result.boot_mode, BootMode::GraphicVnc);
    }

    #[test]
    fn test_parse_direct_no_graphic() {
        let toml_str = r#"
        boot-protocol = "direct"
        boot-mode = "no-graphic"
        hypervisor = "qemu"
        "#;

        let result = parse_boot_metadata(toml_str).unwrap();
        assert_eq!(result.boot_protocol, BootProtocol::Direct);
        assert_eq!(result.boot_mode, BootMode::NoGraphic);
    }

    #[test]
    fn test_parse_dragon_stub_graphic() {
        let toml_str = r#"
        boot-protocol = "dragon-stub"
        boot-mode = "graphic"
        hypervisor = "qemu"
        "#;

        let result = parse_boot_metadata(toml_str).unwrap();
        assert_eq!(result.boot_protocol, BootProtocol::DragonStub);
        assert_eq!(result.boot_mode, BootMode::Graphic);
    }

    #[test]
    fn test_parse_missing_boot_protocol() {
        let toml_str = r#"
        boot-mode = "graphic"
        "#;

        let r = parse_boot_metadata(toml_str);
        assert!(r.is_err());
        let r = r.unwrap_err();
        assert_missing_field(&r.to_string(), "boot-protocol");
    }

    #[test]
    fn test_parse_missing_boot_mode() {
        let toml_str = r#"
        boot-protocol = "grub-legacy"
        hypervisor = "qemu"
        "#;

        let r = parse_boot_metadata(toml_str);
        assert!(r.is_err());
        let r = r.unwrap_err();
        assert_missing_field(&r.to_string(), "boot-mode");
    }

    #[test]
    fn test_parse_invalid_boot_protocol() {
        let toml_str = r#"
        boot-protocol = "invalid-protocol"
        boot-mode = "graphic"
        "#;
        let r = parse_boot_metadata(toml_str);
        assert!(r.is_err());
        let r = r.unwrap_err();
        assert_unknown_variant(&r.to_string(), "invalid-protocol");
    }

    #[test]
    fn test_parse_invalid_boot_mode() {
        let toml_str = r#"
        boot-protocol = "grub-legacy"
        boot-mode = "invalid-mode"
        "#;

        let r = parse_boot_metadata(toml_str);
        assert!(r.is_err());
        let r = r.unwrap_err();
        assert_unknown_variant(&r.to_string(), "invalid-mode");
    }

    #[test]
    fn test_parse_empty_fields() {
        let toml_str = r#"
        boot-protocol = ""
        boot-mode = ""
        "#;

        assert!(parse_boot_metadata(toml_str).is_err());
    }
}
