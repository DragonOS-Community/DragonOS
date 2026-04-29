//! /proc/[pid]/status - 进程状态信息
//!
//! 显示进程的详细状态信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::pid::PidType,
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

    /// 生成 status 文件内容
    fn generate_status_content(&self) -> Result<Vec<u8>, SystemError> {
        let pcb = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
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

        let state = {
            let sched_info = pcb.sched_info();
            sched_info.inner_lock_read_irqsave().state()
        };
        let cpu_id = pcb
            .sched_info()
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(-1);
        let priority = pcb.sched_info().policy();
        let vrtime = pcb.sched_info().sched_entity.vruntime;
        let time = pcb.sched_info().sched_entity.sum_exec_runtime;
        let start_time = pcb.sched_info().sched_entity.exec_start;
        let tty = { pcb.sig_info_irqsave().tty() };

        // Name
        pdata.append(&mut format!("Name:\t{}", name).as_bytes().to_owned());

        // State
        pdata.append(&mut format!("\nState:\t{:?}", state).as_bytes().to_owned());

        // Tgid
        pdata.append(&mut format!("\nTgid:\t{}", self.target.tgid().data()).into());

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
        pdata.append(&mut format!("\npriority:\t{:?}", priority).as_bytes().to_owned());
        pdata.append(
            &mut format!("\npreempt:\t{}", pcb.preempt_count())
                .as_bytes()
                .to_owned(),
        );

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
            &mut format!("\nflags: {:?}\n", pcb.flags().clone())
                .as_bytes()
                .to_owned(),
        );

        pdata.append(
            &mut format!("\nSeccomp:\t{}", pcb.seccomp_mode() as u8)
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
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = self.generate_status_content()?;
        // log::info!("Generated /proc/[pid]/status content");

        proc_read(offset, len, buf, &content)
    }
}
