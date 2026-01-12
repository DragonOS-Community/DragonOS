//! /proc/stat - 系统统计信息

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::{pid::PidType, ProcessManager, ProcessState},
    smp::cpu::smp_cpu_manager,
};
use alloc::{borrow::ToOwned, format, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

/// /proc/stat 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatFileOps;

impl StatFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_stat_content() -> Vec<u8> {
        let mut data: Vec<u8> = Vec::new();
        let cpu_fields = "0 0 0 0 0 0 0 0 0 0";

        data.append(&mut format!("cpu {}\n", cpu_fields).as_bytes().to_owned());

        let mut cpu_count = smp_cpu_manager().present_cpus_count() as usize;
        if cpu_count == 0 {
            cpu_count = 1;
        }
        for cpu_id in 0..cpu_count {
            data.append(
                &mut format!("cpu{} {}\n", cpu_id, cpu_fields)
                    .as_bytes()
                    .to_owned(),
            );
        }

        data.append(&mut b"intr 0\n".to_vec());
        data.append(&mut b"ctxt 0\n".to_vec());
        data.append(&mut b"btime 0\n".to_vec());

        let pidns = ProcessManager::current_pcb().active_pid_ns();
        let processes = pidns.processes_created();
        let pids = pidns.collect_pids();

        let mut procs_running = 0u64;
        let mut procs_blocked = 0u64;
        for pid in pids {
            if let Some(pcb) = pid.pid_task(PidType::PID) {
                let state = pcb.sched_info().inner_lock_read_irqsave().state();
                if state.is_runnable() {
                    procs_running += 1;
                } else if matches!(state, ProcessState::Blocked(false)) {
                    procs_blocked += 1;
                }
            }
        }

        data.append(&mut format!("processes {}\n", processes).as_bytes().to_owned());
        data.append(
            &mut format!("procs_running {}\n", procs_running)
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("procs_blocked {}\n", procs_blocked)
                .as_bytes()
                .to_owned(),
        );

        data.append(&mut b"softirq 0\n".to_vec());

        trim_string(&mut data);
        data
    }
}

impl FileOps for StatFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_stat_content();
        proc_read(offset, len, buf, &content)
    }
}
