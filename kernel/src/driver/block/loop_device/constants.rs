use bitflags::bitflags;
use system_error::SystemError;
/// Loop 设备基础名称
pub const LOOP_BASENAME: &str = "loop";

/// Loop-control 设备基础名称
pub const LOOP_CONTROL_BASENAME: &str = "loop-control";

/// Loop-control 设备的次设备号
pub const LOOP_CONTROL_MINOR: u32 = 237;

/// I/O 排空超时时间 (毫秒)
pub const LOOP_IO_DRAIN_TIMEOUT_MS: u32 = 30_000;

/// I/O 排空检查间隔 (微秒)
pub const LOOP_IO_DRAIN_CHECK_INTERVAL_US: u32 = 10_000;

/// Loop 设备 ioctl 命令
#[repr(u32)]
#[derive(Debug, FromPrimitive)]
pub enum LoopIoctl {
    /// 设置后端文件描述符
    LoopSetFd = 0x4C00,
    /// 清除后端文件绑定
    LoopClrFd = 0x4C01,
    /// 设置设备状态（native legacy ABI）
    LoopSetStatus = 0x4C02,
    /// 获取设备状态（native legacy ABI）
    LoopGetStatus = 0x4C03,
    /// 设置设备状态 (64位)
    LoopSetStatus64 = 0x4C04,
    /// 获取设备状态 (64位)
    LoopGetStatus64 = 0x4C05,
    /// 更换后端文件描述符
    LoopChangeFd = 0x4C06,
    /// 重新计算设备容量
    LoopSetCapacity = 0x4C07,
    /// 设置直接I/O模式
    LoopSetDirectIo = 0x4C08,
    /// 设置块大小
    LoopSetBlockSize = 0x4C09,
    /// 配置设备
    LoopConfigure = 0x4C0A,
}

/// Loop-control 设备 ioctl 命令
#[repr(u32)]
#[derive(Debug, FromPrimitive)]
pub enum LoopControlIoctl {
    /// 添加新的 loop 设备
    Add = 0x4C80,
    /// 删除 loop 设备
    Remove = 0x4C81,
    /// 获取空闲的 loop 设备
    GetFree = 0x4C82,
}

bitflags! {
    /// Loop 设备标志位
    #[derive(Default)]
    pub struct LoopFlags: u32 {
        /// 只读模式
        const READ_ONLY = 1 << 0;
        /// Clear the backing file after the last opener and mount holder leave.
        const AUTOCLEAR = 1 << 2;
        /// Enable partition scanning. Once enabled, status updates cannot
        /// clear it; the complete binding teardown clears the flag.
        const PARTSCAN = 1 << 3;
        /// Effective direct-I/O mode, published by the loop configuration owner.
        const DIRECT_IO = 1 << 4;
    }
}

/// legacy loop_info 中 name 字段长度
pub const LOOP_NAME_SIZE: usize = 64;

/// legacy loop_info 中加密 key 字段长度
pub const LOOP_KEY_SIZE: usize = 32;

/// Linux UAPI: `__kernel_old_dev_t`
///
/// Native x86_64 overrides the asm-generic unsigned int with unsigned long.
/// The other supported 64-bit architectures use the asm-generic definition.
#[cfg(target_arch = "x86_64")]
type KernelOldDevT = u64;
#[cfg(not(target_arch = "x86_64"))]
type KernelOldDevT = u32;

/// Loop 设备状态信息结构体（legacy 版本，对应 Linux 的 `struct loop_info`）
///
/// 注意：这不是 `loop_info64`（后者对应 `LoopStatus64`）。
/// `LOOP_SET_STATUS/LOOP_GET_STATUS`(0x4C02/0x4C03) 必须使用该布局，否则会造成用户态字段错位。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LoopStatus {
    pub lo_number: i32,
    // Explicit C ABI padding keeps whole-structure copyout fully initialized.
    #[cfg(target_arch = "x86_64")]
    pub(super) _pad_after_number: u32,
    pub lo_device: KernelOldDevT,
    pub lo_inode: u64,
    pub lo_rdevice: KernelOldDevT,
    pub lo_offset: i32,
    pub lo_encrypt_type: i32,
    pub lo_encrypt_key_size: i32,
    pub lo_flags: i32,
    pub lo_name: [u8; LOOP_NAME_SIZE],
    pub lo_encrypt_key: [u8; LOOP_KEY_SIZE],
    #[cfg(not(target_arch = "x86_64"))]
    pub(super) _pad_before_init: u32,
    pub lo_init: [u64; 2],
    pub reserved: [u8; 4],
    pub(super) _pad_after_reserved: u32,
}

impl Default for LoopStatus {
    fn default() -> Self {
        Self {
            lo_number: 0,
            #[cfg(target_arch = "x86_64")]
            _pad_after_number: 0,
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_name: [0u8; LOOP_NAME_SIZE],
            lo_encrypt_key: [0u8; LOOP_KEY_SIZE],
            #[cfg(not(target_arch = "x86_64"))]
            _pad_before_init: 0,
            lo_init: [0u64; 2],
            reserved: [0u8; 4],
            _pad_after_reserved: 0,
        }
    }
}

