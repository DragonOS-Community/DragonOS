use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;
use x86_64::instructions::nop;

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, device_manager, driver::Driver, Device, DeviceType, IdTable},
            kobject::{KObjType, KObject, LockedKObjectState},
            kset::KSet,
        },
        input::serio::serio_device::{serio_device_manager, SerioDevice},
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::spinlock::SpinLock,
};

extern "C" {
    fn ps2_mouse_init();
}

static mut PS2_MOUSE_DEVICE : Option<Arc<Ps2MouseDevice>> = None;

pub fn ps2_mouse_device() -> Option<Arc<Ps2MouseDevice>> {
    unsafe { PS2_MOUSE_DEVICE.clone() }
}

const ADDRESS_PORT_ADDRESS: u16 = 0x64;
const DATA_PORT_ADDRESS: u16 = 0x60;

const KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT: u8 = 0xa8;
const KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE: u8 = 0xd4;


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

#[repr(u8)]
enum Command {
    EnablePacketStreaming = 0xF4,
    // SetDefaults = 0xF6,
    InitKeyboard = 0x47,
    GetMouseId = 0xf2,
    SetSampleRate = 0xf3,
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

#[allow(dead_code)]
impl Ps2MouseDevice {
    pub const NAME: &'static str = "ps2-mouse-device";
    pub fn new() -> Self {
        let r = Self {
            inner: SpinLock::new(InnerPs2MouseDevice {
                bus: None,
                class: None,
                driver: None,
                kern_inode: None,
                parent: None,
                kset: None,
                kobj_type: None,

                command_port: ADDRESS_PORT_ADDRESS,
                data_port: DATA_PORT_ADDRESS,
                current_packet: 0,
                current_state: MouseState::new(),
            }),
            kobj_state: LockedKObjectState::new(None),
        };
        return r;
    }

    #[allow(dead_code)]
    pub fn init(&mut self) -> Result<(), SystemError> {
        self.write_command_port(KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT)?;
        for _i in 0..1000 {
            for _j in 0.. 1000 {
                nop();
            }
        }
        self.read_data_port()?;
        
        self.send_command(Command::EnablePacketStreaming as u8)?;
        self.read_data_port()?;
        for _i in 0..1000 {
            for _j in 0.. 1000 {
                nop();
            }
        }
            
        self.send_command(Command::InitKeyboard as u8)?;
        self.read_data_port()?;
        for _i in 0..1000 {
            for _j in 0.. 1000 {
                nop();
            }
        }

        self.set_sample_rate(30)?;
        // self.get_mouse_id()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_mouse_id(&self) -> Result<(), SystemError> {
        self.send_command(Command::GetMouseId as u8)?;
        let _mouse_id = self.read_data_port()?;
        Ok(())
    }

    pub fn set_sample_rate(&self, hz : u8) -> Result<(), SystemError> {
        self.send_command(Command::SetSampleRate as u8)?;
        self.read_data_port()?;
        for _i in 0..1000 {
            for _j in 0.. 1000 {
                nop();
            }
        }


        self.send_command(hz)?;
        for _i in 0..1000 {
            for _j in 0.. 1000 {
                nop();
            }
        }
        self.read_data_port()?;
        Ok(())
    }

    pub fn process_packet(&self) -> Result<(), SystemError>{
        let packet = self.read_data_port()?;
        let mut guard = self.inner.lock();
        match guard.current_packet {
            0 => {
                let flags: MouseFlags = MouseFlags::from_bits_truncate(packet);
                if !flags.contains(MouseFlags::ALWAYS_ONE) {
                    return Ok(());
                }
                guard.current_state.flags = flags;
            }
            1 => {
                let flags = guard.current_state.flags.clone();
                if !flags.contains(MouseFlags::X_OVERFLOW){
                    guard.current_state.x = self.get_x_movement(packet, flags);
                }
            }
            2 => {
                let flags = guard.current_state.flags.clone();
                if !flags.contains(MouseFlags::Y_OVERFLOW){
                    guard.current_state.y = self.get_x_movement(packet, flags);
                }
                kdebug!("Ps2MouseDevice packet :{}, {}, {}", guard.current_state.flags.bits, guard.current_state.x, guard.current_state.y);
            }
            _ => unreachable!(),
        }
        guard.current_packet = (guard.current_packet + 1) % 3;
        Ok(())
    }

    fn get_x_movement(&self, packet: u8, flags : MouseFlags) -> i16 {
        if flags.contains(MouseFlags::X_SIGN) {
            return self.sign_extend(packet)
        } else {
            return packet as i16
        }
    }

    fn get_y_movement(&self, packet: u8, flags : MouseFlags) -> i16 {
        if flags.contains(MouseFlags::Y_SIGN) {
            return self.sign_extend(packet)
        } else {
            return packet as i16
        }
    }

    fn sign_extend(&self, packet: u8) -> i16 {
        ((packet as u16) | 0xFF00) as i16
    }

