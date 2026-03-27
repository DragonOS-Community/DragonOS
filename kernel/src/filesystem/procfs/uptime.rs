use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    sched::cputime::{kcpustat_cpu, CpuUsageStat},
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::uptime_secs,
};
use alloc::{format, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

/// /proc/uptime 文件的 FileOps 实现
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
        let up_secs = uptime_secs();

        let cpu_count = smp_cpu_manager().present_cpus_count() as usize;
        let cpu_count = if cpu_count == 0 { 1 } else { cpu_count };

        let mut idle_ns_total = 0u64;
        for cpu_id in 0..cpu_count {
            let stat = kcpustat_cpu(ProcessorId::new(cpu_id as u32));
            let snapshot = stat.snapshot();
            idle_ns_total = idle_ns_total.saturating_add(snapshot[CpuUsageStat::Idle as usize]);
        }

        let idle_secs = idle_ns_total / 1_000_000_000;
        let idle_hundredths = (idle_ns_total % 1_000_000_000) / 10_000_000;

        let mut data = format!("{up_secs}.00 {idle_secs}.{idle_hundredths:02}\n").into_bytes();
        trim_string(&mut data);
        data
    }
}

impl FileOps for UptimeFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_uptime_content();
        proc_read(offset, len, buf, &content)
    }
}