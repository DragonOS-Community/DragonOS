//! This file contains the configuration for qemu.
//!
//! The file is partitially taken from asterinas osdk, and is Licensed under both GPL-2.0 and MPL-2.0.
//! (GPL-2.0 is compatible with MPL-2.0)
//! https://www.gnu.org/licenses/license-list.zh-cn.html#MPL-2.0

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::{
    common::target_arch::TargetArch,
    utils::{apply_kv_array, get_key, split_to_kv_array},
};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct QemuConfig {
    /// Path prefix for qemu binary.
    ///
    /// If not set, the default path will be used.
    ///
    /// Example:
    /// Fill in `/usr/bin/qemu-system-`,
    /// then for the `x86_64` architecture, `/usr/bin/qemu-system-x86_64` will be used.
    #[serde(rename = "path-prefix")]
    path_prefix: Option<String>,
    /// Arguments to pass to qemu.
    args: String,

    /// Parameters to apply when no-graphic is enabled
    #[serde(rename = "no-graphic-args")]
    pub no_graphic_args: String,

    /// Hardware acceleration
    accelerate: Option<QemuAccel>,
}

impl QemuConfig {
    /// Get the path to the qemu binary
    pub fn path(&self, arch: TargetArch) -> String {
        let arch_name: &str = arch.into();
        if let Some(prefix) = &self.path_prefix {
            format!("{}{}", prefix, arch_name)
        } else {
            format!("qemu-system-{}", arch_name)
        }
    }

    /// Apply the arguments to the qemu configuration
    pub fn apply_qemu_args(&mut self, args: &Vec<String>) -> Result<()> {
        let mut joined =
            split_to_kv_array(&self.args).map_err(|e| anyhow!("apply_qemu_args: {:?}", e))?;

        // Check the soundness of qemu arguments
        for arg in joined.iter() {
            check_qemu_arg(arg).map_err(|e| anyhow!("apply_qemu_args: {:?}", e))?;
        }
        log::warn!("apply_qemu_args: joined: {:?}", joined);

        apply_kv_array(&mut joined, args, " ", MULTI_VALUE_KEYS, SINGLE_VALUE_KEYS)?;

        self.args = joined.join(" ");
        Ok(())
    }

    /// Get the arguments to pass to qemu
    pub fn args(&self) -> String {
        self.args.clone()
    }

    /// Get the hardware acceleration configuration
    pub fn accelerate(&self) -> QemuAccel {
        self.accelerate.clone().unwrap_or(QemuAccel::None)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum QemuAccel {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "kvm")]
    Kvm,
    #[serde(rename = "hvf")]
    Hvf,
    #[serde(rename = "tcg")]
    Tcg,
}

// Below are checked keys in qemu arguments. The key list is non-exhaustive.

/// Keys with multiple values
const MULTI_VALUE_KEYS: &[&str] = &[
    "-device", "-chardev", "-object", "-netdev", "-drive", "-cdrom",
];
/// Keys with only single value
const SINGLE_VALUE_KEYS: &[&str] = &["-cpu", "-machine", "-m", "-serial", "-monitor", "-display"];
/// Keys with no value
const NO_VALUE_KEYS: &[&str] = &["--no-reboot", "-nographic", "-enable-kvm"];
/// Keys are not allowed to set in configuration files and command line
const NOT_ALLOWED_TO_SET_KEYS: &[&str] = &["-kernel", "-append", "-initrd"];

