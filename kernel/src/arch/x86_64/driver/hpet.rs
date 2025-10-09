use core::{
    intrinsics::unlikely,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use acpi::HpetInfo;
use alloc::{string::ToString, sync::Arc, vec::Vec};
use log::{debug, error, info, warn};
use system_error::SystemError;
use x86::time::rdtsc;

use crate::{
    arch::CurrentIrqArch,
    driver::{
        acpi::acpi_manager,
        timers::hpet::{HpetRegisters, HpetTimerRegisters},
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandleFlags, IrqHandler, IrqReturn},
        manage::irq_manager,
        InterruptArch, IrqNumber,
    },
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr,
    },
    time::jiffies::NSEC_PER_JIFFY,
};

// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/include/asm/hpet.h#39
const HPET_CFG_ENABLE: u64 = 0x001;
const HPET_CFG_LEGACY: u64 = 0x002;

static mut HPET_INSTANCE: Option<Hpet> = None;

#[inline(always)]
pub fn hpet_instance() -> &'static Hpet {
    unsafe { HPET_INSTANCE.as_ref().unwrap() }
}

#[inline(always)]
pub fn is_hpet_enabled() -> bool {
    if unsafe { HPET_INSTANCE.as_ref().is_some() } {
        return unsafe { HPET_INSTANCE.as_ref().unwrap().enabled() };
    }
    return false;
}

pub struct Hpet {
    info: HpetInfo,
    _mmio_guard: MMIOSpaceGuard,
    inner: RwLock<InnerHpet>,
    enabled: AtomicBool,
    boot_cfg: u64,
}

struct InnerHpet {
    /// 指向HPET核心寄存器数组，包含HPET的功能寄存器、周期寄存器、通用配置寄存器、通用中断状态寄存器、计数器值寄存器
    registers_ptr: NonNull<HpetRegisters>,
    /// 指向HPET定时器寄存器数组，在功能上跟linux源码中channels类似
    timer_registers_ptr: NonNull<HpetTimerRegisters>,
    /// 定时器启动时配置
    timer_boot_cfg: Vec<u64>,
}

impl Hpet {
    /// HPET0 中断间隔
    pub const HPET0_INTERVAL_USEC: u64 = NSEC_PER_JIFFY as u64 / 1000;

    const HPET0_IRQ: IrqNumber = IrqNumber::new(34);

