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
    process::ProcessControlBlock,
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
        pcb.sig_info_mut().set_oom_score_adj(score);
        Ok(buf.len())
    }
}
