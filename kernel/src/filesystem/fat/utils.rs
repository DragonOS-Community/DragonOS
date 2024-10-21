use alloc::string::String;
use core::char::REPLACEMENT_CHARACTER;

/// FAT文件系统保留开头的2个簇
pub const RESERVED_CLUSTERS: u32 = 2;

/// @brief 将u8转为ascii字符。
/// 当转码成功时，返回对应的ascii字符，否则返回Unicode占位符
pub(super) fn decode_u8_ascii(value: u8) -> char {
    if value <= 0x7f {
        return value as char;
    } else {
        // 如果不是ascii字符，则返回Unicode占位符 U+FFFD
        return REPLACEMENT_CHARACTER;
    }
}

/// 把名称转为inode缓存里面的key
#[inline(always)]
pub(super) fn to_search_name(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// 把名称转为inode缓存里面的key(输入为string，原地替换)
#[inline(always)]
pub(super) fn to_search_name_string(mut name: String) -> String {
    name.make_ascii_uppercase();
    name
}
