use serde::Deserialize;

/// Supported hypervisor types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HypervisorType {
    Qemu,
    CloudHypervisor,
}

impl<'de> Deserialize<'de> for HypervisorType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "qemu" => Ok(HypervisorType::Qemu),
            "cloud-hypervisor" => Ok(HypervisorType::CloudHypervisor),
            _ => Err(serde::de::Error::custom(format!(
                "Unknown hypervisor type: {}",
                s
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{self};

    #[test]
    fn test_deserialize_qemu() {
        let json = r#""qemu""#;
        let hypervisor_type: HypervisorType = serde_json::from_str(json).unwrap();
        assert_eq!(hypervisor_type, HypervisorType::Qemu);
    }

    #[test]
    fn test_deserialize_cloud_hypervisor() {
        let json = r#""cloud-hypervisor""#;
        let hypervisor_type: HypervisorType = serde_json::from_str(json).unwrap();
        assert_eq!(hypervisor_type, HypervisorType::CloudHypervisor);
    }

    #[test]
    fn test_deserialize_invalid_type() {
        let json = r#""invalid-type""#;
        let result: Result<HypervisorType, _> = serde_json::from_str(json);
        assert!(result.is_err());
        let e = result.unwrap_err();
        assert!(e
            .to_string()
            .contains("Unknown hypervisor type: invalid-type"));
    }

    #[test]
    fn test_deserialize_case_insensitivity() {
        let json = r#""QeMu""#;
        let hypervisor_type: HypervisorType = serde_json::from_str(json).unwrap();
        assert_eq!(hypervisor_type, HypervisorType::Qemu);
    }

    #[test]
    fn test_deserialize_empty_string() {
        let json = r#""""#;
        let result: Result<HypervisorType, _> = serde_json::from_str(json);
        assert!(result.is_err());
        let e = result.unwrap_err();
        assert!(e.to_string().contains("Unknown hypervisor type: "));
    }
}
