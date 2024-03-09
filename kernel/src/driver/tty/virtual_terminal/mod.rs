use core::sync::atomic::Ordering;

use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    driver::{
        base::device::{
            device_number::{DeviceNumber, Major},
            device_register, IdTable,
        },
        video::fbdev::base::fbcon::framebuffer_console::BlittingFbConsole,
    },
    filesystem::devfs::devfs_register,
    libs::spinlock::SpinLock,
};

use self::virtual_console::{VirtualConsoleData, CURRENT_VCNUM};

use super::{
    console::ConsoleSwitch,
    termios::{InputMode, TTY_STD_TERMIOS},
    tty_core::{TtyCore, TtyCoreData},
    tty_device::TtyDevice,
    tty_driver::{TtyDriver, TtyDriverManager, TtyDriverType, TtyOperation},
};

pub mod console_map;
pub mod virtual_console;

pub const MAX_NR_CONSOLES: u32 = 63;
pub const VC_MAXCOL: usize = 32767;
pub const VC_MAXROW: usize = 32767;

pub const DEFAULT_RED: [u16; 16] = [
    0x00, 0xaa, 0x00, 0xaa, 0x00, 0xaa, 0x00, 0xaa, 0x55, 0xff, 0x55, 0xff, 0x55, 0xff, 0x55, 0xff,
];

pub const DEFAULT_GREEN: [u16; 16] = [
    0x00, 0x00, 0xaa, 0x55, 0x00, 0x00, 0xaa, 0xaa, 0x55, 0x55, 0xff, 0xff, 0x55, 0x55, 0xff, 0xff,
];

pub const DEFAULT_BLUE: [u16; 16] = [
    0x00, 0x00, 0x00, 0x00, 0xaa, 0xaa, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0x55, 0xff, 0xff, 0xff, 0xff,
];

pub const COLOR_TABLE: &'static [u8] = &[0, 4, 2, 6, 1, 5, 3, 7, 8, 12, 10, 14, 9, 13, 11, 15];

lazy_static! {
    pub static ref VIRT_CONSOLES: Vec<Arc<SpinLock<VirtualConsoleData>>> = {
        let mut v = Vec::with_capacity(MAX_NR_CONSOLES as usize);
        for i in 0..MAX_NR_CONSOLES as usize {
            v.push(Arc::new(SpinLock::new(VirtualConsoleData::new(i))));
        }

        v
    };
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Color {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub transp: u16,
}

impl Color {
    pub fn from_256(col: u32) -> Self {
        let mut color = Self::default();
        if col < 8 {
            color.red = if col & 1 != 0 { 0xaa } else { 0x00 };
            color.green = if col & 2 != 0 { 0xaa } else { 0x00 };
            color.blue = if col & 4 != 0 { 0xaa } else { 0x00 };
        } else if col < 16 {
            color.red = if col & 1 != 0 { 0xff } else { 0x55 };
            color.green = if col & 2 != 0 { 0xff } else { 0x55 };
            color.blue = if col & 4 != 0 { 0xff } else { 0x55 };
        } else if col < 232 {
            color.red = ((col - 16) / 36 * 85 / 2) as u16;
            color.green = ((col - 16) / 6 % 6 * 85 / 2) as u16;
            color.blue = ((col - 16) % 6 * 85 / 2) as u16;
        } else {
            let col = (col * 10 - 2312) as u16;
            color.red = col;
            color.green = col;
            color.blue = col;
        }

        color
    }
}

#[derive(Debug)]
pub struct TtyConsoleDriverInner {
    console: Arc<BlittingFbConsole>,
}

impl TtyConsoleDriverInner {
    pub fn new() -> Result<Self, SystemError> {
        Ok(Self {
            console: Arc::new(BlittingFbConsole::new()?),
        })
    }

    fn do_write(&self, tty: &TtyCoreData, buf: &[u8], mut nr: usize) -> Result<usize, SystemError> {
        // 关闭中断
        let mut vc_data = tty.vc_data_irqsave();

        let mut offset = 0;

        // 这个参数是用来扫描unicode字符的，但是这部分目前未完成，先写着
        let mut rescan = false;
        let mut ch: u32 = 0;

        let mut draw = DrawRegion::default();

        // 首先隐藏光标再写
        vc_data.hide_cursor();

        while nr != 0 {
            if !rescan {
                ch = buf[offset] as u32;
                offset += 1;
                nr -= 1;
            }

            let (tc, rescan_last) = vc_data.translate(&mut ch);
            if tc.is_none() {
                // 表示未转换完成
                continue;
            }

            let tc = tc.unwrap();
            rescan = rescan_last;

            if vc_data.is_control(tc, ch) {
                vc_data.flush(&mut draw);
                vc_data.do_control(ch);
                continue;
            }

            if !vc_data.console_write_normal(tc, ch, &mut draw) {
                continue;
            }
        }

        vc_data.flush(&mut draw);

        // TODO: notify update
        return Ok(offset);
    }
}

impl TtyOperation for TtyConsoleDriverInner {
    fn install(&self, _driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let tty_core = tty.core();
        let mut vc_data = VIRT_CONSOLES[tty_core.index()].lock();

        self.console.con_init(&mut vc_data, true)?;
        if vc_data.complement_mask == 0 {
            vc_data.complement_mask = if vc_data.color_mode { 0x7700 } else { 0x0800 };
        }
        vc_data.s_complement_mask = vc_data.complement_mask;
        // vc_data.bytes_per_row = vc_data.cols << 1;
        vc_data.index = tty_core.index();
        vc_data.bottom = vc_data.rows;
        vc_data.set_driver_funcs(Arc::downgrade(
            &(self.console.clone() as Arc<dyn ConsoleSwitch>),
        ));

        // todo: unicode字符集处理？

        if vc_data.cols > VC_MAXCOL || vc_data.rows > VC_MAXROW {
            return Err(SystemError::EINVAL);
        }

        vc_data.init(None, None, true);
        vc_data.update_attr();

        let window_size = tty_core.window_size_upgradeable();
        if window_size.col == 0 && window_size.row == 0 {
            let mut window_size = window_size.upgrade();
            window_size.col = vc_data.cols as u16;
            window_size.row = vc_data.rows as u16;
            kerror!("window_size {:?}", *window_size);
        }

        if vc_data.utf {
            tty_core.termios_write().input_mode.insert(InputMode::IUTF8);
        } else {
            tty_core.termios_write().input_mode.remove(InputMode::IUTF8);
        }

        // 加入sysfs？

        Ok(())
    }

    fn open(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Ok(())
    }

    fn write_room(&self, _tty: &TtyCoreData) -> usize {
        32768
    }

    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/vt/vt.c#2894
    #[inline(never)]
    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        let ret = self.do_write(tty, buf, nr);
        self.flush_chars(tty);
        ret
    }

    #[inline(never)]
    fn flush_chars(&self, tty: &TtyCoreData) {
        let mut vc_data = tty.vc_data_irqsave();
        vc_data.set_cursor();
    }

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        self.write(tty, &[ch], 1)?;
        Ok(())
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        // TODO
        Err(SystemError::ENOIOCTLCMD)
    }
}

