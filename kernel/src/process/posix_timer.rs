//! POSIX interval timers (timer_create/timer_settime/...) for a process.
//!
//! This is a minimal-but-correct implementation for gVisor `timers.cc` tests:
//! - CLOCK_MONOTONIC based timers
//! - SIGEV_NONE / SIGEV_SIGNAL / SIGEV_THREAD / SIGEV_THREAD_ID
//! - coalescing: at most one pending signal per (signo,timerid); overruns accumulate

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    ipc::signal_types::{PosixSigval, SigCode, SigInfo, SigType},
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, RawPid},
    time::{
        jiffies::NSEC_PER_JIFFY,
        syscall::PosixClockID,
        timer::{clock, Jiffies, Timer, TimerFunction},
        PosixTimeSpec,
    },
};

use core::{mem::size_of, time::Duration};

/// 用户态 itimerspec
#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct PosixItimerspec {
    pub it_interval: PosixTimeSpec,
    pub it_value: PosixTimeSpec,
}

/// 用户态 sigevent（Linux x86_64 下大小为 64B；这里保守定义为 64B）
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PosixSigevent {
    pub sigev_value: u64,
    pub sigev_signo: i32,
    pub sigev_notify: i32,
    pub sigev_notify_thread_id: i32,
    pub _pad: [u8; 44],
}

const _: [(); 64] = [(); size_of::<PosixSigevent>()];

/// sigev_notify 常量（Linux）
pub const SIGEV_SIGNAL: i32 = 0;
pub const SIGEV_NONE: i32 = 1;
pub const SIGEV_THREAD: i32 = 2;
pub const SIGEV_THREAD_ID: i32 = 4;

#[derive(Debug, Copy, Clone)]
pub enum PosixTimerNotify {
    None,
    Signal {
        signo: Signal,
        sigval: PosixSigval,
        target_tid: RawPid,
    },
}

#[derive(Debug)]
pub struct PosixIntervalTimer {
    pub id: i32,
    pub clockid: PosixClockID,
    pub notify: PosixTimerNotify,
    pub interval: PosixTimeSpec,
    pub timer: Option<Arc<Timer>>,
    pub expire_jiffies: Option<u64>,
    pub pending_overrun_acc: i32,
    pub last_overrun: i32,
}

impl PosixIntervalTimer {
    fn is_armed(&self) -> bool {
        self.timer.is_some() && self.expire_jiffies.is_some()
    }
}

#[derive(Debug, Default)]
pub struct ProcessPosixTimers {
    next_id: i32,
    timers: HashMap<i32, PosixIntervalTimer>,
}

impl ProcessPosixTimers {
    pub fn get_timer(&self, timerid: i32) -> Result<&PosixIntervalTimer, SystemError> {
        self.timers.get(&timerid).ok_or(SystemError::EINVAL)
    }

    pub fn get_timer_mut(&mut self, timerid: i32) -> Result<&mut PosixIntervalTimer, SystemError> {
        self.timers.get_mut(&timerid).ok_or(SystemError::EINVAL)
    }

    fn alloc_id(&mut self) -> i32 {
        // Linux timer_t 在用户态通常是 int；这里用递增 id，跳过 0。
        let mut id = self.next_id;
        if id <= 0 {
            id = 1;
        }
        loop {
            if !self.timers.contains_key(&id) {
                self.next_id = id.saturating_add(1);
                return id;
            }
            id = id.saturating_add(1);
        }
    }

