use crate::{
    arch::{io::PortIOArch, CurrentIrqArch, CurrentPortIOArch, CurrentTimeArch},
    driver::acpi::pmtmr::{acpi_pm_read_early, ACPI_PM_OVERRUN, PMTMR_TICKS_PER_SEC},
    exception::InterruptArch,
    time::{TimeArch, PIT_TICK_RATE},
};
use core::{
    cmp::{max, min},
    intrinsics::unlikely,
};
use log::{debug, error, info, warn};
use system_error::SystemError;

use super::hpet::{hpet_instance, is_hpet_enabled};

#[derive(Debug)]
pub struct TSCManager;

static mut TSC_KHZ: u64 = 0;
static mut CPU_KHZ: u64 = 0;

impl TSCManager {
    const DEFAULT_THRESHOLD: u64 = 0x20000;

    /// 初始化TSC
    ///
    /// 目前由于未支持acpi pm timer, 因此调用该函数时，HPET应当完成初始化，否则将无法校准TSC
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/tsc.c#1511
    pub fn init() -> Result<(), SystemError> {
        let cpuid = x86::cpuid::CpuId::new();
        let feat = cpuid.get_feature_info().ok_or(SystemError::ENODEV)?;
        if !feat.has_tsc() {
            error!("TSC is not available");
            return Err(SystemError::ENODEV);
        }

        if unsafe { TSC_KHZ == 0 } {
            if let Err(e) = Self::determine_cpu_tsc_frequency(false) {
                error!("Failed to determine CPU TSC frequency: {:?}", e);
                // todo: mark TSC as unstable clock source
                return Err(e);
            }
        }

        // todo: register TSC as clock source and deal with unstable clock source

        return Ok(());
    }

    /// 获取TSC和CPU总线的频率
    ///
    /// ## 参数
    ///
    /// - `early`：是否在早期初始化
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/tsc.c#1438
    fn determine_cpu_tsc_frequency(early: bool) -> Result<(), SystemError> {
        if unlikely(Self::cpu_khz() != 0 || Self::tsc_khz() != 0) {
            warn!("TSC and CPU frequency already determined");
        }

        if early {
            // todo: 先根据cpuid或者读取msr或者pit来测量TSC和CPU总线的频率
            todo!("detect TSC and CPU frequency by cpuid or msr or pit");
        } else {
            // 使用pit来测量TSC和CPU总线的频率
            Self::set_cpu_khz(Self::calibrate_cpu_by_pit_hpet_ptimer()?);
        }

        // 认为非0的TSC频率是可靠的，并且使用它来检查CPU总线的频率
        if Self::tsc_khz() == 0 {
            Self::set_tsc_khz(Self::cpu_khz());
        } else if (Self::cpu_khz() as i64 - Self::tsc_khz() as i64).abs() * 10
            > Self::cpu_khz() as i64
        {
            // 如果TSC和CPU总线的频率相差太大，那么认为CPU总线的频率是不可靠的,使用TSC的频率
            Self::set_cpu_khz(Self::tsc_khz());
        }

        if Self::cpu_khz() == 0 {
            error!("Failed to determine CPU frequency");
            return Err(SystemError::ENODEV);
        }

        info!(
            "Detected {}.{} MHz processor",
            Self::cpu_khz() / 1000,
            Self::cpu_khz() % 1000
        );
        info!(
            "Detected {}.{} MHz TSC",
            Self::tsc_khz() / 1000,
            Self::tsc_khz() % 1000
        );

        return Ok(());
    }

