use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    sched::loadavg,
};
use alloc::{borrow::ToOwned, format, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

#[derive(Debug)]
pub struct LoadavgFileOps;

impl LoadavgFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn load_int(x: u64) -> u64 {
        x >> loadavg::FSHIFT
    }

    fn load_frac(x: u64) -> u64 {
        Self::load_int((x & (loadavg::FIXED_1 - 1)).saturating_mul(100))
    }

    fn generate_loadavg_content() -> Vec<u8> {
        let loads = loadavg::get_avenrun(loadavg::FIXED_1 / 200, 0);

        let running = loadavg::nr_running();
        let total = crate::process::all_process()
            .lock_irqsave()
            .as_ref()
            .map(|m| m.len() as u32)
            .unwrap_or(0);
        let last_pid = crate::process::ProcessManager::current_pidns()
            .last_pid()
            .data() as u32;

        let mut data: Vec<u8> = Vec::new();
        data.append(
            &mut format!(
                "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
                Self::load_int(loads[0]),
                Self::load_frac(loads[0]),
                Self::load_int(loads[1]),
                Self::load_frac(loads[1]),
                Self::load_int(loads[2]),
                Self::load_frac(loads[2]),
                running,
                total,
                last_pid
            )
            .as_bytes()
            .to_owned(),
        );

        trim_string(&mut data);
        data
    }
}

impl FileOps for LoadavgFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_loadavg_content();
        proc_read(offset, len, buf, &content)
    }
}
