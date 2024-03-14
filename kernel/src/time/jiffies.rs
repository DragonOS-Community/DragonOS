use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{kerror, kinfo, libs::spinlock::SpinLock};

use super::{
    clocksource::{Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum, HZ},
    timer::clock,
    NSEC_PER_SEC,
};
lazy_static! {
    pub static ref DEFAULT_CLOCK: Arc<ClocksourceJiffies> = ClocksourceJiffies::new();
}
pub const CLOCK_TICK_RATE: u32 = HZ as u32 * 100000;
pub const JIFFIES_SHIFT: u32 = 8;
pub const LATCH: u32 = (CLOCK_TICK_RATE + (HZ as u32) / 2) / HZ as u32;
pub const ACTHZ: u32 = sh_div(CLOCK_TICK_RATE, LATCH, 8);
pub const NSEC_PER_JIFFY: u32 = (NSEC_PER_SEC << 8) / ACTHZ;
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
        CycleNum(clock())
    }

    fn clocksource_data(&self) -> ClocksourceData {
        let inner = self.0.lock();
        return inner.data.clone();
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        self.0.lock().self_ref.upgrade().unwrap()
    }
    fn update_clocksource_data(&self, _data: ClocksourceData) -> Result<(), SystemError> {
        let d = &mut self.0.lock().data;
        d.set_flags(_data.flags);
        d.set_mask(_data.mask);
        d.set_max_idle_ns(_data.max_idle_ns);
        d.set_mult(_data.mult);
        d.set_name(_data.name);
        d.set_rating(_data.rating);
        d.set_shift(_data.shift);
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
            watchdog_last: CycleNum(0),
        };
        let jieffies = Arc::new(ClocksourceJiffies(SpinLock::new(InnerJiffies {
            data,
            self_ref: Default::default(),
        })));
        jieffies.0.lock().self_ref = Arc::downgrade(&jieffies);

        return jieffies;
    }
}
pub fn clocksource_default_clock() -> Arc<ClocksourceJiffies> {
    DEFAULT_CLOCK.clone()
}

pub fn jiffies_init() {
    //注册jiffies
    let jiffies = clocksource_default_clock() as Arc<dyn Clocksource>;
    match jiffies.register() {
        Ok(_) => {
            kinfo!("jiffies_init sccessfully");
        }
        Err(_) => {
            kerror!("jiffies_init failed, no default clock running");
        }
    };
}

#[no_mangle]
pub extern "C" fn rs_jiffies_init() {
    jiffies_init();
}
