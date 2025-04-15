use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::ipc::signal::Signal,
    driver::{
        base::{
            char::CharDevice,
            class::Class,
            device::{
                bus::Bus,
                device_number::{DeviceNumber, Major},
                device_register,
                driver::Driver,
                Device, DeviceKObjType, DeviceType, IdTable,
            },
            kobject::{KObject, LockedKObjectState},
            kset::KSet,
        },
        serial::serial_init,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        kernfs::KernFSInode,
        vfs::{
            file::FileMode, syscall::ModeType, FilePrivateData, FileType, IndexNode, Metadata,
            PollableInode,
        },
    },
    init::initcall::INITCALL_DEVICE,
    libs::{
        rwlock::{RwLock, RwLockWriteGuard},
        spinlock::SpinLockGuard,
    },
    mm::VirtAddr,
    net::event_poll::EPollItem,
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

use super::{
    kthread::tty_flush_thread_init,
    pty::unix98pty::ptmx_open,
    sysfs::sys_class_tty_instance,
    termios::WindowSize,
    tty_core::{TtyCore, TtyFlag, TtyIoctlCmd},
    tty_driver::{TtyDriverManager, TtyDriverSubType, TtyDriverType, TtyOperation},
    tty_job_control::TtyJobCtrlManager,
    virtual_terminal::vty_init,
};

#[derive(Debug)]
pub struct InnerTtyDevice {
    /// 当前设备所述的kset
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
    /// 当前设备所述的总线
    bus: Option<Weak<dyn Bus>>,
    inode: Option<Arc<KernFSInode>>,
    driver: Option<Weak<dyn Driver>>,
    can_match: bool,

    metadata: Metadata,
}

impl InnerTtyDevice {
    pub fn new() -> Self {
        Self {
            kset: None,
            parent_kobj: None,
            bus: None,
            inode: None,
            driver: None,
            can_match: false,
            metadata: Metadata::new(FileType::CharDevice, ModeType::from_bits_truncate(0o755)),
        }
    }

    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }
}

#[derive(Debug, PartialEq)]
pub enum TtyType {
    Tty,
    Pty(PtyType),
}

#[derive(Debug, PartialEq)]
pub enum PtyType {
    Ptm,
    Pts,
}

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct TtyDevice {
    name: String,
    id_table: IdTable,
    tty_type: TtyType,
    inner: RwLock<InnerTtyDevice>,
    kobj_state: LockedKObjectState,
    /// TTY所属的文件系统
    fs: RwLock<Weak<DevFS>>,
}

impl TtyDevice {
    pub fn new(name: String, id_table: IdTable, tty_type: TtyType) -> Arc<TtyDevice> {
        let dev_num = id_table.device_number();
        let dev = TtyDevice {
            name,
            id_table,
            inner: RwLock::new(InnerTtyDevice::new()),
            kobj_state: LockedKObjectState::new(None),
            fs: RwLock::new(Weak::default()),
            tty_type,
        };

        dev.inner.write().metadata.raw_dev = dev_num;

        Arc::new(dev)
    }

    pub fn inner_write(&self) -> RwLockWriteGuard<InnerTtyDevice> {
        self.inner.write()
    }

    pub fn name_ref(&self) -> &str {
        &self.name
    }

    fn tty_core(private_data: &FilePrivateData) -> Result<Arc<TtyCore>, SystemError> {
        let (tty, _) = if let FilePrivateData::Tty(tty_priv) = private_data {
            (tty_priv.tty.clone(), tty_priv.mode)
        } else {
            return Err(SystemError::EIO);
        };
        Ok(tty)
    }
}

impl PollableInode for TtyDevice {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let tty = TtyDevice::tty_core(private_data)?;
        tty.ldisc().poll(tty)
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let tty = TtyDevice::tty_core(private_data)?;
        let core = tty.core();
        core.add_epitem(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let tty = TtyDevice::tty_core(private_data)?;
        let core = tty.core();
        core.remove_epitem(epitem)
    }
}