    pub fn create(
        &mut self,
        pcb: &Arc<ProcessControlBlock>,
        clockid: PosixClockID,
        sev: Option<PosixSigevent>,
    ) -> Result<i32, SystemError> {
        // gVisor tests only use CLOCK_MONOTONIC.
        if clockid != PosixClockID::Monotonic {
            return Err(SystemError::EINVAL);
        }

        let sev = sev.unwrap_or(PosixSigevent {
            sigev_value: 0,
            sigev_signo: Signal::SIGALRM as i32,
            sigev_notify: SIGEV_SIGNAL,
            sigev_notify_thread_id: pcb.raw_pid().data() as i32,
            _pad: [0u8; 44],
        });

        let notify = match sev.sigev_notify {
            SIGEV_NONE => PosixTimerNotify::None,
            SIGEV_SIGNAL => {
                let signo = Signal::from(sev.sigev_signo as usize);
                if !signo.is_valid() {
                    return Err(SystemError::EINVAL);
                }
                PosixTimerNotify::Signal {
                    signo,
                    sigval: PosixSigval::from_ptr(sev.sigev_value),
                    target_tid: pcb.raw_pid(),
                }
            }
            SIGEV_THREAD => {
                // 兼容 gVisor 测试：它通过 TimerCreate() 传入 SIGEV_THREAD 并期望用 signo 打断阻塞 syscall。
                // Linux 内核并不会在内核态执行用户回调；glibc/musl 通常在用户态把 SIGEV_THREAD
                // 转换为 SIGEV_THREAD_ID + sigwaitinfo 线程。
                // DragonOS 这里选择退化为“投递信号到当前线程”，以满足 FIFO/OpenInterrupted 场景。
                let signo = Signal::from(sev.sigev_signo as usize);
                if !signo.is_valid() {
                    return Err(SystemError::EINVAL);
                }
                PosixTimerNotify::Signal {
                    signo,
                    sigval: PosixSigval::from_ptr(sev.sigev_value),
                    target_tid: pcb.raw_pid(),
                }
            }
            SIGEV_THREAD_ID => {
                let tid = RawPid::new(sev.sigev_notify_thread_id as usize);
                // Linux 语义：仅允许向“同一线程组”的某个线程投递信号。
                // musl 的 SIGEV_THREAD 实现会创建新线程，并通过 SIGEV_THREAD_ID
                // 将内核 timer 信号定向到该线程，因此这里必须允许非当前 tid。
                let target = ProcessManager::find_task_by_vpid(tid)
                    // 在部分场景下 vpid 映射可能尚未就绪；退化到全局 pid 表查找。
                    .or_else(|| ProcessManager::find(tid))
                    .ok_or(SystemError::EINVAL)?;

                if target.tgid != pcb.tgid {
                    return Err(SystemError::EINVAL);
                }
                let signo = Signal::from(sev.sigev_signo as usize);
                if !signo.is_valid() {
                    return Err(SystemError::EINVAL);
                }
                PosixTimerNotify::Signal {
                    signo,
                    sigval: PosixSigval::from_ptr(sev.sigev_value),
                    target_tid: tid,
                }
            }
            _ => return Err(SystemError::EINVAL),
        };

        let id = self.alloc_id();
        self.timers.insert(
            id,
            PosixIntervalTimer {
                id,
                clockid,
                notify,
                interval: PosixTimeSpec::default(),
                timer: None,
                expire_jiffies: None,
                pending_overrun_acc: 0,
                last_overrun: 0,
            },
        );
        Ok(id)
    }

    pub fn delete(
        &mut self,
        pcb: &Arc<ProcessControlBlock>,
        timerid: i32,
    ) -> Result<(), SystemError> {
        let t = self.timers.remove(&timerid).ok_or(SystemError::EINVAL)?;
        if let Some(timer) = t.timer {
            timer.cancel();
        }
        // 删除/停用会将已排队的 SI_TIMER 的 overrun 重置为 0（与 tests 注释一致）
        if let PosixTimerNotify::Signal { signo, .. } = t.notify {
            pcb.sig_info_mut()
                .sig_pending_mut()
                .posix_timer_reset_overrun(signo, timerid);
        }
        Ok(())
    }

    pub fn gettime(&self, timerid: i32) -> Result<PosixItimerspec, SystemError> {
        let t = self.get_timer(timerid)?;
        let mut out = PosixItimerspec {
            it_interval: t.interval,
            ..Default::default()
        };
        if let Some(exp) = t.expire_jiffies {
            let now = clock();
            if exp > now {
                let remaining_j = exp - now;
                let remaining_ns: u64 = remaining_j.saturating_mul(NSEC_PER_JIFFY as u64);
                out.it_value = PosixTimeSpec {
                    tv_sec: (remaining_ns / 1_000_000_000) as i64,
                    tv_nsec: (remaining_ns % 1_000_000_000) as i64,
                }
            }
        }
        Ok(out)
    }

    pub fn getoverrun(&self, timerid: i32) -> Result<i32, SystemError> {
        let t = self.get_timer(timerid)?;
        Ok(t.last_overrun)
    }

    pub fn settime(
        &mut self,
        pcb: &Arc<ProcessControlBlock>,
        timerid: i32,
        new_value: PosixItimerspec,
    ) -> Result<PosixItimerspec, SystemError> {
        let old = self.gettime(timerid)?;
        let t = self.get_timer_mut(timerid)?;

        // 取消旧 timer
        if let Some(old_timer) = t.timer.take() {
            old_timer.cancel();
        }
        t.expire_jiffies = None;

        // timer_settime 会重置 overrun（包含已排队信号的 overrun）
        t.pending_overrun_acc = 0;
        t.last_overrun = 0;
        if let PosixTimerNotify::Signal { signo, .. } = t.notify {
            pcb.sig_info_mut()
                .sig_pending_mut()
                .posix_timer_reset_overrun(signo, timerid);
        }

        // 更新 interval
        validate_timespec(&new_value.it_interval)?;
        validate_timespec(&new_value.it_value)?;
        t.interval = new_value.it_interval;

        // it_value 为 0 => disarm
        if new_value.it_value.is_empty() {
            return Ok(old);
        }

        let delay = timespec_to_duration(&new_value.it_value)?;
        let expire_jiffies = clock() + <Jiffies as From<Duration>>::from(delay).data();

        let helper = PosixTimerHelper::new(Arc::downgrade(pcb), timerid);
        let new_timer = Timer::new(helper, expire_jiffies);
        new_timer.activate();

        t.expire_jiffies = Some(expire_jiffies);
        t.timer = Some(new_timer);
        Ok(old)
    }
}

#[derive(Debug)]
struct PosixTimerHelper {
    pcb: Weak<ProcessControlBlock>,
    timerid: i32,
}

