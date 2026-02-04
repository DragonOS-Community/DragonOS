use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use log::{info, warn};
use system_error::SystemError;

use crate::{
    arch::{driver::tsc::TSCManager, CurrentTimeArch},
    libs::spinlock::SpinLock,
    time::{
        clocksource::{Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum},
        TimeArch,
    },
};

pub static mut CLOCKSOURCE_TSC: Option<Arc<TscClocksource>> = None;

pub fn clocksource_tsc() -> Arc<TscClocksource> {
    unsafe { CLOCKSOURCE_TSC.as_ref().unwrap().clone() }
}

#[derive(Debug)]
pub struct TscClocksource(SpinLock<InnerTscClocksource>);

#[derive(Debug)]
struct InnerTscClocksource {
    data: ClocksourceData,
    self_ref: Weak<TscClocksource>,
}

impl TscClocksource {
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "tsc".to_string(),
            rating: 300,
            mask: ClocksourceMask::new(u64::MAX),
            mult: 0,
            shift: 0,
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS
                | ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY,
            watchdog_last: CycleNum::new(0),
            cs_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
            cycle_last: CycleNum::new(0),
        };
        let tsc = Arc::new(TscClocksource(SpinLock::new(InnerTscClocksource {
            data,
            self_ref: Default::default(),
        })));
        tsc.0.lock().self_ref = Arc::downgrade(&tsc);

        tsc
    }
}

impl Clocksource for TscClocksource {
    fn read(&self) -> CycleNum {
        CycleNum::new(CurrentTimeArch::get_cycles() as u64)
    }

    fn clocksource_data(&self) -> ClocksourceData {
        let inner = self.0.lock_irqsave();
        inner.data.clone()
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        self.0.lock_irqsave().self_ref.upgrade().unwrap()
    }

    fn update_clocksource_data(&self, data: ClocksourceData) -> Result<(), SystemError> {
        let d = &mut self.0.lock_irqsave().data;
        d.set_name(data.name);
        d.set_rating(data.rating);
        d.set_mask(data.mask);
        d.set_mult(data.mult);
        d.set_shift(data.shift);
        d.set_max_idle_ns(data.max_idle_ns);
        d.set_flags(data.flags);
        d.watchdog_last = data.watchdog_last;
        d.cs_last = data.cs_last;
        d.set_uncertainty_margin(data.uncertainty_margin);
        d.set_maxadj(data.maxadj);
        d.cycle_last = data.cycle_last;
        Ok(())
    }
}

pub fn init_tsc_clocksource() -> Result<(), SystemError> {
    let tsc = TscClocksource::new();
    unsafe {
        CLOCKSOURCE_TSC = Some(tsc.clone());
    }

    let tsc_khz = TSCManager::tsc_khz();
    if tsc_khz == 0 {
        warn!("TSC clocksource registration skipped: frequency unknown");
        return Err(SystemError::ENODEV);
    }
    if tsc_khz > u32::MAX as u64 {
        warn!("TSC clocksource registration skipped: frequency too high");
        return Err(SystemError::EINVAL);
    }

    let tsc_cs = tsc as Arc<dyn Clocksource>;
    tsc_cs.register(1000, tsc_khz as u32)?;
    info!("TSC clocksource registered ({} kHz)", tsc_khz);

    Ok(())
}