#[derive(Debug, Clone)]
pub struct VtModeData {
    mode: VtMode,
    /// 释放请求时触发的信号
    relsig: u16,
    /// 获取请求时触发的信号
    acqsig: u16,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum VtMode {
    /// 自动切换模式，即在请求输入时自动切换到终端
    Auto,
    /// 手动切换模式，需要通过 ioctl 请求切换到终端
    Process,
    /// 等待终端确认，即在切换到终端时等待终端的确认信号
    Ackacq,
}

/// 用于给vc确定要写入的buf位置
#[derive(Debug, Default)]
pub struct DrawRegion {
    /// 偏移量
    pub offset: usize,
    /// 写入数量
    pub size: usize,
    pub x: Option<u32>,
}

// 初始化虚拟终端
#[inline(never)]
pub fn vty_init() -> Result<(), SystemError> {
    // 注册虚拟终端设备并将虚拟终端设备加入到文件系统
    let vc0 = TtyDevice::new(
        "vc0",
        IdTable::new(
            String::from("vc0"),
            Some(DeviceNumber::new(Major::TTY_MAJOR, 0)),
        ),
    );
    // 注册tty设备
    // CharDevOps::cdev_add(
    //     vc0.clone() as Arc<dyn CharDevice>,
    //     IdTable::new(
    //         String::from("vc0"),
    //         Some(DeviceNumber::new(Major::TTY_MAJOR, 0)),
    //     ),
    //     1,
    // )?;

    // CharDevOps::register_chardev_region(DeviceNumber::new(Major::TTY_MAJOR, 0), 1, "/dev/vc/0")?;
    device_register(vc0.clone())?;
    devfs_register("vc0", vc0)?;

    // vcs_init?

    let console_driver = TtyDriver::new(
        MAX_NR_CONSOLES,
        "tty",
        1,
        Major::TTY_MAJOR,
        0,
        TtyDriverType::Console,
        TTY_STD_TERMIOS.clone(),
        Arc::new(TtyConsoleDriverInner::new()?),
    );

    TtyDriverManager::tty_register_driver(console_driver)?;

    CURRENT_VCNUM.store(0, Ordering::SeqCst);

    // 初始化键盘？

    // TODO: 为vc

    Ok(())
}
