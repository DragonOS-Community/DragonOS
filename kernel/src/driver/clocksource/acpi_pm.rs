use crate::{
    alloc::string::ToString,
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::acpi::{
        acpi_manager,
        pmtmr::{ACPI_PM_MASK, PMTMR_TICKS_PER_SEC},
    },
    libs::spinlock::SpinLock,
    time::clocksource::{
        Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum,
    },
};
#[cfg(target_arch = "riscv64")]
use crate::{
    arch::riscv64::asm::mach_timer::{mach_countup, mach_prepare_counter, CALIBRATE_LATCH},
    time::PIT_TICK_RATE,
};
use acpi::fadt::Fadt;
use alloc::sync::{Arc, Weak};
use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicU32, Ordering};
use system_error::SystemError;

// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/clocksource/acpi_pm.c

/// acpi_pmtmr所在的I/O端口
pub static mut PMTMR_IO_PORT: AtomicU32 = AtomicU32::new(0);

/// # 读取acpi_pmtmr当前值，并对齐进行掩码操作
#[inline(always)]
fn read_pmtmr() -> u32 {
    return unsafe { CurrentPortIOArch::in32(PMTMR_IO_PORT.load(Ordering::SeqCst) as u16) }
        & ACPI_PM_MASK as u32;
}

//参考： https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/clocksource/acpi_pm.c#41
/// # 读取acpi_pmtmr的值，并进行多次读取以保证获取正确的值
///
/// ## 返回值
/// - u32: 读取到的acpi_pmtmr值
pub fn acpi_pm_read_verified() -> u32 {
    let mut v2: u32;

    // 因为某些损坏芯片组（如ICH4、PIIX4和PIIX4E）可能导致APCI PM时钟源未锁存
    // 因此需要多次读取以保证获取正确的值
    loop {
        let v1 = read_pmtmr();
        v2 = read_pmtmr();
        let v3 = read_pmtmr();

        if !(unlikely((v2 > v3 || v1 < v3) && v1 > v2 || v1 < v3 && v2 > v3)) {
            break;
        }
    }

    return v2;
}

/// # 作为时钟源的读取函数
///
/// ## 返回值
/// - u64: acpi_pmtmr的当前值
fn acpi_pm_read() -> u64 {
    return read_pmtmr() as u64;
}

pub static mut CLOCKSOURCE_ACPI_PM: Option<Arc<Acpipm>> = None;

pub fn clocksource_acpi_pm() -> Arc<Acpipm> {
    return unsafe { CLOCKSOURCE_ACPI_PM.as_ref().unwrap().clone() };
}

#[derive(Debug)]
pub struct Acpipm(SpinLock<InnerAcpipm>);

#[derive(Debug)]
struct InnerAcpipm {
    data: ClocksourceData,
    self_reaf: Weak<Acpipm>,
}

impl Acpipm {
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "acpi_pm".to_string(),
            rating: 200,
            mask: ClocksourceMask::new(ACPI_PM_MASK),
            mult: 0,
            shift: 0,
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS,
            watchdog_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
        };
        let acpi_pm = Arc::new(Acpipm(SpinLock::new(InnerAcpipm {
            data,
            self_reaf: Default::default(),
        })));
        acpi_pm.0.lock().self_reaf = Arc::downgrade(&acpi_pm);

        return acpi_pm;
    }
}

impl Clocksource for Acpipm {
    fn read(&self) -> CycleNum {
        return CycleNum::new(acpi_pm_read());
    }

    fn clocksource_data(&self) -> ClocksourceData {
        let inner = self.0.lock_irqsave();
        return inner.data.clone();
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        return self.0.lock_irqsave().self_reaf.upgrade().unwrap();
    }

    fn update_clocksource_data(&self, _data: ClocksourceData) -> Result<(), SystemError> {
        let d = &mut self.0.lock_irqsave().data;
        d.set_flags(_data.flags);
        d.set_mask(_data.mask);
        d.set_max_idle_ns(_data.max_idle_ns);
        d.set_mult(_data.mult);
        d.set_name(_data.name);
        d.set_rating(_data.rating);
        d.set_shift(_data.shift);
        d.watchdog_last = _data.watchdog_last;
        return Ok(());
    }
}

#[cfg(target_arch = "riscv64")]
const PMTMR_EXPECTED_RATE: u64 =
    (CALIBRATE_LATCH * (PMTMR_TICKS_PER_SEC >> 10)) / (PIT_TICK_RATE >> 10);

