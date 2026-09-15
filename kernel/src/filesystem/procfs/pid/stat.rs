//! /proc/[pid]/stat - 进程状态信息
//!
//! 以单行格式返回进程的状态信息，兼容 Linux procfs 格式

use core::fmt::Write;
use core::sync::atomic::Ordering;

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_snapshot,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{pid::PidType, resource::RUsageWho, ProcessState, RawPid},
    sched::{cputime::ns_to_clock_t, prio::PrioUtil},
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/stat 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatFileOps {
    target: ProcPidTarget,
    scope: StatScope,
}

/// Which resource-usage accounting view `/proc/<pid>/stat` reports.
///
/// This only selects between process-wide and per-thread statistics; which task
/// is rendered is always `target.task()`.
#[derive(Clone, Copy, Debug)]
pub enum StatScope {
    ThreadGroup,
    Thread,
}

impl StatFileOps {
    pub fn new_inode(
        target: ProcPidTarget,
        scope: StatScope,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target, scope }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

/// 将进程状态转换为 Linux 风格的字符
fn state_to_linux_char(state: ProcessState) -> char {
    match state {
        ProcessState::Runnable => 'R',
        ProcessState::Blocked(interruptable) => {
            if interruptable {
                'S'
            } else {
                'D'
            }
        }
        ProcessState::Stopped => 'T',
        ProcessState::Exited(_) => 'Z',
    }
}

/// 清理 comm 字段，避免包含 ')' 导致解析问题
fn sanitize_comm_for_proc_stat(comm: &str) -> String {
    comm.chars()
        .map(|c| if c == ')' { '_' } else { c })
        .collect()
}

struct ProcStatSnapshot {
    pid: RawPid,
    comm: String,
    state: ProcessState,
    ppid: RawPid,
    tty_nr: i32,
    priority: i64,
    nice: i64,
    num_threads: i64,
    vsize_bytes: u64,
    rss_pages: u64,
    /// Field 38: Linux `task_struct::exit_signal`.
    exit_signal: i32,
    /// Field 39: Linux `task_cpu(task)`, i.e. the runqueue CPU the task was
    /// last placed on.
    processor: i32,
    /// Field 40: Linux `task_struct::rt_priority` (1..=99, 0 when not realtime).
    rt_priority: u32,
    /// Field 41: Linux `task_struct::policy`.
    policy: u32,
    utime: u64,
    stime: u64,
    minflt: usize,
    cminflt: usize,
    majflt: usize,
    cmajflt: usize,
}

/// Writer for the single-line `/proc/[pid]/stat` format.
///
/// Linux writes one field per `seq_put_decimal_*` call in
/// `fs/proc/array.c::do_task_stat()`; keeping the same shape here makes the
/// position of fields 1..52 visible in the source and directly comparable with
/// Linux, instead of relying on counting the spaces of one long format string.
struct StatLine {
    buf: String,
}

impl StatLine {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Fields 1 and 2: `pid (comm)`
    fn push_pid_comm(&mut self, pid: RawPid, comm: &str) {
        // Writing into a String cannot fail, so the fmt::Result is discarded.
        let _ = write!(self.buf, "{} ({})", pid.data(), comm);
    }

    /// Field 3: the single-character process state
    fn push_state(&mut self, state: char) {
        self.buf.push(' ');
        self.buf.push(state);
    }

    /// Writes a signed field, matching Linux `seq_put_decimal_ll()`.
    fn push_ll(&mut self, value: i64) {
        let _ = write!(self.buf, " {}", value);
    }

    /// Writes an unsigned field, matching Linux `seq_put_decimal_ull()`.
    fn push_ull(&mut self, value: u64) {
        let _ = write!(self.buf, " {}", value);
    }

