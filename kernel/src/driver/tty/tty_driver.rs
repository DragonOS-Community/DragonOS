use core::{fmt::Debug, sync::atomic::Ordering};

use alloc::{string::String, sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            char::CharDevOps,
            device::{
                device_number::{DeviceNumber, Major},
                driver::Driver,
            },
            kobject::KObject,
        },
        tty::tty_port::TtyPortState,
    },
    libs::spinlock::SpinLock,
};

use super::{
    termios::Termios,
    tty_core::{TtyCore, TtyCoreData},
    tty_ldisc::TtyLdiscManager,
    tty_port::TTY_PORTS,
    virtual_terminal::virtual_console::CURRENT_VCNUM,
};

lazy_static! {
    static ref TTY_DRIVERS: SpinLock<Vec<Arc<TtyDriver>>> = SpinLock::new(Vec::new());
}

pub struct TtyDriverManager;
impl TtyDriverManager {
    pub fn lookup_tty_driver(dev_num: DeviceNumber) -> Option<(usize, Arc<TtyDriver>)> {
        let drivers_guard = TTY_DRIVERS.lock();
        for (index, driver) in drivers_guard.iter().enumerate() {
            let base = DeviceNumber::new(driver.major, driver.minor_start);
            if dev_num < base || dev_num.data() > base.data() + driver.device_count {
                continue;
            }
            return Some((index, driver.clone()));
        }

        None
    }

    /// ## 注册驱动
    pub fn tty_register_driver(mut driver: TtyDriver) -> Result<(), SystemError> {
        // 查看是否注册设备号
        if driver.major == Major::UNNAMED_MAJOR {
            let dev_num = CharDevOps::alloc_chardev_region(
                driver.minor_start,
                driver.device_count,
                driver.name,
            )?;
            driver.major = dev_num.major();
            driver.minor_start = dev_num.minor();
        } else {
            let dev_num = DeviceNumber::new(driver.major, driver.minor_start);
            CharDevOps::register_chardev_region(dev_num, driver.device_count, driver.name)?;
        }

        driver.flags |= TtyDriverFlag::TTY_DRIVER_INSTALLED;

        // 加入全局TtyDriver表
        TTY_DRIVERS.lock().push(Arc::new(driver));

        // TODO: 加入procfs?

        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Debug)]
#[cast_to([sync] Driver)]
pub struct TtyDriver {
    /// /proc/tty中使用的驱动程序名称
    driver_name: String,
    /// 用于构造/dev节点名称，例如name设置为tty,则按照name_base分配节点tty0,tty1等
    name: &'static str,
    /// 命名基数
    name_base: usize,
    /// 主设备号
    major: Major,
    /// 起始次设备号
    minor_start: u32,
    /// 最多支持的tty数量
    device_count: u32,
    /// tty驱动程序类型
    tty_driver_type: TtyDriverType,
    /// 驱动程序子类型
    tty_driver_sub_type: TtyDriverSubType,
    /// 每个tty的默认termios
    init_termios: Termios,
    /// 懒加载termios,在tty设备关闭时，会将termios按照设备的index保存进这个集合，以便下次打开使用
    saved_termios: Vec<Termios>,
    /// 驱动程序标志
    flags: TtyDriverFlag,
    /// pty链接此driver的入口
    pty: Option<Arc<TtyDriver>>,
    /// 具体类型的tty驱动方法
    driver_funcs: Arc<dyn TtyOperation>,
    /// 管理的tty设备列表
    ttys: SpinLock<HashMap<usize, Arc<TtyCore>>>,
    // procfs入口?
}

