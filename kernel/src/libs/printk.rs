use crate::include::bindings::bindings::{printk_color, BLACK, WHITE};
use ::core::ffi::c_char;
use alloc::vec::Vec;
use core::fmt;
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
