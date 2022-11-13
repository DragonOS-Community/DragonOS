#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::libs::printk::__printk(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n");
    };
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// 指定颜色，彩色输出
/// @param FRcolor 前景色
/// @param BKcolor 背景色
#[macro_export]
macro_rules! printk_color {

    ($FRcolor:expr, $BKcolor:expr, $($arg:tt)*) => {
        use alloc;
        $crate::libs::printk::PrintkWriter.__write_string_color($FRcolor, $BKcolor, alloc::fmt::format(format_args!($($arg)*)).as_str())
    };
}
