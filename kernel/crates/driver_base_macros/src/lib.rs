#![no_std]

/// 获取指定字段
///
/// 当weak指针的strong count为0的时候，清除弱引用
#[macro_export]
macro_rules! get_weak_or_clear {
    ($field:expr) => {{
        if let Some(x) = $field.clone() {
            if x.strong_count() == 0 {
                $field = None;
                None
            } else {
                Some(x)
            }
        } else {
            None
        }
    }};
}
