#![no_std]
#![allow(clippy::needless_return)]

use system_error::SystemError;
pub use unified_init_macros as macros;

/// 统一初始化器
#[derive(Debug)]
pub struct UnifiedInitializer {
    function: &'static UnifiedInitFunction,
    name: &'static str,
}

impl UnifiedInitializer {
    pub const fn new(
        name: &'static str,
        function: &'static UnifiedInitFunction,
    ) -> UnifiedInitializer {
        UnifiedInitializer { function, name }
    }

    /// 调用初始化函数
    pub fn call(&self) -> Result<(), SystemError> {
        (self.function)()
    }

    /// 获取初始化函数的名称
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

pub type UnifiedInitFunction = fn() -> core::result::Result<(), SystemError>;

/// 定义统一初始化器的分布式切片数组(私有)
#[macro_export]
macro_rules! define_unified_initializer_slice {
    ($name:ident) => {
        #[::linkme::distributed_slice]
        static $name: [::unified_init::UnifiedInitializer] = [..];
    };
    () => {
        compile_error!(
            "define_unified_initializer_slice! requires at least one argument: slice_name"
        );
    };
}

/// 定义统一初始化器的分布式切片数组(公开)
#[macro_export]
macro_rules! define_public_unified_initializer_slice {
    ($name:ident) => {
        #[::linkme::distributed_slice]
        pub static $name: [::unified_init::UnifiedInitializer] = [..];
    };
    () => {
        compile_error!(
            "define_unified_initializer_slice! requires at least one argument: slice_name"
        );
    };
}

/// 调用指定数组中的所有初始化器
#[macro_export]
macro_rules! unified_init {
    ($initializer_slice:ident) => {
        for initializer in $initializer_slice.iter() {
            initializer.call().unwrap_or_else(|e| {
                kerror!("Failed to call initializer {}: {:?}", initializer.name(), e);
            });
        }
    };
}
