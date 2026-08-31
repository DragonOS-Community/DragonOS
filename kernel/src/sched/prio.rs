pub const MAX_NICE: i32 = 20;
pub const MIN_NICE: i32 = -20;
pub const NICE_WIDTH: i32 = MAX_NICE - MIN_NICE + 1;

pub const MAX_RT_PRIO: i32 = 100;
pub const MAX_PRIO: i32 = MAX_RT_PRIO + NICE_WIDTH;
#[allow(dead_code)]
pub const DEFAULT_PRIO: i32 = MAX_RT_PRIO + NICE_WIDTH / 2;

pub const MAX_DL_PRIO: i32 = 0;
pub struct PrioUtil;
#[allow(dead_code)]
impl PrioUtil {
    #[inline]
    pub fn nice_to_prio(nice: i32) -> i32 {
        nice + DEFAULT_PRIO
    }

    #[inline]
    pub fn prio_to_nice(prio: i32) -> i32 {
        prio - DEFAULT_PRIO
    }

    #[inline]
    pub fn dl_prio(prio: i32) -> bool {
        return prio < MAX_DL_PRIO;
    }

    #[inline]
    pub fn rt_prio(prio: i32) -> bool {
        return prio < MAX_RT_PRIO;
    }

    /// Convert the internal RT priority (0..=98, high to low) to the legacy
    /// Linux userspace value (99..=1).
    #[inline]
    pub fn internal_rt_prio_to_user(prio: i32) -> Option<i32> {
        (0..MAX_RT_PRIO - 1)
            .contains(&prio)
            .then_some((MAX_RT_PRIO - 1) - prio)
    }
}