    fn read_data_port(&self) -> Result<u8, SystemError> {
        // self.wait_for_write()?;
        let guard = self.inner.lock_irqsave();
        let data = unsafe { CurrentPortIOArch::in8(guard.data_port) };
        Ok(data)
    }

    fn send_command(&self, command: u8) -> Result<(), SystemError> {
        self.write_command_port(KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE)?;
        self.write_data_port(command)?;
        Ok(())
    }

    fn write_data_port(&self, data: u8) -> Result<(), SystemError> {
        self.wait_for_write()?;
        let guard = self.inner.lock_irqsave();
        unsafe {
            CurrentPortIOArch::out8(guard.data_port, data);
        }
        Ok(())
    }

    fn write_command_port(&self, command: u8) -> Result<(), SystemError> {
        self.wait_for_write()?;
        let guard = self.inner.lock_irqsave();
        unsafe {
            CurrentPortIOArch::out8(guard.command_port, command);
        }
        Ok(())
    }

    fn wait_for_read(&self) -> Result<(), SystemError> {
        let guard = self.inner.lock_irqsave();
        let timeout = 100_000;
        for _ in 0..timeout {
            let value = unsafe { CurrentPortIOArch::in8(guard.command_port) };
            if (value & 0x1) == 0x1 {
                return Ok(());
            }
        }
        Err(SystemError::ETIMEDOUT)
    }

    fn wait_for_write(&self) -> Result<(), SystemError> {
        let guard = self.inner.lock_irqsave();
        let timeout = 100_000;
        for _ in 0..timeout {
            let value = unsafe { CurrentPortIOArch::in8(guard.command_port) };
            if (value & 0x2) == 0 {
                return Ok(());
            }
        }
        Err(SystemError::ETIMEDOUT)
    }
}

#[derive(Debug)]
struct InnerPs2MouseDevice {
    bus: Option<Weak<dyn Bus>>,
    class: Option<Arc<dyn Class>>,
    driver: Option<Weak<dyn Driver>>,
    kern_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    kobj_type: Option<&'static dyn KObjType>,

    command_port: u16,
    data_port: u16,
    current_packet: u8,
    current_state: MouseState,
}

impl Device for Ps2MouseDevice {
    fn is_dead(&self) -> bool {
        false
    }

    fn dev_type(&self) -> crate::driver::base::device::DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> crate::driver::base::device::IdTable {
        IdTable::new(self.name(), None)
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>>) {
        self.inner.lock_irqsave().bus = bus;
    }

    fn set_class(&self, class: Option<alloc::sync::Arc<dyn crate::driver::base::class::Class>>) {
        self.inner.lock_irqsave().class = class;
    }

    fn driver(&self) -> Option<alloc::sync::Arc<dyn crate::driver::base::device::driver::Driver>> {
        self.inner.lock_irqsave().driver.clone()?.upgrade()
    }

    fn set_driver(
        &self,
        driver: Option<alloc::sync::Weak<dyn crate::driver::base::device::driver::Driver>>,
    ) {
        kdebug!("xkd mouse setdriver");
        self.inner.lock_irqsave().driver = driver;
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }

    fn bus(&self) -> Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>> {
        self.inner.lock_irqsave().bus.clone()
    }

    fn class(&self) -> Option<Arc<dyn crate::driver::base::class::Class>> {
        self.inner.lock_irqsave().class.clone()
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

    fn set_inode(&self, inode: Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>>) {
        self.inner.lock_irqsave().kern_inode = inode;
    }

    fn inode(&self) -> Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>> {
        self.inner.lock_irqsave().kern_inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner.lock_irqsave().parent.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner.lock_irqsave().parent = parent
    }

    fn kset(&self) -> Option<alloc::sync::Arc<crate::driver::base::kset::KSet>> {
        self.inner.lock_irqsave().kset.clone()
    }

    fn set_kset(&self, kset: Option<alloc::sync::Arc<crate::driver::base::kset::KSet>>) {
        self.inner.lock_irqsave().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        self.inner.lock_irqsave().kobj_type.clone()
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn crate::driver::base::kobject::KObjType>) {
        self.inner.lock_irqsave().kobj_type = ktype;
    }

    fn name(&self) -> alloc::string::String {
        Self::NAME.to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

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
        *self.kobj_state.write() = state;
    }
}

#[unified_init(INITCALL_DEVICE)]
fn ps2_mouse_device_int() -> Result<(), SystemError> {
    kdebug!("ps2_mouse_device initing...");
    let mut device = Ps2MouseDevice::new();
    unsafe { ps2_mouse_init() };
    device.init()?;
    let ptr = Arc::new(device);
    device_manager().device_default_initialize(&(ptr.clone() as Arc<dyn Device>));
    serio_device_manager().register_port(ptr.clone())?;
    unsafe { PS2_MOUSE_DEVICE = Some(ptr) };
    return Ok(());
}
