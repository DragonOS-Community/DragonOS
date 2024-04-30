use bitfield_struct::bitfield;

pub mod ps2_device;

/// PS2键盘控制器的状态寄存器
#[bitfield(u8)]
pub struct Ps2StatusRegister {
    /// 输出缓冲区满标志
    ///
    /// （必须在尝试从 IO 端口 0x60 读取数据之前设置）
    pub outbuf_full: bool,

    /// 输入缓冲区满标志
    ///
    /// （在尝试向 IO 端口 0x60 或 IO 端口 0x64 写入数据之前必须清除）
    pub inbuf_full: bool,

    /// 系统标志
    ///
    /// 如果系统通过自检 (POST)，则意味着在复位时被清除并由固件设置（通过 PS/2 控制器配置字节）
    pub system_flag: bool,

    /// 命令/数据标志
    ///
    /// （0 = 写入输入缓冲区的数据是 PS/2 设备的数据，1 = 写入输入缓冲区的数据是 PS/2 控制器命令的数据）
    pub command_data: bool,

    /// 未知标志1
    ///
    /// 可能是“键盘锁”（现代系统中更可能未使用）
    pub unknown1: bool,

    /// 未知标志2
    ///
    /// 可能是“接收超时”或“第二个 PS/2 端口输出缓冲区已满”
    pub unknown2: bool,
    /// 超时错误标志
    ///
    /// 超时错误（0 = 无错误，1 = 超时错误）
    pub timeout_error: bool,

    /// 奇偶校验错误标志
    ///
    /// （0 = 无错误，1 = 奇偶校验错误）
    pub parity_error: bool,
}
