use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use log::{error, info};
use system_error::SystemError;

use crate::{arch::time::CLOCK_TICK_RATE, libs::spinlock::SpinLock};

use super::{
    clocksource::{Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum, HZ},
    timer::clock,
    NSEC_PER_SEC,
};
lazy_static! {
    pub static ref DEFAULT_CLOCK: Arc<ClocksourceJiffies> = ClocksourceJiffies::new();
}

pub const JIFFIES_SHIFT: u32 = 8;
pub const LATCH: u32 = (CLOCK_TICK_RATE + (HZ as u32) / 2) / HZ as u32;
pub const ACTHZ: u32 = sh_div(CLOCK_TICK_RATE, LATCH, 8);
pub const TICK_NESC: u32 = (NSEC_PER_SEC + (HZ as u32) / 2) / HZ as u32;
//TODO 编写测试，保证始终跳动间隔与现实一致（两种时钟源进行对拍）
pub const NSEC_PER_JIFFY: u32 = (((NSEC_PER_SEC as u64) << 8) / ACTHZ as u64) as u32;
pub const fn sh_div(nom: u32, den: u32, lsh: u32) -> u32 {
    (((nom) / (den)) << (lsh)) + ((((nom) % (den)) << (lsh)) + (den) / 2) / (den)
}

#[derive(Debug)]
pub struct ClocksourceJiffies(SpinLock<InnerJiffies>);

#[derive(Debug)]
pub struct InnerJiffies {
    data: ClocksourceData,
    self_ref: Weak<ClocksourceJiffies>,
}

impl Clocksource for ClocksourceJiffies {
    fn read(&self) -> CycleNum {
        CycleNum::new(clock())
    }

    fn clocksource_data(&self) -> ClocksourceData {
        let inner = self.0.lock_irqsave();
        return inner.data.clone();
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
        return Ok(());
    }

    fn enable(&self) -> Result<i32, SystemError> {
        return Ok(0);
    }
}
impl ClocksourceJiffies {
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "jiffies".to_string(),
            rating: 1,
            mask: ClocksourceMask::new(0xffffffff),
            mult: NSEC_PER_JIFFY << JIFFIES_SHIFT,
            shift: JIFFIES_SHIFT,
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::new(0),
            watchdog_last: CycleNum::new(0),
            cs_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
            cycle_last: CycleNum::new(0),
        };
        let jiffies = Arc::new(ClocksourceJiffies(SpinLock::new(InnerJiffies {
            data,
            self_ref: Default::default(),
        })));
        jiffies.0.lock().self_ref = Arc::downgrade(&jiffies);

        return jiffies;
    }
}
pub fn clocksource_default_clock() -> Arc<ClocksourceJiffies> {
    DEFAULT_CLOCK.clone()
}

pub fn jiffies_init() {
    //注册jiffies
    let jiffies = clocksource_default_clock() as Arc<dyn Clocksource>;
    match jiffies.register(1, 0) {
        Ok(_) => {
            info!("jiffies_init sccessfully");
        }
        Err(_) => {
            error!("jiffies_init failed, no default clock running");
        }
    };
}

#[no_mangle]
pub extern "C" fn rs_jiffies_init() {
    jiffies_init();
}
