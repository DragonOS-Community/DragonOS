use serde::{Deserialize, Deserializer, Serialize};

/// 目标处理器架构
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum TargetArch {
    #[default]
    X86_64,
    RiscV64,
    AArch64,
    LoongArch64,
}

impl TargetArch {
    /// 期望的目标处理器架构（如果修改了枚举，那一定要修改这里）
    pub const EXPECTED: [&'static str; 4] = ["x86_64", "riscv64", "aarch64", "loongarch64"];
}

impl TryFrom<&str> for TargetArch {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "x86_64" => Ok(TargetArch::X86_64),
            "riscv64" => Ok(TargetArch::RiscV64),
            "aarch64" => Ok(TargetArch::AArch64),
            "loongarch64" => Ok(TargetArch::LoongArch64),
            _ => Err(format!("Unknown target arch: {}", value)),
        }
    }
}

impl From<TargetArch> for &str {
    fn from(val: TargetArch) -> Self {
        match val {
            TargetArch::X86_64 => "x86_64",
            TargetArch::RiscV64 => "riscv64",
            TargetArch::AArch64 => "aarch64",
            TargetArch::LoongArch64 => "loongarch64",
        }
    }
}

impl From<TargetArch> for String {
    fn from(val: TargetArch) -> Self {
        let x: &str = val.into();
        x.to_string()
    }
}

impl<'de> Deserialize<'de> for TargetArch {
    fn deserialize<D>(deserializer: D) -> Result<TargetArch, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        let r = TargetArch::try_from(s.as_str());
        match r {
            Ok(v) => Ok(v),
            Err(_) => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(s.as_str()),
                &format!("Expected one of {:?}", TargetArch::EXPECTED).as_str(),
            )),
        }
    }
}

impl Serialize for TargetArch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let string: String = Into::into(*self);
        serializer.serialize_str(string.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_default() {
        let default_arch = TargetArch::default();
        assert_eq!(default_arch, TargetArch::X86_64);
    }

    #[test]
    fn test_try_from_valid() {
        let x86_64 = TargetArch::try_from("x86_64").unwrap();
        assert_eq!(x86_64, TargetArch::X86_64);

        let riscv64 = TargetArch::try_from("riscv64").unwrap();
        assert_eq!(riscv64, TargetArch::RiscV64);

        let aarch64 = TargetArch::try_from("aarch64").unwrap();
        assert_eq!(aarch64, TargetArch::AArch64);

        let loongarch64 = TargetArch::try_from("loongarch64").unwrap();
        assert_eq!(loongarch64, TargetArch::LoongArch64);
    }

    #[test]
    fn test_try_from_invalid() {
        let result = TargetArch::try_from("unknown");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Unknown target arch: unknown");
    }

    #[test]
    fn test_into_str() {
        let x86_64: &str = TargetArch::X86_64.into();
        assert_eq!(x86_64, "x86_64");

        let riscv64: &str = TargetArch::RiscV64.into();
        assert_eq!(riscv64, "riscv64");

        let aarch64: &str = TargetArch::AArch64.into();
        assert_eq!(aarch64, "aarch64");

        let loongarch64: &str = TargetArch::LoongArch64.into();
        assert_eq!(loongarch64, "loongarch64");
    }

    #[test]
    fn test_into_string() {
        let x86_64: String = TargetArch::X86_64.into();
        assert_eq!(x86_64, "x86_64");

        let riscv64: String = TargetArch::RiscV64.into();
        assert_eq!(riscv64, "riscv64");
    }

    #[test]
    fn test_deserialize_valid() {
        let json_x86_64 = r#""x86_64""#;
        let x86_64: TargetArch = serde_json::from_str(json_x86_64).unwrap();
        assert_eq!(x86_64, TargetArch::X86_64);

        let json_riscv64 = r#""riscv64""#;
        let riscv64: TargetArch = serde_json::from_str(json_riscv64).unwrap();
        assert_eq!(riscv64, TargetArch::RiscV64);

        let json_aarch64 = r#""aarch64""#;
        let aarch64: TargetArch = serde_json::from_str(json_aarch64).unwrap();

        assert_eq!(aarch64, TargetArch::AArch64);

        let json_loongarch64 = r#""loongarch64""#;
        let loongarch64: TargetArch = serde_json::from_str(json_loongarch64).unwrap();
        assert_eq!(loongarch64, TargetArch::LoongArch64);
    }

    #[test]
    fn test_deserialize_invalid() {
        let json_unknown = r#""unknown""#;
        let result: Result<TargetArch, _> = serde_json::from_str(json_unknown);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize() {
        let x86_64 = TargetArch::X86_64;
        let serialized_x86_64 = serde_json::to_string(&x86_64).unwrap();
        assert_eq!(serialized_x86_64, r#""x86_64""#);

        let riscv64 = TargetArch::RiscV64;
        let serialized_riscv64 = serde_json::to_string(&riscv64).unwrap();
        assert_eq!(serialized_riscv64, r#""riscv64""#);
    }
}