impl TtyDriver {
    pub fn new(
        count: u32,
        node_name: &'static str,
        node_name_base: usize,
        major: Major,
        minor_start: u32,
        tty_driver_type: TtyDriverType,
        default_termios: Termios,
        driver_funcs: Arc<dyn TtyOperation>,
    ) -> Self {
        TtyDriver {
            driver_name: Default::default(),
            name: node_name,
            name_base: node_name_base,
            major,
            minor_start,
            device_count: count,
            tty_driver_type,
            tty_driver_sub_type: Default::default(),
            init_termios: default_termios,
            flags: TtyDriverFlag::empty(),
            pty: Default::default(),
            driver_funcs,
            ttys: SpinLock::new(HashMap::new()),
            saved_termios: Vec::with_capacity(count as usize),
        }
    }

    pub fn tty_line_name(&self, index: usize) -> String {
        if self
            .flags
            .contains(TtyDriverFlag::TTY_DRIVER_UNNUMBERED_NODE)
        {
            return format!("{}", self.name);
        } else {
            return format!("{}{}", self.name, index + self.name_base);
        }
    }

    pub fn add_tty(&self, tty_core: Arc<TtyCore>) {
        self.ttys.lock().insert(tty_core.core().index(), tty_core);
    }

    #[inline]
    pub fn driver_funcs(&self) -> Arc<dyn TtyOperation> {
        self.driver_funcs.clone()
    }

    #[inline]
    pub fn flags(&self) -> TtyDriverFlag {
        self.flags
    }

    #[inline]
    fn lockup_tty(&self, index: usize) -> Option<Arc<TtyCore>> {
        let device_guard = self.ttys.lock();
        return match device_guard.get(&index) {
            Some(tty) => Some(tty.clone()),
            None => None,
        };
    }

    fn standard_install(&self, tty_core: Arc<TtyCore>) -> Result<(), SystemError> {
        let tty = tty_core.core();
        let tty_index = tty.index();
        // 初始化termios
        if !self.flags.contains(TtyDriverFlag::TTY_DRIVER_RESET_TERMIOS) {
            // 先查看是否有已经保存的termios
            if let Some(t) = self.saved_termios.get(tty_index) {
                let mut termios = t.clone();
                termios.line = self.init_termios.line;
                tty.set_termios(termios);
            }
        }
        // TODO:设置termios波特率？

        tty.add_count();

        self.ttys.lock().insert(tty_index, tty_core);

        Ok(())
    }

    fn driver_install_tty(driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let res = tty.install(driver.clone(), tty.clone());

        if res.is_err() {
            let err = res.unwrap_err();
            if err == SystemError::ENOSYS {
                return driver.standard_install(tty);
            } else {
                return Err(err);
            }
        }

        driver.add_tty(tty);

        Ok(())
    }

    fn init_tty_device(driver: Arc<TtyDriver>, index: usize) -> Result<Arc<TtyCore>, SystemError> {
        let tty = TtyCore::new(driver.clone(), index);

        Self::driver_install_tty(driver.clone(), tty.clone())?;

        let core = tty.core();

        if core.port().is_none() {
            TTY_PORTS[core.index()].setup_tty(Arc::downgrade(&tty));
            tty.set_port(TTY_PORTS[core.index()].clone());
        }

        TtyLdiscManager::ldisc_setup(tty.clone(), None)?;

        Ok(tty)
    }

    /// ## 通过设备号找到对应驱动并且初始化Tty
    pub fn open_tty(dev_num: DeviceNumber) -> Result<Arc<TtyCore>, SystemError> {
        let (index, driver) =
            TtyDriverManager::lookup_tty_driver(dev_num).ok_or(SystemError::ENODEV)?;

        let tty = match driver.lockup_tty(index) {
            Some(tty) => {
                // TODO: 暂时这么写，因为还没写TtyPort
                if tty.core().port().is_none() {
                    kwarn!("{} port is None", tty.core().name());
                } else {
                    if tty.core().port().unwrap().state() == TtyPortState::KOPENED {
                        return Err(SystemError::EBUSY);
                    }
                }

                tty.reopen()?;
                tty
            }
            None => Self::init_tty_device(driver, index)?,
        };

        CURRENT_VCNUM.store(index as isize, Ordering::SeqCst);

        return Ok(tty);
    }