impl IndexNode for TtyDevice {
    fn open(
        &self,
        mut data: SpinLockGuard<FilePrivateData>,
        mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        if let FilePrivateData::Tty(_) = &*data {
            return Ok(());
        }
        if self.tty_type == TtyType::Pty(PtyType::Ptm) {
            return ptmx_open(data, mode);
        }
        let dev_num = self.metadata()?.raw_dev;

        let (index, driver) =
            TtyDriverManager::lookup_tty_driver(dev_num).ok_or(SystemError::ENODEV)?;

        let tty = driver.open_tty(Some(index))?;

        // 设置privdata
        *data = FilePrivateData::Tty(TtyFilePrivateData {
            tty: tty.clone(),
            mode: *mode,
        });

        let ret = tty.open(tty.core());
        if let Err(err) = ret {
            if err == SystemError::ENOSYS {
                return Err(SystemError::ENODEV);
            }
            return Err(err);
        }

        let driver = tty.core().driver();
        // 考虑noctty（当前tty）
        if !(mode.contains(FileMode::O_NOCTTY) && dev_num == DeviceNumber::new(Major::TTY_MAJOR, 0)
            || dev_num == DeviceNumber::new(Major::TTYAUX_MAJOR, 1)
            || (driver.tty_driver_type() == TtyDriverType::Pty
                && driver.tty_driver_sub_type() == TtyDriverSubType::PtyMaster))
        {
            let pcb = ProcessManager::current_pcb();
            let pcb_tty = pcb.sig_info_irqsave().tty();
            if pcb_tty.is_none() && tty.core().contorl_info_irqsave().session.is_none() {
                TtyJobCtrlManager::proc_set_tty(tty);
            }
        }

        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        let (tty, mode) = if let FilePrivateData::Tty(tty_priv) = &*data {
            (tty_priv.tty(), tty_priv.mode)
        } else {
            return Err(SystemError::EIO);
        };

        drop(data);

        let ld = tty.ldisc();
        let mut offset = 0;
        let mut cookie = false;
        loop {
            let mut size = if len > buf.len() { buf.len() } else { len };
            size = ld.read(tty.clone(), buf, size, &mut cookie, offset, mode)?;
            // 没有更多数据
            if size == 0 {
                break;
            }

            offset += size;

            // 缓冲区写满
            if offset >= len {
                break;
            }

            // 没有更多数据
            if !cookie {
                break;
            }
        }

        return Ok(offset);
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        let mut count = len;
        let (tty, mode) = if let FilePrivateData::Tty(tty_priv) = &*data {
            (tty_priv.tty(), tty_priv.mode)
        } else {
            return Err(SystemError::EIO);
        };
        drop(data);
        let ld = tty.ldisc();
        let core = tty.core();
        let mut chunk = 2048;
        if core.flags().contains(TtyFlag::NO_WRITE_SPLIT) {
            chunk = 65536;
        }
        chunk = chunk.min(count);

        let pcb = ProcessManager::current_pcb();
        let mut written = 0;
        loop {
            // 至少需要写多少
            let size = chunk.min(count);

            // 将数据从buf拷贝到writebuf

            let ret = ld.write(tty.clone(), &buf[written..], size, mode)?;

            written += ret;
            count -= ret;

            if count == 0 {
                break;
            }

            if pcb.has_pending_signal_fast() {
                return Err(SystemError::ERESTARTSYS);
            }
        }

        if written > 0 {
            // todo: 更新时间
        }

        Ok(written)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        Ok(self.inner.read().metadata.clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut guard = self.inner_write();
        guard.metadata = metadata.clone();

        Ok(())
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        let (tty, _mode) = if let FilePrivateData::Tty(tty_priv) = &*data {
            (tty_priv.tty(), tty_priv.mode)
        } else {
            return Err(SystemError::EIO);
        };
        drop(data);
        tty.close(tty.clone())
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn ioctl(&self, cmd: u32, arg: usize, data: &FilePrivateData) -> Result<usize, SystemError> {
        let (tty, _) = if let FilePrivateData::Tty(tty_priv) = data {
            (tty_priv.tty(), tty_priv.mode)
        } else {
            return Err(SystemError::EIO);
        };

        match cmd {
            TtyIoctlCmd::TIOCSETD
            | TtyIoctlCmd::TIOCSBRK
            | TtyIoctlCmd::TIOCCBRK
            | TtyIoctlCmd::TCSBRK
            | TtyIoctlCmd::TCSBRKP => {
                TtyJobCtrlManager::tty_check_change(tty.clone(), Signal::SIGTTOU)?;
                if cmd != TtyIoctlCmd::TIOCCBRK {
                    todo!()
                }
            }
            _ => {}
        }

        match cmd {
            TtyIoctlCmd::TIOCGWINSZ => {
                let core = tty.core();
                let winsize = *core.window_size();

                let mut user_writer = UserBufferWriter::new(
                    VirtAddr::new(arg).as_ptr::<WindowSize>(),
                    core::mem::size_of::<WindowSize>(),
                    true,
                )?;

                let err = user_writer.copy_one_to_user(&winsize, 0);
                if err.is_err() {
                    return Err(SystemError::EFAULT);
                }
                return Ok(0);
            }
            TtyIoctlCmd::TIOCSWINSZ => {
                let reader = UserBufferReader::new(
                    arg as *const (),
                    core::mem::size_of::<WindowSize>(),
                    true,
                )?;

                let user_winsize = reader.read_one_from_user::<WindowSize>(0)?;

                let ret = tty.resize(tty.clone(), *user_winsize);

                if ret != Err(SystemError::ENOSYS) {
                    return ret.map(|_| 0);
                } else {
                    return tty.tty_do_resize(*user_winsize).map(|_| 0);
                }
            }
            _ => match TtyJobCtrlManager::job_ctrl_ioctl(tty.clone(), cmd, arg) {
                Ok(_) => {
                    return Ok(0);
                }
                Err(e) => {
                    if e != SystemError::ENOIOCTLCMD {
                        return Err(e);
                    }
                }
            },
        }

        match tty.ioctl(tty.clone(), cmd, arg) {
            Ok(_) => {
                return Ok(0);
            }
            Err(e) => {
                if e != SystemError::ENOIOCTLCMD {
                    return Err(e);
                }
            }
        }
        tty.ldisc().ioctl(tty, cmd, arg)?;

        Ok(0)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }
}

impl DeviceINode for TtyDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }
}

