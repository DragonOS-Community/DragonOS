use core::fmt::Formatter;

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::{
        base::device::{
            device_number::{DeviceNumber, Major},
            device_register, IdTable,
        },
        serial::serial8250::send_to_default_serial8250_port,
    },
    filesystem::devfs::{devfs_register, devfs_unregister},
    init::initcall::INITCALL_LATE,
    libs::{lazy_init::Lazy, rwlock::RwLock, spinlock::SpinLock},
};

use self::virtual_console::VirtualConsoleData;

use super::{
    console::ConsoleSwitch,
    termios::{InputMode, TTY_STD_TERMIOS},
    tty_core::{TtyCore, TtyCoreData},
    tty_device::{TtyDevice, TtyType},
    tty_driver::{TtyDriver, TtyDriverManager, TtyDriverType, TtyOperation},
    tty_port::{DefaultTtyPort, TtyPort},
};

pub mod console_map;
pub mod virtual_console;

pub const MAX_NR_CONSOLES: u32 = 64;
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

pub const COLOR_TABLE: &[u8] = &[0, 4, 2, 6, 1, 5, 3, 7, 8, 12, 10, 14, 9, 13, 11, 15];

lazy_static! {
    static ref VC_MANAGER: VirtConsoleManager = VirtConsoleManager::new();
}

kernel_cmdline_param_kv!(CONSOLE_PARAM, console, "");

/// 获取虚拟终端管理器
#[inline]
pub fn vc_manager() -> &'static VirtConsoleManager {
    &VC_MANAGER
}

pub struct VirtConsole {
    vc_data: Option<Arc<SpinLock<VirtualConsoleData>>>,
    port: Arc<dyn TtyPort>,
    index: Lazy<usize>,
    inner: SpinLock<InnerVirtConsole>,
}

struct InnerVirtConsole {
    vcdev: Option<Arc<TtyDevice>>,
}

impl VirtConsole {
    pub fn new(vc_data: Option<Arc<SpinLock<VirtualConsoleData>>>) -> Arc<Self> {
        Arc::new(Self {
            vc_data,
            port: Arc::new(DefaultTtyPort::new()),
            index: Lazy::new(),
            inner: SpinLock::new(InnerVirtConsole { vcdev: None }),
        })
    }

    pub fn vc_data(&self) -> Option<Arc<SpinLock<VirtualConsoleData>>> {
        self.vc_data.clone()
    }

    pub fn port(&self) -> Arc<dyn TtyPort> {
        self.port.clone()
    }

    pub fn index(&self) -> Option<usize> {
        self.index.try_get().cloned()
    }

    pub fn devfs_setup(&self) -> Result<(), SystemError> {
        let tty_core = self
            .port
            .port_data()
            .internal_tty()
            .ok_or(SystemError::ENODEV)?;
        let tty_core_data = tty_core.core();
        let devnum = *tty_core_data.device_number();
        let vcname = format!("vc{}", self.index.get());

        // 注册虚拟终端设备并将虚拟终端设备加入到文件系统
        let vcdev = TtyDevice::new(
            vcname.clone(),
            IdTable::new(vcname, Some(devnum)),
            TtyType::Tty,
        );

        device_register(vcdev.clone())?;
        devfs_register(vcdev.name_ref(), vcdev.clone())?;
        tty_core_data.set_vc_index(*self.index.get());
        self.inner.lock().vcdev = Some(vcdev);

        Ok(())
    }

    fn devfs_remove(&self) {
        let vcdev = self.inner.lock().vcdev.take();
        if let Some(vcdev) = vcdev {
            devfs_unregister(vcdev.name_ref(), vcdev.clone())
                .inspect_err(|e| {
                    log::error!("virt console: devfs_unregister failed: {:?}", e);
                })
                .ok();
        }
    }
}

