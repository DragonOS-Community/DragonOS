#![allow(dead_code)]
use core::ptr::{read_volatile, write_volatile};

use crate::include::bindings::bindings::atomic_t;

/// @brief 原子的读取指定的原子变量的值
#[inline]
pub fn atomic_read(ato: *const atomic_t) -> i64 {
    unsafe {
        return read_volatile(&(*ato).value);
    }
}

/// @brief 原子的设置原子变量的值
#[inline]
pub fn atomic_set(ato: *mut atomic_t, value: i64) {
    unsafe {
        write_volatile(&mut (*ato).value, value);
    }
}

impl Default for atomic_t {
    fn default() -> Self {
        Self { value: 0 }
    }
}
