//! /proc/[pid]/status - 进程状态信息
//!
//! 显示进程的详细状态信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read_snapshot, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{pid::PidType, RawPid},
};
use alloc::{
    borrow::ToOwned,
    format,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/status 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatusFileOps {
    target: ProcPidTarget,
}

impl StatusFileOps {
    pub fn new(target: ProcPidTarget) -> Self {
        Self { target }
    }

    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(target), InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    /// Render the contents of `status` for the task this node points at.
    ///
    /// This mirrors Linux 6.6 `proc_pid_status()`, where the inode's `struct pid`
    /// selects the task, so `/proc/<pid>/status` and `/proc/<pid>/task/<tid>/status`
    /// share one implementation (`Tgid` still comes from the thread group). The
    /// two selections differ only while `exec` hands the group over in de_thread.
    fn generate_status_content(&self) -> Result<Vec<u8>, SystemError> {
        let pcb = self.target.task().ok_or(SystemError::ESRCH)?;
        let view_pid_ns = self.target.view_pid_ns();
        let mut pdata = Vec::new();

        let (name, user_vm, fd_table) = {
            let basic = pcb.basic();
            (
                basic.name().to_string(),
                basic.user_vm(),
                basic.try_fd_table(),
            )
        };

        let sched_info_guard = pcb.sched_info();
        let state = sched_info_guard.state();
        let cpu_id = sched_info_guard
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(-1);
        // Preserve this pre-existing DragonOS field while Linux policy and
        // effective scheduler class are represented separately.
        let priority = match sched_info_guard.sched_class() {
            crate::sched::SchedClass::Idle => "IDLE",
            _ => match sched_info_guard.policy() {
                crate::sched::LinuxSchedPolicy::Normal => "CFS",
                crate::sched::LinuxSchedPolicy::Fifo => "FIFO",
                crate::sched::LinuxSchedPolicy::Rr => "RR",
            },
        };
        let vrtime = pcb.sched_info().sched_entity.vruntime;
        let time = pcb.sched_info().sched_entity.sum_exec_runtime;
        let start_time = pcb.sched_info().sched_entity.exec_start;
        let tty = { pcb.sig_info_irqsave().tty() };

        // Name
        pdata.append(&mut format!("Name:\t{}", name).as_bytes().to_owned());

        // State
        pdata.append(&mut format!("\nState:\t{:?}", state).as_bytes().to_owned());

        // Tgid
        //
        // Mirror Linux 6.6 `task_tgid_nr_ns()` in proc_pid_status(): the group
        // id is derived from the task pinned above, never from a second PID
        // lookup. Re-resolving the link here would emit `Tgid:\t0` while `Pid`
        // still names the task, if the thread detaches it in between.
        let tgid = pcb
            .task_pid_nr_ns(PidType::TGID, Some(view_pid_ns.clone()))
            .unwrap_or(RawPid::new(0));
        pdata.append(&mut format!("\nTgid:\t{}", tgid.data()).into());

        // Pid
        pdata.append(
            &mut format!("\nPid:\t{}", self.target.vpid().data())
                .as_bytes()
                .to_owned(),
        );

        // Ppid
        pdata.append(
            &mut format!(
                "\nPpid:\t{}",
                pcb.parent_pcb()
                    .and_then(|p| p.task_pid_ptr(PidType::TGID))
                    .map(|pid| pid.pid_nr_ns(view_pid_ns).data() as isize)
                    .unwrap_or(0)
            )
            .as_bytes()
            .to_owned(),
        );

        // TracerPid (TID of the exact tracer task in the reader's PID namespace)
        let tracer_pid = crate::process::ptrace::ptracer_of(&pcb)
            .and_then(|t| t.task_pid_ptr(PidType::PID))
            .map(|pid| pid.pid_nr_ns(view_pid_ns).data() as isize)
            .unwrap_or(0);
        pdata.append(
            &mut format!("\nTracerPid:\t{}", tracer_pid)
                .as_bytes()
                .to_owned(),
        );

        // FDSize
        if matches!(state, crate::process::ProcessState::Exited(_)) {
            pdata.append(&mut format!("\nFDSize:\t{}", 0).into());
        } else {
            pdata.append(
                &mut format!(
                    "\nFDSize:\t{}",
                    fd_table
                        .map(|fd_table| fd_table.read().fd_open_count())
                        .unwrap_or(0)
                )
                .into(),
            );
        }

        // Tty
        let name = if let Some(tty) = tty {
            tty.core().name().clone()
        } else {
            "none".to_string()
        };
        pdata.append(&mut format!("\nTty:\t{}", name).as_bytes().to_owned());

        // 进程在 CPU 上的运行时间
        pdata.append(&mut format!("\nTime:\t{}", time).as_bytes().to_owned());
        // 进程开始运行的时间
        pdata.append(&mut format!("\nStime:\t{}", start_time).as_bytes().to_owned());
        // Kthread
        pdata.append(&mut format!("\nKthread:\t{}", pcb.is_kthread() as usize).into());
        pdata.append(&mut format!("\ncpu_id:\t{}", cpu_id).as_bytes().to_owned());
        pdata.append(&mut format!("\npriority:\t{}", priority).as_bytes().to_owned());

        pdata.append(&mut format!("\nvrtime:\t{}", vrtime).as_bytes().to_owned());

        if let Some(user_vm) = user_vm {
            let address_space_guard = user_vm.read();
            // todo: 当前进程运行过程中占用内存的峰值
            let hiwater_vm: u64 = 0;
            // 进程代码段的大小
            let text = (address_space_guard.end_code - address_space_guard.start_code) / 1024;
            // 进程数据段的大小
            let data = (address_space_guard.end_data - address_space_guard.start_data) / 1024;
            drop(address_space_guard);
            pdata.append(
                &mut format!("\nVmPeak:\t{} kB", hiwater_vm)
                    .as_bytes()
                    .to_owned(),
            );
            pdata.append(&mut format!("\nVmData:\t{} kB", data).as_bytes().to_owned());
            pdata.append(&mut format!("\nVmExe:\t{} kB", text).as_bytes().to_owned());
        }

        pdata.append(
            &mut format!("\nflags: {:?}\n", pcb.flags())
                .as_bytes()
                .to_owned(),
        );

        pdata.append(
            &mut format!("\nNoNewPrivs:\t{}", pcb.no_new_privs())
                .as_bytes()
                .to_owned(),
        );
        pdata.append(
            &mut format!("\nSeccomp:\t{}", pcb.seccomp_mode() as u8)
                .as_bytes()
                .to_owned(),
        );
        let seccomp_filters =
            crate::process::seccomp::SeccompFilter::chain_len(&pcb.seccomp_filter_lock());
        pdata.append(
            &mut format!("\nSeccomp_filters:\t{}", seccomp_filters)
                .as_bytes()
                .to_owned(),
        );

        // 去除多余的 \0 并在结尾添加 \0
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
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Linux serves `/proc/<pid>/status` through `single_open()`, so one fd
        // sees one record: EOF stays EOF even while `Time`/`Stime`/`vrtime`
        // keep growing, and repositioning re-renders.
        proc_read_snapshot(offset, len, buf, &mut data, || {
            self.generate_status_content()
        })
    }
}
