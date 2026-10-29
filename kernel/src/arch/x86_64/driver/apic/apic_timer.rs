use core::cell::RefCell;
use core::hint::spin_loop;
use core::sync::atomic::{fence, Ordering};

use crate::arch::driver::{
    hpet::{hpet_instance, is_hpet_enabled},
    tsc::TSCManager,
};
use crate::arch::interrupt::TrapFrame;
use crate::driver::acpi::pmtmr::{acpi_pm_read_early, ACPI_PM_MASK, PMTMR_TICKS_PER_SEC};
use crate::driver::base::device::DeviceId;
use crate::driver::clocksource::acpi_pm::PMTMR_IO_PORT;
use crate::exception::irqdata::{IrqHandlerData, IrqLineStatus};
use crate::exception::irqdesc::{
    irq_desc_manager, IrqDesc, IrqFlowHandler, IrqHandleFlags, IrqHandler, IrqReturn,
};
use crate::exception::manage::irq_manager;
use crate::exception::IrqNumber;

use crate::mm::percpu::PerCpu;
use crate::smp::core::smp_get_processor_id;
use crate::smp::cpu::ProcessorId;
use crate::time::clocksource::HZ;
use alloc::string::ToString;
use alloc::sync::Arc;
pub use drop;
use log::debug;
use system_error::SystemError;
use x86::cpuid::cpuid;
use x86::msr::{wrmsr, IA32_X2APIC_DIV_CONF, IA32_X2APIC_INIT_COUNT};

use super::lapic_vector::local_apic_chip;
use super::xapic::XApicOffset;
use super::{CurrentApic, LVTRegister, LocalAPIC, LVT};

pub const APIC_TIMER_IRQ_NUM: IrqNumber = IrqNumber::new(151);
const APIC_CALIBRATE_MS: u64 = 10;
const IA32_TSC_DEADLINE: u32 = 0x6e0;

static mut LOCAL_APIC_TIMERS: [RefCell<LocalApicTimer>; PerCpu::MAX_CPU_NUM as usize] =
    [const { RefCell::new(LocalApicTimer::new()) }; PerCpu::MAX_CPU_NUM as usize];

#[allow(dead_code)]
#[inline(always)]
pub(super) fn local_apic_timer_instance(
    cpu_id: ProcessorId,
) -> core::cell::Ref<'static, LocalApicTimer> {
    unsafe { LOCAL_APIC_TIMERS[cpu_id.data() as usize].borrow() }
}

#[inline(always)]
pub(super) fn local_apic_timer_instance_mut(
    cpu_id: ProcessorId,
) -> core::cell::RefMut<'static, LocalApicTimer> {
    unsafe { LOCAL_APIC_TIMERS[cpu_id.data() as usize].borrow_mut() }
}

#[derive(Debug)]
struct LocalApicTimerHandler;

impl IrqHandler for LocalApicTimerHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        // empty (只是为了让编译通过，不会被调用到。真正的处理函数在LocalApicTimerIrqFlowHandler中)
        Ok(IrqReturn::NotHandled)
    }
}

#[derive(Debug)]
struct LocalApicTimerIrqFlowHandler;

impl IrqFlowHandler for LocalApicTimerIrqFlowHandler {
    fn handle(&self, _irq_desc: &Arc<IrqDesc>, trap_frame: &mut TrapFrame) {
        LocalApicTimer::handle_irq(trap_frame).ok();
        CurrentApic.send_eoi();
        fence(Ordering::SeqCst)
    }
}

pub fn apic_timer_init() {
    irq_manager()
        .request_irq(
            APIC_TIMER_IRQ_NUM,
            "LocalApic".to_string(),
            &LocalApicTimerHandler,
            IrqHandleFlags::IRQF_SHARED | IrqHandleFlags::IRQF_PERCPU,
            Some(DeviceId::new(Some("lapic timer"), None).unwrap()),
        )
        .expect("Apic timer init failed");

    LocalApicTimerIntrController.install();
    LocalApicTimerIntrController.enable();
    crate::time::deadline::init_local();
}

