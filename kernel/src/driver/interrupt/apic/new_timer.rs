use core::cell::{Cell, RefCell};

use crate::arch::driver::tsc::TSCManager;
use crate::include::bindings::bindings::APIC_TIMER_IRQ_NUM;
use crate::mm::percpu::PerCpuVar;
use crate::smp::core::smp_get_processor_id;
use x86::{cpuid::cpuid, time::rdtsc};
// use x86::cpuid::CpuId
use crate::driver::interrupt::apic::xapic::XApic;
use crate::exception::InterruptArch;
use crate::kerror;
use crate::{arch::CurrentIrqArch, mm::percpu::PerCpu};
pub use drop;
use x86::msr::{rdmsr, wrmsr, IA32_X2APIC_DIV_CONF, IA32_X2APIC_INIT_COUNT};

use super::xapic::XApicOffset;
use super::{CurrentApic, LVTRegister, LocalAPIC, LVT};

extern "C" {
    fn c_register_apic_timer_irq();
}

static mut LOCAL_APIC_TIMERS: [RefCell<LocalApicTimer>; PerCpu::MAX_CPU_NUM] =
    [const { RefCell::new(LocalApicTimer::new()) }; PerCpu::MAX_CPU_NUM];

#[inline(always)]
pub(super) fn local_apic_timer_instance(cpu_id: u32) -> core::cell::Ref<'static, LocalApicTimer> {
    unsafe { LOCAL_APIC_TIMERS[cpu_id as usize].borrow() }
}

#[inline(always)]
pub(super) fn local_apic_timer_instance_mut(
    cpu_id: u32,
) -> core::cell::RefMut<'static, LocalApicTimer> {
    unsafe { LOCAL_APIC_TIMERS[cpu_id as usize].borrow_mut() }
}

/// 初始化BSP的APIC定时器
///
fn init_bsp_apic_timer() {
    assert!(smp_get_processor_id() == 0);
    // 注册中断处理函数
    unsafe { c_register_apic_timer_irq() };
    let mut local_apic_timer = local_apic_timer_instance_mut(0);
    local_apic_timer.init(
        LocalApicTimerMode::Periodic,
        LocalApicTimer::periodic_default_initial_count(),
        LocalApicTimer::DIVISOR as u32,
    )
}

fn init_ap_apic_timer() {
    let cpu_id = smp_get_processor_id();
    assert!(cpu_id != 0);

    let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
    local_apic_timer.init(
        LocalApicTimerMode::Periodic,
        LocalApicTimer::periodic_default_initial_count(),
        LocalApicTimer::DIVISOR as u32,
    );
}

pub(super) struct LocalApicTimerIntrController;

impl LocalApicTimerIntrController {
    pub(super) fn install(&self, irq_num: u8) {
        if smp_get_processor_id() == 0 {
            init_bsp_apic_timer();
        } else {
            init_ap_apic_timer();
        }
    }

    pub(super) fn uninstall(&self) {
        let cpu_id = smp_get_processor_id();
        let local_apic_timer = local_apic_timer_instance(cpu_id);
        local_apic_timer.stop_current();
    }

    pub(super) fn enable(&self) {
        let cpu_id = smp_get_processor_id();
        let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.start_current();
    }

    pub(super) fn disable(&self) {
        let cpu_id = smp_get_processor_id();
        let local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.stop_current();
    }
}

#[derive(Debug, Copy, Clone)]
pub struct LocalApicTimer {
    mode: LocalApicTimerMode,
    /// IntialCount
    initial_count: u64,
    divisor: u32,
    /// 是否已经触发（oneshot模式）
    triggered: bool,
}

#[derive(Debug, Copy, Clone)]
#[repr(u32)]
enum LocalApicTimerMode {
    Oneshot = 0,
    Periodic = 1,
    Deadline = 2,
}

impl LocalApicTimer {
    /// 定时器中断的间隔
    pub const INTERVAL_MS: u64 = 5;
    pub const DIVISOR: u64 = 3;

