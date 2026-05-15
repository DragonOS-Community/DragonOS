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
    process::{nr_context_switches, total_forks},
    sched::{
        cputime::{kcpustat_cpu, ns_to_clock_t, CpuUsageStat, NR_CPU_STATS},
        loadavg, nr_iowait,
    },
    smp::cpu::smp_cpu_manager,
    time::timekeeping::boottime_seconds,
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

        let present_cpus = smp_cpu_manager()
            .present_cpus()
            .iter_cpu()
            .collect::<Vec<_>>();
        let cpu_ids = if present_cpus.is_empty() {
            vec![crate::smp::cpu::ProcessorId::new(0)]
        } else {
            present_cpus
        };

        let mut total_stats = [0u64; NR_CPU_STATS];
        for cpu_id in &cpu_ids {
            let snapshot = kcpustat_cpu(*cpu_id).snapshot();
            for i in 0..NR_CPU_STATS {
                total_stats[i] += snapshot[i];
            }
        }

        data.append(
            &mut format!(
                "cpu {} {} {} {} {} {} {} {} {} {}\n",
                ns_to_clock_t(total_stats[CpuUsageStat::User as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Nice as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::System as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Idle as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::IoWait as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Irq as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Softirq as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Steal as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::Guest as usize]),
                ns_to_clock_t(total_stats[CpuUsageStat::GuestNice as usize]),
            )
            .as_bytes()
            .to_owned(),
        );

        for cpu_id in &cpu_ids {
            let snapshot = kcpustat_cpu(*cpu_id).snapshot();
            data.append(
                &mut format!(
                    "cpu{} {} {} {} {} {} {} {} {} {} {}\n",
                    cpu_id.data(),
                    ns_to_clock_t(snapshot[CpuUsageStat::User as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Nice as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::System as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Idle as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::IoWait as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Irq as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Softirq as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Steal as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::Guest as usize]),
                    ns_to_clock_t(snapshot[CpuUsageStat::GuestNice as usize]),
                )
                .as_bytes()
                .to_owned(),
            );
        }

        let intr_total = 0u64;
        data.append(&mut format!("intr {}\n", intr_total).as_bytes().to_owned());
        data.append(
            &mut format!("ctxt {}\n", nr_context_switches())
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("btime {}\n", boottime_seconds())
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("processes {}\n", total_forks())
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("procs_running {}\n", loadavg::nr_running())
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("procs_blocked {}\n", nr_iowait())
                .as_bytes()
                .to_owned(),
        );

        let softirq_total = 0u64;
        data.append(&mut format!("softirq {}\n", softirq_total).as_bytes().to_owned());

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
