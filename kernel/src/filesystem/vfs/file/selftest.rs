//! Actual File-body fixture. The inode only counts hooks; this does not
//! substitute for pathname, filesystem, fd-installation or user-copy tests.

use alloc::sync::Arc;
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use system_error::SystemError;

use super::{File, FileData, FileFlags, FilePrivateData, PreparedFile};
use crate::{
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, MemoryAccount, MemoryKind, MemoryPolicy,
    },
    filesystem::{
        anon_inode::{anon_inode_metadata, AnonInodeFs},
        vfs::{FileSystem, IndexNode, InodeMode, Metadata},
    },
    libs::mutex::MutexGuard,
    mm::{allocation::MemoryAllocationError, allocator::kernel_allocator::allocation_footprint},
};

#[derive(Debug)]
struct CountedInode {
    account: Arc<MemoryAccount>,
    fail_open: AtomicBool,
    opens: AtomicUsize,
    closes: AtomicUsize,
}

impl IndexNode for CountedInode {
    fn read_at(
        &self,
        _: usize,
        _: usize,
        _: &mut [u8],
        _: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::EINVAL.into())
    }
    fn write_at(
        &self,
        _: usize,
        _: usize,
        _: &[u8],
        _: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::EINVAL.into())
    }
    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(anon_inode_metadata(InodeMode::from_bits_truncate(0o600)))
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), MemoryAllocationError> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        if self.fail_open.load(Ordering::Relaxed) {
            let charge = self.account.try_charge(MemoryKind::Anon, 1)?;
            drop(charge);
            return Err(SystemError::EINVAL.into());
        }
        Ok(())
    }
    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.closes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

pub(crate) fn run_body_lifetime() -> Result<bool, MemoryAllocationError> {
    let root = MemoryAccount::new(0x776600, None)?;
    let account = MemoryAccount::new(0x776601, Some(root.clone()))?;
    let creator = ObjectAccount::new(&account)?;
    let inode = Arc::try_new(CountedInode {
        account: account.clone(),
        fail_open: AtomicBool::new(false),
        opens: AtomicUsize::new(0),
        closes: AtomicUsize::new(0),
    })
    .map_err(|_| SystemError::ENOMEM)?;
    let footprint = allocation_footprint(core::alloc::Layout::new::<FileData>());
    let body_kind = KernelByteKind::SlabUnreclaimable
        .for_allocation(core::alloc::Layout::new::<FileData>()) as usize;
    let storage = PreparedFile::try_new_for(&creator)?;
    let before_open = account.snapshot();
    let mut ok = before_open.kernel_bytes[body_kind] == footprint
        && inode.opens.load(Ordering::Relaxed) == 0;
    let file = Arc::try_new(File::new_prepared(
        inode.clone(),
        FileFlags::O_RDONLY,
        storage,
    )?)
    .map_err(|_| SystemError::ENOMEM)?;
    let weak = Arc::downgrade(&file);
    ok &= inode.opens.load(Ordering::Relaxed) == 1 && inode.closes.load(Ordering::Relaxed) == 0;
    drop(file);
    ok &= weak.upgrade().is_none()
        && inode.closes.load(Ordering::Relaxed) == 1
        && account.current_pages() == 0
        && account.snapshot().kernel_bytes[body_kind] == 0;
    // The weak thin identity remains allocated while the real body is gone.
    drop(weak);

    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..Default::default()
    });
    ok &= matches!(
        PreparedFile::try_new_for(&creator),
        Err(MemoryAllocationError::Charge(_))
    ) && inode.opens.load(Ordering::Relaxed) == 1
        && account.current_pages() == 0;

    account.set_policy(MemoryPolicy::default());
    let storage = PreparedFile::try_new_for(&creator)?;
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..Default::default()
    });
    inode.fail_open.store(true, Ordering::Relaxed);
    ok &= matches!(
        File::new_prepared(inode.clone(), FileFlags::O_RDONLY, storage),
        Err(MemoryAllocationError::Charge(_))
    ) && inode.opens.load(Ordering::Relaxed) == 2
        && inode.closes.load(Ordering::Relaxed) == 1
        && account.current_pages() == 0
        && root.current_pages() == 0;
    Ok(ok)
}