/// 初始化本地APIC定时器的中断描述符
#[inline(never)]
pub(super) fn local_apic_timer_irq_desc_init() {
    let desc = irq_desc_manager().lookup(APIC_TIMER_IRQ_NUM).unwrap();
    let irq_data: Arc<crate::exception::irqdata::IrqData> = desc.irq_data();
    let mut chip_info_guard = irq_data.chip_info_write_irqsave();
    chip_info_guard.set_chip(Some(local_apic_chip().clone()));

    desc.modify_status(IrqLineStatus::IRQ_LEVEL, IrqLineStatus::empty());
    drop(chip_info_guard);
    desc.set_handler(&LocalApicTimerIrqFlowHandler);
}

/// 初始化BSP的APIC定时器
///
fn init_bsp_apic_timer() {
    debug!("init_bsp_apic_timer");
    assert!(smp_get_processor_id().data() == 0);
    let mut local_apic_timer = local_apic_timer_instance_mut(ProcessorId::new(0));
    let calibrated = local_apic_timer.calibrate_initial_count();
    local_apic_timer.hres_calibrated = calibrated.is_some();
    let initial_count = calibrated.unwrap_or_else(LocalApicTimer::periodic_default_initial_count);
    local_apic_timer.detect_tsc_deadline();
    local_apic_timer.init_periodic(initial_count, LocalApicTimer::DIVISOR as u32);
    debug!("init_bsp_apic_timer done");
}

fn init_ap_apic_timer() {
    debug!("init_ap_apic_timer");
    let cpu_id = smp_get_processor_id();
    assert!(cpu_id.data() != 0);

    let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
    let calibrated = local_apic_timer.calibrate_initial_count();
    local_apic_timer.hres_calibrated = calibrated.is_some();
    let initial_count = calibrated.unwrap_or_else(LocalApicTimer::periodic_default_initial_count);
    local_apic_timer.detect_tsc_deadline();
    local_apic_timer.init_periodic(initial_count, LocalApicTimer::DIVISOR as u32);
    debug!("init_ap_apic_timer done");
}

pub(super) struct LocalApicTimerIntrController;

impl LocalApicTimerIntrController {
    pub(super) fn install(&self) {
        debug!("LocalApicTimerIntrController::install");
        if smp_get_processor_id().data() == 0 {
            init_bsp_apic_timer();
        } else {
            init_ap_apic_timer();
        }
    }

    #[allow(dead_code)]
    pub(super) fn uninstall(&self) {
        let cpu_id = smp_get_processor_id();
        let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.stop_current();
    }

    pub(super) fn enable(&self) {
        debug!("LocalApicTimerIntrController::enable");
        let cpu_id = smp_get_processor_id();
        let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.start_current();
    }

    #[allow(dead_code)]
    pub(super) fn disable(&self) {
        let cpu_id = smp_get_processor_id();
        let mut local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.stop_current();
    }
}

#[derive(Debug, Copy, Clone)]
pub struct LocalApicTimer {
    mode: LocalApicTimerMode,
    /// Whether LVTT is unmasked; remains true after a one-shot counter expires.
    enabled: bool,
    /// IntialCount
    initial_count: u64,
    periodic_count: u64,
    hres_calibrated: bool,
    /// Boot admission only: virtual deadline support and a known virtual TSC rate.
    tsc_deadline_supported: bool,
    divisor: u32,
    /// 是否已经触发（oneshot模式）
    triggered: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
pub enum LocalApicTimerMode {
    Oneshot = 0,
    Periodic = 1,
    Deadline = 2,
}

impl LocalApicTimer {
    /// 定时器中断的间隔
    pub const INTERVAL_MS: u64 = 1000 / HZ;
    pub const DIVISOR: u64 = 4;

