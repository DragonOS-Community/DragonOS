use core::fmt::Debug;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use ida::IdAllocator;
use log::warn;
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            char::CharDevOps,
            device::{
                device_number::{DeviceNumber, Major},
                device_register,
                driver::Driver,
                IdTable,
            },
            kobject::KObject,
        },
        tty::tty_port::TtyPortState,
    },
    filesystem::devfs::devfs_register,
    libs::{
        lazy_init::Lazy,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{
    termios::{Termios, WindowSize},
    tty_core::{TtyCore, TtyCoreData},
    tty_device::TtyDevice,
    tty_ldisc::TtyLdiscManager,
    tty_port::{DefaultTtyPort, TtyPort},
};

lazy_static! {
    pub static ref TTY_DRIVERS: SpinLock<Vec<Arc<TtyDriver>>> = SpinLock::new(Vec::new());
}

pub enum TtyDriverPrivateData {
    Unused,
    /// true表示主设备 false表示从设备
    Pty(bool),
}

pub struct TtyDriverManager;
impl TtyDriverManager {
    pub fn lookup_tty_driver(dev_num: DeviceNumber) -> Option<(usize, Arc<TtyDriver>)> {
        let drivers_guard = TTY_DRIVERS.lock();
        for driver in drivers_guard.iter() {
            let base = DeviceNumber::new(driver.major, driver.minor_start);
            if dev_num < base || dev_num.data() > base.data() + driver.device_count {
                continue;
            }
            return Some(((dev_num.data() - base.data()) as usize, driver.clone()));
        }

        None
    }

    /// ## 注册驱动
    pub fn tty_register_driver(mut driver: TtyDriver) -> Result<Arc<TtyDriver>, SystemError> {
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
        let driver = Arc::new(driver);
        driver.self_ref.init(Arc::downgrade(&driver));
        TTY_DRIVERS.lock().push(driver.clone());

        // TODO: 加入procfs?

        Ok(driver)
    }
}

/// tty 驱动程序的与设备相关的数据
pub trait TtyDriverPrivateField: Debug + Send + Sync {}
pub trait TtyCorePrivateField: Debug + Send + Sync {}

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
    other_pty_driver: RwLock<Weak<TtyDriver>>,
    /// 具体类型的tty驱动方法
    driver_funcs: Arc<dyn TtyOperation>,
    /// 管理的tty设备列表
    ttys: SpinLock<HashMap<usize, Arc<TtyCore>>>,
    /// 管理的端口列表
    ports: RwLock<Vec<Arc<dyn TtyPort>>>,
    /// 与设备相关的私有数据
    private_field: Option<Arc<dyn TtyDriverPrivateField>>,
    /// id分配器
    ida: SpinLock<IdAllocator>,
    self_ref: Lazy<Weak<Self>>,
}

