//! /proc/[pid]/oom_score_adj - OOM killer score adjustment.

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, proc_read_snapshot},
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
pub struct OomScoreFileOps {
    target: ProcPidTarget,
}

impl OomScoreFileOps {
    pub fn new(target: ProcPidTarget) -> Self {
        Self { target }
    }

    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(target), InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn target_process(&self) -> Result<Arc<ProcessControlBlock>, SystemError> {
        self.target.thread_group_leader().ok_or(SystemError::ESRCH)
    }
}

impl FileOps for OomScoreFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // `single_open()`: one fd sees one oom_score record.
        proc_read_snapshot(offset, len, buf, &mut data, || {
            let pcb = self.target_process()?;
            let score = crate::mm::oom::proc_oom_score(&pcb);
            Ok(format!("{}\n", score).into_bytes())
        })
    }
}

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
        // Linux first truncates the write to PROC_NUMBUF - 1, then parses the
        // resulting NUL-terminated C string with kstrtoint(base = 0).
        let bytes = &buf[..buf.len().min(PROC_NUMBUF - 1)];
        let end = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        let input = core::str::from_utf8(&bytes[..end])
            .map_err(|_| SystemError::EINVAL)?
            .trim_ascii();
        let (negative, unsigned) = match input.as_bytes().first() {
            Some(b'-') => (true, &input[1..]),
            Some(b'+') => (false, &input[1..]),
            _ => (false, input),
        };
        let (digits, radix) = if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
            (&unsigned[2..], 16)
        } else if unsigned.starts_with('0') {
            (unsigned, 8)
        } else {
            (unsigned, 10)
        };
        if digits.starts_with(['+', '-']) {
            return Err(SystemError::EINVAL);
        }
        let magnitude = i64::from_str_radix(digits, radix).map_err(|err| match err.kind() {
            core::num::IntErrorKind::PosOverflow | core::num::IntErrorKind::NegOverflow => {
                SystemError::ERANGE
            }
            _ => SystemError::EINVAL,
        })?;
        let signed = if negative { -magnitude } else { magnitude };
        let score = i32::try_from(signed).map_err(|_| SystemError::ERANGE)?;

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

    fn set_score(pcb: &Arc<ProcessControlBlock>, score: i16, min_update: Option<i16>) {
        let mut sig_info = pcb.sig_info_mut();
        sig_info.set_oom_score_adj(score);
        if let Some(min) = min_update {
            sig_info.set_oom_score_adj_min(min);
        }
    }

    fn set_score_for_shared_mm(
        pcb: &Arc<ProcessControlBlock>,
        score: i16,
        min_update: Option<i16>,
    ) {
        Self::set_score(pcb, score, min_update);
        if pcb.is_active_vfork() {
            return;
        }

        let Some(mm) = pcb.basic().user_vm() else {
            return;
        };

        let target_tgid = pcb.raw_tgid();
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
            if tgid == target_tgid {
                continue;
            }
            if seen_tgids.contains(&tgid) {
                continue;
            }
            seen_tgids.push(tgid);

            if leader.raw_pid().data() == 0
                || leader.raw_pid().data() == 1
                || leader.flags().contains(ProcessFlags::KTHREAD)
                || leader.is_active_vfork()
            {
                continue;
            }
            Self::set_score(&leader, score, min_update);
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
        // Reverse guardrail: Linux serves `oom_score_adj` through `snprintf()` +
        // `simple_read_from_buffer()` (`fs/proc/base.c`), not `seq_file`, so this
        // read stays stream-style and must not be frozen per fd. Only
        // `oom_score` (above) is a `single_open()` file.
        let pcb = self.target_process()?;
        let score = pcb.sig_info_irqsave().oom_score_adj();
        let content = format!("{}\n", score);
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let score = Self::parse_score(buf)?;
        let pcb = self.target_process()?;
        let has_cap_sys_resource = capable(CAPFlags::CAP_SYS_RESOURCE);
        let _oom_score_adj_guard = ProcessManager::lock_oom_score_adj();
        let min_score = pcb.sig_info_irqsave().oom_score_adj_min();
        if score < min_score && !has_cap_sys_resource {
            return Err(SystemError::EACCES);
        }
        let min_update = has_cap_sys_resource.then_some(score);
        Self::set_score_for_shared_mm(&pcb, score, min_update);
        Ok(buf.len().min(PROC_NUMBUF - 1))
    }
}
