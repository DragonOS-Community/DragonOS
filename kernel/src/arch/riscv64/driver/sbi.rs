use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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
    SbiDriver::ensure_probed();

    // Use the Debug Console extension (DBCN) when the firmware advertises it;
    // fall back to the legacy console otherwise (e.g. OpenSBI). New RustSBI
    // builds no longer expose the deprecated legacy console (EID 0x01), so
    // relying on it alone would spin without printing anything.
    if SbiDriver::dbcn_usable() && dbcn_write(s) {
        return;
    }

    legacy_write(s);
}

/// Latched once a DBCN write has been rejected, so later output goes straight to
/// the legacy console instead of retrying a firmware that does not honour DBCN.
static DBCN_FAILED: AtomicBool = AtomicBool::new(false);

/// Writes `s` through the Debug Console extension, expanding `\n` to CRLF.
///
/// Returns `false` at the first rejected byte and latches [`DBCN_FAILED`] so the
/// caller falls back to the legacy console instead of losing output silently.
fn dbcn_write(s: &[u8]) -> bool {
    let write = |byte: u8| -> bool {
        if sbi_rt::console_write_byte(byte).is_err() {
            DBCN_FAILED.store(true, Ordering::Relaxed);
            return false;
        }
        true
    };

    for &c in s {
        let ok = match c {
            b'\n' => write(b'\r') && write(b'\n'),
            _ => write(c),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Writes `s` through the deprecated legacy console (EID 0x01).
fn legacy_write(s: &[u8]) {
    for &c in s {
        match c {
            b'\n' => {
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(b'\r' as usize);
                #[allow(deprecated)]
                sbi_rt::legacy::console_putchar(b'\n' as usize);
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

/// SBI extension probing state. A plain "done" flag is not enough: probing runs
/// ecalls, and a trap during probing may re-enter the console, which calls
/// `ensure_probed` again. Marking the probe in progress *before* it starts stops
/// that re-entry from recursing back into `probe_extensions`.
const PROBE_NOT_STARTED: u8 = 0;
const PROBE_IN_PROGRESS: u8 = 1;
const PROBE_DONE: u8 = 2;

static PROBE_STATE: AtomicU8 = AtomicU8::new(PROBE_NOT_STARTED);

#[derive(Debug)]
pub struct SbiDriver;

impl SbiDriver {
    #[inline(never)]
    pub fn early_init() {
        Self::ensure_probed();
    }

    /// Early logging may precede `early_init`; probe before choosing a console.
    ///
    /// Idempotent and safe to call re-entrantly: while a probe is in progress it
    /// returns immediately, so a trap handler that logs during probing cannot
    /// recurse into `probe_extensions`.
    #[inline]
    fn ensure_probed() {
        if PROBE_STATE.load(Ordering::SeqCst) != PROBE_NOT_STARTED {
            return;
        }
        if PROBE_STATE
            .compare_exchange(
                PROBE_NOT_STARTED,
                PROBE_IN_PROGRESS,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        unsafe {
            EXTENSIONS = Self::probe_extensions();
        }
        PROBE_STATE.store(PROBE_DONE, Ordering::SeqCst);
    }

    /// Whether DBCN is advertised by the firmware and has not failed yet.
    fn dbcn_usable() -> bool {
        Self::extensions().contains(SBIExtensions::CONSOLE) && !DBCN_FAILED.load(Ordering::Relaxed)
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