    pub fn tty_driver_type(&self) -> TtyDriverType {
        self.tty_driver_type
    }

    pub fn tty_driver_sub_type(&self) -> TtyDriverSubType {
        self.tty_driver_sub_type
    }

    pub fn init_termios(&self) -> Termios {
        self.init_termios
    }
}

impl KObject for TtyDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }

    fn set_inode(&self, _inode: Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, _parent: Option<alloc::sync::Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<alloc::sync::Arc<crate::driver::base::kset::KSet>> {
        todo!()
    }

    fn set_kset(&self, _kset: Option<alloc::sync::Arc<crate::driver::base::kset::KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        todo!()
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn crate::driver::base::kobject::KObjType>) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, _name: alloc::string::String) {
        todo!()
    }

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn kobj_state_mut(
        &self,
    ) -> crate::libs::rwlock::RwLockWriteGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: crate::driver::base::kobject::KObjectState) {
        todo!()
    }
}

impl Driver for TtyDriver {
    fn id_table(&self) -> Option<crate::driver::base::device::IdTable> {
        todo!()
    }

    fn devices(
        &self,
    ) -> alloc::vec::Vec<alloc::sync::Arc<dyn crate::driver::base::device::Device>> {
        todo!()
    }

    fn add_device(&self, _device: alloc::sync::Arc<dyn crate::driver::base::device::Device>) {
        todo!()
    }

    fn delete_device(&self, _device: &alloc::sync::Arc<dyn crate::driver::base::device::Device>) {
        todo!()
    }

    fn set_bus(&self, _bus: Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>>) {
        todo!()
    }
}

pub trait TtyOperation: Sync + Send + Debug {
    fn install(&self, _driver: Arc<TtyDriver>, _tty: Arc<TtyCore>) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn open(&self, tty: &TtyCoreData) -> Result<(), SystemError>;

    /// ## 获取可写字符数
    fn write_room(&self, _tty: &TtyCoreData) -> usize {
        // 默认
        2048
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError>;

    fn flush_chars(&self, tty: &TtyCoreData);

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError>;

    fn start(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn stop(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn flush_buffer(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn ioctl(&self, tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<(), SystemError>;

    fn chars_in_buffer(&self) -> usize {
        0
    }

    fn set_termios(&self, _tty: Arc<TtyCore>, _old_termios: Termios) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TtyDriverType {
    System,
    Console,
    Serial,
    Pty,
    Scc,
    Syscons,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TtyDriverSubType {
    Undefined,
    Tty,
    Console,
    Syscons,
    Sysptmx,
    PtyMaster,
    PtySlave,
    SerialNormal,
}

impl Default for TtyDriverSubType {
    fn default() -> Self {
        Self::Undefined
    }
}

bitflags! {
    pub struct TtyDriverFlag: u32 {
        /// 表示 tty 驱动程序已安装
        const TTY_DRIVER_INSTALLED		= 0x0001;
        /// 请求 tty 层在最后一个进程关闭设备时重置 termios 设置
        const TTY_DRIVER_RESET_TERMIOS	= 0x0002;
        /// 表示驱动程序将保证在设置了该标志的 tty 上不设置任何特殊字符处理标志(原模式)
        const TTY_DRIVER_REAL_RAW		    = 0x0004;

        /// 以下四个标志位为内存分配相关，目前设计无需使用
        const TTY_DRIVER_DYNAMIC_DEV		= 0x0008;
        const TTY_DRIVER_DEVPTS_MEM		= 0x0010;
        const TTY_DRIVER_HARDWARE_BREAK	= 0x0020;
        const TTY_DRIVER_DYNAMIC_ALLOC	= 0x0040;

        /// 表示不创建带有编号的 /dev 节点。
        /// 例如，创建 /dev/ttyprintk 而不是 /dev/ttyprintk0。仅在为单个 tty 设备分配驱动程序时适用。
        const TTY_DRIVER_UNNUMBERED_NODE	= 0x0080;
    }
}
