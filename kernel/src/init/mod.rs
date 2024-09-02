use core::cmp::min;

use crate::{
    arch::init::ArchBootParams,
    driver::video::fbdev::base::BootTimeScreenInfo,
    libs::rwlock::RwLock,
    mm::{PhysAddr, VirtAddr},
};
#[allow(clippy::module_inception)]
pub mod init;
pub mod initcall;
pub mod initial_kthread;

/// 启动参数
static BOOT_PARAMS: RwLock<BootParams> = RwLock::new(BootParams::new());

#[inline(always)]
pub fn boot_params() -> &'static RwLock<BootParams> {
    &BOOT_PARAMS
}

#[inline(never)]
fn init_intertrait() {
    intertrait::init_caster_map();
}

#[derive(Debug)]
pub struct BootParams {
    pub screen_info: BootTimeScreenInfo,
    #[allow(dead_code)]
    pub arch: ArchBootParams,
    boot_command_line: [u8; Self::BOOT_COMMAND_LINE_SIZE],
}

impl BootParams {
    const DEFAULT: Self = BootParams {
        screen_info: BootTimeScreenInfo::DEFAULT,
        arch: ArchBootParams::DEFAULT,
        boot_command_line: [0u8; Self::BOOT_COMMAND_LINE_SIZE],
    };

    /// 开机命令行参数字符串最大大小
    pub const BOOT_COMMAND_LINE_SIZE: usize = 2048;

    const fn new() -> Self {
        Self::DEFAULT
    }

    /// 开机命令行参数（原始字节数组）
    #[allow(dead_code)]
    pub fn boot_cmdline(&self) -> &[u8] {
        &self.boot_command_line
    }

    /// 开机命令行参数字符串
    pub fn boot_cmdline_str(&self) -> &str {
        core::str::from_utf8(self.boot_cmdline()).unwrap()
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
