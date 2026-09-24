use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, Ordering};

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
    // 该函数可能在 `SbiDriver::early_init` 之前被调用（例如 `scm_init`
    // 的早期日志），此时扩展还没有探测过，先补一次探测，避免误走 legacy 分支。
    SbiDriver::ensure_probed();

    // 优先使用 Debug Console 扩展（DBCN）。新版 RustSBI 已经移除了
    // 废弃的 legacy console（EID 0x01），如果只用 `legacy::console_putchar`，
    // 在它上面运行时会一直空转且没有任何输出。
    if SbiDriver::extensions().contains(SBIExtensions::CONSOLE) {
        for &c in s {
            match c {
                b'\n' => {
                    sbi_rt::console_write_byte(b'\r');
                    sbi_rt::console_write_byte(b'\n');
                }
                b'\r' => {
                    sbi_rt::console_write_byte(b'\r');
                }
                _ => {
                    sbi_rt::console_write_byte(c);
                }
            }
        }
        return;
    }

    // 回退到 legacy console，兼容未实现 DBCN 的 SBI 实现（例如 OpenSBI）。
    for &c in s {
        match c {
            b'\n' => {
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(b'\r' as usize);
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(b'\n' as usize);
            }
            b'\r' => {
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(b'\r' as usize);
            }
            _ => {
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(c as usize);
            }
        }
    }
}

bitflags! {
    pub struct SBIExtensions: u64 {
        /// RISC-V SBI Base extension.
        const BASE = 1 << 0;
        /// Timer programmer extension.
        const TIME = 1 << 1;
        /// Inter-processor Interrupt extension.
        const SPI = 1 << 2;
        /// Remote Fence extension.
        const RFENCE = 1 << 3;
        /// Hart State Monitor extension.
        const HSM = 1 << 4;
        /// System Reset extension.
        const RESET = 1 << 5;
        /// Performance Monitoring Unit extension.
        const PMU = 1 << 6;
        /// Debug Console extension.
        const CONSOLE = 1 << 7;
        /// System Suspend extension.
        const SUSPEND = 1 << 8;
        /// SBI CPPC extension.
        const CPPC = 1 << 9;
        /// Nested Acceleration extension.
        const NACL = 1 << 10;
        /// Steal-time Accounting extension.
        const STA = 1 << 11;
    }
}

static mut EXTENSIONS: SBIExtensions = SBIExtensions::empty();

/// `early_init` 是否已经完成过一次扩展探测。
static PROBED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct SbiDriver;

impl SbiDriver {
    #[inline(never)]
    pub fn early_init() {
        unsafe {
            EXTENSIONS = Self::probe_extensions();
        }
        PROBED.store(true, Ordering::SeqCst);
    }

    /// 如果还没有探测过扩展，就先探测一次。
    ///
    /// 早期日志（如 `scm_init`）发生在 `early_init` 之前，需要在第一次输出前
    /// 保证扩展信息可用，否则会错误地回退到 legacy console。
    #[inline]
    fn ensure_probed() {
        if !PROBED.load(Ordering::SeqCst) {
            Self::early_init();
        }
    }

    /// 获取probe得到的SBI扩展信息。
    pub fn extensions() -> &'static SBIExtensions {
        unsafe { addr_of!(EXTENSIONS).as_ref().unwrap() }
    }

    fn probe_extensions() -> SBIExtensions {
        let mut extensions = SBIExtensions::empty();
        if sbi_rt::probe_extension(sbi_rt::Base).is_available() {
            extensions |= SBIExtensions::BASE;
        }
        if sbi_rt::probe_extension(sbi_rt::Timer).is_available() {
            extensions |= SBIExtensions::TIME;
        }

        if sbi_rt::probe_extension(sbi_rt::Ipi).is_available() {
            extensions |= SBIExtensions::SPI;
        }

        if sbi_rt::probe_extension(sbi_rt::Fence).is_available() {
            extensions |= SBIExtensions::RFENCE;
        }

        if sbi_rt::probe_extension(sbi_rt::Hsm).is_available() {
            extensions |= SBIExtensions::HSM;
        }

        if sbi_rt::probe_extension(sbi_rt::Reset).is_available() {
            extensions |= SBIExtensions::RESET;
        }

        if sbi_rt::probe_extension(sbi_rt::Pmu).is_available() {
            extensions |= SBIExtensions::PMU;
        }

        if sbi_rt::probe_extension(sbi_rt::Console).is_available() {
            extensions |= SBIExtensions::CONSOLE;
        }

        if sbi_rt::probe_extension(sbi_rt::Suspend).is_available() {
            extensions |= SBIExtensions::SUSPEND;
        }

        if sbi_rt::probe_extension(sbi_rt::Cppc).is_available() {
            extensions |= SBIExtensions::CPPC;
        }

        if sbi_rt::probe_extension(sbi_rt::Nacl).is_available() {
            extensions |= SBIExtensions::NACL;
        }

        if sbi_rt::probe_extension(sbi_rt::Sta).is_available() {
            extensions |= SBIExtensions::STA;
        }

        return extensions;
    }
}
