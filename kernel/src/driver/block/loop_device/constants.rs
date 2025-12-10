use bitflags::bitflags;
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
    /// 设置设备状态 (32位兼容)
    LoopSetStatus = 0x4C02,
    /// 获取设备状态 (32位兼容)
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
    }
}

/// Loop 设备状态信息结构体 (64位版本)
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct LoopStatus64 {
    /// 文件内偏移量
    pub lo_offset: u64,
    /// 大小限制 (0 表示无限制)
    pub lo_sizelimit: u64,
    /// 标志位
    pub lo_flags: u32,
    /// 填充字段
    pub __pad: u32,
}

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
