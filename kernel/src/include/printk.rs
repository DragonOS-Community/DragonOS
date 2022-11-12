use crate::libs::printk::PrintkWriter;

// 声明全局的printk输出器
pub static mut PRINTK_WRITER: PrintkWriter = PrintkWriter {};

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
