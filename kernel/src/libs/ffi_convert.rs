/// @brief 由bindgen生成的结构体转换成rust原生定义的结构体的特性
pub trait FFIBind2Rust<T> {
    /// 转换为不可变引用
    fn convert_ref(src: *const T) -> Option<&'static Self>;
    /// 转换为可变引用
    fn convert_mut(src: *mut T) -> Option<&'static mut Self>;
}



pub fn __convert_mut<'a, S, D>(src:*mut S) ->Option<&'a mut D>{
    return unsafe {
        core::mem::transmute::<
            *mut S,
            *mut D,
        >(src)
        .as_mut()
    };
}

pub fn __convert_ref<'a, S, D>(src:*const S) ->Option<&'a D>{
    return unsafe {
        core::mem::transmute::<
            *const S,
            *const D,
        >(src)
        .as_ref()
    };
}
