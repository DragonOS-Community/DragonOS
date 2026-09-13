/// User visible nice range, matching Linux (`include/linux/sched/prio.h`).
///
/// The fair scheduling class spans `[MAX_RT_PRIO, MAX_PRIO)`. Everything below
/// `MAX_RT_PRIO` is reserved for the realtime and deadline classes, so
/// `NICE_WIDTH` is exactly the number of fair priority levels and the length of
/// the load weight table.
pub const MIN_NICE: i32 = -20;
pub const MAX_NICE: i32 = 19;
pub const NICE_WIDTH: i32 = MAX_NICE - MIN_NICE + 1;

pub const MAX_RT_PRIO: i32 = 100;
pub const MAX_PRIO: i32 = MAX_RT_PRIO + NICE_WIDTH;

/// Internal priority of a task that has never been given an explicit nice
/// value, i.e. nice 0.
///
/// This is the single source of truth for the initial `prio`/`static_prio`/
/// `normal_prio` of a new PCB. Do not spell it as a literal or derive it from
/// `MAX_PRIO` at the use site: the two only coincide while `NICE_WIDTH == 40`.
pub const DEFAULT_PRIO: i32 = MAX_RT_PRIO + NICE_WIDTH / 2;

pub const MAX_DL_PRIO: i32 = 0;

// Pin the derived values to the Linux ABI. These are the numbers that procfs,
// syscall arguments and userspace tooling (`top`, `ps`, `chrt`) encode.
const _: () = {
    assert!(NICE_WIDTH == 40);
    assert!(MAX_PRIO == 140);
    assert!(DEFAULT_PRIO == 120);
};

pub struct PrioUtil;
#[allow(dead_code)]
impl PrioUtil {
    /// Domain: `nice` is the user visible value in `[MIN_NICE, MAX_NICE]` and
    /// `prio` is an internal fair priority in `[MAX_RT_PRIO, MAX_PRIO)`.
    #[inline]
    pub fn nice_to_prio(nice: i32) -> i32 {
        debug_assert!((MIN_NICE..=MAX_NICE).contains(&nice));
        nice + DEFAULT_PRIO
    }

    /// Inverse of [`PrioUtil::nice_to_prio`]; only meaningful for a fair class
    /// priority. Realtime and deadline tasks have no meaningful nice value.
    #[inline]
    pub fn prio_to_nice(prio: i32) -> i32 {
        debug_assert!((MAX_RT_PRIO..MAX_PRIO).contains(&prio));
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

    /// Convert the legacy Linux userspace RT priority (1..=99, high to low)
    /// to DragonOS's internal priority (98..=0, low numeric value wins).
    #[inline]
    pub fn user_rt_prio_to_internal(prio: i32) -> Option<i32> {
        (1..MAX_RT_PRIO)
            .contains(&prio)
            .then_some((MAX_RT_PRIO - 1) - prio)
    }
}
