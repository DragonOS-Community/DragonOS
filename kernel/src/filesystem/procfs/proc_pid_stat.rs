use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    mm::MemoryManagementArch,
    process::{pid::PidType, ProcessControlBlock, ProcessManager, ProcessState, RawPid},
};

use super::{ProcFSInode, ProcfsFilePrivateData};

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

fn sanitize_comm_for_proc_stat(comm: &str) -> String {
    // BusyBox/procps 使用 strrchr(')') 定位 comm 结束位置。
    // 若 comm 中包含 ')' 会破坏解析，先做最小替换。
    comm.chars()
        .map(|c| if c == ')' { '_' } else { c })
        .collect()
}

fn generate_linux_proc_stat_line(
    pid: RawPid,
    comm: &str,
    state: ProcessState,
    ppid: RawPid,
    pcb: &Arc<ProcessControlBlock>,
) -> String {
    // Linux /proc/[pid]/stat 字段：
    // pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    // utime stime cutime cstime priority nice num_threads itrealvalue starttime vsize rss ...
    //
    // BusyBox 1.35.0 只强依赖到 rss 字段位置，且字段顺序必须对齐。
    let comm = sanitize_comm_for_proc_stat(comm);
    let state_ch = state_to_linux_char(state);

    // 尽量填真实值；拿不到的先填 0，不影响 BusyBox 解析，只影响输出质量。
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
    // Linux 语义：进程线程组中的线程数量（包含主线程）。
    // 线程组成员由 Pid(TGID)->tasks[PidType::TGID] 维护；用它计数比本地缓存更可靠。
    let num_threads: i64 = pcb
        .task_pid_ptr(PidType::TGID)
        .map(|tgid_pid| tgid_pid.tasks_iter(PidType::TGID).count() as i64)
        .unwrap_or(1);
    let itrealvalue: i64 = 0;
    let starttime: u64 = 0;

    // vsize: bytes, rss: pages
    // 使用传入的 pcb，避免重复查找
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

    // rsslim 及后续字段给足占位，避免对方 parser 依赖更多字段时出问题
    format!(
        "{pid} ({comm}) {state_ch} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} \
{minflt} {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} \
{num_threads} {itrealvalue} {starttime} {vsize_bytes} {rss_pages} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid = pid.data(),
        ppid = ppid.data(),
    )
}

impl ProcFSInode {
    /// 生成进程 stat 信息的公共逻辑
    fn generate_pid_stat_data(pid: RawPid) -> Result<String, SystemError> {
        let pcb = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

        let comm = pcb.basic().name().to_string();
        let sched = pcb.sched_info();
        let state = sched.inner_lock_read_irqsave().state();

        let ppid = pcb
            .parent_pcb()
            .map(|p| p.raw_pid())
            .unwrap_or(RawPid::new(0));

        Ok(generate_linux_proc_stat_line(pid, &comm, state, ppid, &pcb))
    }

    /// /proc/<pid>/stat
    #[inline(never)]
    pub(super) fn open_pid_stat(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        let pid = self.fdata.pid.ok_or(SystemError::EINVAL)?;

        let s = Self::generate_pid_stat_data(pid)?;
        pdata.data = s.into_bytes();
        Ok(pdata.data.len() as i64)
    }

    /// /proc/<pid>/task/<tid>/stat（最小实现：先按进程视图输出）
    #[inline(never)]
    pub(super) fn open_pid_task_tid_stat(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        let pid = self.fdata.pid.ok_or(SystemError::EINVAL)?;
        // 目前内核线程/用户线程还没有独立的 tid 视图，这里先占位：tid 仅用于路径匹配。
        let _tid = self.fdata.tid.ok_or(SystemError::EINVAL)?;

        let s = Self::generate_pid_stat_data(pid)?;
        pdata.data = s.into_bytes();
        Ok(pdata.data.len() as i64)
    }
}
