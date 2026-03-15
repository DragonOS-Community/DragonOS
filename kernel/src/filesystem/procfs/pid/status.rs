use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{ProcessManager, ProcessState, RawPid},
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/status 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatusFileOps {
    /// 存储 PID，在读取时动态查找进程
    pid: RawPid,
}

impl StatusFileOps {
    pub fn new(pid: RawPid) -> Self {
        Self { pid }
    }

    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(pid), InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn state_to_string(pcb: &crate::process::ProcessControlBlock, state: ProcessState) -> String {
        if pcb.is_dead() {
            return "Dead".to_string();
        }
        if pcb.is_zombie() {
            return "Zombie".to_string();
        }

        match state {
            ProcessState::Runnable => "Runnable".to_string(),
            ProcessState::Blocked(true) => "Blocked(Interruptable)".to_string(),
            ProcessState::Blocked(false) => "Blocked(Uninterruptable)".to_string(),
            ProcessState::Stopped => "Stopped".to_string(),
            ProcessState::Exited(code) => format!("Exited({code})"),
        }
    }

    fn thread_count(pcb: &Arc<crate::process::ProcessControlBlock>) -> usize {
        let leader = if pcb.is_thread_group_leader() {
            pcb.clone()
        } else {
            pcb.threads_read_irqsave()
                .group_leader()
                .unwrap_or_else(|| pcb.clone())
        };

        let ti = leader.threads_read_irqsave();
        let mut cnt = 1usize;

        for weak in ti.group_tasks_clone() {
            if let Some(task) = weak.upgrade() {
                if !task.is_dead() {
                    cnt += 1;
                }
            }
        }

        cnt
    }

    fn push_line(buf: &mut String, key: &str, value: impl core::fmt::Display) {
        buf.push_str(&format!("{key:<12}: {value}\n"));
    }

    /// 生成 status 文件内容
    fn generate_status_content(&self) -> Result<Vec<u8>, SystemError> {
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        let sched_info = pcb.sched_info();
        let state = sched_info.inner_lock_read_irqsave().state();
        let state_str = Self::state_to_string(&pcb, state);

        let tgid: usize = pcb
            .task_tgid_vnr()
            .unwrap_or(crate::process::RawPid::new(0))
            .into();

        let pid = pcb.task_pid_vnr().data();

        let ppid = pcb.parent_pcb().map(|p| p.task_pid_vnr().data()).unwrap_or(0);

        let threads = Self::thread_count(&pcb);

        let fd_size = if matches!(state, ProcessState::Exited(_)) {
            0usize
        } else {
            pcb.fd_table().read().fd_open_count()
        };

        let tty_name = if let Some(tty) = pcb.sig_info_irqsave().tty() {
            tty.core().name().clone()
        } else {
            "none".to_string()
        };

        let cpu_id = sched_info
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(-1);

        let priority = sched_info.policy();
        let vrtime = sched_info.sched_entity.vruntime;
        let exec_runtime = sched_info.sched_entity.sum_exec_runtime;
        let start_time = sched_info.sched_entity.exec_start;

        let mut vm_peak_kb = 0u64;
        let mut vm_size_kb = 0u64;
        let mut vm_data_kb = 0u64;
        let mut vm_exe_kb = 0u64;
        let vm_rss_kb = 0u64;

        if let Some(user_vm) = pcb.basic().user_vm() {
            let as_guard = user_vm.read();

            vm_size_kb = (as_guard.vma_usage_bytes() / 1024) as u64;
            vm_peak_kb = vm_size_kb;

            vm_exe_kb = ((as_guard.end_code.data().saturating_sub(as_guard.start_code.data()))
                / 1024) as u64;

            let brk_bytes = as_guard.brk.data().saturating_sub(as_guard.brk_start.data());
            let data_bytes =
                as_guard.end_data.data().saturating_sub(as_guard.start_data.data());
            vm_data_kb = (core::cmp::max(brk_bytes, data_bytes) / 1024) as u64;
        }

        let mut s = String::new();

        s.push_str("DragonOS Process Status\n");
        s.push_str("=======================\n");

        // 基础身份信息
        Self::push_line(&mut s, "Name", pcb.basic().name());
        Self::push_line(&mut s, "State", state_str);
        Self::push_line(&mut s, "Pid", pid);
        Self::push_line(&mut s, "Tgid", tgid);
        Self::push_line(&mut s, "PPid", ppid);
        Self::push_line(&mut s, "Threads", threads);
        Self::push_line(&mut s, "FDSize", fd_size);
        s.push('\n');

        // 内存相关
        Self::push_line(&mut s, "VmSize", format!("{vm_size_kb} kB"));
        Self::push_line(&mut s, "VmPeak", format!("{vm_peak_kb} kB"));
        Self::push_line(&mut s, "VmData", format!("{vm_data_kb} kB"));
        Self::push_line(&mut s, "VmExe", format!("{vm_exe_kb} kB"));
        Self::push_line(&mut s, "VmRSS", format!("{vm_rss_kb} kB"));
        s.push('\n');

        // 调度 / 运行态扩展
        Self::push_line(&mut s, "Tty", tty_name);
        Self::push_line(&mut s, "Kthread", pcb.is_kthread() as usize);
        Self::push_line(&mut s, "CpuId", cpu_id);
        Self::push_line(&mut s, "Priority", format!("{priority:?}"));
        Self::push_line(&mut s, "Preempt", pcb.preempt_count());
        Self::push_line(&mut s, "Vruntime", vrtime);
        Self::push_line(&mut s, "ExecRuntime", exec_runtime);
        Self::push_line(&mut s, "StartTime", start_time);
        Self::push_line(&mut s, "Flags", format!("{:?}", pcb.flags().clone()));

        let mut pdata = s.into_bytes();
        trim_string(&mut pdata);
        Ok(pdata)
    }
}

impl FileOps for StatusFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = self.generate_status_content()?;
        proc_read(offset, len, buf, &content)
    }
}
