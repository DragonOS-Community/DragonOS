use core::ptr::NonNull;

use crate::libs::volatile::Volatile;

#[repr(C, packed)]
pub struct HpetRegisters {
    /// HPET 功能寄存器，包含计时器的功能信息
    capabilties: Volatile<u32>,
    /// HPET 时钟周期寄存器，表示计时器的时钟周期
    period: Volatile<u32>,
    /// 保留字段，用于填充内存对齐
    _reserved0: Volatile<u64>,
    /// HPET 通用配置寄存器，用于配置计时器
    general_config: Volatile<u64>,
    /// 保留字段，用于填充内存对齐
    _reserved1: Volatile<u64>,
    /// HPET 通用中断状态寄存器，表示中断状态
    general_intr_status: Volatile<u64>,
    /// 保留字段，用于填充内存对齐
    _reserved2: [Volatile<u64>; 25],
    /// HPET 计数器值寄存器，表示当前的计数值
    main_counter_value: Volatile<u64>,
    /// 保留字段，用于填充内存对齐
    _reserved3: Volatile<u64>,
    // 这里后面跟着各个定时器的寄存器（数量由capabilties决定）
}

impl HpetRegisters {
    /// 获取 HPET Timer 的数量
    pub fn timers_num(&self) -> usize {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        let cap = unsafe { volread!(p, capabilties) };
        (cap >> 8) as usize & 0x1f
    }

    /// 获取 HPET 计数器的周期
    pub fn counter_clock_period(&self) -> u64 {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        let period = unsafe { volread!(p, period) };
        period as u64
    }

    /// 获取 HPET 计数器的频率
    pub fn frequency(&self) -> u64 {
        1_000_000_000_000_000 / self.counter_clock_period()
    }

    pub fn main_counter_value(&self) -> u64 {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        let main_counter_value = unsafe { volread!(p, main_counter_value) };
        main_counter_value
    }

    pub unsafe fn write_main_counter_value(&mut self, value: u64) {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        volwrite!(p, main_counter_value, value);
    }

    pub fn general_config(&self) -> u64 {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        unsafe { volread!(p, general_config) }
    }

    pub unsafe fn write_general_config(&mut self, value: u64) {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        volwrite!(p, general_config, value);
    }

    #[allow(dead_code)]
    pub fn general_intr_status(&self) -> u64 {
        let p = NonNull::new(self as *const HpetRegisters as *mut HpetRegisters).unwrap();
        unsafe { volread!(p, general_intr_status) }
    }
}

#[repr(C, packed)]
pub struct HpetTimerRegisters {
    config: Volatile<u64>,
    comparator_value: Volatile<u64>,
    fsb_interrupt_route: [Volatile<u64>; 2],
}

impl HpetTimerRegisters {
    pub fn config(&self) -> u64 {
        let p = NonNull::new(self as *const HpetTimerRegisters as *mut HpetTimerRegisters).unwrap();
        unsafe { volread!(p, config) }
    }

    pub unsafe fn write_config(&mut self, value: u64) {
        let p = NonNull::new(self as *const HpetTimerRegisters as *mut HpetTimerRegisters).unwrap();
        unsafe { volwrite!(p, config, value) }
    }

    #[allow(dead_code)]
    pub fn comparator_value(&self) -> u64 {
        let p = NonNull::new(self as *const HpetTimerRegisters as *mut HpetTimerRegisters).unwrap();
        unsafe { volread!(p, comparator_value) }
    }

    pub unsafe fn write_comparator_value(&mut self, value: u64) {
        let p = NonNull::new(self as *const HpetTimerRegisters as *mut HpetTimerRegisters).unwrap();
        unsafe { volwrite!(p, comparator_value, value) }
    }
}