    /// Appends the newline and yields the finished line
    fn finish(mut self) -> String {
        self.buf.push('\n');
        self.buf
    }
}

/// Renders the `/proc/[pid]/stat` line (fields 1..52).
///
/// The field order matches Linux `fs/proc/array.c::do_task_stat()` one to one;
/// every field gets its own statement with its field number in the trailing
/// comment. Signed fields go through `push_ll` and unsigned ones through
/// `push_ull`, mirroring Linux `seq_put_decimal_ll/ull`.
///
/// Placeholder convention: fields Linux itself always emits as 0 are written as
/// 0, and fields whose accounting DragonOS does not implement yet are written
/// as 0 placeholders with the reason given per source group. A placeholder
/// never moves the position of any later field.
fn generate_linux_proc_stat_line(snapshot: &ProcStatSnapshot) -> String {
    let comm = sanitize_comm_for_proc_stat(&snapshot.comm);
    let mut line = StatLine::new();

    line.push_pid_comm(snapshot.pid, &comm); // 1 pid, 2 comm
    line.push_state(state_to_linux_char(snapshot.state)); // 3 state
    line.push_ll(snapshot.ppid.data() as i64); // 4 ppid
    line.push_ll(0); // 5 pgrp
    line.push_ll(0); // 6 session
    line.push_ll(snapshot.tty_nr as i64); // 7 tty_nr
    line.push_ll(0); // 8 tpgid
    line.push_ull(0); // 9 flags
    line.push_ull(snapshot.minflt as u64); // 10 minflt
    line.push_ull(snapshot.cminflt as u64); // 11 cminflt
    line.push_ull(snapshot.majflt as u64); // 12 majflt
    line.push_ull(snapshot.cmajflt as u64); // 13 cmajflt

    // 14/15: CPU time of this task. For `whole=1` (i.e. `/proc/<pid>/stat`)
    // Linux aggregates the whole thread group; DragonOS does not implement that
    // aggregation yet, which is a pre-existing deviation.
    line.push_ull(snapshot.utime); // 14 utime
    line.push_ull(snapshot.stime); // 15 stime
    line.push_ll(0); // 16 cutime
    line.push_ll(0); // 17 cstime

    // Field 18 is Linux `task_prio()` and field 19 is the nice value itself: a
    // fair task at nice 0 reports 20 and 0, which is the PRI/NI pair `ps`
    // prints. Both fields are signed.
    line.push_ll(snapshot.priority); // 18 priority
    line.push_ll(snapshot.nice); // 19 nice
    line.push_ll(snapshot.num_threads); // 20 num_threads
    line.push_ull(0); // 21 itrealvalue (Linux always emits 0)
    line.push_ull(0); // 22 starttime
    line.push_ull(snapshot.vsize_bytes); // 23 vsize
    line.push_ull(snapshot.rss_pages); // 24 rss

    // 25..37 are grouped below; 38..41 are the scheduling fields this fixes.
    line.push_ull(0); // 25 rsslim

    // 26..28: text and stack boundaries.
    line.push_ull(0); // 26 startcode
    line.push_ull(0); // 27 endcode
    line.push_ull(0); // 28 startstack
    line.push_ull(0); // 29 kstkesp (Linux emits 0 outside core dumps)
    line.push_ull(0); // 30 kstkeip (Linux emits 0 outside core dumps)

    // 31..34: signal mask and disposition counters.
    line.push_ull(0); // 31 signal
    line.push_ull(0); // 32 blocked
    line.push_ull(0); // 33 sigignore
    line.push_ull(0); // 34 sigcatch
    line.push_ull(0); // 35 wchan
    line.push_ull(0); // 36 nswap (Linux always emits 0)
    line.push_ull(0); // 37 cnswap (Linux always emits 0)
    line.push_ll(snapshot.exit_signal as i64); // 38 exit_signal
    line.push_ll(snapshot.processor as i64); // 39 processor (task_cpu)
    line.push_ull(snapshot.rt_priority as u64); // 40 rt_priority
    line.push_ull(snapshot.policy as u64); // 41 policy
    line.push_ull(0); // 42 delayacct_blkio_ticks (0 without delay accounting)

    // 43..52: guest time, mm boundaries and the exit code.
    line.push_ull(0); // 43 guest_time
    line.push_ll(0); // 44 cguest_time
    line.push_ull(0); // 45 start_data
    line.push_ull(0); // 46 end_data
    line.push_ull(0); // 47 start_brk
    line.push_ull(0); // 48 arg_start
    line.push_ull(0); // 49 arg_end
    line.push_ull(0); // 50 env_start
    line.push_ull(0); // 51 env_end
    line.push_ll(0); // 52 exit_code

    line.finish()
}

impl StatFileOps {
    /// Render the single line reported by this file.
    ///
    /// Linux serves `/proc/<pid>/stat` through `single_open()`, so the line is
    /// produced when the file is read and then frozen for that fd.
    fn generate_content(&self) -> Result<Vec<u8>, SystemError> {
        let pcb = self.target.task().ok_or(SystemError::ESRCH)?;

        let (comm, user_vm) = {
            let basic = pcb.basic();
            (basic.name().to_string(), basic.user_vm())
        };
        let state = pcb.sched_info().state();
        let tty_nr = {
            pcb.sig_info_irqsave()
                .tty()
                .map(|tty| tty.core().device_number().new_encode_dev() as i32)
                .unwrap_or(0)
        };
        let cpu_time = pcb.cputime();
        let utime = ns_to_clock_t(cpu_time.utime.load(Ordering::Relaxed));
        let stime = ns_to_clock_t(cpu_time.stime.load(Ordering::Relaxed));
        let fault_usage = match self.scope {
            StatScope::ThreadGroup => pcb.get_rusage(RUsageWho::RUsageSelf),
            StatScope::Thread => pcb.get_rusage(RUsageWho::RusageThread),
        }
        .unwrap_or_default();
        let child_usage = pcb
            .get_rusage(RUsageWho::RUsageChildren)
            .unwrap_or_default();
        // Field 18 is Linux's `task_prio()`, the dynamic priority offset into
        // the realtime range, and field 19 is the nice value itself. They are
        // two different numbers: a fair task at nice 0 reports `20` and `0`,
        // which is what `ps` and `top` print as `PRI` and `NI`.
        let priority = PrioUtil::task_prio(pcb.sched_info().prio()) as i64;
        let nice = PrioUtil::prio_to_nice(pcb.sched_info().static_prio()) as i64;
        // Fields 38..41 are per-task scheduling state: the exit signal the task
        // was cloned with, the runqueue CPU it last ran on, its user visible
        // realtime priority and its policy. Without them `ps`, `top` and
        // `chrt` cannot tell a FIFO task from a fair one.
        let exit_signal = pcb.exit_signal();
        let rt_priority = pcb.sched_info().rt_priority();
        let policy = pcb.sched_info().policy().to_u8() as u32;
        let num_threads = pcb
            .task_pid_ptr(PidType::TGID)
            .map(|tgid_pid| tgid_pid.tasks_iter(PidType::TGID).count() as i64)
            .unwrap_or(1);
        let (vsize_bytes, rss_pages) = user_vm
            .map(|vm| {
                let guard = vm.read_guard_no_reservations();
                let bytes = guard.vma_usage_bytes();
                let pages = vm.resident_pages();
                (bytes as u64, pages as u64)
            })
            .unwrap_or((0, 0));
        let processor = pcb
            .sched_info()
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(0);

        let ppid = pcb
            .parent_pcb()
            .and_then(|p| p.task_pid_ptr(PidType::TGID))
            .map(|pid| pid.pid_nr_ns(self.target.view_pid_ns()))
            .unwrap_or(RawPid::new(0));

        let content = generate_linux_proc_stat_line(&ProcStatSnapshot {
            pid: self.target.vpid(),
            comm,
            state,
            ppid,
            tty_nr,
            priority,
            nice,
            num_threads,
            vsize_bytes,
            rss_pages,
            exit_signal,
            processor,
            rt_priority,
            policy,
            utime,
            stime,
            minflt: fault_usage.ru_minflt,
            cminflt: child_usage.ru_minflt,
            majflt: fault_usage.ru_majflt,
            cmajflt: child_usage.ru_majflt,
        });
        Ok(content.into_bytes())
    }
}

impl FileOps for StatFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        proc_read_snapshot(offset, len, buf, &mut data, || self.generate_content())
    }
}
