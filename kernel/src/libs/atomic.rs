use core::ptr::read_volatile;

use crate::include::bindings::bindings::atomic_t;

/// @brief 原子的读取指定的原子变量的值
#[inline]
pub fn atomic_read(ato:*const atomic_t)-> i64{
    unsafe{
        return read_volatile(&(*ato).value);
    }
}