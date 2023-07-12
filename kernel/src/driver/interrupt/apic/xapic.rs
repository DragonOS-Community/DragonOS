use core::ptr::{read_volatile, write_volatile};

use super::{LVTRegister, LocalAPIC, LVT};

/// TODO：统一变量
/// @brief local APIC 寄存器地址偏移量
#[derive(Debug)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
#[repr(u32)]
pub enum LocalApicOffset {
    // 定义各个寄存器的地址偏移量
    LOCAL_APIC_OFFSET_Local_APIC_ID = 0x20,
    LOCAL_APIC_OFFSET_Local_APIC_Version = 0x30,
    LOCAL_APIC_OFFSET_Local_APIC_TPR = 0x80,
    LOCAL_APIC_OFFSET_Local_APIC_APR = 0x90,
    LOCAL_APIC_OFFSET_Local_APIC_PPR = 0xa0,
    LOCAL_APIC_OFFSET_Local_APIC_EOI = 0xb0,
    LOCAL_APIC_OFFSET_Local_APIC_RRD = 0xc0,
    LOCAL_APIC_OFFSET_Local_APIC_LDR = 0xd0,
    LOCAL_APIC_OFFSET_Local_APIC_DFR = 0xe0,
    LOCAL_APIC_OFFSET_Local_APIC_SVR = 0xf0,

    LOCAL_APIC_OFFSET_Local_APIC_ISR_31_0 = 0x100, // In-Service Register
    LOCAL_APIC_OFFSET_Local_APIC_ISR_63_32 = 0x110,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_95_64 = 0x120,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_127_96 = 0x130,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_159_128 = 0x140,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_191_160 = 0x150,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_223_192 = 0x160,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_255_224 = 0x170,

    LOCAL_APIC_OFFSET_Local_APIC_TMR_31_0 = 0x180, // Trigger Mode Register
    LOCAL_APIC_OFFSET_Local_APIC_TMR_63_32 = 0x190,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_95_64 = 0x1a0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_127_96 = 0x1b0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_159_128 = 0x1c0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_191_160 = 0x1d0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_223_192 = 0x1e0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_255_224 = 0x1f0,

    LOCAL_APIC_OFFSET_Local_APIC_IRR_31_0 = 0x200, // Interrupt Request Register
    LOCAL_APIC_OFFSET_Local_APIC_IRR_63_32 = 0x210,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_95_64 = 0x220,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_127_96 = 0x230,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_159_128 = 0x240,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_191_160 = 0x250,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_223_192 = 0x260,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_255_224 = 0x270,

    LOCAL_APIC_OFFSET_Local_APIC_ESR = 0x280, // Error Status Register

    LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI = 0x2f0, // Corrected Machine Check Interrupt Register

    LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0 = 0x300, // Interrupt Command Register
    LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32 = 0x310,

    LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER = 0x320,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL = 0x330,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR = 0x340,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0 = 0x350,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1 = 0x360,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR = 0x370,
    // 初始计数寄存器（定时器专用）
    LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG = 0x380,
    // 当前计数寄存器（定时器专用）
    LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG = 0x390,
    LOCAL_APIC_OFFSET_Local_APIC_CLKDIV = 0x3e0,
}

impl Into<u32> for LocalApicOffset {
    fn into(self) -> u32 {
        self as u32
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct XApic {
    /// 当前xAPIC的MMIO空间起始地址。注意，每个CPU都有自己的xAPIC，所以这个地址是每个CPU都不一样的。
    map_vaddr: usize,
}

impl XApic {
    /// 读取指定寄存器的值
    #[allow(dead_code)]
    pub unsafe fn read(&self, reg: u32) -> u32 {
        read_volatile((self.map_vaddr + reg as usize) as *const u32)
    }

    /// 将指定的值写入寄存器
    #[allow(dead_code)]
    pub unsafe fn write(&mut self, reg: u32, value: u32) {
        write_volatile((self.map_vaddr + reg as usize) as *mut u32, value);
        self.read(0x20); // 等待写操作完成，通过读取进行同步
    }
}

impl XApic {
    /// 创建新的XAPIC实例
    #[allow(dead_code)]
    pub unsafe fn new(addr: usize) -> Self {
        XApic { map_vaddr: addr }
    }
}

const X1: u32 = 0x0000000B; // 将除数设置为1，即不除频率
const PERIODIC: u32 = 0x00020000; // 周期性模式
const ENABLE: u32 = 0x00000100; // 单元使能
const MASKED: u32 = 0x00010000; // 中断屏蔽
const PCINT: u32 = 0x0340; // Performance Counter LVT
const LEVEL: u32 = 0x00008000; // 电平触发
const BCAST: u32 = 0x00080000; // 发送到所有APIC，包括自己
const DELIVS: u32 = 0x00001000; // 传递状态
const INIT: u32 = 0x00000500; // INIT/RESET

//中断请求
const T_IRQ0: u32 = 32; // IRQ 0 对应于 T_IRQ 中断
const IRQ_TIMER: u32 = 0;
const IRQ_KBD: u32 = 1;
const IRQ_COM1: u32 = 4;
const IRQ_IDE: u32 = 14;
const IRQ_ERROR: u32 = 19;
const IRQ_SPURIOUS: u32 = 31;

impl LocalAPIC for XApic {
    /// @brief 判断处理器是否支持apic
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Fail to get CPU feature.")
            .has_apic();
    }

    /// @return true -> 函数运行成功
    fn init_current_cpu(&mut self) -> bool {
        unsafe {
            // 设置 Spurious Interrupt Vector Register
            self.write(
                LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_SVR.into(),
                ENABLE | (T_IRQ0 + IRQ_SPURIOUS),
            );

            // 定时器从 lapic[TICR] 开始按总线频率反复倒计时，然后发出中断。
            // 如果需要更精确的时间保持，TICR 应该使用外部时间源进行校准。
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_CLKDIV.into(), X1);
            self.write(
                LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER.into(),
                PERIODIC | (T_IRQ0 + IRQ_TIMER),
            );
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG.into(), 10000000);

            // 禁用逻辑中断线
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0.into(), MASKED);
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1.into(), MASKED);

            // 禁用支持性能计数器溢出中断的机器
            if (self.read(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_Version.into()) >> 16 & 0xFF) >= 4 {
                self.write(PCINT, MASKED);
            }

            // 将错误中断映射到 IRQ_ERROR
            self.write(
                LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR.into(),
                T_IRQ0 + IRQ_ERROR,
            );

            // 清除错误状态寄存器（需要连续写入两次）
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ESR.into(), 0);
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ESR.into(), 0);

            // 确认任何未完成的中断
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_EOI.into(), 0);

            // 发送 Init Level De-Assert 信号以同步仲裁ID
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32.into(), 0);
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into(), BCAST | INIT | LEVEL);
            while self.read(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into()) & DELIVS != 0 {}

            // 启用 APIC 上的中断（但不在处理器上启用）
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_TPR.into(), 0);
        }

        true
    }

    /// 发送 EOI（End Of Interrupt）
    fn send_eoi(&mut self) {
        unsafe {
            self.write(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_EOI.into(), 0);
        }
    }

    /// 获取版本号
    fn version(&self) -> u32 {
        unsafe { self.read(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_Version.into()) }
    }

    /// 获取ID
    fn id(&self) -> u32 {
        unsafe { self.read(LocalApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ID.into()) >> 24 }
    }

    /// 设置LVT寄存器的值
    fn set_lvt(&mut self, register: LVTRegister, lvt: LVT) {
        unsafe {
            self.write(register.into(), lvt.data);
        }
    }
}
