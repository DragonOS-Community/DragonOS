// use crate::common::io::*;
// use crate::common::kprint::*;
// use crate::common::unistd::*;
use crate::libs::spinlock::SpinLock;
use ::core::arch::asm;
use ::core::sync::atomic::{AtomicU8, Ordering};
use ::core::u128;
use x86::msr::{wrmsr, rdmsr};
use crate::include::bindings::bindings::pt_regs;
use crate::include::bindings::bindings::sched_update_jiffies;
use lazy_static::lazy_static;

extern "C" {
    pub static __apic_enable_state: u8;
}

// TODO

#[inline(never)]
pub fn io_mfence() {
    unsafe {
        asm!("mfence");
    }
}
pub fn hlt() {
    unsafe {
        asm!("hlt\n\t");
    }
}
pub fn __write4b(vaddr: u64, value: u64) {
    unsafe {
        asm!(
            "movl $1, %eax\n\tmovl %eax, ($0)"
        );
    }
}

#[inline(always)]
fn __read4b(vaddr: u64) -> u64 {
    vaddr
    // let retval: u64;
    // TODO
}


pub const APIC_XAPIC_ENABLED: u8 = 0;
pub static APIC_X2APIC_ENABLED: AtomicU8 = 1.into();
// pub static CURRENT_APIC_STATE: *const u8 = &__apic_enable_state;
pub static CURRENT_APIC_STATE: AtomicU8 = AtomicU8::new(0);

// local apic 寄存器虚拟地址偏移量表

pub const LOCAL_APIC_OFFSET_Local_APIC_ID: u32 = 0x20;
pub const LOCAL_APIC_OFFSET_Local_APIC_Version: u32 = 0x30;
pub const LOCAL_APIC_OFFSET_Local_APIC_TPR: u32 = 0x80;
pub const LOCAL_APIC_OFFSET_Local_APIC_APR: u32 = 0x90;
pub const LOCAL_APIC_OFFSET_Local_APIC_PPR: u32 = 0xa0;
pub const LOCAL_APIC_OFFSET_Local_APIC_EOI: u32 = 0xb0;
pub const LOCAL_APIC_OFFSET_Local_APIC_RRD: u32 = 0xc0;
pub const LOCAL_APIC_OFFSET_Local_APIC_LDR: u32 = 0xd0;
pub const LOCAL_APIC_OFFSET_Local_APIC_DFR: u32 = 0xe0;
pub const LOCAL_APIC_OFFSET_Local_APIC_SVR: u32 = 0xf0;

pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_31_0: u32 = 0x100;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_63_32: u32 = 0x110;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_95_64: u32 = 0x120;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_127_96: u32 = 0x130;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_159_128: u32 = 0x140;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_191_160: u32 = 0x150;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_223_192: u32 = 0x160;
pub const LOCAL_APIC_OFFSET_Local_APIC_ISR_255_224: u32 = 0x170;

pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_31_0: u32 = 0x180;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_63_32: u32 = 0x190;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_95_64: u32 = 0x1a0;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_127_96: u32 = 0x1b0;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_159_128: u32 = 0x1c0;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_191_160: u32 = 0x1d0;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_223_192: u32 = 0x1e0;
pub const LOCAL_APIC_OFFSET_Local_APIC_TMR_255_224: u32 = 0x1f0;

pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_31_0: u32 = 0x200;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_63_32: u32 = 0x210;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_95_64: u32 = 0x220;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_127_96: u32 = 0x230;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_159_128: u32 = 0x240;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_191_160: u32 = 0x250;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_223_192: u32 = 0x260;
pub const LOCAL_APIC_OFFSET_Local_APIC_IRR_255_224: u32 = 0x270;

pub const LOCAL_APIC_OFFSET_Local_APIC_ESR: u32 = 0x280;

pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI: u32 = 0x2f0;
pub const LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0: u32 = 0x300;
pub const LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32: u32 = 0x310;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER: u64 = 0x320;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL: u32 = 0x330;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR: u32 = 0x340;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0: u32 = 0x350;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1: u32 = 0x360;
pub const LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR: u32 = 0x370;
pub const LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG: u64 = 0x380;
pub const LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG: u64 = 0x390;
pub const LOCAL_APIC_OFFSET_Local_APIC_CLKDIV: u64 = 0x3e0;

