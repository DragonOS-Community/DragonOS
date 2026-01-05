#![no_std]
#![deny(clippy::all)]
#![allow(clippy::crate_in_macro_def)]

/// 定义一个bool类型的参数
///
/// # 参数
///
/// - `$varname`: 参数的变量名
/// - `$name`: 参数的名称
/// - `$default_bool`: 默认值
/// - `$inv`: 是否反转
#[macro_export]
macro_rules! kernel_cmdline_param_arg {
    ($varname:ident, $name:ident, $default_bool:expr, $inv:expr) => {
        #[::linkme::distributed_slice(crate::init::cmdline::KCMDLINE_PARAM_ARG)]
        static $varname: crate::init::cmdline::KernelCmdlineParameter =
            crate::init::cmdline::KernelCmdlineParamBuilder::new(
                stringify!($name),
                crate::init::cmdline::KCmdlineParamType::Arg,
            )
            .default_bool($default_bool)
            .inv($inv)
            .build()
            .unwrap();
    };
}

/// 定义一个key-value类型的参数
///
/// # 参数
/// - `$varname`: 参数的变量名
/// - `$name`: 参数的名称
/// - `$default_str`: 默认值
#[macro_export]
macro_rules! kernel_cmdline_param_kv {
    ($varname:ident, $name:ident, $default_str:expr) => {
        #[::linkme::distributed_slice(crate::init::cmdline::KCMDLINE_PARAM_KV)]
        static $varname: crate::init::cmdline::KernelCmdlineParameter =
            crate::init::cmdline::KernelCmdlineParamBuilder::new(
                stringify!($name),
                crate::init::cmdline::KCmdlineParamType::KV,
            )
            .default_str($default_str)
            .build()
            .unwrap();
    };
}

/// 定义一个内存管理初始化之前就要设置的key-value类型的参数
///
/// # 参数
/// - `$varname`: 参数的变量名
/// - `$name`: 参数的名称
/// - `$default_str`: 默认值
#[macro_export]
macro_rules! kernel_cmdline_param_early_kv {
    ($varname:ident, $name:ident, $default_str:expr) => {
        #[::linkme::distributed_slice(crate::init::cmdline::KCMDLINE_PARAM_EARLY_KV)]
        static $varname: crate::init::cmdline::KernelCmdlineParameter = {
            static ___KV: crate::init::cmdline::KernelCmdlineEarlyKV = {
                const { assert!($default_str.len() < KernelCmdlineEarlyKV::VALUE_MAX_LEN) };
                crate::init::cmdline::KernelCmdlineParamBuilder::new(
                    stringify!($name),
                    crate::init::cmdline::KCmdlineParamType::EarlyKV,
                )
                .default_str($default_str)
                .build_early_kv()
                .unwrap()
            };
            crate::init::cmdline::KernelCmdlineParameter::EarlyKV(&___KV)
        };
    };
}
