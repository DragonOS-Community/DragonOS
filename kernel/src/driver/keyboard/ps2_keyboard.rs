use core::{ffi::c_void, hint::spin_loop, sync::atomic::AtomicI32};

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use bitfield_struct::bitfield;
use unified_init::macros::unified_init;

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::{
        base::device::device_number::{DeviceNumber, Major},
        tty::tty_device::TTY_DEVICES,
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandleFlags, IrqHandler, IrqReturn},
        manage::irq_manager,
        IrqNumber,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        vfs::{
            core::generate_inode_id, file::FileMode, syscall::ModeType, FilePrivateData,
            FileSystem, FileType, IndexNode, Metadata,
        },
    },
    init::initcall::INITCALL_DEVICE,
    libs::{keyboard_parser::TypeOneFSM, rwlock::RwLock, spinlock::SpinLock},
    time::TimeSpec,
};
use system_error::SystemError;

/// PS2键盘的中断向量号
const PS2_KEYBOARD_INTR_VECTOR: IrqNumber = IrqNumber::new(0x21);

const PORT_PS2_KEYBOARD_DATA: u8 = 0x60;
const PORT_PS2_KEYBOARD_STATUS: u8 = 0x64;
const PORT_PS2_KEYBOARD_CONTROL: u8 = 0x64;

/// 向键盘发送配置命令
const PS2_KEYBOARD_COMMAND_WRITE: u8 = 0x60;
/// 读取键盘的配置值
const PS2_KEYBOARD_COMMAND_READ: u8 = 0x20;
/// 初始化键盘控制器的配置值
const PS2_KEYBOARD_PARAM_INIT: u8 = 0x47;

/// PS2键盘控制器的状态寄存器
#[bitfield(u8)]
struct StatusRegister {
    /// 输出缓冲区满标志
    ///
    /// （必须在尝试从 IO 端口 0x60 读取数据之前设置）
    outbuf_full: bool,

    /// 输入缓冲区满标志
    ///
    /// （在尝试向 IO 端口 0x60 或 IO 端口 0x64 写入数据之前必须清除）
    inbuf_full: bool,

    /// 系统标志
    ///
    /// 如果系统通过自检 (POST)，则意味着在复位时被清除并由固件设置（通过 PS/2 控制器配置字节）
    system_flag: bool,

    /// 命令/数据标志
    ///
    /// （0 = 写入输入缓冲区的数据是 PS/2 设备的数据，1 = 写入输入缓冲区的数据是 PS/2 控制器命令的数据）
    command_data: bool,

    /// 未知标志1
    ///
    /// 可能是“键盘锁”（现代系统中更可能未使用）
    unknown1: bool,

    /// 未知标志2
    ///
    /// 可能是“接收超时”或“第二个 PS/2 端口输出缓冲区已满”
    unknown2: bool,
    /// 超时错误标志
    ///
    /// 超时错误（0 = 无错误，1 = 超时错误）
    timeout_error: bool,

    /// 奇偶校验错误标志
    ///
    /// （0 = 无错误，1 = 奇偶校验错误）
    parity_error: bool,
}

#[derive(Debug)]
pub struct LockedPS2KeyBoardInode(RwLock<PS2KeyBoardInode>);

lazy_static! {
    static ref PS2_KEYBOARD_FSM: SpinLock<TypeOneFSM> = {
        let tty0 = TTY_DEVICES
            .read()
            .get("tty0")
            .expect("Initializing PS2_KEYBOARD_FSM: Cannot found TTY0!")
            .clone();
        SpinLock::new(TypeOneFSM::new(tty0))
    };
}

#[derive(Debug)]
pub struct PS2KeyBoardInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedPS2KeyBoardInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

impl LockedPS2KeyBoardInode {
    pub fn new() -> Arc<Self> {
        let inode = PS2KeyBoardInode {
            // uuid: Uuid::new_v5(),
            self_ref: Weak::default(),
            fs: Weak::default(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::CharDevice, // 文件夹，block设备，char设备
                mode: ModeType::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::new(Major::INPUT_MAJOR, 0), // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedPS2KeyBoardInode(RwLock::new(inode)));
        result.0.write().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedPS2KeyBoardInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.write().fs = fs;
    }
}

fn ps2_keyboard_register() {
    devfs_register("ps2_keyboard", LockedPS2KeyBoardInode::new())
        .expect("Failed to register ps/2 keyboard");
}

impl IndexNode for LockedPS2KeyBoardInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.read().metadata.clone());
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.write();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn fs(&self) -> alloc::sync::Arc<dyn FileSystem> {
        return self.0.read().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

#[derive(Debug)]
struct Ps2KeyboardIrqHandler;

impl IrqHandler for Ps2KeyboardIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dev_id: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        // 先检查状态寄存器，看看是否有数据
        let status = unsafe { CurrentPortIOArch::in8(PORT_PS2_KEYBOARD_STATUS.into()) };
        let status = StatusRegister::from(status);
        if !status.outbuf_full() {
            return Ok(IrqReturn::NotHandled);
        }

        let input = unsafe { CurrentPortIOArch::in8(PORT_PS2_KEYBOARD_DATA.into()) };
        // wait_ps2_keyboard_read();
        PS2_KEYBOARD_FSM.lock().parse(input);

        return Ok(IrqReturn::Handled);
    }
}

impl Ps2KeyboardIrqHandler {
    const INTR_HANDLE_FLAGS: IrqHandleFlags =
        IrqHandleFlags::from_bits_truncate(IrqHandleFlags::IRQF_TRIGGER_RISING.bits());
}

/// 等待 PS/2 键盘的输入缓冲区为空
fn wait_ps2_keyboard_write() {
    let mut status = StatusRegister::new();
    loop {
        status = StatusRegister::from(unsafe {
            CurrentPortIOArch::in8(PORT_PS2_KEYBOARD_STATUS.into())
        });
        if !status.inbuf_full() {
            break;
        }

        spin_loop();
    }
}
#[unified_init(INITCALL_DEVICE)]
fn ps2_keyboard_init() -> Result<(), SystemError> {
    // ======== 初始化键盘控制器，写入配置值 =========
    wait_ps2_keyboard_write();
    unsafe {
        CurrentPortIOArch::out8(PORT_PS2_KEYBOARD_CONTROL.into(), PS2_KEYBOARD_COMMAND_WRITE);
        wait_ps2_keyboard_write();
        CurrentPortIOArch::out8(PORT_PS2_KEYBOARD_DATA.into(), PS2_KEYBOARD_PARAM_INIT);
        wait_ps2_keyboard_write();
    }

    // 执行一百万次nop，等待键盘控制器把命令执行完毕
    for _ in 0..1000000 {
        spin_loop();
    }

    irq_manager()
        .request_irq(
            PS2_KEYBOARD_INTR_VECTOR,
            "ps2keyboard".to_string(),
            &Ps2KeyboardIrqHandler,
            Ps2KeyboardIrqHandler::INTR_HANDLE_FLAGS,
            None,
        )
        .expect("Failed to request irq for ps2 keyboard");

    // 先读一下键盘的数据，防止由于在键盘初始化之前，由于按键被按下从而导致接收不到中断。
    let status = unsafe { CurrentPortIOArch::in8(PORT_PS2_KEYBOARD_STATUS.into()) };
    let status = StatusRegister::from(status);
    if status.outbuf_full() {
        unsafe { CurrentPortIOArch::in8(PORT_PS2_KEYBOARD_DATA.into()) };
    }

    // 将设备挂载到devfs
    ps2_keyboard_register();

    Ok(())
}
