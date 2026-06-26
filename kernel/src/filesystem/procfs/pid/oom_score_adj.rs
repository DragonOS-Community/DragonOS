//! /proc/[pid]/oom_score_adj - OOM killer score adjustment.

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{
        cred::{capable, CAPFlags},
        ProcessControlBlock, ProcessFlags, ProcessManager,
    },
};
use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

const OOM_SCORE_ADJ_MIN: i16 = -1000;
const OOM_SCORE_ADJ_MAX: i16 = 1000;
const PROC_NUMBUF: usize = 13;

#[derive(Debug)]
pub struct OomScoreAdjFileOps {
    target: ProcPidTarget,
}

impl OomScoreAdjFileOps {
    pub fn new(target: ProcPidTarget) -> Self {
        Self { target }
    }

    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(target), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    fn target_process(&self) -> Result<Arc<ProcessControlBlock>, SystemError> {
        self.target.thread_group_leader().ok_or(SystemError::ESRCH)
    }

    fn parse_score(buf: &[u8]) -> Result<i16, SystemError> {
        let len = buf.len().min(PROC_NUMBUF - 1);
        let input = core::str::from_utf8(&buf[..len]).map_err(|_| SystemError::EINVAL)?;
        let score = input
            .trim()
            .parse::<i32>()
            .map_err(|_| SystemError::EINVAL)?;

        if !(OOM_SCORE_ADJ_MIN as i32..=OOM_SCORE_ADJ_MAX as i32).contains(&score) {
            return Err(SystemError::EINVAL);
        }

        Ok(score as i16)
    }

    fn task_shares_mm(
        task: &Arc<ProcessControlBlock>,
        mm: &Arc<crate::mm::ucontext::AddressSpace>,
    ) -> bool {
        task.basic()
            .user_vm()
            .is_some_and(|task_mm| task_mm.id() == mm.id() || Arc::ptr_eq(&task_mm, mm))
    }

    fn set_score_for_shared_mm(pcb: &Arc<ProcessControlBlock>, score: i16) {
        let Some(mm) = pcb.basic().user_vm() else {
            pcb.sig_info_mut().set_oom_score_adj(score);
            return;
        };

        let mut seen_tgids = alloc::vec::Vec::new();
        for pid in ProcessManager::get_all_processes() {
            let Some(task) = ProcessManager::find(pid) else {
                continue;
            };
            if !Self::task_shares_mm(&task, &mm) {
                continue;
            }

            let leader = ProcessManager::find(task.raw_tgid()).unwrap_or(task);
            let tgid = leader.raw_tgid();
            if seen_tgids.contains(&tgid) {
                continue;
            }
            seen_tgids.push(tgid);

            if leader.raw_pid().data() == 0
                || leader.raw_pid().data() == 1
                || leader.flags().contains(ProcessFlags::KTHREAD)
            {
                continue;
            }
            leader.sig_info_mut().set_oom_score_adj(score);
        }
    }
}

impl FileOps for OomScoreAdjFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = self.target_process()?;
        let score = pcb.sig_info_irqsave().oom_score_adj();
        let content = format!("{}\n", score);
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if offset != 0 {
            return Err(SystemError::EINVAL);
        }

        let score = Self::parse_score(buf)?;
        let pcb = self.target_process()?;
        if score < 0 && !capable(CAPFlags::CAP_SYS_RESOURCE) {
            return Err(SystemError::EACCES);
        }
        Self::set_score_for_shared_mm(&pcb, score);
        Ok(buf.len())
    }
}
