use crate::include::bindings::bindings::{printk_color, BLACK, WHITE};
use crate::include::printk::PRINTK_WRITER;
use ::core::ffi::c_char;
use core::fmt::Arguments;
pub struct PrintkWriter {}

impl PrintkWriter {
    /// 调用C语言编写的printk_color,并输出白底黑字（暂时只支持ascii字符）
    /// @param str: 要写入的字符
    fn __write_string(&mut self, s: &str) {
        unsafe {
            // printk_color(WHITE, BLACK, s.as_ptr() as *const c_char);
        }
    }
}

/// 为Printk Writer实现core::fmt::Write, 使得能够借助Rust自带的格式化组件，格式化字符并输出
impl core::fmt::Write for PrintkWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.__write_string(s);
        return Ok(());
    }
}

pub fn __printk(args: core::fmt::Arguments) {
    use core::fmt::Write;
    unsafe {
        PRINTK_WRITER.write_fmt(args).unwrap();
    }
}
