#![allow(dead_code)]
use core::arch::x86_64::_popcnt64;

/// @brief ffz - 寻找u64中的第一个0所在的位（从第0位开始寻找）
/// 请注意，如果x中没有0,那么结果将是未定义的。请确保传入的x至少存在1个0
///
/// @param x 目标u64
/// @return i32 bit-number(0..63) of the first (least significant) zero bit.
#[inline]
pub fn ffz(x: u64) -> i32 {
    return unsafe { _popcnt64((x & ((!x) - 1)).try_into().unwrap()) };
}