    /// IoApicManager 初值为0或false
    pub const fn new() -> Self {
        LocalApicTimer {
            mode: LocalApicTimerMode::Periodic,
            initial_count: 0,
            divisor: 0,
            triggered: false,
        }
    }

    /// 周期模式下的默认初始值
    pub fn periodic_default_initial_count() -> u64 {
        let cpu_khz = TSCManager::cpu_khz();

        // 疑惑：这里使用khz吗？
        // 我觉得应该是hz，但是由于旧的代码是测量出initcnt的，而不是计算的
        // 然后我发现使用hz会导致计算出来的initcnt太大，导致系统卡顿，而khz的却能跑
        let count = cpu_khz * Self::INTERVAL_MS / (1000 * Self::DIVISOR);
        return count;
    }

    /// Init this manager.
    ///
    /// At this time, it does nothing.
    fn init(&mut self, mode: LocalApicTimerMode, initial_count: u64, divisor: u32) {
        self.stop_current();
        self.triggered = false;
        match mode {
            LocalApicTimerMode::Periodic => self.install_periodic_mode(initial_count, divisor),
            LocalApicTimerMode::Oneshot => todo!(),
            LocalApicTimerMode::Deadline => todo!(),
        }
    }

    fn install_periodic_mode(&mut self, initial_count: u64, divisor: u32) {
        self.mode = LocalApicTimerMode::Periodic;
        self.set_divisor(divisor);
        self.set_initial_cnt(initial_count);
        self.setup_lvt(APIC_TIMER_IRQ_NUM as u8, true, LocalApicTimerMode::Periodic);
    }

    fn setup_lvt(&mut self, vector: u8, mask: bool, mode: LocalApicTimerMode) {
        let mode: u32 = mode as u32;
        let data = (mode << 17) | (vector as u32) | (if mask { 1 << 16 } else { 0 });
        let lvt = LVT::new(LVTRegister::Timer, 0).unwrap();

        CurrentApic.set_lvt(lvt);
    }

    fn set_divisor(&mut self, divisor: u32) {
        self.divisor = divisor;
        CurrentApic.set_timer_divisor(divisor as u32);
    }

    fn set_initial_cnt(&mut self, initial_count: u64) {
        self.initial_count = initial_count;
        CurrentApic.set_timer_initial_count(initial_count);
    }

    fn start_current(&mut self) {
        let mut lvt = CurrentApic.read_lvt(LVTRegister::Timer);
        lvt.set_mask(false);
        CurrentApic.set_lvt(lvt);
    }

    fn stop_current(&self) {
        let mut lvt = CurrentApic.read_lvt(LVTRegister::Timer);
        lvt.set_mask(true);
        CurrentApic.set_lvt(lvt);
    }

    /// 检查是否支持TSC-Deadline
    ///
    /// 此函数调用cpuid，请避免多次调用此函数。
    /// 如果支持TSC-Deadline模式，则除非TSC为常数，否则不会启用该模式。
    pub fn is_deadline_mode_supported(&self) -> bool {
        let res = cpuid!(1);
        return (res.ecx & (1 << 24)) != 0;
    }
}

impl CurrentApic {
    fn set_timer_divisor(&self, divisor: u32) {
        if self.x2apic_enabled() {
            unsafe { wrmsr(IA32_X2APIC_DIV_CONF, divisor.into()) };
        } else {
            unsafe {
                self.write_xapic_register(
                    XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_CLKDIV,
                    divisor.into(),
                )
            };
        }
    }

    fn set_timer_initial_count(&self, initial_count: u64) {
        if self.x2apic_enabled() {
            unsafe {
                wrmsr(IA32_X2APIC_INIT_COUNT.into(), initial_count);
            }
        } else {
            unsafe {
                self.write_xapic_register(
                    XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG,
                    initial_count as u32,
                )
            };
        }
    }
}
