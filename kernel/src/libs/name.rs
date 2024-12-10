use core::any::type_name;

use alloc::string::{String, ToString};

#[allow(dead_code)]
pub fn get_full_type_name<T>(_: &T) -> String {
    type_name::<T>().to_string()
}

pub fn get_type_name<T>(_: &T) -> String {
    let full_name = type_name::<T>();
    full_name[(full_name.rfind("::").unwrap_or(0) + 2)..].to_string()
}
