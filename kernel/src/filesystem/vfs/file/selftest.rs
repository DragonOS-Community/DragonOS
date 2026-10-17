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

// Exercise the real File completion/recovery path. Only the storage hooks are
// synthetic: no data engine, global fault injector or recovery callback is
// substituted. Userspace tests separately cover actual filesystem contents.
#[derive(Debug)]
struct SyncProbeInode {
    account: Arc<MemoryAccount>,
    cache: Option<Arc<crate::filesystem::page_cache::PageCache>>,
    backing: Option<Arc<File>>,
    observed_file: crate::libs::spinlock::SpinLock<Option<alloc::sync::Weak<File>>>,
    expected_position: usize,
    expected_range: (usize, usize, bool),
    writes: AtomicUsize,
    syncs: AtomicUsize,
    valid: AtomicBool,
    terminal_error: bool,
    synchronous_after_write: bool,
}

const SYNC_PROBE_TRANSLATION: usize = 4096;

impl IndexNode for SyncProbeInode {
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
        len: usize,
        _: &[u8],
        _: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(len)
    }

    fn write_primary_with_sync_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: super::PrimaryWriteSource<'_, '_>,
        intent: super::WriteSyncIntent,
        _data: MutexGuard<FilePrivateData>,
        stage: &mut super::WritePrivilegeStage<'_>,
    ) -> Result<super::DelegatedWriteResult, MemoryAllocationError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        stage.complete();
        let backing = self.backing.as_ref().ok_or(SystemError::EINVAL)?;
        source.with_buffer(len, |buffer| {
            backing.pwrite_with_sync_intent(offset + SYNC_PROBE_TRANSLATION, len, buffer, intent)
        })
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), MemoryAllocationError> {
        let attempt = self.syncs.fetch_add(1, Ordering::Relaxed);
        let position_valid = self
            .observed_file
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
            .is_some_and(|file| file.pos() == self.expected_position);
        if (start, end, datasync) != self.expected_range || !position_valid {
            self.valid.store(false, Ordering::Relaxed);
        }
        if let Some(backing) = self.backing.as_ref() {
            return backing.try_sync_range_and_check_wb_error(
                start + SYNC_PROBE_TRANSLATION,
                end + SYNC_PROBE_TRANSLATION,
                datasync,
            );
        }
        if attempt == 0 {
            // Generate an actual typed quota failure on an isolated account.
            // Restore its margin before returning: real recovery can then
            // retry safely without reclaiming or selecting any OOM victim.
            self.account.set_policy(MemoryPolicy {
                max: Some(0),
                ..Default::default()
            });
            let admission = self.account.try_charge(MemoryKind::Anon, 1);
            self.account.set_policy(MemoryPolicy::default());
            return match admission {
                Err(error) => Err(error.into()),
                Ok(charge) => {
                    drop(charge);
                    self.valid.store(false, Ordering::Relaxed);
                    Err(SystemError::EINVAL.into())
                }
            };
        }
        if self.terminal_error {
            Err(SystemError::EIO.into())
        } else {
            Ok(())
        }
    }

    fn configure_open_file(&self, _: &FilePrivateData, behavior: &mut super::OpenFileBehavior) {
        if self.backing.is_some() {
            behavior.post_write_sync = super::PostWriteSyncPolicy::Delegated;
        }
    }

    fn page_cache(&self) -> Option<Arc<crate::filesystem::page_cache::PageCache>> {
        self.cache.clone()
    }

    fn mount_flags(&self) -> super::MountFlags {
        if self.synchronous_after_write && self.writes.load(Ordering::Relaxed) > 0 {
            super::MountFlags::SYNCHRONOUS
        } else {
            super::MountFlags::empty()
        }
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
}

