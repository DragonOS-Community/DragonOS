use crate::{include::bindings::bindings::{atomic_inc, atomic_t, atomic_dec}, kwarn};

use super::{ffi_convert::{FFIBind2Rust, __convert_mut, __convert_ref}, atomic::atomic_read};

#[derive(Debug, Copy, Clone)]
pub struct RefCount {
    pub refs: atomic_t,
}

/// @brief 将给定的来自bindgen的refcount_t解析为Rust的RefCount的引用
impl FFIBind2Rust<crate::include::bindings::bindings::refcount_struct> for RefCount{
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::refcount_struct,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::refcount_struct,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}

/// @brief 以指定的值初始化refcount
macro_rules! REFCOUNT_INIT {
    ($x:expr) => {
        $crate::libs::refcount::RefCount {
            refs: atomic_t { value: $x },
        }
    };
}

/// @brief 引用计数自增1
#[allow(dead_code)]
#[inline]
pub fn refcount_inc(r: &mut RefCount) {
    if atomic_read(&r.refs) == 0{
        kwarn!("Refcount increased from 0, may be use-after free");
    }
    
    unsafe {
        atomic_inc(&mut r.refs);
    }
}

/// @brief 引用计数自减1
#[allow(dead_code)]
#[inline]
pub fn refcount_dec(r: &mut RefCount){
    unsafe{
        atomic_dec(&mut r.refs);
    }
}


