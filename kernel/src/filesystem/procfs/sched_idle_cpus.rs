//! /proc/sched_idle_cpus - 在线 idle CPU 位图（调试用）

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    sched::idle_cpus_snapshot,
    smp::cpu::{smp_cpu_manager, smp_cpu_manager_initialized, ProcessorId},
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

#[derive(Debug)]
pub struct SchedIdleCpusFileOps;

impl SchedIdleCpusFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_content() -> Vec<u8> {
        let mask = idle_cpus_snapshot();

        if !smp_cpu_manager_initialized() {
            return b"\n".to_vec();
        }

        let max_cpu_id = smp_cpu_manager()
            .possible_cpus()
            .last()
            .or_else(|| smp_cpu_manager().present_cpus().last())
            .map(|cpu| cpu.data() as usize)
            .unwrap_or(0);
        let mut out = String::with_capacity(max_cpu_id + 2);

        // Keep bit positions aligned with logical CPU ids instead of
        // compressing to the number of present CPUs.
        for cpu in 0..=max_cpu_id {
            let cpu = ProcessorId::new(cpu as u32);
            let bit = if smp_cpu_manager().is_online_cpu(cpu) && mask.get(cpu).unwrap_or(false) {
                '1'
            } else {
                '0'
            };
            out.push(bit);
        }

        out.push('\n');
        out.into_bytes()
    }
}

impl FileOps for SchedIdleCpusFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_content();
        proc_read(offset, len, buf, &content)
    }
}