    /// IoApicManager 初值为0或false
    pub const fn new() -> Self {
        LocalApicTimer {
            mode: LocalApicTimerMode::Periodic,
            enabled: false,
            initial_count: 0,
            periodic_count: 0,
            hres_calibrated: false,
            tsc_deadline_supported: false,
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
        let count = cpu_khz * Self::INTERVAL_MS / (Self::DIVISOR);
        return count;
    }

    fn calibrate_initial_count(&mut self) -> Option<u64> {
        self.stop_current();
        self.set_divisor(Self::DIVISOR as u32);
        self.set_initial_cnt(u32::MAX as u64);
        self.set_lvt_masked(LocalApicTimerMode::Periodic);
        self.start_current();

        if is_hpet_enabled() {
            let period_fs = hpet_instance().period();
            if period_fs == 0 {
                return None;
            }
            let start = hpet_instance().main_counter_value();
            let target_ticks = (APIC_CALIBRATE_MS * 1_000_000u64 * 1_000_000u64) / period_fs;
            while (hpet_instance().main_counter_value() - start) < target_ticks {
                spin_loop();
            }
        } else if PMTMR_IO_PORT.load(Ordering::SeqCst) != 0 {
            let start = acpi_pm_read_early() as u64;
            let target_ticks = PMTMR_TICKS_PER_SEC * APIC_CALIBRATE_MS / 1000;
            while ((acpi_pm_read_early() as u64).wrapping_sub(start) & ACPI_PM_MASK) < target_ticks
            {
                spin_loop();
            }
        } else {
            let start = TSCManager::cpu_khz();
            if start == 0 {
                return None;
            }
            let tsc_start = unsafe { x86::time::rdtsc() };
            let mut tsc_now = tsc_start;
            let target = start * APIC_CALIBRATE_MS;
            while (tsc_now - tsc_start) < target {
                tsc_now = unsafe { x86::time::rdtsc() };
            }
        }

        let cur = CurrentApic.read_timer_current_count() as u64;
        self.stop_current();

        let elapsed = (u32::MAX as u64).saturating_sub(cur);
        if elapsed == 0 {
            return None;
        }

        let apic_khz = elapsed / APIC_CALIBRATE_MS;
        // apic_khz already accounts for the programmed divider.
        let initial_count = apic_khz * Self::INTERVAL_MS;
        Some(initial_count)
    }

    fn detect_tsc_deadline(&mut self) {
        let features = cpuid!(1).ecx;
        // Like Linux, trust an advertised virtual deadline timer. Bare metal
        // retains the bus timer until its model/microcode errata are supported.
        self.tsc_deadline_supported =
            features & (1 << 31) != 0 && features & (1 << 24) != 0 && TSCManager::tsc_khz() != 0;
    }

    fn init_periodic(&mut self, count: u64, divisor: u32) {
        self.stop_current();
        self.triggered = false;
        self.install_periodic_mode(count, divisor);
    }

    fn init_oneshot(&mut self, count: u64, divisor: u32) {
        self.stop_current();
        self.triggered = false;
        self.set_divisor(divisor);
        // Unmask before starting a tiny counter: it must not expire masked.
        self.setup_lvt(
            APIC_TIMER_IRQ_NUM.data() as u8,
            false,
            LocalApicTimerMode::Oneshot,
        );
        self.set_initial_cnt(count);
    }

    fn program_tsc_deadline(&mut self, delta_ns: u64) -> bool {
        if !self.tsc_deadline_supported {
            return false;
        }
        if self.mode != LocalApicTimerMode::Deadline || !self.enabled {
            self.stop_current();
            self.setup_lvt(
                APIC_TIMER_IRQ_NUM.data() as u8,
                false,
                LocalApicTimerMode::Deadline,
            );
        }
        // Linux weak_wrmsr_fence: order prior stores and the LVTT MMIO write
        // before a nonserializing deadline WRMSR, and order the TSC sample.
        // No `nomem`: the compiler must also preserve this ordering.
        let now = unsafe {
            core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
            x86::time::rdtsc()
        };
        let Some(deadline) = tsc_deadline_value(delta_ns, TSCManager::tsc_khz(), now) else {
            // Caller switches the actual hardware back to bus oneshot.
            return false;
        };
        unsafe { wrmsr(IA32_TSC_DEADLINE, deadline) };
        true
    }

    fn install_periodic_mode(&mut self, initial_count: u64, divisor: u32) {
        debug!(
            "install_periodic_mode: initial_count = {}, divisor = {}",
            initial_count, divisor
        );
        self.mode = LocalApicTimerMode::Periodic;
        self.periodic_count = initial_count;
        self.set_divisor(divisor);
        self.setup_lvt(
            APIC_TIMER_IRQ_NUM.data() as u8,
            true,
            LocalApicTimerMode::Periodic,
        );
        self.set_initial_cnt(initial_count);
    }

    fn setup_lvt(&mut self, vector: u8, mask: bool, mode: LocalApicTimerMode) {
        self.mode = mode;
        let mode: u32 = mode as u32;
        let data = (mode << 17) | (vector as u32) | (if mask { 1 << 16 } else { 0 });
        let lvt = LVT::new(LVTRegister::Timer, data).unwrap();

        CurrentApic.set_lvt(lvt);
        self.enabled = !mask;
    }

    fn set_divisor(&mut self, divisor: u32) {
        self.divisor = divisor;
        CurrentApic.set_timer_divisor(divisor);
    }

    fn set_initial_cnt(&mut self, initial_count: u64) {
        self.initial_count = initial_count;
        CurrentApic.set_timer_initial_count(initial_count);
    }

    fn start_current(&mut self) {
        let mut lvt = CurrentApic.read_lvt(LVTRegister::Timer);
        lvt.set_mask(false);
        CurrentApic.set_lvt(lvt);
        self.enabled = true;
    }

    fn stop_current(&mut self) {
        let mut lvt = CurrentApic.read_lvt(LVTRegister::Timer);
        lvt.set_mask(true);
        CurrentApic.set_lvt(lvt);
        self.enabled = false;
        // A masked absolute deadline must not survive mode changes or kexec.
        // Never access this MSR on a CPU that did not pass boot admission.
        if self.tsc_deadline_supported && self.mode == LocalApicTimerMode::Deadline {
            unsafe { wrmsr(IA32_TSC_DEADLINE, 0) };
        }
    }

    fn set_lvt_masked(&mut self, mode: LocalApicTimerMode) {
        self.setup_lvt(APIC_TIMER_IRQ_NUM.data() as u8, true, mode);
    }

    pub(super) fn handle_irq(trap_frame: &TrapFrame) -> Result<IrqReturn, SystemError> {
        // sched_update_jiffies();
        crate::time::deadline::handle_irq(trap_frame);
        return Ok(IrqReturn::Handled);
    }
}

/// Caller holds the local deadline queue with local IRQs disabled. Never keep
/// this RefCell borrow alive while dispatching a deadline callback.
pub(crate) fn program_deadline_delta(delta_ns: u64) {
    let mut timer = local_apic_timer_instance_mut(smp_get_processor_id());
    if timer.program_tsc_deadline(delta_ns) {
        return;
    }
    let count = ((delta_ns as u128 * timer.periodic_count as u128)
        .div_ceil(LocalApicTimer::INTERVAL_MS as u128 * 1_000_000))
    .clamp(1, u32::MAX as u128) as u64;
    if timer.mode == LocalApicTimerMode::Oneshot && timer.enabled {
        // The mode and divider are already installed. Like Linux lapic_next_event,
        // rearm only the count; redundant LVTT/divider writes cause VM exits.
        timer.set_initial_cnt(count);
    } else {
        let divisor = timer.divisor;
        // This also disarms Deadline mode after a numeric conversion failure.
        timer.init_oneshot(count, divisor);
    }
}

/// Relative nanoseconds become a fresh absolute deadline in this CPU's TSC
/// domain. Zero is reserved for disarm; no wrapping deadline is ever written.
fn tsc_deadline_value(delta_ns: u64, tsc_khz: u64, now: u64) -> Option<u64> {
    if tsc_khz == 0 {
        return None;
    }
    let cycles = (delta_ns as u128 * tsc_khz as u128)
        .div_ceil(1_000_000)
        .max(1);
    let cycles = u64::try_from(cycles).ok()?;
    now.checked_add(cycles)
}

/// Called only after local IRQs are disabled, before disabling this APIC.
pub(super) fn shutdown_local() {
    local_apic_timer_instance_mut(smp_get_processor_id()).stop_current();
}

/// Pure conversion checks; does not touch an APIC or change any live timer.
pub(crate) fn tsc_deadline_conversion_selftest() -> bool {
    tsc_deadline_value(0, 1_000_000, 100) == Some(101)
        && tsc_deadline_value(1, 1_000_000, 100) == Some(101)
        && tsc_deadline_value(499, 3_000_000, 100) == Some(1597)
        && tsc_deadline_value(1, 1_000_001, 100) == Some(102)
        && tsc_deadline_value(1, 999_999, 100) == Some(101)
        && tsc_deadline_value(1, 0, 100).is_none()
        && tsc_deadline_value(u64::MAX, u64::MAX, 0).is_none()
        && tsc_deadline_value(1, 1_000_000, u64::MAX).is_none()
        && tsc_deadline_value(1, 1_000_000, u64::MAX - 1) == Some(u64::MAX)
        && tsc_deadline_value(10, 1_000_000, 900) == Some(910)
        && tsc_deadline_value(10, 1_000_000, 1900) == Some(1910)
}

/// Test only the ordinary stop path, then restore the live generic target.
/// None means this invocation did not observe a future nonzero deadline.
#[cfg(feature = "mm_selftest")]
pub(crate) fn stop_clear_selftest() -> Option<bool> {
    use crate::arch::CurrentIrqArch;
    use crate::exception::InterruptArch;

    let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let (stopped, result) = {
        let mut timer = local_apic_timer_instance_mut(smp_get_processor_id());
        if !timer.tsc_deadline_supported
            || CurrentApic.read_lvt(LVTRegister::Timer).timer_mode()
                != Some(LocalApicTimerMode::Deadline)
        {
            (false, None)
        } else {
            let before = unsafe { x86::msr::rdmsr(IA32_TSC_DEADLINE) };
            if before == 0 {
                (false, None)
            } else {
                timer.stop_current();
                let masked = CurrentApic.read_lvt(LVTRegister::Timer).data() & LVT::MASKED != 0;
                let cleared = unsafe { x86::msr::rdmsr(IA32_TSC_DEADLINE) } == 0;
                // An already expired deadline can clear naturally; do not
                // count that as evidence that stop explicitly cleared it.
                let now = unsafe {
                    core::arch::asm!("lfence", options(nostack, preserves_flags));
                    x86::time::rdtsc()
                };
                let result = if !masked || !cleared {
                    Some(false)
                } else {
                    (before > now).then_some(true)
                };
                (true, result)
            }
        }
    }; // Release the timer borrow before queue -> timer lock ordering.
    if stopped {
        // Restore even on a false/unknown result. No callback is dispatched.
        crate::time::deadline::reprogram_local();
    }
    drop(irq);
    result
}

pub(crate) fn restore_periodic() {
    let mut timer = local_apic_timer_instance_mut(smp_get_processor_id());
    if timer.mode != LocalApicTimerMode::Periodic {
        let count = timer.periodic_count;
        let divisor = timer.divisor;
        timer.init_periodic(count, divisor);
        timer.start_current();
    } else if !timer.enabled {
        timer.start_current();
    }
}

pub(crate) fn deadline_capable() -> bool {
    let timer = local_apic_timer_instance(smp_get_processor_id());
    timer.hres_calibrated && timer.periodic_count > 0
}

impl TryFrom<u8> for LocalApicTimerMode {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0b00 => {
                return Ok(LocalApicTimerMode::Oneshot);
            }
            0b01 => {
                return Ok(LocalApicTimerMode::Periodic);
            }
            0b10 => {
                return Ok(LocalApicTimerMode::Deadline);
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }
}

impl CurrentApic {
    fn set_timer_divisor(&self, divisor: u32) {
        let div_conf = match divisor {
            1 => 0b1011,
            2 => 0b0000,
            4 => 0b0001,
            8 => 0b0010,
            16 => 0b0011,
            32 => 0b1000,
            64 => 0b1001,
            128 => 0b1010,
            _ => 0b1011,
        };
        if self.x2apic_enabled() {
            unsafe { wrmsr(IA32_X2APIC_DIV_CONF, div_conf.into()) };
        } else {
            unsafe {
                self.write_xapic_register(
                    XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_CLKDIV,
                    div_conf,
                )
            };
        }
    }

    fn set_timer_initial_count(&self, initial_count: u64) {
        if self.x2apic_enabled() {
            unsafe {
                wrmsr(IA32_X2APIC_INIT_COUNT, initial_count);
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