    fn new(mut hpet_info: HpetInfo) -> Result<Self, SystemError> {
        let paddr = PhysAddr::new(hpet_info.base_address);
        let map_size = size_of::<HpetRegisters>();
        let mmio = mmio_pool().create_mmio(map_size)?;
        unsafe { mmio.map_phys(paddr, map_size)? };
        let hpet = unsafe {
            (mmio.vaddr().data() as *const HpetRegisters)
                .as_ref()
                .unwrap()
        };
        let tm_num = hpet.timers_num();
        debug!("HPET0_INTERVAL_USEC: {}", Self::HPET0_INTERVAL_USEC);
        info!("HPET has {} timers", tm_num);
        hpet_info.hpet_number = tm_num as u8;

        drop(mmio);
        if tm_num == 0 {
            return Err(SystemError::ENODEV);
        }

        let bytes_to_map = size_of::<HpetRegisters>()
            + hpet_info.hpet_number as usize * size_of::<HpetTimerRegisters>();
        let mmio = mmio_pool().create_mmio(bytes_to_map)?;

        unsafe { mmio.map_phys(paddr, bytes_to_map)? };
        let ptr = NonNull::new(mmio.vaddr().data() as *mut HpetRegisters).unwrap();
        // 记录hpet启动时配置
        let reg = unsafe { ptr.as_ptr().as_mut().unwrap() };
        let mut cfg = reg.general_config();
        let boot_cfg = cfg;
        // 设置hpet通用配置寄存器，禁用HPET、禁用legacy模式
        cfg &= !(HPET_CFG_ENABLE | HPET_CFG_LEGACY);
        unsafe { reg.write_general_config(cfg) };

        let timer_ptr = NonNull::new(
            (mmio.vaddr().data() + size_of::<HpetRegisters>()) as *mut HpetTimerRegisters,
        )
        .unwrap();
        // 记录hpet定时器启动时配置
        let mut timer_boot_cfg = Vec::with_capacity(tm_num as usize);
        for i in 0..tm_num {
            let timer_reg = unsafe { timer_ptr.as_ptr().add(i).as_ref().unwrap() };
            let cfg = timer_reg.config();
            timer_boot_cfg.push(cfg);
        }

        let hpet = Hpet {
            info: hpet_info,
            _mmio_guard: mmio,
            inner: RwLock::new(InnerHpet {
                registers_ptr: ptr,
                timer_registers_ptr: timer_ptr,
                timer_boot_cfg,
            }),
            enabled: AtomicBool::new(false),
            boot_cfg,
        };

        return Ok(hpet);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// 使能HPET
    pub fn hpet_enable(&self) -> Result<(), SystemError> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // ！！！这里是临时糊代码的，需要在apic重构的时候修改！！！
        let (inner_guard, regs) = unsafe { self.hpet_regs_mut() };
        let freq = regs.frequency();
        debug!("HPET frequency: {} Hz", freq);
        let ticks = Self::HPET0_INTERVAL_USEC * freq / 1000000;
        if ticks == 0 || ticks > freq * 8 {
            error!("HPET enable: ticks '{ticks}' is invalid");
            return Err(SystemError::EINVAL);
        }
        if unlikely(regs.timers_num() == 0) {
            return Err(SystemError::ENODEV);
        }

        drop(inner_guard);

        if !self.is_counting() {
            return Err(SystemError::ENODEV);
        }

        let (inner_guard, timer_reg) = unsafe { self.timer_mut(0).ok_or(SystemError::ENODEV) }?;

        // 设置定时器0为周期定时，边沿触发，默认投递到IO APIC的2号引脚(看conf寄存器的高32bit，哪一位被置1，则可以投递到哪一个I/O apic引脚)
        unsafe {
            timer_reg.write_config(0x004c);
            timer_reg.write_comparator_value(ticks);
        }
        drop(inner_guard);

        irq_manager().request_irq(
            Self::HPET0_IRQ,
            "HPET0".to_string(),
            &HpetIrqHandler,
            IrqHandleFlags::IRQF_TRIGGER_RISING,
            None,
        )?;

        self.enabled.store(true, Ordering::SeqCst);

        let (inner_guard, regs) = unsafe { self.hpet_regs_mut() };

        // 置位旧设备中断路由兼容标志位
        let mut cfg = regs.general_config();
        cfg |= HPET_CFG_LEGACY;
        unsafe { regs.write_general_config(cfg) };

        drop(inner_guard);

        info!("HPET enabled");

        drop(irq_guard);
        return Ok(());
    }