impl KObject for TtyDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<crate::filesystem::kernfs::KernFSInode>>) {
        self.inner.write().inode = inode;
    }

    fn inode(&self) -> Option<Arc<crate::filesystem::kernfs::KernFSInode>> {
        self.inner.read().inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner.read().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner.write().parent_kobj = parent
    }

    fn kset(&self) -> Option<Arc<crate::driver::base::kset::KSet>> {
        self.inner.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<crate::driver::base::kset::KSet>>) {
        self.inner.write().kset = kset
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        Some(&DeviceKObjType)
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn crate::driver::base::kobject::KObjType>) {}

    fn name(&self) -> alloc::string::String {
        self.name.to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {
        // self.name = name
    }

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(
        &self,
    ) -> crate::libs::rwlock::RwLockWriteGuard<crate::driver::base::kobject::KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: crate::driver::base::kobject::KObjectState) {
        *self.kobj_state.write() = state
    }
}

impl Device for TtyDevice {
    fn dev_type(&self) -> crate::driver::base::device::DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> crate::driver::base::device::IdTable {
        self.id_table.clone()
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.read().bus.clone()
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>>) {
        self.inner.write().bus = bus
    }

    fn set_class(&self, _class: Option<Weak<dyn crate::driver::base::class::Class>>) {
        // do nothing
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        sys_class_tty_instance()
            .cloned()
            .map(|x| x as Arc<dyn Class>)
    }

    fn driver(&self) -> Option<Arc<dyn crate::driver::base::device::driver::Driver>> {
        self.inner.read().driver.clone()?.upgrade()
    }

    fn set_driver(
        &self,
        driver: Option<alloc::sync::Weak<dyn crate::driver::base::device::driver::Driver>>,
    ) {
        self.inner.write().driver = driver
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner.read().can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner.write().can_match = can_match
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<alloc::sync::Weak<dyn crate::driver::base::device::Device>> {
        None
    }

    fn set_dev_parent(
        &self,
        _dev_parent: Option<alloc::sync::Weak<dyn crate::driver::base::device::Device>>,
    ) {
        todo!()
    }
}

impl CharDevice for TtyDevice {
    fn read(&self, _len: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn write(&self, _len: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn sync(&self) -> Result<(), SystemError> {
        todo!()
    }
}

#[derive(Debug, Clone)]
pub struct TtyFilePrivateData {
    pub tty: Arc<TtyCore>,
    pub mode: FileMode,
}

impl TtyFilePrivateData {
    pub fn tty(&self) -> Arc<TtyCore> {
        self.tty.clone()
    }
}

/// 初始化tty设备和console子设备
#[unified_init(INITCALL_DEVICE)]
#[inline(never)]
pub fn tty_init() -> Result<(), SystemError> {
    let console = TtyDevice::new(
        "console".to_string(),
        IdTable::new(
            String::from("console"),
            Some(DeviceNumber::new(Major::TTYAUX_MAJOR, 1)),
        ),
        TtyType::Tty,
    );

    // 将设备注册到devfs，TODO：这里console设备应该与tty在一个设备group里面
    device_register(console.clone())?;
    devfs_register(&console.name.clone(), console)?;

    serial_init()?;

    tty_flush_thread_init();
    return vty_init();
}
