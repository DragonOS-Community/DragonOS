use core::hint::spin_loop;

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use kdepends::ringbuffer::{AllocRingBuffer, RingBuffer};
use log::{debug, error};
use system_error::SystemError;

use crate::{
    arch::{io::PortIOArch, CurrentIrqArch, CurrentPortIOArch},
    driver::{
        base::{
            class::Class,
            device::{
                bus::Bus, device_manager, device_number::DeviceNumber, driver::Driver, Device,
                DeviceCommonData, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        input::{
            ps2_dev::ps2_device::Ps2Device,
            serio::serio_device::{serio_device_manager, SerioDevice},
        },
    },
    exception::InterruptArch,
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        kernfs::KernFSInode,
        vfs::{
            core::generate_inode_id, syscall::ModeType, utils::DName, FilePrivateData, FileSystem,
            FileType, IndexNode, Metadata,
        },
    },
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    time::PosixTimeSpec,
};

static mut PS2_MOUSE_DEVICE: Option<Arc<Ps2MouseDevice>> = None;

pub fn ps2_mouse_device() -> Option<Arc<Ps2MouseDevice>> {
    unsafe { PS2_MOUSE_DEVICE.clone() }
}

const ADDRESS_PORT_ADDRESS: u16 = 0x64;
const DATA_PORT_ADDRESS: u16 = 0x60;

const KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT: u8 = 0xa8;
const KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE: u8 = 0xd4;

const MOUSE_BUFFER_CAPACITY: usize = 15;

bitflags! {
    /// Represents the flags currently set for the mouse.
    #[derive(Default)]
    pub struct MouseFlags: u8 {
        /// Whether or not the left mouse button is pressed.
        const LEFT_BUTTON = 0b0000_0001;

        /// Whether or not the right mouse button is pressed.
        const RIGHT_BUTTON = 0b0000_0010;

        /// Whether or not the middle mouse button is pressed.
        const MIDDLE_BUTTON = 0b0000_0100;

        /// Whether or not the packet is valid or not.
        const ALWAYS_ONE = 0b0000_1000;

        /// Whether or not the x delta is negative.
        const X_SIGN = 0b0001_0000;

        /// Whether or not the y delta is negative.
        const Y_SIGN = 0b0010_0000;

        /// Whether or not the x delta overflowed.
        const X_OVERFLOW = 0b0100_0000;

        /// Whether or not the y delta overflowed.
        const Y_OVERFLOW = 0b1000_0000;
    }
}

#[derive(Debug)]
enum PsMouseCommand {
    SampleRate(u8),
    EnablePacketStreaming,
    // SetDefaults = 0xF6,
    InitKeyboard,
    GetMouseId,
    SetSampleRate,
}

impl From<PsMouseCommand> for u8 {
    fn from(val: PsMouseCommand) -> Self {
        match val {
            PsMouseCommand::SampleRate(x) => x,
            PsMouseCommand::EnablePacketStreaming => 0xf4,
            PsMouseCommand::InitKeyboard => 0x47,
            PsMouseCommand::GetMouseId => 0xf2,
            PsMouseCommand::SetSampleRate => 0xf3,
        }
    }
}

#[derive(Debug)]
pub struct MouseState {
    flags: MouseFlags,
    x: i16,
    y: i16,
}

#[allow(dead_code)]
impl MouseState {
    /// Returns a new `MouseState`.
    pub const fn new() -> MouseState {
        MouseState {
            flags: MouseFlags::empty(),
            x: 0,
            y: 0,
        }
    }

    /// Returns true if the left mouse button is currently down.
    pub fn left_button_down(&self) -> bool {
        self.flags.contains(MouseFlags::LEFT_BUTTON)
    }

    /// Returns true if the left mouse button is currently up.
    pub fn left_button_up(&self) -> bool {
        !self.flags.contains(MouseFlags::LEFT_BUTTON)
    }

    /// Returns true if the right mouse button is currently down.
    pub fn right_button_down(&self) -> bool {
        self.flags.contains(MouseFlags::RIGHT_BUTTON)
    }

    /// Returns true if the right mouse button is currently up.
    pub fn right_button_up(&self) -> bool {
        !self.flags.contains(MouseFlags::RIGHT_BUTTON)
    }

    /// Returns true if the x axis has moved.
    pub fn x_moved(&self) -> bool {
        self.x != 0
    }

    /// Returns true if the y axis has moved.
    pub fn y_moved(&self) -> bool {
        self.y != 0
    }

    /// Returns true if the x or y axis has moved.
    pub fn moved(&self) -> bool {
        self.x_moved() || self.y_moved()
    }

    /// Returns the x delta of the mouse state.
    pub fn get_x(&self) -> i16 {
        self.x
    }

    /// Returns the y delta of the mouse state.
    pub fn get_y(&self) -> i16 {
        self.y
    }
}

#[derive(Debug)]
#[cast_to([sync] Device, SerioDevice)]
pub struct Ps2MouseDevice {
    inner: SpinLock<InnerPs2MouseDevice>,
    kobj_state: LockedKObjectState,
}

impl Ps2MouseDevice {
    pub const NAME: &'static str = "psmouse";
    pub fn new() -> Self {
        let r = Self {
            inner: SpinLock::new(InnerPs2MouseDevice {
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                current_packet: 0,
                current_state: MouseState::new(),
                buf: AllocRingBuffer::new(MOUSE_BUFFER_CAPACITY),
                devfs_metadata: Metadata {
                    dev_id: 1,
                    inode_id: generate_inode_id(),
                    size: 4096,
                    blk_size: 0,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    file_type: FileType::CharDevice, // 文件夹，block设备，char设备
                    mode: ModeType::from_bits_truncate(0o644),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(), // 这里用来作为device number
                },
                device_inode_fs: None,
            }),
            kobj_state: LockedKObjectState::new(None),
        };
        return r;
    }

    pub fn init(&self) -> Result<(), SystemError> {
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        self.write_control_port(KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT)?;
        for _i in 0..1000 {
            for _j in 0..1000 {
                spin_loop();
            }
        }
        self.read_data_port().ok();

        self.send_command_to_ps2mouse(PsMouseCommand::EnablePacketStreaming)
            .map_err(|e| {
                error!("ps2 mouse init error: {:?}", e);
                e
            })?;
        self.read_data_port().ok();
        for _i in 0..1000 {
            for _j in 0..1000 {
                spin_loop();
            }
        }

        // self.send_command_to_ps2mouse(PsMouseCommand::InitKeyboard)?;
        self.do_send_command(DATA_PORT_ADDRESS as u8, PsMouseCommand::InitKeyboard.into())?;
        self.read_data_port().ok();
        for _i in 0..1000 {
            for _j in 0..1000 {
                spin_loop();
            }
        }

        self.set_sample_rate(20)?;
        // self.get_mouse_id()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_mouse_id(&self) -> Result<(), SystemError> {
        self.send_command_to_ps2mouse(PsMouseCommand::GetMouseId)?;
        let _mouse_id = self.read_data_port()?;
        Ok(())
    }

    /// 设置鼠标采样率
    ///
    /// `hz` 合法值为 10,20,40,60,80,100,200
    pub fn set_sample_rate(&self, hz: u8) -> Result<(), SystemError> {
        const SAMPLE_RATE: [u8; 7] = [10, 20, 40, 60, 80, 100, 200];
        if !SAMPLE_RATE.contains(&hz) {
            return Err(SystemError::EINVAL);
        }

        self.send_command_to_ps2mouse(PsMouseCommand::SetSampleRate)?;
        self.read_data_port().ok();
        for _i in 0..1000 {
            for _j in 0..1000 {
                spin_loop();
            }
        }

        self.send_command_to_ps2mouse(PsMouseCommand::SampleRate(hz))?;
        for _i in 0..1000 {
            for _j in 0..1000 {
                spin_loop();
            }
        }
        self.read_data_port().ok();
        Ok(())
    }

    /// # 函数的功能
    /// 鼠标设备处理数据包
    pub fn process_packet(&self) -> Result<(), SystemError> {
        let packet = self.read_data_port()?;
        let mut guard = self.inner.lock();
        guard.buf.push(packet); // 更新缓冲区
        match guard.current_packet {
            0 => {
                let flags: MouseFlags = MouseFlags::from_bits_truncate(packet);
                if !flags.contains(MouseFlags::ALWAYS_ONE) {
                    return Ok(());
                }
                guard.current_state.flags = flags;
            }
            1 => {
                let flags = guard.current_state.flags;
                if !flags.contains(MouseFlags::X_OVERFLOW) {
                    guard.current_state.x = self.get_x_movement(packet, flags);
                }
            }
            2 => {
                let flags = guard.current_state.flags;
                if !flags.contains(MouseFlags::Y_OVERFLOW) {
                    guard.current_state.y = self.get_y_movement(packet, flags);
                }

                // debug!(
                //     "Ps2MouseDevice packet : flags:{}, x:{}, y:{}\n",
                //     guard.current_state.flags.bits,
                //     guard.current_state.x,
                //     guard.current_state.y
                // );
            }
            _ => unreachable!(),
        }
        guard.current_packet = (guard.current_packet + 1) % 3;
        Ok(())
    }

    fn get_x_movement(&self, packet: u8, flags: MouseFlags) -> i16 {
        if flags.contains(MouseFlags::X_SIGN) {
            return self.sign_extend(packet);
        } else {
            return packet as i16;
        }
    }

    fn get_y_movement(&self, packet: u8, flags: MouseFlags) -> i16 {
        if flags.contains(MouseFlags::Y_SIGN) {
            return self.sign_extend(packet);
        } else {
            return packet as i16;
        }
    }

    fn sign_extend(&self, packet: u8) -> i16 {
        ((packet as u16) | 0xFF00) as i16
    }

    fn read_data_port(&self) -> Result<u8, SystemError> {
        self.wait_for_write()?;
        let cmd = unsafe { CurrentPortIOArch::in8(ADDRESS_PORT_ADDRESS) };
        if (cmd & 0x21) == 0x21 {
            let data = unsafe { CurrentPortIOArch::in8(DATA_PORT_ADDRESS) };
            return Ok(data);
        } else {
            return Err(SystemError::ENODATA);
        }
    }

    #[inline(never)]
    fn send_command_to_ps2mouse(&self, command: PsMouseCommand) -> Result<(), SystemError> {
        self.do_send_command(KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE, command.into())?;
        Ok(())
    }

    #[inline(never)]
    fn do_send_command(&self, ctrl: u8, command: u8) -> Result<(), SystemError> {
        self.write_control_port(ctrl)?;
        self.write_data_port(command)?;
        return Ok(());
    }

    fn write_data_port(&self, data: u8) -> Result<(), SystemError> {
        self.wait_for_write()?;
        unsafe {
            CurrentPortIOArch::out8(DATA_PORT_ADDRESS, data);
        }
        Ok(())
    }

    fn write_control_port(&self, command: u8) -> Result<(), SystemError> {
        self.wait_for_write()?;
        unsafe {
            CurrentPortIOArch::out8(ADDRESS_PORT_ADDRESS, command);
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn wait_for_read(&self) -> Result<(), SystemError> {
        let timeout = 100_000;
        for _ in 0..timeout {
            let value = unsafe { CurrentPortIOArch::in8(ADDRESS_PORT_ADDRESS) };
            if (value & 0x1) == 0x1 {
                return Ok(());
            }
        }
        Err(SystemError::ETIMEDOUT)
    }

    fn wait_for_write(&self) -> Result<(), SystemError> {
        let timeout = 100_000;
        for _ in 0..timeout {
            let value = unsafe { CurrentPortIOArch::in8(ADDRESS_PORT_ADDRESS) };
            if (value & 0x2) == 0 {
                return Ok(());
            }
        }
        Err(SystemError::ETIMEDOUT)
    }

    fn inner(&self) -> SpinLockGuard<InnerPs2MouseDevice> {
        self.inner.lock_irqsave()
    }
}

#[derive(Debug)]
struct InnerPs2MouseDevice {
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,

    /// 鼠标数据
    current_state: MouseState,
    current_packet: u8,
    /// 鼠标数据环形缓冲区
    buf: AllocRingBuffer<u8>,

    /// device inode要求的字段
    device_inode_fs: Option<Weak<DevFS>>,
    devfs_metadata: Metadata,
}

impl Device for Ps2MouseDevice {
    fn is_dead(&self) -> bool {
        false
    }

    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(self.name().to_string(), None)
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn set_class(&self, class: Option<alloc::sync::Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<alloc::sync::Arc<dyn Driver>> {
        self.inner().device_common.driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<alloc::sync::Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }

    fn bus(&self) -> Option<alloc::sync::Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }

        return r;
    }

    fn dev_parent(&self) -> Option<alloc::sync::Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<alloc::sync::Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

impl SerioDevice for Ps2MouseDevice {
    fn write(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
        _data: u8,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn open(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn close(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn start(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn stop(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }
}

impl KObject for Ps2MouseDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<alloc::sync::Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<alloc::sync::Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent
    }

    fn kset(&self) -> Option<alloc::sync::Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<alloc::sync::Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> alloc::string::String {
        Self::NAME.to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

impl DeviceINode for Ps2MouseDevice {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.inner.lock_irqsave().device_inode_fs = Some(fs);
    }
}

impl IndexNode for Ps2MouseDevice {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        let mut guard = self.inner.lock_irqsave();
        guard.buf.clear();
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        let mut guard = self.inner.lock_irqsave();
        guard.buf.clear();
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mut guard = self.inner.lock_irqsave();

        if guard.buf.len() >= 3 {
            for item in buf.iter_mut().take(3) {
                *item = guard.buf.dequeue().unwrap();
            }
            return Ok(3);
        } else {
            return Ok(0);
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.inner
            .lock_irqsave()
            .device_inode_fs
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<alloc::string::String>, SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.inner.lock_irqsave().devfs_metadata.clone())
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from(self.name()))
    }
}

impl Ps2Device for Ps2MouseDevice {}

pub fn rs_ps2_mouse_device_init(dev_parent: Arc<dyn Device>) -> Result<(), SystemError> {
    debug!("ps2_mouse_device initializing...");
    let psmouse = Arc::new(Ps2MouseDevice::new());

    device_manager().device_default_initialize(&(psmouse.clone() as Arc<dyn Device>));
    psmouse.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
    serio_device_manager().register_port(psmouse.clone() as Arc<dyn SerioDevice>)?;

    devfs_register(&psmouse.name(), psmouse.clone()).map_err(|e| {
        error!(
            "register psmouse device '{}' to devfs failed: {:?}",
            psmouse.name(),
            e
        );
        device_manager().remove(&(psmouse.clone() as Arc<dyn Device>));
        e
    })?;

    unsafe { PS2_MOUSE_DEVICE = Some(psmouse) };
    return Ok(());
}