/// Post-commit Charge retries synchronization only, preserving the strongest
/// intent, translated exact range, unpublished position and typed errseq.
/// The owning deferred completions must also release every retained File.
pub(crate) fn run_write_completion() -> Result<bool, MemoryAllocationError> {
    let mut valid = true;
    for (terminal_error, mapping_error, full_sync, synchronous_after_write) in [
        (false, false, true, false),
        (true, false, true, false),
        (false, true, true, false),
        (false, false, false, false),
        // Neither OFD requests sync initially. Only the leaf's post-write
        // mount-policy resample can discover Data and retain its retry range.
        (false, false, false, true),
    ] {
        // Root accounts intentionally bypass controller limits. Use an
        // isolated child so max=0 really produces a recoverable quota error.
        let root = MemoryAccount::new(0x776602, None)?;
        let account = MemoryAccount::new(0x776603, Some(root.clone()))?;
        let cache = crate::filesystem::page_cache::PageCache::new_unowned(None, None);
        let leaf = Arc::try_new(SyncProbeInode {
            account: account.clone(),
            cache: Some(cache.clone()),
            backing: None,
            observed_file: crate::libs::spinlock::SpinLock::new(None),
            expected_position: 37,
            expected_range: (4133, 4141, !full_sync),
            writes: AtomicUsize::new(0),
            syncs: AtomicUsize::new(0),
            valid: AtomicBool::new(true),
            terminal_error,
            synchronous_after_write,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let backing = Arc::try_new(File::new(
            leaf.clone(),
            FileFlags::O_RDWR
                | if full_sync {
                    FileFlags::O_SYNC
                } else if synchronous_after_write {
                    FileFlags::empty()
                } else {
                    FileFlags::O_DSYNC
                },
        )?)
        .map_err(|_| SystemError::ENOMEM)?;
        let weak_backing = Arc::downgrade(&backing);
        let layer = Arc::try_new(SyncProbeInode {
            account: account.clone(),
            cache: None,
            backing: Some(backing.clone()),
            observed_file: crate::libs::spinlock::SpinLock::new(None),
            expected_position: 37,
            expected_range: (37, 45, !full_sync),
            writes: AtomicUsize::new(0),
            syncs: AtomicUsize::new(0),
            valid: AtomicBool::new(true),
            terminal_error: false,
            synchronous_after_write: false,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let outer = Arc::try_new(File::new(
            layer.clone(),
            FileFlags::O_RDWR
                | if synchronous_after_write {
                    FileFlags::empty()
                } else {
                    FileFlags::O_DSYNC
                },
        )?)
        .map_err(|_| SystemError::ENOMEM)?;
        let weak_outer = Arc::downgrade(&outer);
        *leaf.observed_file.lock() = Some(weak_outer.clone());
        *layer.observed_file.lock() = Some(weak_outer.clone());
        outer.lseek(crate::driver::base::block::SeekFrom::SeekSet(37))?;
        if mapping_error {
            // Record after File captures its initial OFD errseq cursor.
            cache.record_writeback_error_with_superblock(SystemError::EIO);
        }
        let outcome = outer.write(9, b"sync-once");
        let outcome_valid = if terminal_error || mapping_error {
            matches!(&outcome, Err(SystemError::EIO)) && outer.pos() == 37
        } else {
            matches!(&outcome, Ok(9)) && outer.pos() == 46
        };
        let progress_valid = leaf.writes.load(Ordering::Relaxed) == 1
            && layer.writes.load(Ordering::Relaxed) == 1
            && leaf.syncs.load(Ordering::Relaxed) == 2
            && layer.syncs.load(Ordering::Relaxed) == 1
            && leaf.valid.load(Ordering::Relaxed)
            && layer.valid.load(Ordering::Relaxed)
            && account.current_pages() == 0
            && root.current_pages() == 0;
        if !outcome_valid || !progress_valid {
            log::error!(
                "File completion selftest: terminal={terminal_error} mapping={mapping_error} \
                 full={full_sync} resample={synchronous_after_write} outcome={outcome:?} \
                 pos={} writes={}/{} syncs={}/{} ranges={}/{}",
                outer.pos(),
                leaf.writes.load(Ordering::Relaxed),
                layer.writes.load(Ordering::Relaxed),
                leaf.syncs.load(Ordering::Relaxed),
                layer.syncs.load(Ordering::Relaxed),
                leaf.valid.load(Ordering::Relaxed),
                layer.valid.load(Ordering::Relaxed),
            );
        }
        valid &= outcome_valid && progress_valid;
        if mapping_error {
            // The recovered operation, rather than its failed admission
            // attempt, observes and consumes this OFD's EIO exactly once.
            valid &= backing.check_and_advance_wb_error(&cache).is_ok();
        }
        drop(outer);
        drop(layer);
        drop(backing);
        valid &= weak_outer.upgrade().is_none() && weak_backing.upgrade().is_none();
    }
    Ok(valid)
}

/// Synthetic storage hooks exercise the real legacy loop and charge recovery.
/// They inspect operation boundaries without dereferencing userspace input;
/// actual copy and record semantics are covered by legacy_writev_semantics.
#[derive(Debug)]
struct LegacyWriteProbe {
    account: Arc<MemoryAccount>,
    observed_file: crate::libs::spinlock::SpinLock<Option<alloc::sync::Weak<File>>>,
    first_calls: AtomicUsize,
    second_calls: AtomicUsize,
    valid: AtomicBool,
    terminal_error: bool,
}

impl IndexNode for LegacyWriteProbe {
    fn supports_write_iter_flags(&self) -> bool {
        false
    }

    fn configure_open_file(&self, _: &FilePrivateData, behavior: &mut super::OpenFileBehavior) {
        behavior.post_write_sync = super::PostWriteSyncPolicy::NotApplicable;
    }

    fn write_user_buffer_at(
        &self,
        offset: usize,
        len: usize,
        _: &crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, MemoryAllocationError> {
        drop(data);
        if self
            .observed_file
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
            .is_none_or(|file| file.pos() != 37)
        {
            self.valid.store(false, Ordering::Relaxed);
        }
        match (offset, len) {
            (37, 3) => {
                self.first_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Some(3))
            }
            (40, 5) => {
                let attempt = self.second_calls.fetch_add(1, Ordering::Relaxed);
                if self.terminal_error {
                    return Err(SystemError::EIO.into());
                }
                if attempt == 0 {
                    self.account.set_policy(MemoryPolicy {
                        max: Some(0),
                        ..Default::default()
                    });
                    let admission = self.account.try_charge(MemoryKind::Anon, 1);
                    self.account.set_policy(MemoryPolicy::default());
                    return match admission {
                        Err(error) => Err(error.into()),
                        Ok(charge) => {
                            drop(charge);
                            self.valid.store(false, Ordering::Relaxed);
                            Err(SystemError::EINVAL.into())
                        }
                    };
                }
                Ok(Some(5))
            }
            _ => {
                self.valid.store(false, Ordering::Relaxed);
                Err(SystemError::EINVAL.into())
            }
        }
    }

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
        // The direct-user hook must own each original legacy invocation.
        self.valid.store(false, Ordering::Relaxed);
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
}

pub(crate) fn run_legacy_writev_completion() -> Result<bool, MemoryAllocationError> {
    let mut valid = true;
    for terminal_error in [false, true] {
        let root = MemoryAccount::new(0x776604, None)?;
        let account = MemoryAccount::new(0x776605, Some(root.clone()))?;
        let inode = Arc::try_new(LegacyWriteProbe {
            account: account.clone(),
            observed_file: crate::libs::spinlock::SpinLock::new(None),
            first_calls: AtomicUsize::new(0),
            second_calls: AtomicUsize::new(0),
            valid: AtomicBool::new(true),
            terminal_error,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let file = Arc::try_new(File::new(inode.clone(), FileFlags::O_RDWR)?)
            .map_err(|_| SystemError::ENOMEM)?;
        let weak = Arc::downgrade(&file);
        *inode.observed_file.lock() = Some(weak.clone());
        file.lseek(crate::driver::base::block::SeekFrom::SeekSet(37))?;
        // These address-only ranges are never dereferenced by this fixture.
        // No userspace import or copy primitive is replaced for the test.
        let vectors = [
            crate::filesystem::vfs::iov::IoVec {
                iov_base: 0x1000 as *mut u8,
                iov_len: 3,
            },
            crate::filesystem::vfs::iov::IoVec {
                iov_base: 0x2000 as *mut u8,
                iov_len: 5,
            },
        ];
        let outcome = {
            let _position = file.lock_position();
            file.write_user_legacy_vectors_at(37, &vectors, true)
        };
        let expected = if terminal_error { 3 } else { 8 };
        valid &= matches!(&outcome, Ok(written) if *written == expected)
            && file.pos() == 37 + expected
            && inode.first_calls.load(Ordering::Relaxed) == 1
            && inode.second_calls.load(Ordering::Relaxed) == if terminal_error { 1 } else { 2 }
            && inode.valid.load(Ordering::Relaxed)
            && account.current_pages() == 0
            && root.current_pages() == 0;
        drop(file);
        valid &= weak.upgrade().is_none();
    }
    Ok(valid)
}
