//! /proc/[pid]/stat - 进程状态信息
//!
//! 以单行格式返回进程的状态信息，兼容 Linux procfs 格式

use core::sync::atomic::Ordering;

use crate::libs::mutex::MutexGuard;
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::MemoryManagementArch,
    process::{pid::PidType, ProcessState, RawPid},
    sched::{cputime::ns_to_clock_t, prio::PrioUtil},
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/[pid]/stat 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatFileOps {
    target: ProcPidTarget,
}

impl StatFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
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
    processor: i32,
    utime: u64,
    stime: u64,
}

/// 生成 Linux 风格的 /proc/[pid]/stat 行
fn generate_linux_proc_stat_line(snapshot: ProcStatSnapshot) -> String {
    let comm = sanitize_comm_for_proc_stat(&snapshot.comm);
    let state_ch = state_to_linux_char(snapshot.state);

    // 尽量填真实值；拿不到的先填 0
    let pgrp: usize = 0;
    let session: usize = 0;
    let tpgid: i32 = 0;
    let flags: u64 = 0;
    let minflt: u64 = 0;
    let cminflt: u64 = 0;
    let majflt: u64 = 0;
    let cmajflt: u64 = 0;

    let cutime: i64 = 0;
    let cstime: i64 = 0;

    let itrealvalue: i64 = 0;

    // starttime: 进程启动时间（暂时为 0，需要 PCB 添加 start_time 字段）
    let starttime: u64 = 0;

    format!(
        "{pid} ({comm}) {state_ch} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} \
{minflt} {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} \
{num_threads} {itrealvalue} {starttime} {vsize_bytes} {rss_pages} 0 0 0 0 0 0 0 0 0 0 0 0 0 {processor} 0 0 0 0 0\n",
        pid = snapshot.pid.data(),
        ppid = snapshot.ppid.data(),
        tty_nr = snapshot.tty_nr,
        utime = snapshot.utime,
        stime = snapshot.stime,
        priority = snapshot.priority,
        nice = snapshot.nice,
        num_threads = snapshot.num_threads,
        vsize_bytes = snapshot.vsize_bytes,
        rss_pages = snapshot.rss_pages,
        processor = snapshot.processor,
    )
}

impl FileOps for StatFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
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
        let priority = pcb.sched_info().prio() as i64;
        let nice = PrioUtil::prio_to_nice(pcb.sched_info().static_prio()) as i64;
        let num_threads = pcb
            .task_pid_ptr(PidType::TGID)
            .map(|tgid_pid| tgid_pid.tasks_iter(PidType::TGID).count() as i64)
            .unwrap_or(1);
        let (vsize_bytes, rss_pages) = user_vm
            .map(|vm| {
                let guard = vm.read_guard_no_reservations();
                let bytes = guard.vma_usage_bytes();
                let pages = (bytes.saturating_add(MMArch::PAGE_SIZE - 1)) >> MMArch::PAGE_SHIFT;
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

        let content = generate_linux_proc_stat_line(ProcStatSnapshot {
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
            processor,
            utime,
            stime,
        });
        proc_read(offset, len, buf, content.as_bytes())
    }
}