pub const APIC_LOCAL_APIC_VIRT_BASE_ADDR: u64 =  SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + LOCAL_APIC_MAPPING_OFFSET;
pub const SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE: u64 = 0xffffa00000000000;
pub const LOCAL_APIC_MAPPING_OFFSET: u64 = 0xfee00000;

// 中断控制结构体
pub struct INT_CMD_REG {
    vector: u8,         // 0~7
    deliver_mode: u8,   // 8~10
    dest_mode: u8,      // 11
    deliver_status: u8, // 12
    res_1: u8,          // 13
    level: u8,          // 14
    trigger: u8,        // 15
    res_2: u8,          // 16~17
    dest_shorthand: u8, // 18~19
    res_3: u16,         // 20~31

    destination: REGDestination,
}

// 中断控制结构体的目标字段
pub enum REGDestination{
    Physical(u128),
    Logical(u128)
}
// 物理模式目标字段
pub struct APIC_Destination {
    res_4: u32, // 32~55
    dest_field: u8, // 56~63
}

// IO APIC的中断定向寄存器的结构体
pub struct apic_IO_APIC_RTE_entry {
    vector: u8,          // 0~7
    deliver_mode: u8,    // [10:8] 投递模式默认为NMI
    dest_mode: u8,       // 11 目标模式(0位物理模式，1为逻辑模式)
    deliver_status: u8,  // 12 投递状态
    polarity: u8,        // 13 电平触发极性
    remote_IRR: u8,      // 14 远程IRR标志位（只读）
    trigger_mode: u8,    // 15 触发模式（0位边沿触发，1为电平触发）
    mask: u8,            // 16 屏蔽标志位，（0为未屏蔽， 1为已屏蔽）
    // reserved: u15,       // [31:17]位保留

    destination: RTEDestination,
}

// 中断定向寄存器的目标字段

pub enum RTEDestination{
    Physical(u128),
    Logical(u128)
}

impl RTEDestination{

}
// 物理模式的目标字段
pub struct apic_IO_APIC_RTE_entry_Destination_Physical {
    reserved1: usize, // [55:32] 保留
    phy_dest: u8,   // [59:56] APIC ID
    reserved2: usize,  // [63:60] 保留
}

// 逻辑模式的目标字段
pub struct apic_IO_APIC_RTE_entry_Destination_Logical {
    reserved1: usize, // [55:32] 保留
    logical_dest: u8, // [63:56] 自定义APIC ID
}

// APIC的寄存器的参数定义

pub const LOCAL_APIC_FIXED: u8 = 0;
pub const IO_APIC_FIXED: u8 = 0;
pub const ICR_APIC_FIXED: u8 = 0;

pub const IO_APIC_Lowest_Priority: u8 = 1;
pub const ICR_Lowest_Priority: u8 = 1;

pub const LOCAL_APIC_SMI: u8 = 2;
pub const APIC_SMI: u8 = 2;
pub const ICR_SMI: u8 = 2;

pub const LOCAL_APIC_NMI: u8 = 4;
pub const APIC_NMI: u8 = 4;
pub const ICR_NMI: u8 = 4;

pub const LOCAL_APIC_INIT: u8 = 5;
pub const APIC_INIT: u8 = 5;
pub const ICR_INIT: u8 = 5;

pub const ICR_Start_up: u8 = 6;

pub const IO_APIC_ExtINT: u8 = 7;

// 时钟模式
pub const APIC_LVT_Timer_One_Shot: u8 = 0;
pub const APIC_LVT_Timer_Periodic: u8 = 1;
pub const APIC_LVT_Timer_TSC_Deadline: u8 = 2;

// 屏蔽
pub const UNMASKED: u8 = 0;
pub const MASKED: u8 = 1;
pub const APIC_LVT_INT_MASKED: u64 = 0x10000;

// 触发模式
pub const EDGE_TRIGGER: u8 = 0; // 边沿触发
pub const Level_TRIGGER: u8 = 1; // 电平触发

