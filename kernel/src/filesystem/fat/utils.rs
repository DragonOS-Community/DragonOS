use core::char::REPLACEMENT_CHARACTER;

/// FAT文件系统保留开头的2个簇
pub const RESERVED_CLUSTERS: u32 = 2;

/// @brief 将u8转为ascii字符。
/// 当转码成功时，返回对应的ascii字符，否则返回Unicode占位符
pub fn decode_u8_ascii(value: u8) -> char {
    if value <= 0x7f {
        return value as char;
    } else {
        // 如果不是ascii字符，则返回Unicode占位符 U+FFFD
        return REPLACEMENT_CHARACTER;
    }
}
