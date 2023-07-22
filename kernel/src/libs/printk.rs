#![allow(unused)]
use crate::include::bindings::bindings::{printk_color, BLACK, WHITE};
use ::core::ffi::c_char;
use alloc::vec::Vec;
use core::fmt;

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
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_YELLOW, $crate::libs::printk::COLOR_BLACK, "[ WARN ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!($($arg)*)) + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_RED, $crate::libs::printk::COLOR_BLACK, "[ ERROR ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!("({}:{})\t", file!(), line!())) +
                                                                alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}

#[macro_export]
macro_rules! kBUG {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_RED, $crate::libs::printk::COLOR_BLACK, "[ BUG ] ");
        $crate::libs::printk::PrintkWriter.__write_string((alloc::fmt::format(format_args!("({}:{})\t", file!(), line!())) +
                                                                alloc::fmt::format(format_args!($($arg)*)).as_str() + "\n").as_str())
    }
}

pub struct PrintkWriter;

impl PrintkWriter {
    /// 调用C语言编写的printk_color,并输出白底黑字（暂时只支持ascii字符）
    /// @param str: 要写入的字符
    pub fn __write_string(&mut self, s: &str) {
        let str_to_print = self.__utf8_to_ascii(s);
        unsafe {
            printk_color(WHITE, BLACK, str_to_print.as_ptr() as *const c_char);
        }
    }

    pub fn __write_string_color(&self, fr_color: u32, bk_color: u32, s: &str) {
        let str_to_print = self.__utf8_to_ascii(s);
        unsafe {
            printk_color(fr_color, bk_color, str_to_print.as_ptr() as *const c_char);
        }
    }

    /// 将s这个utf8字符串，转换为ascii字符串
    /// @param s 待转换的utf8字符串
    /// @return Vec<u8> 转换结束后的Ascii字符串
    pub fn __utf8_to_ascii(&self, s: &str) -> Vec<u8> {
        let mut ascii_str: Vec<u8> = Vec::with_capacity(s.len() + 1);
        for byte in s.bytes() {
            match byte {
                0..=127 => {
                    ascii_str.push(byte);
                }
                _ => {}
            }
        }
        ascii_str.push(b'\0');
        return ascii_str;
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
    use fmt::Write;
    PrintkWriter.write_fmt(args).unwrap();
}