impl TtyDriver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        count: u32,
        node_name: &'static str,
        node_name_base: usize,
        major: Major,
        minor_start: u32,
        tty_driver_type: TtyDriverType,
        default_termios: Termios,
        driver_funcs: Arc<dyn TtyOperation>,
        private_field: Option<Arc<dyn TtyDriverPrivateField>>,
    ) -> Self {
        let mut ports: Vec<Arc<dyn TtyPort>> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            ports.push(Arc::new(DefaultTtyPort::new()))
        }
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
            other_pty_driver: Default::default(),
            driver_funcs,
            ttys: SpinLock::new(HashMap::new()),
            saved_termios: Vec::with_capacity(count as usize),
            ports: RwLock::new(ports),
            private_field,
            ida: SpinLock::new(IdAllocator::new(0, count as usize).unwrap()),
            self_ref: Lazy::new(),
        }
    }

    pub fn tty_line_name(&self, index: usize) -> String {
        if self
            .flags
            .contains(TtyDriverFlag::TTY_DRIVER_UNNUMBERED_NODE)
        {
            return self.name.to_string();
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

    /// ## 获取该驱动对应的设备的设备号
    #[inline]
    pub fn device_number(&self, index: usize) -> Option<DeviceNumber> {
        if index >= self.device_count as usize {
            return None;
        }
        Some(DeviceNumber::new(
            self.major,
            self.minor_start + index as u32,
        ))
    }

    fn self_ref(&self) -> Arc<Self> {
        self.self_ref.get().upgrade().unwrap()
    }

    #[inline]
    pub fn init_termios(&self) -> Termios {
        self.init_termios
    }

    #[inline]
    pub fn init_termios_mut(&mut self) -> &mut Termios {
        &mut self.init_termios
    }

    #[inline]
    pub fn other_pty_driver(&self) -> Option<Arc<TtyDriver>> {
        self.other_pty_driver.read().upgrade()
    }

    pub fn set_other_pty_driver(&self, driver: Weak<TtyDriver>) {
        *self.other_pty_driver.write() = driver
    }

    #[inline]
    pub fn set_subtype(&mut self, tp: TtyDriverSubType) {
        self.tty_driver_sub_type = tp;
    }

    #[inline]
    pub fn ttys(&self) -> SpinLockGuard<HashMap<usize, Arc<TtyCore>>> {
        self.ttys.lock()
    }

    #[inline]
    pub fn saved_termios(&self) -> &Vec<Termios> {
        &self.saved_termios
    }

    #[inline]
    pub fn flags(&self) -> TtyDriverFlag {
        self.flags
    }

    #[inline]
    fn lookup_tty(&self, index: usize) -> Option<Arc<TtyCore>> {
        let ret = self
            .driver_funcs()
            .lookup(index, TtyDriverPrivateData::Unused);
        if let Err(SystemError::ENOSYS) = ret {
            let device_guard = self.ttys.lock();
            return device_guard.get(&index).cloned();
        }
        ret.ok()
    }

    pub fn standard_install(&self, tty_core: Arc<TtyCore>) -> Result<(), SystemError> {
        let tty = tty_core.core();
        tty.init_termios();
        // TODO:设置termios波特率？

        tty.add_count();

        self.ttys.lock().insert(tty.index(), tty_core);

        Ok(())
    }

    fn driver_install_tty(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let res = tty.install(self.self_ref(), tty.clone());

        if let Err(err) = res {
            if err == SystemError::ENOSYS {
                return self.standard_install(tty);
            } else {
                log::error!(
                    "driver_install_tty: Failed to install. name: {}, err: {:?}",
                    tty.core().name(),
                    err
                );
                return Err(err);
            }
        }

        self.add_tty(tty);

        Ok(())
    }

    pub fn init_tty_device(&self, index: Option<usize>) -> Result<Arc<TtyCore>, SystemError> {
        // 如果传入的index为None，那么就自动分配index
        let idx: usize;
        if let Some(i) = index {
            if self.ida.lock().exists(i) {
                return Err(SystemError::EINVAL);
            }
            idx = i;
        } else {
            idx = self.ida.lock().alloc().ok_or(SystemError::EBUSY)?;
        }
        log::debug!("init_tty_device: create TtyCore");
        let tty = TtyCore::new(self.self_ref(), idx);

        log::debug!("init_tty_device: to driver_install_tty");
        self.driver_install_tty(tty.clone())?;
        log::debug!(
            "init_tty_device: driver_install_tty done, index: {}, dev_name: {:?}",
            idx,
            tty.core().name(),
        );

        let core = tty.core();

        if core.port().is_none() {
            let ports = self.ports.read();
            ports[core.index()].setup_internal_tty(Arc::downgrade(&tty));
            tty.set_port(ports[core.index()].clone());
        }
        log::debug!("init_tty_device: to ldisc_setup");
        TtyLdiscManager::ldisc_setup(tty.clone(), tty.core().link())?;

        // 在devfs创建对应的文件

        log::debug!("init_tty_device: to new tty device");
        let device = TtyDevice::new(
            core.name().clone(),
            IdTable::new(self.tty_line_name(idx), Some(*core.device_number())),
            super::tty_device::TtyType::Tty,
        );
        log::debug!("init_tty_device: to devfs_register");
        devfs_register(device.name_ref(), device.clone())?;
        log::debug!("init_tty_device: to device_register");
        device_register(device)?;
        Ok(tty)
    }

    /// ## 通过设备号找到对应驱动并且初始化Tty
    pub fn open_tty(&self, index: Option<usize>) -> Result<Arc<TtyCore>, SystemError> {
        let mut tty: Option<Arc<TtyCore>> = None;

        if index.is_some() {
            if let Some(t) = self.lookup_tty(index.unwrap()) {
                if t.core().port().is_none() {
                    warn!("{} port is None", t.core().name());
                } else if t.core().port().unwrap().state() == TtyPortState::KOPENED {
                    return Err(SystemError::EBUSY);
                }

                t.reopen()?;
                tty = Some(t);
            }
        }
        if tty.is_none() {
            tty = Some(self.init_tty_device(index)?);
        }
        let tty = tty.ok_or(SystemError::ENODEV)?;

        return Ok(tty);
    }

    pub fn tty_driver_type(&self) -> TtyDriverType {
        self.tty_driver_type
    }

    pub fn tty_driver_sub_type(&self) -> TtyDriverSubType {
        self.tty_driver_sub_type
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

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        self.write(tty, &[ch], 1).map(|_| ())
    }

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

    fn lookup(
        &self,
        _index: usize,
        _priv_data: TtyDriverPrivateData,
    ) -> Result<Arc<TtyCore>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError>;

    fn resize(&self, _tty: Arc<TtyCore>, _winsize: WindowSize) -> Result<(), SystemError> {
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
