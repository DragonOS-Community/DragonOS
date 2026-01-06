//! /proc/[pid]/stat - 进程状态信息
//!
//! 以单行格式返回进程的状态信息，兼容 Linux procfs 格式

use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::spinlock::SpinLockGuard,
    mm::MemoryManagementArch,
    process::{pid::PidType, ProcessControlBlock, ProcessManager, ProcessState, RawPid},
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
    pid: RawPid,
}

impl StatFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
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
        ProcessState::Stopped(_) => 'T',
        ProcessState::Exited(_) => 'Z',
        _ => 'X',
    }
}

/// 清理 comm 字段，避免包含 ')' 导致解析问题
fn sanitize_comm_for_proc_stat(comm: &str) -> String {
    comm.chars()
        .map(|c| if c == ')' { '_' } else { c })
        .collect()
}

/// 生成 Linux 风格的 /proc/[pid]/stat 行
fn generate_linux_proc_stat_line(
    pid: RawPid,
    comm: &str,
    state: ProcessState,
    ppid: RawPid,
    pcb: &Arc<ProcessControlBlock>,
) -> String {
    let comm = sanitize_comm_for_proc_stat(comm);
    let state_ch = state_to_linux_char(state);

    // 尽量填真实值；拿不到的先填 0
    let pgrp: usize = 0;
    let session: usize = 0;
    let tty_nr: i32 = 0;
    let tpgid: i32 = 0;
    let flags: u64 = 0;
    let minflt: u64 = 0;
    let cminflt: u64 = 0;
    let majflt: u64 = 0;
    let cmajflt: u64 = 0;
    let utime: u64 = 0;
    let stime: u64 = 0;
    let cutime: i64 = 0;
    let cstime: i64 = 0;
    let priority: i64 = 0;
    let nice: i64 = 0;

    // 线程组中的线程数量
    let num_threads: i64 = pcb
        .task_pid_ptr(PidType::TGID)
        .map(|tgid_pid| tgid_pid.tasks_iter(PidType::TGID).count() as i64)
        .unwrap_or(1);
    let itrealvalue: i64 = 0;
    let starttime: u64 = 0;

    // vsize: bytes, rss: pages
    let (vsize_bytes, rss_pages) = pcb
        .basic()
        .user_vm()
        .map(|vm| {
            let guard = vm.read();
            let bytes = guard.vma_usage_bytes();
            let pages = (bytes.saturating_add(MMArch::PAGE_SIZE - 1)) >> MMArch::PAGE_SHIFT;
            (bytes as u64, pages as u64)
        })
        .unwrap_or((0, 0));

    format!(
        "{pid} ({comm}) {state_ch} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} \
{minflt} {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} \
{num_threads} {itrealvalue} {starttime} {vsize_bytes} {rss_pages} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid = pid.data(),
        ppid = ppid.data(),
    )
}

impl FileOps for StatFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        let comm = pcb.basic().name().to_string();
        let sched = pcb.sched_info();
        let state = sched.inner_lock_read_irqsave().state();

        let ppid = pcb
            .parent_pcb()
            .map(|p| p.raw_pid())
            .unwrap_or(RawPid::new(0));

        let content = generate_linux_proc_stat_line(self.pid, &comm, state, ppid, &pcb);
        proc_read(offset, len, buf, content.as_bytes())
    }
}
