//! 这个文件实现的是调度过程中涉及到的时钟
//!
use crate::{arch::CurrentTimeArch, smp::cpu::ProcessorId, time::TimeArch};

pub struct SchedClock;

impl SchedClock {
    #[inline]
    pub fn sched_clock_cpu(_cpu: ProcessorId) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            if crate::arch::driver::tsc::TSCManager::cpu_khz() == 0 {
                // TSC not calibrated yet
                return 0;
            }
            return CurrentTimeArch::cycles2ns(CurrentTimeArch::get_cycles()) as u64;
        }

        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        {
            return CurrentTimeArch::cycles2ns(CurrentTimeArch::get_cycles()) as u64;
        }
    }
}

bitflags! {
    pub struct ClockUpdataFlag: u8 {
        /// 请求在下一次调用 __schedule() 时跳过时钟更新
        const RQCF_REQ_SKIP = 0x01;
        /// 表示跳过时钟更新正在生效，update_rq_clock() 的调用将被忽略。
        const RQCF_ACT_SKIP = 0x02;
        /// 调试标志，指示自上次固定 rq::lock 以来是否已调用过
        const RQCF_UPDATE = 0x04;
    }
}