    /// 测量CPU总线的频率
    ///
    /// 使用pit、hpet、ptimer来测量CPU总线的频率
    fn calibrate_cpu_by_pit_hpet_ptimer() -> Result<u64, SystemError> {
        let hpet = is_hpet_enabled();
        debug!(
            "Calibrating TSC with {}",
            if hpet { "HPET" } else { "PMTIMER" }
        );

        let mut tsc_pit_min = u64::MAX;
        let mut tsc_ref_min = u64::MAX;

        // 默认的校准参数
        let cal_ms = 10;
        let cal_latch = PIT_TICK_RATE / (1000 / cal_ms);
        let cal_pit_loops = 1000;

        // 如果第一轮校准失败，那么使用这些参数（因为虚拟化平台的问题，第一轮校准可能失败）
        let cal2_ms = 50;
        let cal2_latch = PIT_TICK_RATE / (1000 / cal2_ms);
        let cal2_pit_loops = 5000;

        let mut latch = cal_latch;
        let mut loopmin = cal_pit_loops;
        let mut ms = cal_ms;

        let mut global_ref1 = 0;
        let mut global_ref2 = 0;

        for i in 0..3 {
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

            let (tsc1, ref1) = Self::read_refs(hpet);
            let tsc_pit_khz = Self::pit_calibrate_tsc(latch, ms, loopmin).unwrap_or(u64::MAX);
            let (tsc2, ref2) = Self::read_refs(hpet);
            drop(irq_guard);

            global_ref1 = ref1;
            global_ref2 = ref2;

            // 选用最小的tsc_pit_khz
            tsc_pit_min = min(tsc_pit_min, tsc_pit_khz);

            // HPET或者PTIMER可能是不可用的
            if ref1 == ref2 {
                debug!("HPET/PMTIMER not available");
                continue;
            }

            // 检查采样是否被打断
            if tsc1 == u64::MAX || tsc2 == u64::MAX {
                continue;
            }

            let mut tsc2 = (tsc2 - tsc1) * 1000000;

            if hpet {
                tsc2 = Self::calc_hpet_ref(tsc2, ref1, ref2);
            } else {
                tsc2 = Self::calc_pmtimer_ref(tsc2, ref1, ref2);
            }

            tsc_ref_min = min(tsc_ref_min, tsc2);

            // 检查与参考值的误差
            let mut delta = tsc_pit_min * 100;
            delta /= tsc_ref_min;

            // 如果误差在10%以内，那么认为测量成功
            // 返回参考值，因为它是更精确的
            if (90..=110).contains(&delta) {
                info!(
                    "PIT calibration matches {}. {} loops",
                    if hpet { "HPET" } else { "PMTIMER" },
                    i + 1
                );
                return Ok(tsc_ref_min);
            }

            if i == 1 && tsc_pit_min == u64::MAX {
                latch = cal2_latch;
                ms = cal2_ms;
                loopmin = cal2_pit_loops;
            }
        }

        if tsc_pit_min == u64::MAX {
            warn!("Unable to calibrate against PIT");

            // 如果没有参考值，那么禁用tsc
            if (!hpet) && (global_ref1 == 0) && (global_ref2 == 0) {
                warn!("No reference (HPET/PMTIMER) available");
                return Err(SystemError::ENODEV);
            }

            if tsc_ref_min == u64::MAX {
                warn!("Unable to calibrate against HPET/PMTIMER");
                return Err(SystemError::ENODEV);
            }

            info!(
                "Using {} reference calibration",
                if hpet { "HPET" } else { "PMTIMER" }
            );
            return Ok(tsc_ref_min);
        }

        // We don't have an alternative source, use the PIT calibration value
        if (!hpet) && (global_ref1 == 0) && (global_ref2 == 0) {
            info!("Using PIT calibration value");
            return Ok(tsc_pit_min);
        }

        // The alternative source failed, use the PIT calibration value
        if tsc_ref_min == u64::MAX {
            warn!("Unable to calibrate against HPET/PMTIMER, using PIT calibration value");
            return Ok(tsc_pit_min);
        }

        // The calibration values differ too much. In doubt, we use
        // the PIT value as we know that there are PMTIMERs around
        // running at double speed. At least we let the user know:
        warn!(
            "PIT calibration deviates from {}: tsc_pit_min={}, tsc_ref_min={}",
            if hpet { "HPET" } else { "PMTIMER" },
            tsc_pit_min,
            tsc_ref_min
        );

        info!("Using PIT calibration value");
        return Ok(tsc_pit_min);
    }

