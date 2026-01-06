//! /proc/[pid]/status - 进程状态信息
//!
//! 显示进程的详细状态信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{ProcessManager, RawPid},
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

    /// 生成 status 文件内容
    fn generate_status_content(&self) -> Result<Vec<u8>, SystemError> {
        // 动态查找进程，确保获取最新状态
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;
        let mut pdata = Vec::new();

        // Name
        pdata.append(
            &mut format!("Name:\t{}", pcb.basic().name())
                .as_bytes()
                .to_owned(),
        );

        let sched_info_guard = pcb.sched_info();
        let state = sched_info_guard.inner_lock_read_irqsave().state();
        let cpu_id = sched_info_guard
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(-1);

        let priority = sched_info_guard.policy();
        let vrtime = sched_info_guard.sched_entity.vruntime;
        let time = sched_info_guard.sched_entity.sum_exec_runtime;
        let start_time = sched_info_guard.sched_entity.exec_start;

        // State
        pdata.append(&mut format!("\nState:\t{:?}", state).as_bytes().to_owned());

        // Tgid
        pdata.append(
            &mut format!(
                "\nTgid:\t{}",
                pcb.task_tgid_vnr()
                    .unwrap_or(crate::process::RawPid::new(0))
                    .into()
            )
            .into(),
        );

        // Pid
        pdata.append(
            &mut format!("\nPid:\t{}", pcb.task_pid_vnr().data())
                .as_bytes()
                .to_owned(),
        );

        // Ppid
        pdata.append(
            &mut format!(
                "\nPpid:\t{}",
                pcb.parent_pcb()
                    .map(|p| p.task_pid_vnr().data() as isize)
                    .unwrap_or(-1)
            )
            .as_bytes()
            .to_owned(),
        );

        // FDSize
        if matches!(state, crate::process::ProcessState::Exited(_)) {
            pdata.append(&mut format!("\nFDSize:\t{}", 0).into());
        } else {
            pdata.append(
                &mut format!("\nFDSize:\t{}", pcb.fd_table().read().fd_open_count()).into(),
            );
        }

        // Tty
        let name = if let Some(tty) = pcb.sig_info_irqsave().tty() {
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

        if let Some(user_vm) = pcb.basic().user_vm() {
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
