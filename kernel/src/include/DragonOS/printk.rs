#![allow(unused)]
// ====== 定义颜色 ======
/// 白色
pub const COLOR_WHITE: u32 = 0x00ffffff;
/// 黑色
pub const COLOR_BLACK: u32 = 0x00000000;
/// 红色
pub const COLOR_RED: u32 = 0x00ff0000;
/// 橙色
pub const COLOR_ORANGE: u32 = 0x00ff8000;
/// 黄色
pub const COLOR_YELLOW: u32 = 0x00ffff00;
/// 绿色
pub const COLOR_GREEN: u32 = 0x0000ff00;
/// 蓝色
pub const COLOR_BLUE: u32 = 0x000000ff;
/// 靛色
pub const COLOR_INDIGO: u32 = 0x0000ffff;
/// 紫色
pub const COLOR_PURPLE: u32 = 0x008000ff;

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

#[macro_export]
macro_rules! kdebug {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!("[ DEBUG ] ({}:{})\t", file!(), line!()))+
                                                                alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string((alloc::string::String::from("[ INFO ] ")+ alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kwarn {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::include::DragonOS::printk::COLOR_YELLOW, $crate::include::DragonOS::printk::COLOR_BLACK, "[ WARN ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!($($arg)*)) + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::include::DragonOS::printk::COLOR_RED, $crate::include::DragonOS::printk::COLOR_BLACK, "[ ERROR ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!("({}:{})\t", file!(), line!())) +
                                                                alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kBUG {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::include::DragonOS::printk::COLOR_RED, $crate::include::DragonOS::printk::COLOR_BLACK, "[ BUG ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!("({}:{})\t", file!(), line!())) +
                                                                alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}