/// Loop 设备状态信息结构体 (64位版本)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LoopStatus64 {
    /// ioctl r/o
    pub lo_device: u64,
    /// ioctl r/o
    pub lo_inode: u64,
    /// ioctl r/o
    pub lo_rdevice: u64,
    /// 文件内偏移量
    pub lo_offset: u64,
    /// 大小限制 (0 表示无限制)
    pub lo_sizelimit: u64,
    /// ioctl r/o
    pub lo_number: u32,
    /// obsolete, ignored
    pub lo_encrypt_type: u32,
    /// ioctl w/o
    pub lo_encrypt_key_size: u32,
    /// 标志位
    pub lo_flags: u32,
    pub lo_file_name: [u8; LOOP_NAME_SIZE],
    pub lo_crypt_name: [u8; LOOP_NAME_SIZE],
    pub lo_encrypt_key: [u8; LOOP_KEY_SIZE],
    pub lo_init: [u64; 2],
}

impl Default for LoopStatus64 {
    fn default() -> Self {
        Self {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_sizelimit: 0,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_file_name: [0u8; LOOP_NAME_SIZE],
            lo_crypt_name: [0u8; LOOP_NAME_SIZE],
            lo_encrypt_key: [0u8; LOOP_KEY_SIZE],
            lo_init: [0u64; 2],
        }
    }
}

/// Native Linux `struct loop_config`, shared by CONFIGURE and legacy SET_FD.
/// All initial mapping/status fields are committed with the binding itself.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LoopConfig {
    pub fd: u32,
    pub block_size: u32,
    pub info: LoopStatus64,
    pub reserved: [u64; 8],
}

impl LoopStatus64 {
    /// Native legacy input is normalized before the configuration owner checks
    /// state and parameters. Linux ignores its obsolete crypto fields and
    /// resets sizelimit, which has no representation in this ABI.
    // old_dev_t is u64 on x86_64 and u32 on the generic 64-bit ABIs.
    #[allow(clippy::unnecessary_cast)]
    pub fn from_legacy(info: &LoopStatus) -> Self {
        Self {
            lo_number: info.lo_number as u32,
            lo_device: info.lo_device as u64,
            lo_inode: info.lo_inode,
            lo_rdevice: info.lo_rdevice as u64,
            // Preserve sign extension; the owner reports EOVERFLOW for a
            // negative offset after checking that the device is bound.
            lo_offset: info.lo_offset as i64 as u64,
            lo_flags: info.lo_flags as u32,
            lo_file_name: info.lo_name,
            ..Self::default()
        }
    }
}

impl TryFrom<LoopStatus64> for LoopStatus {
    type Error = SystemError;

    fn try_from(info: LoopStatus64) -> Result<Self, Self::Error> {
        Ok(Self {
            lo_number: info.lo_number as i32,
            lo_device: KernelOldDevT::try_from(info.lo_device)
                .map_err(|_| SystemError::EOVERFLOW)?,
            lo_inode: info.lo_inode,
            lo_rdevice: KernelOldDevT::try_from(info.lo_rdevice)
                .map_err(|_| SystemError::EOVERFLOW)?,
            lo_offset: i32::try_from(info.lo_offset).map_err(|_| SystemError::EOVERFLOW)?,
            lo_flags: info.lo_flags as i32,
            lo_name: info.lo_file_name,
            ..Self::default()
        })
    }
}

// These are native Linux UAPI layouts. Legacy is not a compat32 ABI; DragonOS
// currently executes ELF64 only. There must be no implicit padding in the
// structures sent through protected byte copyout.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(align_of::<LoopStatus>() == 8);
    #[cfg(target_arch = "x86_64")]
    {
        assert!(size_of::<LoopStatus>() == 168);
        assert!(offset_of!(LoopStatus, lo_device) == 8);
        assert!(offset_of!(LoopStatus, lo_offset) == 32);
        assert!(offset_of!(LoopStatus, lo_name) == 48);
        assert!(offset_of!(LoopStatus, lo_init) == 144);
        assert!(offset_of!(LoopStatus, reserved) == 160);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        assert!(size_of::<LoopStatus>() == 160);
        assert!(offset_of!(LoopStatus, lo_device) == 4);
        assert!(offset_of!(LoopStatus, lo_offset) == 20);
        assert!(offset_of!(LoopStatus, lo_name) == 36);
        assert!(offset_of!(LoopStatus, lo_init) == 136);
        assert!(offset_of!(LoopStatus, reserved) == 152);
    }
    assert!(size_of::<LoopConfig>() == 304);
    assert!(align_of::<LoopConfig>() == 8);
    assert!(offset_of!(LoopConfig, info) == 8);
    assert!(offset_of!(LoopConfig, reserved) == 240);
    assert!(size_of::<LoopStatus64>() == 232);
    assert!(align_of::<LoopStatus64>() == 8);
    assert!(offset_of!(LoopStatus64, lo_offset) == 24);
    assert!(offset_of!(LoopStatus64, lo_flags) == 52);
    assert!(offset_of!(LoopStatus64, lo_file_name) == 56);
    assert!(offset_of!(LoopStatus64, lo_init) == 216);
};

/// Loop 设备状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopState {
    /// 未绑定状态
    Unbound,
    /// 已绑定状态
    Bound,
    /// 正在停止运行 (不再接受新 I/O)
    Rundown,
    /// 正在排空活跃 I/O
    Draining,
    /// 正在删除
    Deleting,
}
