use core::fmt::{self, Write};

use super::lib_ui::textui::{textui_putstr, FontColor};

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
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ DEBUG ] ({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)))

    }
}

#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ INFO ] ({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)))
    }
}

#[macro_export]
macro_rules! kwarn {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::lib_ui::textui::FontColor::YELLOW, $crate::libs::lib_ui::textui::FontColor::BLACK, "[ WARN ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::lib_ui::textui::FontColor::RED, $crate::libs::lib_ui::textui::FontColor::BLACK, "[ ERROR ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kBUG {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::lib_ui::textui::FontColor::RED, $crate::libs::lib_ui::textui::FontColor::BLACK, "[ BUG ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

pub struct PrintkWriter;

impl PrintkWriter {
    #[inline]
    pub fn __write_fmt(&mut self, args: fmt::Arguments) {
        self.write_fmt(args).ok();
    }

    /// 并输出白底黑字
    /// @param str: 要写入的字符
    pub fn __write_string(&mut self, s: &str) {
        textui_putstr(s, FontColor::WHITE, FontColor::BLACK).ok();
    }

    pub fn __write_string_color(&self, fr_color: FontColor, bk_color: FontColor, s: &str) {
        textui_putstr(s, fr_color, bk_color).ok();
    }
}

/// 为Printk Writer实现core::fmt::Write, 使得能够借助Rust自带的格式化组件，格式化字符并输出
impl fmt::Write for PrintkWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.__write_string(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn __printk(args: fmt::Arguments) {
    PrintkWriter.write_fmt(args).unwrap();
}
