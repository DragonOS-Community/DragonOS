use self::legacy::console_putchar;

/// The SBI S-mode driver.
///
/// Some code takes from `https://github.com/repnop/sbi.git`
mod ecall;
pub mod legacy;

/// Error codes returned by SBI calls
///
/// note: `SBI_SUCCESS` is not represented here since this is to be used as the
/// error type in a `Result`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SbiError {
    /// The SBI call failed
    Failed,
    /// The SBI call is not implemented or the functionality is not available
    NotSupported,
    /// An invalid parameter was passed
    InvalidParameter,
    /// The SBI implementation has denied execution of the call functionality
    Denied,
    /// An invalid address was passed
    InvalidAddress,
    /// The resource is already available
    AlreadyAvailable,
    /// The resource was previously started
    AlreadyStarted,
    /// The resource was previously stopped
    AlreadyStopped,
}

impl SbiError {
    #[inline]
    fn new(n: isize) -> Self {
        match n {
            -1 => SbiError::Failed,
            -2 => SbiError::NotSupported,
            -3 => SbiError::InvalidParameter,
            -4 => SbiError::Denied,
            -5 => SbiError::InvalidAddress,
            -6 => SbiError::AlreadyAvailable,
            -7 => SbiError::AlreadyStarted,
            -8 => SbiError::AlreadyStopped,
            n => unreachable!("bad SBI error return value: {}", n),
        }
    }
}

impl core::fmt::Display for SbiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                SbiError::AlreadyAvailable => "resource is already available",
                SbiError::Denied => "SBI implementation denied execution",
                SbiError::Failed => "call to SBI failed",
                SbiError::InvalidAddress => "invalid address passed",
                SbiError::InvalidParameter => "invalid parameter passed",
                SbiError::NotSupported => "SBI call not implemented or functionality not available",
                SbiError::AlreadyStarted => "resource was already started",
                SbiError::AlreadyStopped => "resource was already stopped",
            }
        )
    }
}

/// 向控制台打印字符串。
///
/// 该函数接受一个字节切片 `s` 作为输入，并迭代切片中的每个字节 `c`。
/// 然后调用 `console_putchar` 函数，将 `c` 的值作为参数传递给它。
///
/// # 安全性
/// 该函数被标记为 `unsafe`，因为它调用了 `console_putchar` 函数，
/// 而假设该函数执行可能有副作用或违反内存安全的底层操作。
/// 调用者有责任确保 `s` 切片是有效的并且正确终止的。
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
pub unsafe fn console_putstr(s: &[u8]) {
    for c in s {
        unsafe { console_putchar(*c) };
    }
}
