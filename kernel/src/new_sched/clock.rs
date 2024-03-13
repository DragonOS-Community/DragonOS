/*
    这个文件实现的是调度过程中设计到的时钟
*/

use crate::time::{clocksource::HZ, timer::clock, NSEC_PER_SEC};

pub struct SchedClock;

impl SchedClock {
    /// 这里是半精度计时，可以使用更高精度的计时器
    #[inline]
    pub fn sched_clock_cpu(_cpu: usize) -> u128 {
        clock() as u128 * (NSEC_PER_SEC as u128 / HZ as u128)
    }
}

bitflags! {
    pub struct ClockUpdataFlag: u8 {
        /// 请求在下一次调用 __schedule() 时跳过时钟更新
        const RQCF_REQ_SKIP = 0x01;
        /// 表示跳过时钟更新正在生效，update_rq_clock() 的调用将被忽略。
        const RQCF_ACT_SKIP = 0x02;
        /// 调试标志，指示自上次固定 rq::lock 以来是否已调用过
        const RQCF_UPDATE = 0x03;
    }
}
