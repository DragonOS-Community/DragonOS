//! Memory control-file admission and resource-lock-free actions.
//!
//! The caller validates permissions and the file generation under UPDATE.
//! An action pins that admitted CSS; disabling/re-enabling a controller cannot
//! redirect an in-flight write to a different generation.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    cgroup::{
        memory::{abi, MemoryAccount},
        CgroupNode,
    },
    mm::{allocator::page_frame::PageFrameCount, page::PageReclaimer},
    process::ProcessManager,
};

use super::files::CgroupCoreFile;

pub(super) enum MemoryWrite {
    Published,
    High(Arc<MemoryAccount>),
    Max(Arc<MemoryAccount>),
    Reclaim {
        account: Arc<MemoryAccount>,
        pages: u64,
    },
}

pub(super) fn is_memory_write(ty: CgroupCoreFile) -> bool {
    matches!(
        ty,
        CgroupCoreFile::MemoryMin
            | CgroupCoreFile::MemoryLow
            | CgroupCoreFile::MemoryHigh
            | CgroupCoreFile::MemoryMax
            | CgroupCoreFile::MemoryOomGroup
            | CgroupCoreFile::MemoryReclaim
    )
}

/// This does not allocate output, reclaim, wait or select an OOM victim.
pub(super) fn prepare_write_locked(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    input: &[u8],
) -> Result<MemoryWrite, SystemError> {
    let input = core::str::from_utf8(input).map_err(|_| SystemError::EINVAL)?;
    let account = cgroup
        .local_memory_generation()
        .ok_or(SystemError::ENODEV)?
        .account;
    if ty == CgroupCoreFile::MemoryReclaim {
        return Ok(MemoryWrite::Reclaim {
            account,
            pages: abi::parse_reclaim_pages(input)?,
        });
    }
    let mut policy = account.policy();
    if ty == CgroupCoreFile::MemoryOomGroup {
        policy.oom_group = abi::parse_oom_group(input)?;
    } else {
        let pages = abi::parse_limit_pages(input)?;
        let limit = (pages != abi::PAGE_COUNTER_MAX).then_some(pages);
        match ty {
            CgroupCoreFile::MemoryMin => policy.min = pages,
            CgroupCoreFile::MemoryLow => policy.low = pages,
            CgroupCoreFile::MemoryHigh => policy.high = limit,
            CgroupCoreFile::MemoryMax => policy.max = limit,
            _ => return Err(SystemError::EINVAL),
        }
    }
    account.set_policy(policy);
    Ok(match ty {
        CgroupCoreFile::MemoryHigh => MemoryWrite::High(account),
        CgroupCoreFile::MemoryMax => MemoryWrite::Max(account),
        _ => MemoryWrite::Published,
    })
}

impl MemoryWrite {
    /// Called after releasing UPDATE, inode and file-private-data guards.
    pub(super) fn execute(self) -> Result<(), SystemError> {
        match self {
            Self::Published => Ok(()),
            Self::High(account) => {
                let mut retries = 16u8;
                loop {
                    let Some(high) = account.policy().high else {
                        return Ok(());
                    };
                    let usage = account.current_pages();
                    if usage <= high || signal_pending() || !account.is_online() {
                        return Ok(());
                    }
                    let progress = PageReclaimer::shrink_account(
                        &account,
                        PageFrameCount::new((usage - high) as usize),
                    )?;
                    if progress.reclaimed == 0 {
                        if retries == 0 {
                            return Ok(());
                        }
                        retries -= 1;
                    }
                }
            }
            Self::Max(account) => {
                // Publishing the limit is not rolled back by a signal or by
                // the absence of a killable victim, matching memory_max_write.
                match crate::cgroup::memory::runtime::enforce_max(&account) {
                    Err(SystemError::EINTR) => Ok(()),
                    result => result,
                }
            }
            Self::Reclaim { account, pages } => {
                let mut remaining = pages;
                let mut retries = 16u8;
                while remaining != 0 {
                    if signal_pending() {
                        return Err(SystemError::EINTR);
                    }
                    // Linux uses a bounded cluster per pass. Reclaimed pages
                    // are committed progress; never roll them back on failure.
                    let progress = PageReclaimer::shrink_account(
                        &account,
                        PageFrameCount::new(remaining.min(32) as usize),
                    )?;
                    remaining = remaining.saturating_sub(progress.reclaimed as u64);
                    if progress.reclaimed == 0 {
                        if retries == 0 {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        retries -= 1;
                    }
                }
                Ok(())
            }
        }
    }
}

fn signal_pending() -> bool {
    let task = ProcessManager::current_pcb();
    task.has_pending_signal_fast()
}
