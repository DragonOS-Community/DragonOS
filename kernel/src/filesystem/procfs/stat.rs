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
    sched::cputime::{kcpustat_cpu, ns_to_clock_t, CpuUsageStat, NR_CPU_STATS},
    smp::cpu::{smp_cpu_manager, ProcessorId},
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

        // 获取 CPU 数量
        let cpu_count = smp_cpu_manager().present_cpus_count() as usize;
        let cpu_count = if cpu_count == 0 { 1 } else { cpu_count };

        // 汇总所有 CPU 的统计
        let mut total_stats = [0u64; NR_CPU_STATS];
        for cpu_id in 0..cpu_count {
            let stat = kcpustat_cpu(ProcessorId::new(cpu_id as u32));
            let snapshot = stat.snapshot();
            for i in 0..NR_CPU_STATS {
                total_stats[i] += snapshot[i];
            }
        }

        // 输出总 CPU 行（8 个字段：user nice system idle iowait irq softirq steal）
        data.append(
            &mut format!(
                "cpu {} {} {} {} {} {} {} {}\n",
                ns_to_clock_t(total_stats[CpuUsageStat::User as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Nice as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::System as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Idle as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::IoWait as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Irq as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Softirq as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Steal as usize]),
            )
            .as_bytes()
            .to_owned(),
        );

        // 输出每个 CPU 的统计行
        for cpu_id in 0..cpu_count {
            let stat = kcpustat_cpu(ProcessorId::new(cpu_id as u32));
            let snapshot = stat.snapshot();
            data.append(
                &mut format!(
                    "cpu{} {} {} {} {} {} {} {} {}\n",
                    cpu_id,
                    ns_to_clock_t(snapshot[CpuUsageStat::User as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Nice as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::System as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Idle as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::IoWait as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Irq as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Softirq as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Steal as usize]),
                )
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
