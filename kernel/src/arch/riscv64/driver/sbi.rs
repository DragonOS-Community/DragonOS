/// 向控制台打印字符串。
///
/// 该函数接受一个字节切片 `s` 作为输入，并迭代切片中的每个字节 `c`。
/// 然后调用 `sbi_rt::console_write_byte` 函数，将 `c` 的值作为参数传递给它。
///
/// # 安全性
///
/// 这个函数是安全的，因为对SBI环境的操作不涉及不安全内存的访问操作。
///
/// # 参数
///
/// * `s` - 表示要打印的字符串的字节切片。
///
/// # 示例
///
/// ```
/// let message = b"Hello, World!";
/// console_putstr(message);
/// ```
pub fn console_putstr(s: &[u8]) {
    for c in s {
        sbi_rt::console_write_byte(*c);
    }
}