// 投递模式
pub const IDLE: u8 = 0; // 挂起
pub const SEND_PENDING: u8 = 1; // 发送等待

// destination shorthand
pub const ICR_No_Shorthand: u8 = 0;
pub const ICR_Self: u8 = 1;
pub const ICR_ALL_INCLUDE_Self: u8 = 2;
pub const ICR_ALL_EXCLUDE_Self: u8 = 3;

// 投递目标模式
pub const DEST_PHYSICAL: u8 = 0; // 物理模式
pub const DEST_LOGIC: u8 = 1; // 逻辑模式

// level
pub const ICR_LEVEL_DE_ASSERT: u8 = 0;
pub const ICR_LEVEL_ASSERT: u8 = 1;

// 远程IRR标志位, 在处理Local APIC标志位时置位，在收到处理器发来的EOI命令时复位
pub const IRR_RESET: u8 = 0;
pub const IRR_ACCEPT: u8 = 1;

// 电平触发极性
pub const POLARITY_HIGH: u8 = 0;
pub const POLARITY_LOW: u8 = 1;
// 5ms产生一次中断
pub const APIC_TIMER_INTERVAL: u64 = 5;
pub const APIC_TIMER_DIVISOR: u64 = 3;
pub const APIC_TIMER_IRQ_NUM: u64 = 151;

pub struct ApicTimer {
    apic_timer_ticks_result: u64,
    apic_timer_init_lock: SpinLock<u32>,
    bsp_initialized: bool,
}

impl ApicTimer {
    pub fn new() -> ApicTimer {
        ApicTimer {
            apic_timer_ticks_result: 0,
            apic_timer_init_lock: SpinLock::new(1),
            bsp_initialized: false,
        }
    }

    /// 设置apic定时器的分频计数
    pub fn set_div(&self, divider: u64) {
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            wrmsr(0x83e, divider);
            }
        } else {
            unsafe {
                __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_CLKDIV, divider);
            }
        }
    }

    /// 设置apic定时器的初始计数值
    pub fn set_init_cnt(&self, init_cnt: u64) {
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            wrmsr(0x838, init_cnt as u64);
            }
        } else {
            unsafe {
                __write4b(
                    APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG,
                    init_cnt,
                );
            }
        }
    }

    /// 设置apic定时器的LVT，并启动定时器
    pub fn set_LVT(&self, vector: u64, mask: bool, mode: ApicTimerMode) {
        let mut val: u64 = (mode as u64) << 17 | vector;
        if mask {
            val |= APIC_LVT_INT_MASKED;
        }
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            wrmsr(0x832, val);
            }
        } else {
            unsafe {
                __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER, val);
            }
        }
    }

    /// 写入apic定时器的LVT值
    pub fn write_LVT(&self, value: u64) {
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            wrmsr(0x832, value);
            }
        } else {
            unsafe {
                __write4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER, value);
            }
        }
    }

    /// 获取apic定时器的LVT值
    pub fn get_LVT(&self) -> u64 {
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            return rdmsr(0x832) as u64;
            }
        } else {
            unsafe {
                return __read4b(APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER);
            }
        }
    }

    /// 获取apic定时器当前计数值
    pub fn get_current(&self) -> u32 {
        if CURRENT_APIC_STATE.load(Ordering::Relaxed) == APIC_X2APIC_ENABLED.load(Ordering::Relaxed) {
            unsafe{
            return rdmsr(0x839) as u32;
            }
        } else {
            unsafe {
                return __read4b(
                    APIC_LOCAL_APIC_VIRT_BASE_ADDR + LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG,
                ).try_into().unwrap();
            }
        }
    }

    /// 停止apic定时器
    pub fn stop(&self) {
        let mut val: u64 = self.get_LVT().into();
        val |= APIC_LVT_INT_MASKED;
        self.write_LVT(val);
    }

    /// 初始化local APIC定时器
    pub fn init(&self) {
        let mut apic_timer = self.apic_timer_init_lock.lock();
        if self.apic_timer_ticks_result == 0 {
            //println!("APIC timer ticks in 5ms is equal to ZERO!");
            loop {
                unsafe {
                    hlt();
                }
            }
        }
        // kinfo!("Successfully initialized apic timer for cpu {}", proc_current_cpu_id);
    }
}