fn check_qemu_arg(arg: &str) -> Result<()> {
    let key = if let Some(key) = get_key(arg, " ") {
        key
    } else {
        arg.to_string()
    };

    if NOT_ALLOWED_TO_SET_KEYS.contains(&key.as_str()) {
        return Err(anyhow!("`{}` is not allowed to set", arg));
    }

    if NO_VALUE_KEYS.contains(&key.as_str()) && key.as_str() != arg {
        return Err(anyhow!("`{}` cannot have value", arg));
    }

    if (SINGLE_VALUE_KEYS.contains(&key.as_str()) || MULTI_VALUE_KEYS.contains(&key.as_str()))
        && key.as_str() == arg
    {
        return Err(anyhow!("`{}` must have value", arg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qemu_config_path() {
        let config = QemuConfig {
            path_prefix: Some("/usr/bin/qemu-system-".to_string()),
            args: "".to_string(),
            ..Default::default()
        };

        assert_eq!(
            config.path(TargetArch::X86_64),
            "/usr/bin/qemu-system-x86_64"
        );
        assert_eq!(
            config.path(TargetArch::RiscV64),
            "/usr/bin/qemu-system-riscv64"
        );
    }

    #[test]
    fn test_qemu_config_path_default() {
        let config = QemuConfig {
            path_prefix: None,
            args: "".to_string(),
            ..Default::default()
        };

        assert_eq!(config.path(TargetArch::X86_64), "qemu-system-x86_64");
        assert_eq!(config.path(TargetArch::RiscV64), "qemu-system-riscv64");
    }

    #[test]
    fn test_apply_qemu_args() -> Result<()> {
        let mut config = QemuConfig {
            path_prefix: None,
            args: "-m 1G -nographic".to_string(),
            ..Default::default()
        };

        let args = vec!["-m 2G".to_string(), "-enable-kvm".to_string()];
        config.apply_qemu_args(&args)?;

        assert_eq!(config.args, "-m 2G -nographic -enable-kvm");
        Ok(())
    }

    #[test]
    fn test_apply_qemu_args_invalid() {
        // 不允许直接设置 -kernel
        let mut config = QemuConfig {
            path_prefix: None,
            args: "-kernel path/to/kernel".to_string(),
            ..Default::default()
        };

        let args = vec!["".to_string()];
        let result = config.apply_qemu_args(&args);

        assert!(result.is_err());
    }

    #[test]
    fn test_check_qemu_arg_valid() -> Result<()> {
        assert!(check_qemu_arg("-m 1G").is_ok());
        assert!(check_qemu_arg("-nographic").is_ok());
        Ok(())
    }

    #[test]
    fn test_check_qemu_arg_invalid() {
        assert!(check_qemu_arg("-kernel path/to/kernel").is_err());
        assert!(check_qemu_arg("-m").is_err());
        assert!(check_qemu_arg("-nographic value").is_err());
    }

    #[test]
    fn test_apply_qemu_args_multi_value_keys() -> Result<()> {
        let mut config = QemuConfig {
            path_prefix: None,
            args: "-device virtio-net-pci,netdev=net0 -netdev user,id=net0".to_string(),
            ..Default::default()
        };

        let args = vec![
            "-device virtio-net-pci,netdev=net1".to_string(),
            "-netdev user,id=net1".to_string(),
        ];
        config.apply_qemu_args(&args)?;

        assert_eq!(
            config.args,
            "-device virtio-net-pci,netdev=net0 -device virtio-net-pci,netdev=net1 -netdev user,id=net0 -netdev user,id=net1"
        );
        Ok(())
    }

    #[test]
    fn test_apply_qemu_args_multi_value_keys_invalid() {
        let mut config = QemuConfig {
            path_prefix: None,
            args: "-device virtio-net-pci,netdev=net0".to_string(),
            ..Default::default()
        };

        let args = vec!["-device".to_string()];
        let result = config.apply_qemu_args(&args);

        assert!(result.is_err());
    }

    #[test]
    fn test_check_qemu_arg_multi_value_keys_valid() -> Result<()> {
        assert!(check_qemu_arg("-device virtio-net-pci,netdev=net0").is_ok());
        assert!(check_qemu_arg("-chardev socket,id=chr0,path=/tmp/qemu.sock").is_ok());
        Ok(())
    }

    #[test]
    fn test_check_qemu_arg_multi_value_keys_invalid() {
        assert!(check_qemu_arg("-device").is_err());
        assert!(check_qemu_arg("-chardev").is_err());
    }

    #[test]
    fn test_qemu_config_args() {
        let mut config = QemuConfig {
            path_prefix: None,
            args: "-m 1G -nographic".to_string(),
            ..Default::default()
        };

        config.apply_qemu_args(&vec![]).unwrap();
        assert_eq!(config.args(), "-m 1G -nographic");
    }

    #[test]
    fn test_qemu_accelerate_args() {
        let s = r#""kvm""#;
        let result: Result<QemuAccel, serde_json::Error> = serde_json::from_str(s);
        match result {
            Ok(QemuAccel::Kvm) => assert!(true),
            _ => assert!(false, "Expected Ok(QemuAccel::Kvm) but got {:?}", result),
        }

        let s = r#""tcg""#;
        let result: Result<QemuAccel, serde_json::Error> = serde_json::from_str(s);
        match result {
            Ok(QemuAccel::Tcg) => assert!(true),
            _ => assert!(false, "Expected Ok(QemuAccel::Tcg) but got {:?}", result),
        }

        let s = r#""none""#;
        let result: Result<QemuAccel, serde_json::Error> = serde_json::from_str(s);
        match result {
            Ok(QemuAccel::None) => assert!(true),
            _ => assert!(false, "Expected Ok(QemuAccel::None) but got {:?}", result),
        }

        let s = r#""hvf""#;
        let result: Result<QemuAccel, serde_json::Error> = serde_json::from_str(s);
        match result {
            Ok(QemuAccel::Hvf) => assert!(true),
            _ => assert!(false, "Expected Ok(QemuAccel::Hvf) but got {:?}", result),
        }

        let s = r#""invalid""#;
        let result: Result<QemuAccel, serde_json::Error> = serde_json::from_str(s);
        assert!(result.is_err(), "Expected Err but got {:?}", result);
    }
}
