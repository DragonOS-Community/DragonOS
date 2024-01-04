use core::cmp::min;

use crate::{
    arch::init::ArchBootParams,
    driver::{
        tty::init::tty_early_init,
        video::{fbdev::base::BootTimeScreenInfo, VideoRefreshManager},
    },
    libs::{lib_ui::screen_manager::scm_init, rwlock::RwLock},
};

mod c_adapter;

pub mod initcall;
pub mod initial_kthread;

/// 启动参数
static BOOT_PARAMS: RwLock<BootParams> = RwLock::new(BootParams::new());

#[inline(always)]
pub fn boot_params() -> &'static RwLock<BootParams> {
    &BOOT_PARAMS
}

fn init_intertrait() {
    intertrait::init_caster_map();
}

/// 在内存管理初始化之前，执行的初始化
pub fn init_before_mem_init() {
    tty_early_init().expect("tty early init failed");
    let video_ok = unsafe { VideoRefreshManager::video_init().is_ok() };
    scm_init(video_ok);
}

#[derive(Debug)]
pub struct BootParams {
    pub screen_info: BootTimeScreenInfo,
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
        let pos = pos.unwrap_or_else(|| self.boot_command_line.len() - 1) as isize;

        let avail = self.boot_command_line.len() as isize - pos - 1;
        if avail <= 0 {
            return;
        }

        let len = min(avail as usize, data.len());
        let pos = pos as usize;
        self.boot_command_line[pos..pos + len].copy_from_slice(&data[0..len]);

        self.boot_command_line[pos + len] = 0;
    }
}
