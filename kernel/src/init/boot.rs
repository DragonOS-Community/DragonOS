use core::cmp::min;

use acpi::rsdp::Rsdp;
use alloc::string::String;
use system_error::SystemError;

use crate::{
    arch::init::ArchBootParams,
    driver::video::fbdev::base::BootTimeScreenInfo,
    libs::lazy_init::Lazy,
    mm::{PhysAddr, VirtAddr},
};

use super::boot_params;
#[derive(Debug)]
pub struct BootParams {
    pub screen_info: BootTimeScreenInfo,
    bootloader_name: Option<String>,
    #[allow(dead_code)]
    pub arch: ArchBootParams,
    boot_command_line: [u8; Self::BOOT_COMMAND_LINE_SIZE],
    pub acpi: BootloaderAcpiArg,
}

impl BootParams {
    const DEFAULT: Self = BootParams {
        screen_info: BootTimeScreenInfo::DEFAULT,
        bootloader_name: None,
        arch: ArchBootParams::DEFAULT,
        boot_command_line: [0u8; Self::BOOT_COMMAND_LINE_SIZE],
        acpi: BootloaderAcpiArg::NotProvided,
    };

    /// 开机命令行参数字符串最大大小
    pub const BOOT_COMMAND_LINE_SIZE: usize = 2048;

    pub(super) const fn new() -> Self {
        Self::DEFAULT
    }

    /// 开机命令行参数（原始字节数组）
    #[allow(dead_code)]
    pub fn boot_cmdline(&self) -> &[u8] {
        &self.boot_command_line
    }

    /// 开机命令行参数字符串
    pub fn boot_cmdline_str(&self) -> &str {
        core::str::from_utf8(&self.boot_cmdline()[..self.boot_cmdline_len()]).unwrap()
    }

    #[allow(dead_code)]
    pub fn bootloader_name(&self) -> Option<&str> {
        self.bootloader_name.as_deref()
    }

    pub fn boot_cmdline_len(&self) -> usize {
        self.boot_command_line
            .iter()
            .position(|&x| x == 0)
            .unwrap_or(self.boot_command_line.len())
    }

    /// 追加开机命令行参数
    ///
    /// 如果开机命令行参数已经满了，则不会追加。
    /// 如果超过了最大长度，则截断。
    ///
    /// ## 参数
    ///
    /// - `data`：追加的数据
    pub fn boot_cmdline_append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut pos: Option<usize> = None;
        // 寻找结尾
        for (i, x) in self.boot_command_line.iter().enumerate() {
            if *x == 0 {
                pos = Some(i);
                break;
            }
        }
        let pos = pos.unwrap_or(self.boot_command_line.len() - 1) as isize;

        let avail = self.boot_command_line.len() as isize - pos - 1;
        if avail <= 0 {
            return;
        }

        let len = min(avail as usize, data.len());
        let pos = pos as usize;
        self.boot_command_line[pos..pos + len].copy_from_slice(&data[0..len]);

        self.boot_command_line[pos + len] = 0;
    }

    /// 获取FDT的虚拟地址
    #[allow(dead_code)]
    pub fn fdt(&self) -> Option<VirtAddr> {
        #[cfg(target_arch = "riscv64")]
        return Some(self.arch.arch_fdt());

        #[cfg(target_arch = "x86_64")]
        return None;
    }

    /// 获取FDT的物理地址
    #[allow(dead_code)]
    pub fn fdt_paddr(&self) -> Option<PhysAddr> {
        #[cfg(target_arch = "riscv64")]
        return Some(self.arch.fdt_paddr);

        #[cfg(target_arch = "x86_64")]
        return None;
    }
}

/// 开机引导回调，用于初始化内核启动参数
pub trait BootCallbacks: Send + Sync {
    /// 初始化引导程序名称
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError>;
    /// 初始化ACPI参数
    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError>;
    /// 初始化内核命令行参数
    ///
    /// 该函数应该把内核命令行参数追加到`boot_params().boot_cmdline`中
    fn init_kernel_cmdline(&self) -> Result<(), SystemError>;
    /// 初始化帧缓冲区信息
    ///
    /// - 该函数应该把帧缓冲区信息写入`scinfo`中。
    /// - 该函数应该在内存管理初始化之前调用。
    fn early_init_framebuffer_info(
        &self,
        scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError>;

    /// 初始化内存块
    fn early_init_memory_blocks(&self) -> Result<(), SystemError>;
}

static BOOT_CALLBACKS: Lazy<&'static dyn BootCallbacks> = Lazy::new();

/// 注册开机引导回调
pub fn register_boot_callbacks(callbacks: &'static dyn BootCallbacks) {
    BOOT_CALLBACKS.init(callbacks);
}

/// 获取开机引导回调
pub fn boot_callbacks() -> &'static dyn BootCallbacks {
    let p = BOOT_CALLBACKS
        .try_get()
        .expect("Boot callbacks not initialized");

    *p
}

pub(super) fn boot_callback_except_early() {
    let mut boot_params = boot_params().write();
    boot_params.bootloader_name = boot_callbacks()
        .init_bootloader_name()
        .expect("Failed to init bootloader name");
    boot_params.acpi = boot_callbacks()
        .init_acpi_args()
        .unwrap_or(BootloaderAcpiArg::NotProvided);
}

/// ACPI information from the bootloader.
#[derive(Copy, Clone, Debug)]
pub enum BootloaderAcpiArg {
    /// The bootloader does not provide one, a manual search is needed.
    NotProvided,
    /// Physical address of the RSDP.
    #[allow(dead_code)]
    Rsdp(PhysAddr),
    /// Address of RSDT provided in RSDP v1.
    Rsdt(Rsdp),
    /// Address of XSDT provided in RSDP v2+.
    Xsdt(Rsdp),
}
