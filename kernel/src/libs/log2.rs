/// 将数值向上舍入到最近的2的幂次
///
/// 示例:
/// - roundup_pow_of_two(5) = 8
/// - roundup_pow_of_two(8) = 8
/// - roundup_pow_of_two(33) = 64
#[inline]
pub const fn round_up_pow_of_two(n: usize) -> usize {
    if n < 2 {
        return 1;
    }

    1usize << (usize::BITS - (n - 1).leading_zeros())
}

#[inline]
#[allow(unused)]
pub const fn round_down_pow_of_two(n: usize) -> usize {
    if n < 2 {
        return n;
    }

    1usize << (usize::BITS - (n).leading_zeros() - 1)
}

/// 检查一个数是否是2的幂次
#[inline]
#[allow(unused)]
pub const fn is_power_of_two(n: usize) -> bool {
    n > 0 && (n & (n - 1)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundup_pow_of_two() {
        // 边界情况
        assert_eq!(round_up_pow_of_two(0), 1);
        assert_eq!(round_up_pow_of_two(1), 1);

        // 已经是2的幂次
        assert_eq!(round_up_pow_of_two(2), 2);
        assert_eq!(round_up_pow_of_two(8), 8);
        assert_eq!(round_up_pow_of_two(64), 64);
        assert_eq!(round_up_pow_of_two(1024), 1024);

        // 需要向上舍入
        assert_eq!(round_up_pow_of_two(3), 4);
        assert_eq!(round_up_pow_of_two(5), 8);
        assert_eq!(round_up_pow_of_two(9), 16);
        assert_eq!(round_up_pow_of_two(33), 64);
        assert_eq!(round_up_pow_of_two(100), 128);
    }

    #[test]
    fn test_const_evaluation() {
        // 这些会在编译时求值
        const VAL1: usize = round_up_pow_of_two(5);
        const VAL2: usize = round_up_pow_of_two(100);

        assert_eq!(VAL1, 8);
        assert_eq!(VAL2, 128);
    }
}
