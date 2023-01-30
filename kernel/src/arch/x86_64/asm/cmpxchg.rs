// 该函数在cmpxchg.c中实现
extern "C" {
    fn __try_cmpxchg_q(ptr: *mut u64, old_ptr: *mut u64, new_ptr: *mut u64) -> bool;
}

/// @brief 封装lock cmpxchg指令
/// 由于Rust实现这部分的内联汇编比较麻烦（实在想不出办法），因此使用C的实现。
#[inline]
pub unsafe fn try_cmpxchg_q(ptr: *mut u64, old_ptr: *mut u64, new_ptr: *mut u64) -> bool {
    let retval = __try_cmpxchg_q(ptr, old_ptr, new_ptr);
    return retval;
}