/// # 验证ACPI PM Timer的运行速率是否在预期范围内(在x86_64架构以外的情况下验证)
///
/// ## 返回值
/// - i32：如果为0则表示在预期范围内，否则不在
#[cfg(target_arch = "riscv64")]
fn verify_pmtmr_rate() -> i32 {
    let mut count: u32 = 0;

    mach_prepare_counter();
    let value1 = clocksource_acpi_pm().read().data();
    mach_countup(&mut count);
    let value2 = clocksource_acpi_pm().read().data();
    let delta = (value2 - value1) & ACPI_PM_MASK;

    if (delta < (PMTMR_EXPECTED_RATE * 19) / 20) || (delta > (PMTMR_EXPECTED_RATE * 21) / 20) {
        kinfo!(
            "PM Timer running at invalid rate: {}",
            100 * delta / PMTMR_EXPECTED_RATE
        );
        return -1;
    }

    return 0;
}
#[cfg(target_arch = "x86_64")]
fn verify_pmtmr_rate() -> i32 {
    return 0;
}

const ACPI_PM_MONOTONIC_CHECKS: u32 = 10;
const ACPI_PM_READ_CHECKS: u32 = 10000;

/// # 解析fadt
fn acpi_parse_fadt() -> Result<(), SystemError> {
    let fadt = acpi_manager()
        .tables()
        .unwrap()
        .find_table::<Fadt>()
        .expect("failed to find FADT table");
    let pm_timer_block = fadt.pm_timer_block().map_err(|_| SystemError::ENODEV)?;
    let pm_timer_block = pm_timer_block.ok_or(SystemError::ENODEV)?;
    let pmtmr_addr = pm_timer_block.address;
    unsafe {
        PMTMR_IO_PORT.store(pmtmr_addr as u32, Ordering::SeqCst);
    }
    kinfo!("apic_pmtmr I/O port: {}", unsafe {
        PMTMR_IO_PORT.load(Ordering::SeqCst)
    });

    return Ok(());
}

/// # 初始化ACPI PM Timer作为系统时钟源
// #[unified_init(INITCALL_FS)]
pub fn init_acpi_pm_clocksource() -> Result<(), SystemError> {
    let acpi_pm = Acpipm::new();
    unsafe {
        CLOCKSOURCE_ACPI_PM = Some(acpi_pm);
    }

    // 解析fadt
    acpi_parse_fadt()?;

    // 检查pmtmr_io_port是否被设置
    if unsafe { PMTMR_IO_PORT.load(Ordering::SeqCst) } == 0 {
        return Err(SystemError::ENODEV);
    }

    // 验证ACPI PM Timer作为时钟源的稳定性和一致性
    for j in 0..ACPI_PM_MONOTONIC_CHECKS {
        let mut cnt = 100 * j;
        while cnt > 0 {
            cnt -= 1;
        }

        let value1 = clocksource_acpi_pm().read().data();
        let mut i = 0;
        for _ in 0..ACPI_PM_READ_CHECKS {
            i += 1;
            let value2 = clocksource_acpi_pm().read().data();
            if value2 == value1 {
                continue;
            }
            if value2 > value1 {
                break;
            }
            if (value2 < value1) && (value2 < 0xfff) {
                break;
            }
            kinfo!("PM Timer had inconsistens results: {} {}", value1, value2);
            unsafe {
                PMTMR_IO_PORT.store(0, Ordering::SeqCst);
            }
            return Err(SystemError::EINVAL);
        }
        if i == ACPI_PM_READ_CHECKS {
            kinfo!("PM Timer failed consistency check: {}", value1);
            unsafe {
                PMTMR_IO_PORT.store(0, Ordering::SeqCst);
            }
            return Err(SystemError::EINVAL);
        }
    }

    // 检查ACPI PM Timer的频率是否正确
    if verify_pmtmr_rate() != 0 {
        unsafe {
            PMTMR_IO_PORT.store(0, Ordering::SeqCst);
        }
    }

    // 检查TSC时钟源的监视器是否被禁用，如果被禁用则将时钟源的标志设置为CLOCK_SOURCE_MUST_VERIFY
    // 没有实现clocksource_selecet_watchdog函数，所以这里设置为false
    let tsc_clocksource_watchdog_disabled = false;
    if tsc_clocksource_watchdog_disabled {
        clocksource_acpi_pm().0.lock_irqsave().data.flags |=
            ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY;
    }

    // 注册ACPI PM Timer
    let acpi_pmtmr = clocksource_acpi_pm() as Arc<dyn Clocksource>;
    match acpi_pmtmr.register(100, PMTMR_TICKS_PER_SEC as u32) {
        Ok(_) => {
            kinfo!("ACPI PM Timer registered as clocksource sccessfully");
            return Ok(());
        }
        Err(_) => {
            kinfo!("ACPI PM Timer init registered failed");
            return Err(SystemError::ENOSYS);
        }
    };
}
