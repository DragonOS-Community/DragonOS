use core::{
    intrinsics::unlikely,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use acpi::HpetInfo;
use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

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
        softirq::{softirq_vectors, SoftirqNumber},
        InterruptArch, IrqNumber,
    },
    kdebug, kerror, kinfo,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        volatile::volwrite,
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr,
    },
    time::timer::{clock, timer_get_first_expire, update_timer_jiffies},
};

static mut HPET_INSTANCE: Option<Hpet> = None;

#[inline(always)]
pub fn hpet_instance() -> &'static Hpet {
    unsafe { HPET_INSTANCE.as_ref().unwrap() }
}

pub struct Hpet {
    info: HpetInfo,
    _mmio_guard: MMIOSpaceGuard,
    inner: RwLock<InnerHpet>,
    enabled: AtomicBool,
}

struct InnerHpet {
    registers_ptr: NonNull<HpetRegisters>,
    timer_registers_ptr: NonNull<HpetTimerRegisters>,
}

impl Hpet {
    /// HPET0 中断间隔为 10ms
    pub const HPET0_INTERVAL_USEC: u64 = 10000;

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
        kinfo!("HPET has {} timers", tm_num);
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
        let timer_ptr = NonNull::new(
            (mmio.vaddr().data() + size_of::<HpetRegisters>()) as *mut HpetTimerRegisters,
        )
        .unwrap();

        let hpet = Hpet {
            info: hpet_info,
            _mmio_guard: mmio,
            inner: RwLock::new(InnerHpet {
                registers_ptr: ptr,
                timer_registers_ptr: timer_ptr,
            }),
            enabled: AtomicBool::new(false),
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
        kdebug!("HPET frequency: {} Hz", freq);
        let ticks = Self::HPET0_INTERVAL_USEC * freq / 1000000;
        if ticks <= 0 || ticks > freq * 8 {
            kerror!("HPET enable: ticks '{ticks}' is invalid");
            return Err(SystemError::EINVAL);
        }
        if unlikely(regs.timers_num() == 0) {
            return Err(SystemError::ENODEV);
        }

        unsafe { regs.write_main_counter_value(0) };

        drop(inner_guard);

        let (inner_guard, timer_reg) = unsafe { self.timer_mut(0).ok_or(SystemError::ENODEV) }?;

        let timer_reg = NonNull::new(timer_reg as *mut HpetTimerRegisters).unwrap();

        unsafe {
            // 设置定时器0为周期定时，边沿触发，默认投递到IO APIC的2号引脚(看conf寄存器的高32bit，哪一位被置1，则可以投递到哪一个I/O apic引脚)
            volwrite!(timer_reg, config, 0x004c);
            volwrite!(timer_reg, comparator_value, ticks);
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

        // 置位旧设备中断路由兼容标志位、定时器组使能标志位
        unsafe { regs.write_general_config(3) };

        drop(inner_guard);

        kinfo!("HPET enabled");

        drop(irq_guard);
        return Ok(());
    }

    fn inner(&self) -> RwLockReadGuard<InnerHpet> {
        self.inner.read()
    }

    fn inner_mut(&self) -> RwLockWriteGuard<InnerHpet> {
        self.inner.write()
    }

    #[allow(dead_code)]
    fn timer(&self, index: u8) -> Option<(RwLockReadGuard<InnerHpet>, &HpetTimerRegisters)> {
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

    unsafe fn timer_mut(
        &self,
        index: u8,
    ) -> Option<(RwLockWriteGuard<InnerHpet>, &mut HpetTimerRegisters)> {
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

    unsafe fn hpet_regs(&self) -> (RwLockReadGuard<InnerHpet>, &HpetRegisters) {
        let inner = self.inner();
        let regs = unsafe { inner.registers_ptr.as_ref() };
        return (inner, regs);
    }

    unsafe fn hpet_regs_mut(&self) -> (RwLockWriteGuard<InnerHpet>, &mut HpetRegisters) {
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
        kdebug!("HPET period: {}", period);

        drop(inner_guard);
        return period;
    }

    /// 处理HPET的中断
    pub(super) fn handle_irq(&self, timer_num: u32) {
        if timer_num == 0 {
            assert!(CurrentIrqArch::is_irq_enabled() == false);
            update_timer_jiffies(Self::HPET0_INTERVAL_USEC, Self::HPET0_INTERVAL_USEC as i64);

            if let Ok(first_expire) = timer_get_first_expire() {
                if first_expire <= clock() {
                    softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
                }
            }
        }
    }
}

pub fn hpet_init() -> Result<(), SystemError> {
    let hpet_info = HpetInfo::new(acpi_manager().tables().unwrap()).map_err(|e| {
        kerror!("Failed to get HPET info: {:?}", e);
        SystemError::ENODEV
    })?;

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
