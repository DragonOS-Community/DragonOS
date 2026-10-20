use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct PartitionConfig {
    #[serde(rename = "type")]
    pub partition_type: PartitionType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
pub enum PartitionType {
    /// Disk image is not partitioned
    #[default]
    #[serde(rename = "none")]
    None,
    /// Use MBR partition table
    #[serde(rename = "mbr")]
    Mbr,
    /// Use GPT partition table
    #[serde(rename = "gpt")]
    Gpt,
}

impl PartitionConfig {
    /// Determines whether the disk image should be partitioned
    ///
    /// Returns `true` if the partition type is not `None`, otherwise returns `false`.
    pub fn image_should_be_partitioned(&self) -> bool {
        self.partition_type != PartitionType::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_partition_type() {
        let test_cases = vec![
            (r#"type = "none""#, PartitionType::None),
            (r#"type = "mbr""#, PartitionType::Mbr),
            (r#"type = "gpt""#, PartitionType::Gpt),
        ];

        for (config_content, expected_type) in test_cases {
            let partition_config: PartitionConfig = toml::from_str(config_content).unwrap();
            assert_eq!(partition_config.partition_type, expected_type);
        }
    }
}
