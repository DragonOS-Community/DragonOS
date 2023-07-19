#![allow(unused)]
use crate::{
    driver::uart::uart::c_uart_send_str,
    include::bindings::bindings::{printk_color, BLACK, WHITE},
};
use ::core::ffi::c_char;
use alloc::vec::Vec;
use core::{
    fmt::{self, Write},
    intrinsics::{likely, unlikely},
    sync::atomic::{AtomicBool, Ordering},
};

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
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ DEBUG ] ({}:{})\t{}\n", file!(), line!(),format_args!($($arg)*)))

    }
}

#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ INFO ] ({}:{})\t{}\n", file!(), line!(),format_args!($($arg)*)))
    }
}

#[macro_export]
macro_rules! kwarn {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_YELLOW, $crate::libs::printk::COLOR_BLACK, "[ WARN ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t{}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_RED, $crate::libs::printk::COLOR_BLACK, "[ ERROR ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t{}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kBUG {
    ($($arg:tt)*) => {
        $crate::libs::printk::PrintkWriter.__write_string_color($crate::libs::printk::COLOR_RED, $crate::libs::printk::COLOR_BLACK, "[ BUG ] ");
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t{}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

pub struct PrintkWriter;

/// 由于内存管理初始化完成之前，无法使用动态内存分配，所以需要在内存管理初始化完成之后才能使用动态内存分配
static ALLOW_ALLOC_ATOMIC: AtomicBool = AtomicBool::new(false);
static mut ALLOW_ALLOC_BOOL: bool = false;

impl PrintkWriter {
    #[inline]
    pub fn __write_fmt(&mut self, args: fmt::Arguments) {
        self.write_fmt(args);
    }

    /// 调用C语言编写的printk_color,并输出白底黑字（暂时只支持ascii字符）
    /// @param str: 要写入的字符
    pub fn __write_string(&mut self, s: &str) {
        if unlikely(!self.allow_alloc()) {
            self.__write_string_on_stack(s);
            return;
        }
        let str_to_print = self.__utf8_to_ascii(s);
        unsafe {
            printk_color(WHITE, BLACK, str_to_print.as_ptr() as *const c_char);
        }
    }

    pub fn __write_string_color(&self, fr_color: u32, bk_color: u32, s: &str) {
        if unlikely(!self.allow_alloc()) {
            self.__write_string_on_stack(s);
            return;
        }

        let str_to_print = self.__utf8_to_ascii(s);
        unsafe {
            printk_color(fr_color, bk_color, str_to_print.as_ptr() as *const c_char);
        }
    }

    #[inline]
    fn allow_alloc(&self) -> bool {
        // 由于allow_alloc只可能由false变为true
        // 因此采用两种方式读取它，一种是原子操作，一种是普通的bool，以优化性能。
        if likely(unsafe { ALLOW_ALLOC_BOOL }) {
            return true;
        } else {
            return ALLOW_ALLOC_ATOMIC.load(Ordering::SeqCst);
        }
    }

    /// 允许动态内存分配
    pub fn enable_alloc(&self) {
        ALLOW_ALLOC_ATOMIC.store(true, Ordering::SeqCst);
        unsafe {
            ALLOW_ALLOC_BOOL = true;
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

    fn __write_string_on_stack(&self, s: &str) {
        let s_len = s.len();
        assert!(s_len < 1024, "s_len is too long");
        let mut str_to_print: [u8; 1024] = [0; 1024];
        let mut i = 0;
        for byte in s.bytes() {
            match byte {
                0..=127 => {
                    str_to_print[i] = byte;
                    i += 1;
                }
                _ => {}
            }
        }
        str_to_print[i] = b'\0';
        unsafe {
            printk_color(WHITE, BLACK, str_to_print.as_ptr() as *const c_char);
        }
    }

    fn __write_string_color_on_stack(&self, fr_color: u32, bk_color: u32, s: &str) {
        let s_len = s.len();
        assert!(s_len < 1024, "s_len is too long");
        let mut str_to_print: [u8; 1024] = [0; 1024];
        let mut i = 0;
        for byte in s.bytes() {
            match byte {
                0..=127 => {
                    str_to_print[i] = byte;
                    i += 1;
                }
                _ => {}
            }
        }
        str_to_print[i] = b'\0';
        unsafe {
            printk_color(fr_color, bk_color, str_to_print.as_ptr() as *const c_char);
        }
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