    /// 尝试使用PIT来校准tsc时间，并且返回tsc的频率（khz）。
    /// 如果失败，那么返回None
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/tsc.c#389
    fn pit_calibrate_tsc(latch: u64, ms: u64, loopmin: u64) -> Option<u64> {
        // 当前暂时没写legacy pic的驱动，因此这里直接返回
        let has_legacy_pic = false;
        if !has_legacy_pic {
            let mut cnt = 10000;
            while cnt > 0 {
                cnt -= 1;
            }

            return None;
        }

        unsafe {
            // Set the Gate high, disable speaker
            let d = (CurrentPortIOArch::in8(0x61) & (!0x02)) | 0x01;
            CurrentPortIOArch::out8(0x61, d);

            // Setup CTC channel 2* for mode 0, (interrupt on terminal
            // count mode), binary count. Set the latch register to 50ms
            // (LSB then MSB) to begin countdown.
            CurrentPortIOArch::out8(0x43, 0xb0);
            CurrentPortIOArch::out8(0x42, (latch & 0xff) as u8);
            CurrentPortIOArch::out8(0x42, ((latch >> 8) & 0xff) as u8);
        }

        let mut tsc = CurrentTimeArch::get_cycles() as u64;
        let t1 = tsc;
        let mut t2 = tsc;
        let mut pitcnt = 0u64;
        let mut tscmax = 0u64;
        let mut tscmin = u64::MAX;
        while unsafe { (CurrentPortIOArch::in8(0x61) & 0x20) == 0 } {
            t2 = CurrentTimeArch::get_cycles() as u64;
            let delta = t2 - tsc;
            tsc = t2;

            tscmin = min(tscmin, delta);
            tscmax = max(tscmax, delta);

            pitcnt += 1;
        }

        // Sanity checks:
        //
        // If we were not able to read the PIT more than loopmin
        // times, then we have been hit by a massive SMI
        //
        // If the maximum is 10 times larger than the minimum,
        // then we got hit by an SMI as well.
        if pitcnt < loopmin || tscmax > 10 * tscmin {
            return None;
        }

        let mut delta = t2 - t1;
        delta /= ms;

        return Some(delta);
    }

    /// 读取tsc和参考值
    ///
    /// ## 参数
    ///
    /// - `hpet_enabled`：是否启用hpet
    ///
    /// ## 返回
    ///
    /// - `Ok((tsc, ref))`：tsc和参考值
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/tsc.c#317
    fn read_refs(hpet_enabled: bool) -> (u64, u64) {
        let thresh = if Self::tsc_khz() == 0 {
            Self::DEFAULT_THRESHOLD
        } else {
            Self::tsc_khz() >> 5
        };

        let mut ref_ret = 0;
        for _ in 0..5 {
            let t1 = CurrentTimeArch::get_cycles() as u64;
            if hpet_enabled {
                ref_ret = hpet_instance().main_counter_value();
            } else {
                ref_ret = acpi_pm_read_early() as u64;
            }
            let t2 = CurrentTimeArch::get_cycles() as u64;
            if (t2 - t1) < thresh {
                return (t2, ref_ret);
            }
        }

        warn!("TSCManager: Failed to read reference value, tsc delta too high");
        return (u64::MAX, ref_ret);
    }

    /// 根据HPET的参考值计算tsc的频率
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/tsc.c#339
    fn calc_hpet_ref(mut deltatsc: u64, ref1: u64, mut ref2: u64) -> u64 {
        if ref2 <= ref1 {
            ref2 += 0x100000000;
        }

        ref2 -= ref1;
        let mut tmp = ref2 * hpet_instance().period();

        tmp /= 1000000;

        deltatsc /= tmp;

        return deltatsc;
    }

    /// 根据PMtimer的参考值计算tsc的频率
    fn calc_pmtimer_ref(mut deltatsc: u64, ref1: u64, mut ref2: u64) -> u64 {
        if unlikely(ref1 == 0 && ref2 == 0) {
            return u64::MAX;
        }

        if ref2 < ref1 {
            ref2 += ACPI_PM_OVERRUN;
        }

        ref2 -= ref1;

        let mut tmp = ref2 * 1000000000;

        tmp /= PMTMR_TICKS_PER_SEC;

        deltatsc /= tmp;

        return deltatsc;
    }

    pub fn tsc_khz() -> u64 {
        unsafe { TSC_KHZ }
    }

    pub fn cpu_khz() -> u64 {
        unsafe { CPU_KHZ }
    }

    fn set_cpu_khz(khz: u64) {
        unsafe {
            CPU_KHZ = khz;
        }
    }

    fn set_tsc_khz(khz: u64) {
        unsafe {
            TSC_KHZ = khz;
        }
    }
}