struct InnerVirtConsoleManager {
    consoles: HashMap<usize, Arc<VirtConsole>>,
    ida: IdAllocator,
}
pub struct VirtConsoleManager {
    inner: SpinLock<InnerVirtConsoleManager>,

    current_vc: RwLock<Option<(Arc<VirtConsole>, usize)>>,
}

impl VirtConsoleManager {
    pub const DEFAULT_VC_NAMES: [&'static str; 4] = ["tty0", "ttyS0", "tty1", "ttyS1"];

    pub fn new() -> Self {
        let ida = IdAllocator::new(0, MAX_NR_CONSOLES as usize).unwrap();
        let consoles = HashMap::new();

        Self {
            inner: SpinLock::new(InnerVirtConsoleManager { consoles, ida }),
            current_vc: RwLock::new(None),
        }
    }

    pub fn get(&self, index: usize) -> Option<Arc<VirtConsole>> {
        let inner = self.inner.lock();
        inner.consoles.get(&index).cloned()
    }

    pub fn alloc(&self, vc: Arc<VirtConsole>) -> Option<usize> {
        let mut inner = self.inner.lock();
        let index = inner.ida.alloc()?;
        vc.index.init(index);
        if let Some(vc_data) = vc.vc_data.as_ref() {
            vc_data.lock().vc_index = index;
        }

        inner.consoles.insert(index, vc);
        Some(index)
    }

    /// 释放虚拟终端
    pub fn free(&self, index: usize) {
        let mut inner = self.inner.lock();
        if let Some(vc) = inner.consoles.remove(&index) {
            vc.devfs_remove();
        }
        inner.ida.free(index);
    }

    /// 获取当前虚拟终端
    pub fn current_vc(&self) -> Option<Arc<VirtConsole>> {
        self.current_vc.read().as_ref().map(|(vc, _)| vc.clone())
    }

    pub fn current_vc_index(&self) -> Option<usize> {
        self.current_vc.read().as_ref().map(|(_, index)| *index)
    }

    pub fn current_vc_tty_name(&self) -> Option<String> {
        self.current_vc()
            .and_then(|vc| vc.port().port_data().internal_tty())
            .map(|tty| tty.core().name().to_string())
    }

    /// 设置当前虚拟终端
    pub fn set_current_vc(&self, vc: Arc<VirtConsole>) {
        let index = *vc.index.get();
        *self.current_vc.write() = Some((vc, index));
    }

    /// 通过tty名称查找虚拟终端
    ///
    /// # Arguments
    ///
    /// * `name` - tty名称 (如ttyS0)
    pub fn lookup_vc_by_tty_name(&self, name: &str) -> Option<Arc<VirtConsole>> {
        let inner = self.inner.lock();
        for (_index, vc) in inner.consoles.iter() {
            let found = vc
                .port
                .port_data()
                .internal_tty()
                .map(|tty| tty.core().name().as_str() == name)
                .unwrap_or(false);

            if found {
                return Some(vc.clone());
            }
        }

        None
    }

    pub fn setup_default_vc(&self) {
        let mut console_value_str = CONSOLE_PARAM.value_str().unwrap_or("").trim();
        if !console_value_str.is_empty() {
            // 删除前缀/dev/
            console_value_str = console_value_str
                .strip_prefix("/dev/")
                .unwrap_or(console_value_str);
            if let Some(vc) = self.lookup_vc_by_tty_name(console_value_str) {
                log::info!("Set vc by cmdline: {}", console_value_str);
                self.set_current_vc(vc);
                return;
            } else {
                panic!(
                    "virt console: set vc by cmdline failed, name: {}",
                    console_value_str
                );
            }
        } else {
            for name in Self::DEFAULT_VC_NAMES.iter() {
                if let Some(vc) = self.lookup_vc_by_tty_name(name) {
                    log::info!("Set default vc with tty device: {}", name);
                    self.set_current_vc(vc);
                    return;
                }
            }
        }

        panic!("virt console: setup default vc failed");
    }
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

pub struct TtyConsoleDriverInner {
    console: Arc<dyn ConsoleSwitch>,
}

impl core::fmt::Debug for TtyConsoleDriverInner {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "TtyConsoleDriverInner")
    }
}

