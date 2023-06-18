use crate::kdebug;

pub mod ioapic;
pub mod x2apic;
pub mod xapic;

pub trait LocalAPIC {
    /// @brief 判断当前处理器是否支持这个类型的apic
    ///
    /// @return true 当前处理器支持这个类型的apic
    /// @return false 当前处理器不支持这个类型的apic
    fn support() -> bool;

    /// @brief 为当前处理器初始化local apic
    ///
    /// @return true 初始化成功
    /// @return false 初始化失败
    fn init_current_cpu(&mut self) -> bool;

    /// @brief 发送EOI信号（End of interrupt）
    fn send_eoi(&mut self);

    /// @brief 获取APIC版本号
    fn version(&self) -> u32;

    /// @brief 获取当前处理器的APIC ID
    fn id(&self) -> u32;

    /// @brief 设置LVT寄存器
    ///
    /// @param register 寄存器
    /// @param lvt 要被设置成的值
    fn set_lvt(&mut self, register: LVTRegister, lvt: LVT);
}

/// @brief 所有LVT寄存器的枚举类型
#[allow(dead_code)]
#[repr(u32)]
#[derive(Debug)]
pub enum LVTRegister {
    /// CMCI寄存器
    ///
    /// 如果支持CMCI功能，那么，当修正的机器错误超过阈值时，Local APIC通过CMCI寄存器的配置，
    /// 向处理器核心投递中断消息
    CMCI = 0x02F0,
    /// 定时器寄存器
    ///
    /// 当APIC定时器产生中断信号时，Local APIC通过定时器寄存器的设置，向处理器投递中断消息
    Timer = 0x0320,
    /// 温度传感器寄存器
    ///
    /// 当处理器内部的温度传感器产生中断请求信号时，Local APIC会通过温度传感器寄存器的设置，
    /// 向处理器投递中断消息。
    Thermal = 0x0330,
    /// 性能监控计数器寄存器
    ///
    /// 当性能检测计数器寄存器溢出，产生中断请求时，Local APIC将会根据这个寄存器的配置，
    /// 向处理器投递中断消息
    PerformanceMonitor = 0x0340,
    /// 当处理器的LINT0引脚接收到中断请求信号时，Local APIC会根据这个寄存器的配置，
    /// 向处理器投递中断消息
    LINT0 = 0x0350,
    /// 当处理器的LINT0引脚接收到中断请求信号时，Local APIC会根据这个寄存器的配置，
    /// 向处理器投递中断消息
    LINT1 = 0x0360,
    /// 错误寄存器
    ///
    /// 当APIC检测到内部错误而产生中断请求信号时，它将会通过错误寄存器的设置，向处理器投递中断消息
    ErrorReg = 0x0370,
}

impl Into<u32> for LVTRegister {
    fn into(self) -> u32 {
        self as u32
    }
}

#[derive(Debug)]
pub struct LVT {
    data: u32,
}

impl LVT {
    pub fn new(vector: u8, mode: DeliveryMode, status: DeliveryStatus) {
        // new函数还需要加入更多的形参，以便完整的设置LVT
        todo!()
    }

    /// @brief 判断LVT结构体的值是否合法
    pub fn validate(&self) -> Result<(), LVTError> {
        todo!()
    }
}

/// @brief LVT结构体的错误
#[derive(Debug)]
pub enum LVTError {
    // todo
}

/// @brief
#[allow(dead_code)]
#[derive(Debug)]
pub enum DeliveryMode {
    /// 由LVT寄存器的向量号区域指定中断向量号
    Fixed = 0b000,
    /// 通过处理器的SMI信号线，向处理器投递SMI中断请求。
    /// 由于兼容性的原因，使用此投递模式时，LVT的中断向量号区域必须设置为0。
    SMI = 0b010,
    /// 向处理器投递不可屏蔽中断，并忽略向量号区域
    NMI = 0b100,
    /// 向处理器投递INIT中断请求，处理器会执行初始化的过程。
    /// 由于兼容性的原因，使用此投递模式时，LVT的中断向量号区域必须设置为0。
    /// CMCI、温度传感器、性能监控计数器等寄存器均不支持INIT投递模式
    INIT = 0b101,

    /// 向目标处理器投递Start-Up IPI。
    ///
    /// 这个向量通常由多核引导模块调用（请参阅Intel开发手册Volume3 Section 8.4,
    /// Multiple-Processor (MP) Initialization）。
    /// 如果源APIC无法投递这个IPI，它不会自动重发。如果Start-Up IPI未成功投递，
    /// 则交由软件决定是否在必要时重新投递SIPI
    StartUp = 0b110,

    /// ExtINT模式可以将类8259A中断控制器产生的中断请求投递到处理器，并接收类
    /// 8259A中断控制器提供的中断向量号。
    /// CMCI、温度传感器、性能监控计数器等寄存器均不支持ExtINT投递模式
    ExtINT = 0b111,
}

/// @brief 投递状态
#[derive(Debug)]
#[allow(dead_code)]
pub enum DeliveryStatus {
    /// 空闲态。
    /// 此状态表明，当前中断源未产生中断，或者产生的中断已经投递到处理器，并被处理器处理。
    Idle = 0,
    /// 发送挂起状态。
    /// 此状态表明，中断源产生的请求已经投递至处理器，但尚未被处理器处理。
    SendPending = 1,
}

#[no_mangle]
pub extern "C" fn rs_apic_init() -> i32 {
    let r = apic_init();
    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err();
    }
}

/// @brief 初始化apic
pub fn apic_init() -> Result<(), i32> {
    kdebug!("Support xAPIC?.. {}", xapic::XApic::support());
    kdebug!("Support x2APIC?.. {}", x2apic::X2Apic::support());
    return Ok(());
}
