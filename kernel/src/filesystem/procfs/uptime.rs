//! /proc/uptime - system uptime and idle time
//!
//! Linux format: two space-separated values, each with two decimal places:
//!   <uptime seconds> <total idle seconds across all CPUs>

use crate::filesystem::procfs::template::configure_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_snapshot_operation,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    sched::cputime::{kcpustat_cpu, CpuUsageStat},
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::timekeeping::boottime_now,
};
use alloc::{
    format,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use system_error::SystemError;

#[derive(Debug)]
pub struct UptimeFileOps;

impl UptimeFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_uptime_content() -> Vec<u8> {
        let uptime = boottime_now();
        let uptime_secs = uptime.tv_sec;
        let uptime_centis = uptime.tv_nsec / 10_000_000;

        // Linux reports total idle time summed over all present CPUs.
        let present_cpus = smp_cpu_manager()
            .present_cpus()
            .iter_cpu()
            .collect::<Vec<_>>();
        let cpu_ids = if present_cpus.is_empty() {
            vec![ProcessorId::new(0)]
        } else {
            present_cpus
        };

        let mut idle_ns: u64 = 0;
        for cpu_id in &cpu_ids {
            idle_ns = idle_ns
                .saturating_add(kcpustat_cpu(*cpu_id).snapshot()[CpuUsageStat::Idle as usize]);
        }
        let idle_secs = idle_ns / 1_000_000_000;
        let idle_centis = (idle_ns % 1_000_000_000) * 100 / 1_000_000_000;

        // Linux procfs files are plain text and do not append a NUL
        // terminator. gopsutil (used by sysinfo-collector) treats a trailing
        // NUL as an extra line and rejects /proc/uptime with
        // "wrong uptime format", so emit exactly one newline-terminated line.
        format!(
            "{}.{:02} {}.{:02}\n",
            uptime_secs, uptime_centis, idle_secs, idle_centis
        )
        .into_bytes()
    }
}

impl FileOps for UptimeFileOps {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        configure_seq_file(behavior);
    }

    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Seq;

    fn seq_operation(
        &self,
        operation: crate::filesystem::procfs::utils::ProcfsSeqOperation<'_>,
        data: &mut MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        proc_snapshot_operation(operation, data, || Ok(Self::generate_uptime_content()))
    }
}