impl PosixTimerHelper {
    fn new(pcb: Weak<ProcessControlBlock>, timerid: i32) -> Box<Self> {
        Box::new(Self { pcb, timerid })
    }
}

impl TimerFunction for PosixTimerHelper {
    fn run(&mut self) -> Result<(), SystemError> {
        let pcb = match self.pcb.upgrade() {
            Some(p) => p,
            None => return Ok(()),
        };

        // 在 softirq/timer 上下文执行：核心逻辑放在持锁区域内，避免并发打架。
        let mut timers = pcb.posix_timers_irqsave();
        let t = match timers.timers.get_mut(&self.timerid) {
            Some(t) => t,
            None => return Ok(()),
        };

        // 已经被 disarm/delete
        if !t.is_armed() {
            return Ok(());
        }

        // 周期性定时器：先重建下一次，避免 gettime() 在回调窗口看到 0（PeriodicSilent 期望仍在运行）
        let is_periodic = !t.interval.is_empty();
        if is_periodic {
            let interval = timespec_to_duration(&t.interval)?;
            let expire_jiffies = clock() + <Jiffies as From<Duration>>::from(interval).data();
            let helper = PosixTimerHelper::new(Arc::downgrade(&pcb), self.timerid);
            let next_timer = Timer::new(helper, expire_jiffies);
            next_timer.activate();
            t.expire_jiffies = Some(expire_jiffies);
            t.timer = Some(next_timer);
        } else {
            // one-shot：本次触发后应停止
            t.timer = None;
            t.expire_jiffies = None;
        }

        match t.notify {
            PosixTimerNotify::None => {
                // 无信号
            }
            PosixTimerNotify::Signal {
                signo,
                sigval,
                target_tid,
            } => {
                // 注意：DragonOS 当前 signal 发送路径对非 RT 信号的“去重检查”是看 shared_pending，
                // 但实际入队在 per-thread pending，因此这里必须基于当前线程 pending 来做合并/overrun。
                let mut siginfo_guard = pcb.sig_info_mut();
                // 计算“是否未阻塞且 handler=SIG_IGN”，必须使用已持有的 siginfo_guard，
                // 否则在持有写锁时再次调用 pcb.sig_info_irqsave() 会造成自锁死（PeriodicGroupDirectedSignal 复现）。
                let ignored_and_unblocked = {
                    let mut blocked = *siginfo_guard.sig_blocked();
                    if pcb.flags().contains(ProcessFlags::RESTORE_SIG_MASK) {
                        blocked.insert(*siginfo_guard.saved_sigmask());
                    }
                    let is_blocked = blocked.contains(signo.into_sigset());
                    if is_blocked {
                        false
                    } else {
                        pcb.sighand()
                            .handler(signo)
                            .map(|x| x.is_ignore())
                            .unwrap_or(false)
                    }
                };
                let pending = siginfo_guard.sig_pending_mut();

                // 1) 若已有该 timer 的信号：在队列项上累加 overrun
                if pending.posix_timer_exists(signo, self.timerid) {
                    let bump = 1i32.saturating_add(t.pending_overrun_acc);
                    t.pending_overrun_acc = 0;
                    pending.posix_timer_bump_overrun(signo, self.timerid, bump);
                } else {
                    // 2) 若 signo 已有其他来源的 pending（如 tgkill 提前排队）：本次无法入队，记为 overrun
                    if pending.queue().find(signo).0.is_some() {
                        t.pending_overrun_acc = t.pending_overrun_acc.saturating_add(1);
                    } else if ignored_and_unblocked {
                        // 3) 未阻塞且 handler=SIG_IGN：Linux 语义下会丢弃；tests 期望这也计入 overrun
                        t.pending_overrun_acc = t.pending_overrun_acc.saturating_add(1);
                    } else {
                        // 4) 可以入队：构造 SI_TIMER siginfo（确保只入队一次）
                        let overrun = t.pending_overrun_acc;
                        t.pending_overrun_acc = 0;
                        t.last_overrun = overrun;

                        drop(siginfo_guard); // 避免在 send_signal 路径中再次取 sig_info 锁导致死锁

                        let mut info = SigInfo::new(
                            signo,
                            0,
                            SigCode::Timer,
                            SigType::PosixTimer {
                                timerid: self.timerid,
                                overrun,
                                sigval,
                            },
                        );

                        // 当前实现：target_tid 必须属于当前进程；否则在 create 时已拒绝
                        let target = ProcessManager::find_task_by_vpid(target_tid)
                            .unwrap_or_else(|| pcb.clone());
                        let _ = signo.send_signal_info_to_pcb(Some(&mut info), target);
                    }
                }
            }
        }

        Ok(())
    }
}

fn validate_timespec(ts: &PosixTimeSpec) -> Result<(), SystemError> {
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn timespec_to_duration(ts: &PosixTimeSpec) -> Result<Duration, SystemError> {
    validate_timespec(ts)?;
    Ok(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

// 注意：不要在持有 pcb.sig_info_mut() 写锁时调用任何会再次获取 sig_info_* 锁的函数，
// 否则会造成自锁死。相关判断请在持锁区内使用已拿到的 guard 直接计算。
