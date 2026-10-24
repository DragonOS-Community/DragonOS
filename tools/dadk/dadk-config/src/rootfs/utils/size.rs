use serde::Deserializer;

use crate::rootfs::RootFSConfigFile;

/// 自定义反序列化函数，用于解析表示磁盘镜像大小的值。
///
/// 此函数支持两种输入格式：
/// 1. 纯数字：直接将其视为字节数。
/// 2. 带单位的字符串：如"1M"、"1G"，其中单位支持K（千字节）、M（兆字节）、G（千兆字节）。
///
/// 函数将输入值解析为`usize`类型，表示字节数。
///
/// # 参数
/// - `deserializer`: 一个实现了`Deserializer` trait的对象，用于读取和解析输入数据。
///
/// # 返回值
/// 返回一个`Result<usize, D::Error>`，其中：
/// - `Ok(usize)`表示解析成功，返回对应的字节数。
/// - `Err(D::Error)`表示解析失败，返回错误信息。
///
/// # 错误处理
/// - 如果输入是非法的字符串（无法解析或单位不合法），将返回自定义错误。
/// - 如果输入类型既不是整数也不是字符串，将返回类型错误。
pub fn deserialize_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    // 使用serde的deserialize_any方法来处理不同类型的输入
    let value = serde::de::Deserialize::deserialize(deserializer)?;

    // 匹配输入值的类型，进行相应的转换
    let r = match value {
        toml::Value::Integer(num) => {
            // 如果是整数类型，直接转换成usize
            Ok(num as usize)
        }
        toml::Value::String(s) => {
            // 如果是字符串类型，解析如"1M"这样的表示
            parse_size_from_string(&s)
                .ok_or_else(|| serde::de::Error::custom("Invalid string for size"))
        }
        _ => Err(serde::de::Error::custom("Invalid type for size")),
    };

    r.map(|size| (size + RootFSConfigFile::LBA_SIZE - 1) & !(RootFSConfigFile::LBA_SIZE - 1))
}

/// Parses a size string with optional unit suffix (K, M, G) into a usize value.
///
/// This function takes a string that represents a size, which can be a plain
/// number or a number followed by a unit suffix (K for kilobytes, M for megabytes,
/// G for gigabytes). It converts this string into an equivalent usize value in bytes.
///
/// # Parameters
/// - `size_str`: A string slice that contains the size to parse. This can be a simple
///   numeric string or a numeric string followed by a unit ('K', 'M', 'G').
///
/// # Returns
/// An `Option<usize>` where:
/// - `Some(usize)` contains the parsed size in bytes if the input string is valid.
/// - `None` if the input string is invalid or contains an unsupported unit.
fn parse_size_from_string(size_str: &str) -> Option<usize> {
    if size_str.chars().all(|c| c.is_ascii_digit()) {
        // 如果整个字符串都是数字，直接解析返回
        return size_str.parse::<usize>().ok();
    }

    let mut chars = size_str.chars().rev();
    let unit = chars.next()?;
    let number_str: String = chars.rev().collect();
    let number = number_str.parse::<usize>().ok()?;

    match unit.to_ascii_uppercase() {
        'K' => Some(number * 1024),
        'M' => Some(number * 1024 * 1024),
        'G' => Some(number * 1024 * 1024 * 1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size_from_string() {
        // 正常情况，不带单位
        assert_eq!(parse_size_from_string("1024"), Some(1024));

        // 正常情况，带有单位
        assert_eq!(parse_size_from_string("1K"), Some(1024));
        assert_eq!(parse_size_from_string("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_from_string("3G"), Some(3 * 1024 * 1024 * 1024));

        // 边界情况
        assert_eq!(parse_size_from_string("0K"), Some(0));
        assert_eq!(parse_size_from_string("0M"), Some(0));
        assert_eq!(parse_size_from_string("0G"), Some(0));

        // 小写情况
        assert_eq!(parse_size_from_string("1k"), Some(1024));
        assert_eq!(parse_size_from_string("2m"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_from_string("3g"), Some(3 * 1024 * 1024 * 1024));

        // 错误的单位
        assert_eq!(parse_size_from_string("1T"), None);
        assert_eq!(parse_size_from_string("2X"), None);

        // 错误的数字格式
        assert_eq!(parse_size_from_string("aK"), None);
        assert_eq!(parse_size_from_string("1.5M"), None);

        // 空字符串
        assert_eq!(parse_size_from_string(""), None);

        // 只单位没有数字
        assert_eq!(parse_size_from_string("K"), None);

        // 数字后有多余字符
        assert_eq!(parse_size_from_string("1KextrK"), None);
    }
}
