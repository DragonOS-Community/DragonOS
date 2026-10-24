use core::cell::RefCell;
use core::hint::spin_loop;
use core::sync::atomic::{fence, AtomicU64, Ordering};

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
    local_apic_timer.init(
        LocalApicTimerMode::Periodic,
        initial_count,
        LocalApicTimer::DIVISOR as u32,
    );
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
    local_apic_timer.init(
        LocalApicTimerMode::Periodic,
        initial_count,
        LocalApicTimer::DIVISOR as u32,
    );
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
        let local_apic_timer = local_apic_timer_instance(cpu_id);
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
        let local_apic_timer = local_apic_timer_instance_mut(cpu_id);
        local_apic_timer.stop_current();
    }
}

#[derive(Debug, Copy, Clone)]
pub struct LocalApicTimer {
    mode: LocalApicTimerMode,
    /// IntialCount
    initial_count: u64,
    periodic_count: u64,
    hres_calibrated: bool,
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
            initial_count: 0,
            periodic_count: 0,
            hres_calibrated: false,
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

    /// Init this manager.
    ///
    /// At this time, it does nothing.
    fn init(&mut self, mode: LocalApicTimerMode, initial_count: u64, divisor: u32) {
        self.stop_current();
        self.triggered = false;
        match mode {
            LocalApicTimerMode::Periodic => self.install_periodic_mode(initial_count, divisor),
            LocalApicTimerMode::Oneshot => {
                self.mode = mode;
                self.set_divisor(divisor);
                // The counter starts as soon as initial_count is written.
                // A tiny count can expire before a later LVT unmask, losing
                // the only one-shot interrupt and stopping the ordinary tick.
                // Caller holds local IRQs disabled; program LVTT first.
                self.setup_lvt(APIC_TIMER_IRQ_NUM.data() as u8, false, mode);
                self.set_initial_cnt(initial_count);
            }
            LocalApicTimerMode::Deadline => todo!(),
        }
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
        let mode: u32 = mode as u32;
        let data = (mode << 17) | (vector as u32) | (if mask { 1 << 16 } else { 0 });
        let lvt = LVT::new(LVTRegister::Timer, data).unwrap();

        CurrentApic.set_lvt(lvt);
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
    }

    fn stop_current(&self) {
        let mut lvt = CurrentApic.read_lvt(LVTRegister::Timer);
        lvt.set_mask(true);
        CurrentApic.set_lvt(lvt);
    }

    fn set_lvt_masked(&mut self, mode: LocalApicTimerMode) {
        self.setup_lvt(APIC_TIMER_IRQ_NUM.data() as u8, true, mode);
    }