    fn inner(&self) -> RwLockReadGuard<'_, InnerHpet> {
        self.inner.read()
    }

    fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerHpet> {
        self.inner.write()
    }

    #[allow(dead_code)]
    fn timer(&self, index: u8) -> Option<(RwLockReadGuard<'_, InnerHpet>, &HpetTimerRegisters)> {
        let inner = self.inner();
        if index >= self.info.hpet_number {
            return None;
        }
        let timer_regs = unsafe {
            inner
                .timer_registers_ptr
                .as_ptr()
                .add(index as usize)
                .as_ref()
                .unwrap()
        };
        return Some((inner, timer_regs));
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn timer_mut(
        &self,
        index: u8,
    ) -> Option<(RwLockWriteGuard<'_, InnerHpet>, &mut HpetTimerRegisters)> {
        let inner = self.inner_mut();
        if index >= self.info.hpet_number {
            return None;
        }
        let timer_regs = unsafe {
            inner
                .timer_registers_ptr
                .as_ptr()
                .add(index as usize)
                .as_mut()
                .unwrap()
        };
        return Some((inner, timer_regs));
    }

    unsafe fn hpet_regs(&self) -> (RwLockReadGuard<'_, InnerHpet>, &HpetRegisters) {
        let inner = self.inner();
        let regs = unsafe { inner.registers_ptr.as_ref() };
        return (inner, regs);
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn hpet_regs_mut(&self) -> (RwLockWriteGuard<'_, InnerHpet>, &mut HpetRegisters) {
        let mut inner = self.inner_mut();
        let regs = unsafe { inner.registers_ptr.as_mut() };
        return (inner, regs);
    }

    pub fn main_counter_value(&self) -> u64 {
        let (inner_guard, regs) = unsafe { self.hpet_regs() };
        let value = regs.main_counter_value();

        drop(inner_guard);
        return value;
    }

    pub fn period(&self) -> u64 {
        let (inner_guard, regs) = unsafe { self.hpet_regs() };
        let period = regs.counter_clock_period();
        debug!("HPET period: {}", period);

        drop(inner_guard);
        return period;
    }

    /// 处理HPET的中断
    pub(super) fn handle_irq(&self, timer_num: u32) {
        if timer_num == 0 {
            assert!(!CurrentIrqArch::is_irq_enabled());
        }
    }

    /// 验证hpet计数器是否正在计数
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/hpet.c#895
    fn is_counting(&self) -> bool {
        self.restart_counter();

        let (_, regs) = unsafe { self.hpet_regs() };
        let t = regs.main_counter_value();
        // 获取当前时间戳(TSC)值
        let start = unsafe { rdtsc() };

        loop {
            // 如果hpet计数器值发生变化，说明hpet计数器正在计数
            if t != regs.main_counter_value() {
                return true;
            }
            // 获取当前时间戳(TSC)值
            let now = unsafe { rdtsc() };
            if (now - start) >= 200_000 {
                break;
            }
        }

        // 如果在200,000个TSC周期内，HPET计数器值没有发生变化，则HPET计数器没有在计数
        warn!("HPET counter is not counting");
        return false;
    }

    /// 重置HPET计数器
    fn restart_counter(&self) {
        self.stop_counter();
        self.reset_counter();
        self.start_counter();
    }

    /// 停止HPET计数器
    fn stop_counter(&self) {
        let (inner_guard, regs) = unsafe { self.hpet_regs_mut() };
        debug!("HPET general config: {:#x}", regs.general_config());
        let mut cfg = regs.general_config();
        cfg &= !HPET_CFG_ENABLE;
        unsafe { regs.write_general_config(cfg) };

        drop(inner_guard);
    }

    /// 重置HPET计数器
    fn reset_counter(&self) {
        let (_, regs) = unsafe { self.hpet_regs_mut() };
        unsafe { regs.write_main_counter_value(0) };
    }

    /// 启动HPET计数器
    fn start_counter(&self) {
        let (_, regs) = unsafe { self.hpet_regs_mut() };
        let mut cfg = regs.general_config();
        cfg |= HPET_CFG_ENABLE;
        unsafe { regs.write_general_config(cfg) };
    }

    /// 关闭hpet
    pub fn hpet_disable(&self) {
        debug!("HPET disable");
        if !is_hpet_enabled() {
            return;
        }

        // 恢复启动时的配置
        let mut cfg = self.boot_cfg;
        cfg &= !HPET_CFG_ENABLE;
        let (_, regs) = unsafe { self.hpet_regs_mut() };
        unsafe { regs.write_general_config(cfg) };

        // 恢复各个定时器启动时的配置
        for i in 0..self.info.hpet_number {
            let (inner_guard, timer_reg) = unsafe { self.timer_mut(i).unwrap() };
            unsafe {
                timer_reg.write_config(inner_guard.timer_boot_cfg[i as usize]);
            }
            drop(inner_guard);
        }

        // 如果HPET在启动时已经启用了，重新启用它
        if (self.boot_cfg & HPET_CFG_ENABLE) != 0 {
            let (_, regs) = unsafe { self.hpet_regs_mut() };
            unsafe { regs.write_general_config(self.boot_cfg) };
        }
    }
}

pub fn hpet_init() -> Result<(), SystemError> {
    let hpet_info =
        HpetInfo::new(acpi_manager().tables().unwrap()).map_err(|_| SystemError::ENODEV)?;

    let hpet_instance = Hpet::new(hpet_info)?;
    unsafe {
        HPET_INSTANCE = Some(hpet_instance);
    }

    return Ok(());
}

#[derive(Debug)]
struct HpetIrqHandler;

impl IrqHandler for HpetIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        hpet_instance().handle_irq(0);
        return Ok(IrqReturn::Handled);
    }
}
