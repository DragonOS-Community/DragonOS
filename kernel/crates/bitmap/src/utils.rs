use crate::traits::BitOps;

/// 检查索引是否超出范围
/// 
/// ## 泛型
/// 
/// - `T`：每个位图元素的类型
/// 
/// ## 参数
/// 
/// - `index`：索引
/// - `elements`：位图元素个数
pub fn bmp_overflow<T: BitOps>(index: usize, elements: usize) -> bool {
    if core::intrinsics::unlikely(index >= elements * T::bit_size()) {
        return true;
    }
    return false;
}