    /// 检查是否支持TSC-Deadline
    ///
    /// 此函数调用cpuid，请避免多次调用此函数。
    /// 如果支持TSC-Deadline模式，则除非TSC为常数，否则不会启用该模式。
    #[allow(dead_code)]
    pub fn is_deadline_mode_supported(&self) -> bool {
        let res = cpuid!(1);
        return (res.ecx & (1 << 24)) != 0;
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
    let count = ((delta_ns as u128 * timer.periodic_count as u128)
        .div_ceil(LocalApicTimer::INTERVAL_MS as u128 * 1_000_000))
    .clamp(1, u32::MAX as u128) as u64;
    let divisor = timer.divisor;
    let pcount = timer.periodic_count;
    timer.init(LocalApicTimerMode::Oneshot, count, divisor);
    timer.start_current();
    tmict_probe(0, delta_ns, count, pcount);
}

pub(crate) fn restore_periodic() {
    let mut timer = local_apic_timer_instance_mut(smp_get_processor_id());
    if timer.mode != LocalApicTimerMode::Periodic {
        let count = timer.periodic_count;
        let divisor = timer.divisor;
        timer.init(LocalApicTimerMode::Periodic, count, divisor);
        timer.start_current();
        tmict_probe(1, 0, count, count);
    }
}

// ===========================================================================
// TMICT 探针：APIC 单次/周期武装计数采样（默认静默）
//
// 设计要点（R16 教训）：
// 1) **所有采样基线必须 per-CPU**。早期版本用单个 static，被两个 CPU 交替
//    swap，相邻两次采样跨 CPU 混合，差分彻底失真（曾据此误判 tsc_hz=9.1GHz）。
// 2) **PMTMR 是 24bit 计数器**（~4.7ms 回绕），直接跨窗口差分得到的是无意义
//    值；这里用 per-CPU 扩展计数器把回绕累加进 64bit，据此交叉定频 TSC。
// 3) **打印用编译期开关关闭**：本函数由 program_deadline_delta 在 rq/deadline
//    临界区内（IF=0）调用，直写串口会占用数 ms，正是"console in irq-off"
//    类缺陷。需要时钟取证时把 TMICT_PRINT 改为 true。
// ===========================================================================

/// 打印开关：默认关闭（见文件头注释 3）。
const TMICT_PRINT: bool = false;

static TP_TOTAL: AtomicU64 = AtomicU64::new(0);
static TP_ONESHOT: AtomicU64 = AtomicU64::new(0);
static TP_PERIODIC: AtomicU64 = AtomicU64::new(0);
static TP_LAST_DELTA: AtomicU64 = AtomicU64::new(0);
static TP_LAST_CNT: AtomicU64 = AtomicU64::new(0);
static TP_LAST_PCOUNT: AtomicU64 = AtomicU64::new(0);
static TP_LINES: AtomicU64 = AtomicU64::new(0);

const PROBE_CPUS: usize = PerCpu::MAX_CPU_NUM as usize;
/// 上次打印的 TSC 窗口起点（per-CPU）。
static TP_WINDOW_TSC: [AtomicU64; PROBE_CPUS] = [const { AtomicU64::new(0) }; PROBE_CPUS];
/// PMTMR 扩展计数与 TSC 的配对基线（per-CPU，用于交叉定频）。
static TP_PM_EXT_REF: [AtomicU64; PROBE_CPUS] = [const { AtomicU64::new(0) }; PROBE_CPUS];
static TP_PM_TSC_REF: [AtomicU64; PROBE_CPUS] = [const { AtomicU64::new(0) }; PROBE_CPUS];

/// PMTMR 原始读数上一次采样值（per-CPU，回绕累加用）。
static PM_LAST_RAW: [AtomicU64; PROBE_CPUS] = [const { AtomicU64::new(u64::MAX) }; PROBE_CPUS];
/// PMTMR 扩展后的 64bit 计数（per-CPU）。
static PM_EXT: [AtomicU64; PROBE_CPUS] = [const { AtomicU64::new(0) }; PROBE_CPUS];

/// 读 PMTMR 并扩展到 64bit（本 CPU 独占槽位，仅探针使用）。
fn pm_extended(cpu: usize) -> u64 {
    let raw = (acpi_pm_read_early() as u64) & ACPI_PM_MASK;
    let last = PM_LAST_RAW[cpu].load(Ordering::Relaxed);
    if last == u64::MAX {
        PM_LAST_RAW[cpu].store(raw, Ordering::Relaxed);
        PM_EXT[cpu].store(raw, Ordering::Relaxed);
        return raw;
    }
    let delta = raw.wrapping_sub(last) & ACPI_PM_MASK;
    PM_LAST_RAW[cpu].store(raw, Ordering::Relaxed);
    PM_EXT[cpu].fetch_add(delta, Ordering::Relaxed) + delta
}

/// 记录一次武装事件；`kind`: 0 = 单次（deadline），1 = 周期（tick 恢复）。
///
/// 计数始终更新；打印仅在 `TMICT_PRINT` 且处于 1s 窗口或每 2048 次事件时发生，
/// 并限制总行数，避免探针自身成为噪声源。
fn tmict_probe(kind: usize, delta_ns: u64, cnt: u64, pcount: u64) {
    if kind == 0 {
        TP_ONESHOT.fetch_add(1, Ordering::Relaxed);
    } else {
        TP_PERIODIC.fetch_add(1, Ordering::Relaxed);
    }
    TP_LAST_DELTA.store(delta_ns, Ordering::Relaxed);
    TP_LAST_CNT.store(cnt, Ordering::Relaxed);
    TP_LAST_PCOUNT.store(pcount, Ordering::Relaxed);
    let total = TP_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if !TMICT_PRINT {
        return;
    }

    let cpu = smp_get_processor_id().data() as usize;
    if cpu >= PROBE_CPUS {
        return;
    }
    let khz = TSCManager::cpu_khz();
    let tsc = unsafe { x86::time::rdtsc() };
    let window_elapsed =
        khz > 0 && tsc.wrapping_sub(TP_WINDOW_TSC[cpu].load(Ordering::Relaxed)) > khz * 1_000_000;
    if !window_elapsed && !total.is_multiple_of(2048) {
        return;
    }
    // 总行数上限：探针只在取证时短暂开启，无需无限输出。
    if TP_LINES.fetch_add(1, Ordering::Relaxed) >= 64 {
        return;
    }
    TP_WINDOW_TSC[cpu].store(tsc, Ordering::Relaxed);

    // PMTMR(3.579545MHz，宿主保证真实速率) × TSC 交叉定频：
    // 真实 TSC 频率 = Δtsc / Δpmtmr × 3.579545e6。
    let tsc_hz = if PMTMR_IO_PORT.load(Ordering::SeqCst) != 0 {
        let pm = pm_extended(cpu);
        let p0 = TP_PM_EXT_REF[cpu].swap(pm, Ordering::Relaxed);
        let t0 = TP_PM_TSC_REF[cpu].swap(tsc, Ordering::Relaxed);
        let d_tsc = tsc.wrapping_sub(t0);
        let d_pm = pm.wrapping_sub(p0);
        if p0 != 0 && d_pm > 100_000 {
            Some((d_tsc as u128 * 3_579_545u128 / d_pm as u128) as u64)
        } else {
            None
        }
    } else {
        None
    };

    log::warn!(
        "tmict cpu={} tot={} 1shot={} per={} delta={}ns cnt={:#x} pcount={:#x} tsc_hz={}",
        cpu,
        total,
        TP_ONESHOT.load(Ordering::Relaxed),
        TP_PERIODIC.load(Ordering::Relaxed),
        TP_LAST_DELTA.load(Ordering::Relaxed),
        TP_LAST_CNT.load(Ordering::Relaxed),
        TP_LAST_PCOUNT.load(Ordering::Relaxed),
        tsc_hz.map_or_else(|| "init".to_string(), |v| v.to_string())
    );
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