impl TtyConsoleDriverInner {
    pub fn new() -> Result<Self, SystemError> {
        let console = {
            #[cfg(not(target_arch = "riscv64"))]
            {
                Arc::new(crate::driver::video::fbdev::base::fbcon::framebuffer_console::BlittingFbConsole::new()?)
            }

            #[cfg(target_arch = "riscv64")]
            crate::driver::video::console::dummycon::dummy_console()
        };

        Ok(Self { console })
    }

    fn do_install(&self, tty: Arc<TtyCore>, vc: &Arc<VirtConsole>) -> Result<(), SystemError> {
        let tty_core = tty.core();

        let binding = vc.vc_data().unwrap();
        let mut vc_data = binding.lock();

        self.console.con_init(vc, &mut vc_data, true)?;
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
        }

        if vc_data.utf {
            tty_core.termios_write().input_mode.insert(InputMode::IUTF8);
        } else {
            tty_core.termios_write().input_mode.remove(InputMode::IUTF8);
        }

        // 设置tty的端口为vc端口
        vc.port().setup_internal_tty(Arc::downgrade(&tty));
        tty.set_port(vc.port());
        vc.devfs_setup()?;
        // 加入sysfs？

        Ok(())
    }
}

impl TtyOperation for TtyConsoleDriverInner {
    fn install(&self, _driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let vc = VirtConsole::new(Some(Arc::new(SpinLock::new(VirtualConsoleData::new(
            usize::MAX,
        )))));
        vc_manager().alloc(vc.clone()).ok_or(SystemError::EBUSY)?;
        self.do_install(tty, &vc)
            .inspect_err(|_| vc_manager().free(vc.index().unwrap()))?;

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
        // if String::from_utf8_lossy(buf) == "Hello world!\n" {
        //     loop {}
        // }
        send_to_default_serial8250_port(buf);
        let ret = tty.do_write(buf, nr);
        self.flush_chars(tty);
        ret
    }

    #[inline(never)]
    fn flush_chars(&self, tty: &TtyCoreData) {
        let vc_data = tty.vc_data().unwrap();
        let mut vc_data_guard = vc_data.lock_irqsave();
        vc_data_guard.set_cursor();
    }

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        self.write(tty, &[ch], 1)?;
        Ok(())
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        // TODO
        Err(SystemError::ENOIOCTLCMD)
    }

    fn close(&self, _tty: Arc<TtyCore>) -> Result<(), SystemError> {
        Ok(())
    }

    fn resize(
        &self,
        _tty: Arc<TtyCore>,
        _winsize: super::termios::WindowSize,
    ) -> Result<(), SystemError> {
        todo!()
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
    if let Ok(tty_console_driver_inner) = TtyConsoleDriverInner::new() {
        const NAME: &str = "tty";
        let console_driver = TtyDriver::new(
            MAX_NR_CONSOLES,
            NAME,
            0,
            Major::TTY_MAJOR,
            0,
            TtyDriverType::Console,
            *TTY_STD_TERMIOS,
            Arc::new(tty_console_driver_inner),
            None,
        );

        TtyDriverManager::tty_register_driver(console_driver).inspect_err(|e| {
            log::error!("tty console: register driver {} failed: {:?}", NAME, e);
        })?;
    }

    Ok(())
}

#[unified_init(INITCALL_LATE)]
fn vty_late_init() -> Result<(), SystemError> {
    let (_, console_driver) =
        TtyDriverManager::lookup_tty_driver(DeviceNumber::new(Major::TTY_MAJOR, 0))
            .ok_or(SystemError::ENODEV)?;
    console_driver.init_tty_device(None).ok();

    vc_manager().setup_default_vc();
    Ok(())
}