/// 初始化AP核的apic时钟
pub extern "C" fn apic_timer_ap_core_init() {
    let apic_timer = ApicTimer::new();
    while !apic_timer.bsp_initialized {
        // pause();TODO
    }
    apic_timer.init();
}

/// 启用apic定时器
pub extern "C" fn apic_timer_enable(irq_num: u64) {
    // 启动apic定时器
    io_mfence();
    let mut val: u64 = apic_timer_get_LVT();
    io_mfence();
    val &= !APIC_LVT_INT_MASKED;
    io_mfence();
    apic_timer_write_LVT(val.try_into().unwrap());
    io_mfence();
}

/// 禁用apic定时器
pub fn apic_timer_disable(irq_num: u64) {
    let apic_timer = ApicTimer::new();
    apic_timer.stop();
}

/// 安装local apic定时器中断
pub fn apic_timer_install(irq_num: u64, arg: *mut u64) -> u64 {
    let apic_timer = ApicTimer::new();
    io_mfence();
    apic_timer.stop();
    io_mfence();
    apic_timer.set_div(APIC_TIMER_DIVISOR);
    io_mfence();
    unsafe {
        apic_timer.set_init_cnt(*arg as u64);
    }
    io_mfence();
    apic_timer.set_LVT(APIC_TIMER_IRQ_NUM as u64, true, ApicTimerMode::Periodic);
    io_mfence();
    irq_num
}

/// 卸载local apic定时器中断
pub extern "C" fn apic_timer_uninstall(irq_num: u64) {
    let apic_timer = ApicTimer::new();
    apic_timer.write_LVT(APIC_LVT_INT_MASKED);
    io_mfence();
}

/// local apic定时器的中断处理函数
pub extern "C" fn apic_timer_handler(number: u64, param: u64, regs: *mut pt_regs) {
    io_mfence();
    unsafe {
        sched_update_jiffies();
    }
    io_mfence();
}

/// 默认值为OneShot模式
impl Default for ApicTimerMode {
    fn default() -> Self {
        ApicTimerMode::OneShot
    }
}

/// apic定时器工作模式
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ApicTimerMode {
    /// 一次性定时模式。使用倒计时值的单次定时模式
    OneShot = 0b00,
    /// 周期性定时模式。使用倒计时值的周期性定时模式
    Periodic = 0b01,
    /// TSC截止时间模式。使用IA32_TSC_DEADLINE MSR中的绝对目标值的TSC截止时间模式
    TSCDeadline = 0b10,
}

/// 获取apic定时器的LVT值
pub extern "C" fn apic_timer_get_LVT() -> u64 {
    let apic_timer = ApicTimer::new();
    apic_timer.get_LVT()
}

/// 写入apic定时器的LVT值
pub extern "C" fn apic_timer_write_LVT(value: u32) {
    let apic_timer = ApicTimer::new();
    apic_timer.write_LVT(value.into());
}

/// 获取apic定时器当前计数值
pub fn apic_timer_get_current() -> u32 {
    let apic_timer = ApicTimer::new();
    apic_timer.get_current()
}

/// 停止apic定时器
pub fn apic_timer_stop() {
    let apic_timer = ApicTimer::new();
    apic_timer.stop();
}

/// 设置apic定时器的分频计数
pub fn apic_timer_set_div(divider: u64) {
    let apic_timer = ApicTimer::new();
    apic_timer.set_div(divider);
}

/// 设置apic定时器的初始计数值
pub fn apic_timer_set_init_cnt(init_cnt: u32) {
    let apic_timer = ApicTimer::new();
    apic_timer.set_init_cnt(init_cnt.into());
}

/// 设置apic定时器的LVT，并启动定时器
pub fn apic_timer_set_LVT(vector: u32, mask: u32, mode: u32) {
    let apic_timer = ApicTimer::new();
    let mode = match mode {
        0 => ApicTimerMode::OneShot,
        1 => ApicTimerMode::Periodic,
        2 => ApicTimerMode::TSCDeadline,
        _ => panic!("Invalid mode"),
    };
    apic_timer.set_LVT(vector.into(), mask != 0, mode);
}