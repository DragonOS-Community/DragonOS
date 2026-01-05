use crate::alloc::vec::Vec;

/// 合并连续的整数为范围
///
/// ## 参数
/// - `set`: 整数集合，必须已排序
///
/// ## 返回值
/// - `Vec<(start, count)>`: 范围列表
///
/// ## 示例
/// ```
/// merge_ranges(&[1, 2, 3, 5, 6, 8])
/// // 返回 [(1, 3), (5, 2), (8, 1)]
/// ```
pub fn merge_ranges(set: &[usize]) -> Vec<(usize, usize)> {
    if set.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = set[0];
    let mut count = 1;

    for &page_index in &set[1..] {
        if page_index == start + count {
            count += 1; // 连续，扩展范围
        } else {
            ranges.push((start, count)); // 保存当前范围
            start = page_index;
            count = 1;
        }
    }

    ranges.push((start, count)); // 最后一个范围
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        // 边界情况：空输入
        let result = merge_ranges(&[]);
        assert_eq!(result, vec![]);
    }

    #[test]
    fn test_single_page() {
        // 边界情况：单个页面
        let result = merge_ranges(&[5]);
        assert_eq!(result, vec![(5, 1)]);
    }

    #[test]
    fn test_fully_consecutive() {
        // 完全连续的页面
        let result = merge_ranges(&[1, 2, 3, 4, 5]);
        assert_eq!(result, vec![(1, 5)]);
    }

    #[test]
    fn test_no_consecutive() {
        // 完全不连续的页面
        let result = merge_ranges(&[1, 3, 5, 7, 9]);
        assert_eq!(result, vec![(1, 1), (3, 1), (5, 1), (7, 1), (9, 1)]);
    }

    #[test]
    fn test_mixed_ranges() {
        // 文档示例：混合连续和不连续
        let result = merge_ranges(&[1, 2, 3, 5, 6, 8]);
        assert_eq!(result, vec![(1, 3), (5, 2), (8, 1)]);
    }

    #[test]
    fn test_two_consecutive_pairs() {
        // 两对连续的范围
        let result = merge_ranges(&[0, 1, 10, 11]);
        assert_eq!(result, vec![(0, 2), (10, 2)]);
    }

    #[test]
    fn test_start_from_zero() {
        // 从 0 开始的连续范围
        let result = merge_ranges(&[0, 1, 2]);
        assert_eq!(result, vec![(0, 3)]);
    }

    #[test]
    fn test_large_gap() {
        // 大间隔的不连续页面
        let result = merge_ranges(&[0, 100, 200]);
        assert_eq!(result, vec![(0, 1), (100, 1), (200, 1)]);
    }

    #[test]
    fn test_long_consecutive_sequence() {
        // 长连续序列（模拟大文件预读）
        let input: Vec<usize> = (0..128).collect();
        let result = merge_ranges(&input);
        assert_eq!(result, vec![(0, 128)]);
    }

    #[test]
    fn test_alternating_pattern() {
        // 交替的连续和间断模式
        let result = merge_ranges(&[1, 2, 4, 5, 7, 8, 10]);
        assert_eq!(result, vec![(1, 2), (4, 2), (7, 2), (10, 1)]);
    }

    #[test]
    fn test_real_world_sparse() {
        // 真实场景：稀疏的页面缓存（部分页面已存在）
        let result = merge_ranges(&[0, 5, 6, 7, 12, 20, 21]);
        assert_eq!(result, vec![(0, 1), (5, 3), (12, 1), (20, 2)]);
    }
}
