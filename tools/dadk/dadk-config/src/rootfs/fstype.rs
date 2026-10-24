use serde::{Deserialize, Deserializer};

/// Possible filesystem types for rootfs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsType {
    Fat32,
    Ext4,
}

impl<'de> Deserialize<'de> for FsType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut s = String::deserialize(deserializer)?;
        s.make_ascii_lowercase();
        match s.as_str() {
            "fat32" => Ok(FsType::Fat32),
            "ext4" => Ok(FsType::Ext4),
            _ => Err(serde::de::Error::custom("invalid fs type")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{self, Value};

    fn deserialize_fs_type(input: &str) -> Result<FsType, serde_json::Error> {
        let json = Value::String(input.to_string());
        serde_json::from_value(json)
    }

    #[test]
    fn test_deserialize_fat32_lowercase() {
        let r = deserialize_fs_type("fat32");
        assert_eq!(r.is_ok(), true);
        let fs_type = r.unwrap();
        assert_eq!(fs_type, FsType::Fat32);
    }

    #[test]
    fn test_deserialize_fat32_mixed_case() {
        let r = deserialize_fs_type("FAT32");
        assert_eq!(r.is_ok(), true);
        let fs_type = r.unwrap();
        assert_eq!(fs_type, FsType::Fat32);
    }

    #[test]
    fn test_deserialize_ext4() {
        let r = deserialize_fs_type("ext4");
        assert_eq!(r.is_ok(), true);
        let fs_type = r.unwrap();
        assert_eq!(fs_type, FsType::Ext4);
    }

    #[test]
    fn testdeserialize_random_string() {
        assert!(deserialize_fs_type("abc123").is_err());
    }
}
