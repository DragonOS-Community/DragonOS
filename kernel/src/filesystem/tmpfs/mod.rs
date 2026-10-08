use core::any::Any;
use core::fmt::Write;
use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicU64, Ordering};

mod seals;
use seals::{MemfdSeals, F_ALL_SEALS, F_SEAL_EXEC, F_SEAL_SEAL};

use crate::filesystem::page_cache::{PageCache, PageCacheBackend, PageCacheWritebackDomain};
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::{FileSystemMakerData, FSMAKER};
use crate::libs::rwsem::RwSem;
use crate::mm::allocator::page_frame::FrameAllocator;
use crate::mm::fault::PageFaultHandler;
use crate::mm::page::Page;
use crate::process::namespace::user_namespace::{
    make_kgid, make_kuid, map_id_up, UserNamespace, INIT_USER_NAMESPACE,
};
use crate::register_mountable_fs;
use crate::{
    arch::mm::LockedFrameAllocator,
    arch::MMArch,
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::vfs::{vcore::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    libs::mutex::{Mutex, MutexGuard},
    mm::MemoryManagementArch,
    process::ProcessManager,
    time::PosixTimeSpec,
};

use alloc::string::ToString;
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::vfs::{
    file::{File, FileFlags, FilePrivateData},
    mount::MountFlags,
    utils::DName,
    FileSystem, FsCreationContext, FsInfo, FsReconfigureRequest, FsconfigPreparedData, IndexNode,
    InodeFlags, InodeId, InodeMode, LinkMutationCoordinator, LinkRemovalOutcome, Metadata,
    MetadataUpdate, OpenFileBehavior, PostWriteSyncPolicy, RenameOutcome, SetMetadataMask,
    SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::{Magic, MountableFileSystem, SuperBlock};
use lazy_static::lazy_static;

const TMPFS_MAX_NAMELEN: usize = 255;
const TMPFS_BLOCK_SIZE: u64 = 4096;

const TMPFS_DEFAULT_MIN_SIZE_BYTES: usize = 16 * 1024 * 1024; // 16MiB
const TMPFS_DEFAULT_MAX_SIZE_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4GiB
const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

#[derive(Debug)]
struct TmpfsPageCacheBackend {
    inode: Weak<dyn IndexNode>,
    fs: Weak<Tmpfs>,
}

impl TmpfsPageCacheBackend {
    fn new(inode: Weak<dyn IndexNode>, fs: Weak<Tmpfs>) -> Self {
        Self { inode, fs }
    }
}

impl PageCacheBackend for TmpfsPageCacheBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        Ok(buf.len())
    }

    fn npages(&self) -> usize {
        let inode = match self.inode.upgrade() {
            Some(inode) => inode,
            None => return 0,
        };
        match inode.metadata() {
            Ok(metadata) => {
                let size = metadata.size.max(0) as usize;
                if size == 0 {
                    0
                } else {
                    (size + MMArch::PAGE_SIZE - 1) >> MMArch::PAGE_SHIFT
                }
            }
            Err(_) => 0,
        }
    }

    fn reserve_page(&self) -> Result<(), SystemError> {
        self.fs
            .upgrade()
            .ok_or(SystemError::EIO)?
            .increase_size(MMArch::PAGE_SIZE as u64)
    }

    fn release_page(&self) {
        if let Some(fs) = self.fs.upgrade() {
            fs.decrease_size(MMArch::PAGE_SIZE);
        }
    }
}

fn new_tmpfs_page_cache(
    inode: Weak<dyn IndexNode>,
    backend: Arc<dyn PageCacheBackend>,
    fs: &Weak<Tmpfs>,
) -> Result<Arc<PageCache>, SystemError> {
    let fs = fs.upgrade().ok_or(SystemError::EIO)?;
    match fs.writeback_domain.as_ref() {
        Some(domain) => PageCache::new_filesystem_shmem(inode, backend, domain),
        None => Ok(PageCache::new_shmem(Some(inode), Some(backend))),
    }
}

fn tmpfs_move_entry_between_dirs(
    src_dir: &mut TmpfsInode,
    dst_dir: &mut TmpfsInode,
    old_key: &DName,
    new_key: &DName,
    flags: RenameFlags,
    context: &crate::filesystem::vfs::permission::InodeOpContext,
) -> Result<RenameOutcome, SystemError> {
    tmpfs_require_live_dir(src_dir)?;
    tmpfs_require_live_dir(dst_dir)?;

    let src_self = src_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;
    let dst_self = dst_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;

    let inode_to_move = src_dir
        .children
        .get(old_key)
        .cloned()
        .ok_or(SystemError::ENOENT)?;
    let old_type = inode_to_move.0.lock().metadata.file_type;

    if flags.contains(RenameFlags::EXCHANGE) {
        let existing = dst_dir
            .children
            .get(new_key)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if Arc::ptr_eq(&inode_to_move, &existing) {
            return Ok(RenameOutcome::NoOp);
        }
        tmpfs_check_rename(
            &src_dir.metadata,
            &dst_dir.metadata,
            &inode_to_move,
            Some(&existing),
            true,
            context,
        )?;
        let now = PosixTimeSpec::now();
        let existing_type = existing.0.lock().metadata.file_type;

        src_dir.children.insert(old_key.clone(), existing.clone());
        dst_dir
            .children
            .insert(new_key.clone(), inode_to_move.clone());
        if old_type == FileType::Dir {
            src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_sub(1);
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_add(1);
        }
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_add(1);
        }

        {
            let mut moved = inode_to_move.0.lock();
            moved.parent = Arc::downgrade(&dst_self);
            moved.name = new_key.clone();
            moved.metadata.ctime = now;
        }
        {
            let mut replaced = existing.0.lock();
            replaced.parent = Arc::downgrade(&src_self);
            replaced.name = old_key.clone();
            replaced.metadata.ctime = now;
        }
        tmpfs_touch_dir(src_dir, now);
        tmpfs_touch_dir(dst_dir, now);
        return Ok(RenameOutcome::Exchange);
    }

    let mut whiteout = None;
    let mut replaced = None;
    if let Some(existing) = dst_dir.children.get(new_key).cloned() {
        if flags.contains(RenameFlags::NOREPLACE) {
            return Err(SystemError::EEXIST);
        }

        // Avoid self-deadlock: `existing` may be `src_dir`/`dst_dir` itself.
        if Arc::ptr_eq(&existing, &src_self) {
            // Example: rename("dir/subdir", "dir") -> ENOTEMPTY (dir not empty).
            // Linux expects ENOTEMPTY for this case (TargetIsAncestorOfSource).
            return Err(SystemError::ENOTEMPTY);
        }
        if Arc::ptr_eq(&existing, &dst_self) {
            // Shouldn't happen in normal tmpfs (no self entry), but treat as busy.
            return Err(SystemError::EBUSY);
        }

        let (existing_id, existing_type, existing_dir_nonempty) = {
            let guard = existing.0.lock();
            let t = guard.metadata.file_type;
            let nonempty = t == FileType::Dir && !guard.children.is_empty();
            (guard.metadata.inode_id, t, nonempty)
        };

        let to_move_id = inode_to_move.0.lock().metadata.inode_id;
        if existing_id == to_move_id {
            return Ok(RenameOutcome::NoOp);
        }

        tmpfs_check_rename(
            &src_dir.metadata,
            &dst_dir.metadata,
            &inode_to_move,
            Some(&existing),
            false,
            context,
        )?;

        if old_type == FileType::Dir && existing_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if old_type != FileType::Dir && existing_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        if old_type == FileType::Dir && existing_dir_nonempty {
            return Err(SystemError::ENOTEMPTY);
        }

        // Complete fallible whiteout preparation before touching either name.
        if flags.contains(RenameFlags::WHITEOUT) {
            whiteout = Some(tmpfs_prepare_whiteout(src_dir, old_key, context)?);
        }
        // Remove existing destination entry (replacement).
        dst_dir.children.remove(new_key);
        let mut existing_guard = existing.0.lock();
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            existing_guard.metadata.nlinks = 0;
            replaced = Some(LinkRemovalOutcome::LastLink);
        } else {
            existing_guard.metadata.nlinks = existing_guard.metadata.nlinks.saturating_sub(1);
            replaced = Some(if existing_guard.metadata.nlinks == 0 {
                LinkRemovalOutcome::LastLink
            } else {
                LinkRemovalOutcome::StillLinked
            });
        }
        existing_guard.metadata.ctime = PosixTimeSpec::now();
    } else {
        tmpfs_check_rename(
            &src_dir.metadata,
            &dst_dir.metadata,
            &inode_to_move,
            None,
            false,
            context,
        )?;
    }

    if flags.contains(RenameFlags::WHITEOUT) {
        let whiteout = match whiteout {
            Some(inode) => inode,
            None => tmpfs_prepare_whiteout(src_dir, old_key, context)?,
        };
        // Replace the existing key, retaining the map node for this commit.
        src_dir.children.insert(old_key.clone(), whiteout);
    } else {
        src_dir.children.remove(old_key);
    }
    if old_type == FileType::Dir {
        src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_sub(1);
        dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_add(1);
    }

    // Insert into destination directory and update inode bookkeeping.
    dst_dir
        .children
        .insert(new_key.clone(), inode_to_move.clone());
    let mut moved = inode_to_move.0.lock();
    moved.parent = Arc::downgrade(&dst_self);
    moved.name = new_key.clone();
    let now = PosixTimeSpec::now();
    moved.metadata.ctime = now;
    tmpfs_touch_dir(src_dir, now);
    tmpfs_touch_dir(dst_dir, now);

    Ok(RenameOutcome::Moved { replaced })
}

fn tmpfs_check_rename(
    source_parent: &Metadata,
    target_parent: &Metadata,
    source: &Arc<LockedTmpfsInode>,
    target: Option<&Arc<LockedTmpfsInode>>,
    exchange: bool,
    context: &crate::filesystem::vfs::permission::InodeOpContext,
) -> Result<(), SystemError> {
    let source = source.0.lock().metadata.clone();
    crate::filesystem::vfs::permission::check_inode_delete(
        source_parent,
        &source,
        source.file_type == FileType::Dir,
        context,
    )?;
    if let Some(target) = target {
        let target = target.0.lock().metadata.clone();
        let directory = if exchange {
            target.file_type == FileType::Dir
        } else {
            source.file_type == FileType::Dir
        };
        crate::filesystem::vfs::permission::check_inode_delete(
            target_parent,
            &target,
            directory,
            context,
        )
    } else {
        crate::filesystem::vfs::permission::check_parent_create(target_parent, context, false)
    }
}

/// Linux shmem_fallocate commits allocation/size first and deliberately
/// ignores file_modified's result. Only this attribute stage is best-effort;
/// callers must propagate allocation, seal, quota and hole-punch errors.
fn tmpfs_fallocate_modified(
    inode: &mut TmpfsInode,
    context: &crate::filesystem::vfs::permission::InodeOpContext,
) -> bool {
    let size = inode.metadata.size.max(0) as usize;
    match crate::filesystem::vfs::vcore::prepare_backing_fallocate_metadata(
        context,
        &inode.metadata,
        size,
        false,
    ) {
        Ok((metadata, mask)) => {
            crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, &metadata, mask);
            mask.contains(SetMetadataMask::MODE)
        }
        Err(_) => false,
    }
}

fn tmpfs_touch_dir(dir: &mut TmpfsInode, now: PosixTimeSpec) {
    dir.metadata.mtime = now;
    dir.metadata.ctime = now;
}

fn tmpfs_require_live_dir(dir: &TmpfsInode) -> Result<(), SystemError> {
    if dir.metadata.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if dir.metadata.nlinks == 0 {
        return Err(SystemError::ENOENT);
    }
    Ok(())
}

fn tmpfs_prepare_whiteout(
    dir: &TmpfsInode,
    name: &DName,
    context: &crate::filesystem::vfs::permission::InodeOpContext,
) -> Result<Arc<LockedTmpfsInode>, SystemError> {
    crate::filesystem::vfs::permission::check_parent_create(&dir.metadata, context, false)?;
    let init = crate::filesystem::vfs::permission::child_inode_init_with_context(
        &dir.metadata,
        FileType::CharDevice,
        InodeMode::S_IFCHR | InodeMode::from_bits_truncate(0o600),
        context,
    )?;
    let now = PosixTimeSpec::now();
    let whiteout = Arc::try_new(LockedTmpfsInode::new(TmpfsInode {
        parent: dir.self_ref.clone(),
        self_ref: Weak::default(),
        children: BTreeMap::new(),
        page_cache: None,
        metadata: Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            file_type: FileType::CharDevice,
            mode: init.mode,
            nlinks: 1,
            uid: init.uid,
            gid: init.gid,
            raw_dev: WHITEOUT_DEV,
            flags: InodeFlags::empty(),
        },
        fs: dir.fs.clone(),
        special_node: None,
        inline_symlink: None,
        name: name.clone(),
        tmpfile_linkable: false,
    }))
    .map_err(|_| SystemError::ENOMEM)?;
    whiteout.0.lock().self_ref = Arc::downgrade(&whiteout);
    Ok(whiteout)
}

#[derive(Debug)]
pub struct LockedTmpfsInode(
    pub Mutex<TmpfsInode>,
    RwSem<()>,
    LinkMutationCoordinator,
    Option<Mutex<MemfdSeals>>,
    Option<Vec<u8>>,
);

impl LockedTmpfsInode {
    fn prepare_primary_write_locked(
        &self,
        offset: usize,
        len: usize,
        context: &super::vfs::permission::InodeOpContext,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<(Arc<PageCache>, usize), SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
        offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        let permitted_len = if let Some(state) = self.3.as_ref() {
            state
                .lock()
                .permitted_write_len(offset, len, inode.metadata.size as usize)?
        } else {
            len
        };
        let mut mode_changed = false;
        if inode.metadata.file_type == FileType::File {
            if let Some(cred) = &context.cred {
                let view = context.view_metadata(&inode.metadata);
                let (requested, derived_mask) =
                    super::vfs::vcore::prepare_write_side_effect_metadata_with_cred(
                        view,
                        inode.metadata.size.max(0) as usize,
                        cred,
                    );
                if derived_mask.contains(SetMetadataMask::MODE) {
                    let mask = SetMetadataMask::MODE
                        | SetMetadataMask::CTIME
                        | SetMetadataMask::WRITE_SIDE_EFFECT;
                    let backing =
                        context.backing_metadata_request(&inode.metadata, &requested, mask)?;
                    super::vfs::merge_metadata_masked(&mut inode.metadata, &backing, mask);
                    mode_changed = true;
                }
            }
        }
        drop(inode);
        stage.complete();
        if mode_changed {
            stage.commit_attrib();
        }
        Ok((page_cache, permitted_len))
    }

    /// The primary caller retains this content guard across privilege removal,
    /// user-buffer faults and data publication; never reenter public write_at.
    fn write_data_locked(
        &self,
        offset: usize,
        permitted_len: usize,
        buf: &[u8],
        page_cache: &Arc<PageCache>,
        _content: &crate::libs::rwsem::RwSemReadGuard<'_, ()>,
    ) -> Result<usize, SystemError> {
        let write_end = offset + permitted_len;
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (write_end - 1) >> MMArch::PAGE_SHIFT;
        let mut written = 0usize;
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let write_start = core::cmp::max(offset, page_start);
            let page_write_end = core::cmp::min(write_end, page_end);
            let page_write_len = page_write_end.saturating_sub(write_start);
            if page_write_len == 0 {
                continue;
            }

            let pin = match page_cache.manager().commit_overwrite_pinned(page_index) {
                Ok(pin) => pin,
                Err(err) => {
                    if written == 0 {
                        return Err(err);
                    }
                    break;
                }
            };

            // prefault 用户缓冲区，避免后续在持页锁时缺页
            volatile_read!(buf[written]);
            volatile_read!(buf[written + page_write_len - 1]);

            let page = pin.page();
            let mut page_guard = page.write();
            unsafe {
                let page_offset = write_start - page_start;
                page_guard.as_slice_mut()[page_offset..page_offset + page_write_len]
                    .copy_from_slice(&buf[written..written + page_write_len]);
            }
            page_guard.add_flags(crate::mm::page::PageFlags::PG_DIRTY);
            if let Err(err) = page_cache.mark_page_dirty_page_locked(page_index, &page_guard) {
                page_guard.remove_flags(crate::mm::page::PageFlags::PG_DIRTY);
                if written == 0 {
                    return Err(err);
                }
                break;
            }
            written += page_write_len;
        }

        // Quota is charged by page-cache membership. Logical size advances
        // only through the prefix which was actually copied.
        let mut inode = self.0.lock();
        let committed_end = offset + written;
        if committed_end > inode.metadata.size as usize {
            inode.metadata.size = committed_end as i64;
        }
        Ok(written)
    }

    fn move_with_inode_context(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<RenameOutcome, SystemError> {
        // tmpfs rename should move a directory entry (dentry move), not create
        // a hardlink+unlink pair. The latter breaks directory moves (unlink()
        // rejects directories) and can also lead to incorrect link/size accounting.

        let old_key = DName::from(old_name);
        let new_key = DName::from(new_name);

        // Target must be a directory in tmpfs.
        let target_locked = target
            .clone()
            .downcast_arc::<LockedTmpfsInode>()
            .ok_or(SystemError::EINVAL)?;

        // Lock ordering: lock by inode_id to avoid deadlocks.
        let self_id = self.0.lock().metadata.inode_id;
        let target_id = target_locked.0.lock().metadata.inode_id;

        if self_id == target_id {
            // Same directory rename.
            let mut dir = self.0.lock();
            tmpfs_require_live_dir(&dir)?;
            let inode_to_move = dir
                .children
                .get(&old_key)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            let old_type = inode_to_move.0.lock().metadata.file_type;

            if flags.contains(RenameFlags::EXCHANGE) {
                let existing = dir
                    .children
                    .get(&new_key)
                    .cloned()
                    .ok_or(SystemError::ENOENT)?;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                let existing_id = existing.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(RenameOutcome::NoOp);
                }

                tmpfs_check_rename(
                    &dir.metadata,
                    &dir.metadata,
                    &inode_to_move,
                    Some(&existing),
                    true,
                    context,
                )?;

                let now = PosixTimeSpec::now();
                dir.children.insert(old_key.clone(), existing.clone());
                dir.children.insert(new_key.clone(), inode_to_move.clone());
                let mut existing = existing.0.lock();
                existing.name = old_key;
                existing.metadata.ctime = now;
                let mut moved = inode_to_move.0.lock();
                moved.name = new_key;
                moved.metadata.ctime = now;
                tmpfs_touch_dir(&mut dir, now);
                return Ok(RenameOutcome::Exchange);
            }

            let mut whiteout = None;
            let mut replaced = None;
            if let Some(existing) = dir.children.get(&new_key).cloned() {
                if flags.contains(RenameFlags::NOREPLACE) {
                    return Err(SystemError::EEXIST);
                }

                // If destination already refers to the same inode, it's a no-op.
                let existing_id = existing.0.lock().metadata.inode_id;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(RenameOutcome::NoOp);
                }

                tmpfs_check_rename(
                    &dir.metadata,
                    &dir.metadata,
                    &inode_to_move,
                    Some(&existing),
                    false,
                    context,
                )?;

                let existing_type = existing.0.lock().metadata.file_type;
                if old_type == FileType::Dir && existing_type != FileType::Dir {
                    return Err(SystemError::ENOTDIR);
                }
                if old_type != FileType::Dir && existing_type == FileType::Dir {
                    return Err(SystemError::EISDIR);
                }

                if old_type == FileType::Dir && !existing.0.lock().children.is_empty() {
                    return Err(SystemError::ENOTEMPTY);
                }

                if flags.contains(RenameFlags::WHITEOUT) {
                    whiteout = Some(tmpfs_prepare_whiteout(&dir, &old_key, context)?);
                }
                // Remove existing destination entry (replacement).
                dir.children.remove(&new_key);
                let mut existing_guard = existing.0.lock();
                if existing_type == FileType::Dir {
                    dir.metadata.nlinks = dir.metadata.nlinks.saturating_sub(1);
                    existing_guard.metadata.nlinks = 0;
                    replaced = Some(LinkRemovalOutcome::LastLink);
                } else {
                    existing_guard.metadata.nlinks =
                        existing_guard.metadata.nlinks.saturating_sub(1);
                    replaced = Some(if existing_guard.metadata.nlinks == 0 {
                        LinkRemovalOutcome::LastLink
                    } else {
                        LinkRemovalOutcome::StillLinked
                    });
                }
                existing_guard.metadata.ctime = PosixTimeSpec::now();
            } else {
                tmpfs_check_rename(
                    &dir.metadata,
                    &dir.metadata,
                    &inode_to_move,
                    None,
                    false,
                    context,
                )?;
            }

            if flags.contains(RenameFlags::WHITEOUT) {
                let whiteout = match whiteout {
                    Some(inode) => inode,
                    None => tmpfs_prepare_whiteout(&dir, &old_key, context)?,
                };
                dir.children.insert(old_key.clone(), whiteout);
            } else {
                dir.children.remove(&old_key);
            }
            dir.children.insert(new_key.clone(), inode_to_move.clone());
            let now = PosixTimeSpec::now();
            let mut moved = inode_to_move.0.lock();
            moved.name = new_key;
            moved.metadata.ctime = now;
            tmpfs_touch_dir(&mut dir, now);
            return Ok(RenameOutcome::Moved { replaced });
        }

        // Cross-directory move.
        // Lock both directories in a stable order.
        if self_id < target_id {
            let mut src_dir = self.0.lock();
            let mut dst_dir = target_locked.0.lock();
            return tmpfs_move_entry_between_dirs(
                &mut src_dir,
                &mut dst_dir,
                &old_key,
                &new_key,
                flags,
                context,
            );
        } else {
            let mut dst_dir = target_locked.0.lock();
            let mut src_dir = self.0.lock();
            return tmpfs_move_entry_between_dirs(
                &mut src_dir,
                &mut dst_dir,
                &old_key,
                &new_key,
                flags,
                context,
            );
        }
    }
    fn new(inode: TmpfsInode) -> Self {
        Self::new_with_memfd(inode, None)
    }

    fn new_with_memfd(inode: TmpfsInode, memfd: Option<(Vec<u8>, u32)>) -> Self {
        let (seals, name) = match memfd {
            Some((name, bits)) => (Some(Mutex::new(MemfdSeals::new(bits))), Some(name)),
            None => (None, None),
        };
        Self(
            Mutex::new(inode),
            RwSem::new(()),
            LinkMutationCoordinator::new(),
            seals,
            name,
        )
    }

    pub fn get_seals(&self) -> Result<u32, SystemError> {
        if self.0.lock().metadata.file_type != FileType::File {
            return Err(SystemError::EINVAL);
        }
        Ok(self
            .3
            .as_ref()
            .map_or(F_SEAL_SEAL, |state| state.lock().bits))
    }

    pub fn add_seals(&self, requested: u32) -> Result<(), SystemError> {
        if requested & !F_ALL_SEALS != 0 {
            return Err(SystemError::EINVAL);
        }
        let _size_guard = self.1.write();
        let executable = {
            let inode = self.0.lock();
            if inode.metadata.file_type != FileType::File {
                return Err(SystemError::EINVAL);
            }
            inode.metadata.mode.bits() & 0o111 != 0
        };
        let Some(state) = self.3.as_ref() else {
            return Err(SystemError::EPERM);
        };
        state.lock().add(requested, executable)
    }

    pub fn prepare_memfd_mmap(
        &self,
        flags: crate::mm::VmFlags,
    ) -> Result<(crate::mm::VmFlags, bool), SystemError> {
        match self.3.as_ref() {
            Some(state) => state.lock().prepare_map(flags),
            None => Ok((flags, false)),
        }
    }

    pub fn finish_memfd_mmap_prepare(&self) {
        if let Some(state) = self.3.as_ref() {
            let mut state = state.lock();
            state.pending_writable_maps -= 1;
        }
    }

    fn memfd_map_open(&self, flags: crate::mm::VmFlags) -> bool {
        if !flags.contains(crate::mm::VmFlags::VM_SHARED | crate::mm::VmFlags::VM_MAYWRITE) {
            return false;
        }
        if let Some(state) = self.3.as_ref() {
            state.lock().writable_maps += 1;
            return true;
        }
        false
    }

    fn memfd_map_close(&self, flags: crate::mm::VmFlags) {
        if flags.contains(crate::mm::VmFlags::VM_SHARED | crate::mm::VmFlags::VM_MAYWRITE) {
            if let Some(state) = self.3.as_ref() {
                state.lock().writable_maps -= 1;
            }
        }
    }

    pub fn pin_memfd_remote_write(&self) -> bool {
        if let Some(state) = self.3.as_ref() {
            state.lock().remote_writers += 1;
            return true;
        }
        false
    }

    pub fn unpin_memfd_remote_write(&self) {
        if let Some(state) = self.3.as_ref() {
            state.lock().remote_writers -= 1;
        }
    }

    fn check_exec_mode_change(&self, old: InodeMode, new: InodeMode) -> Result<(), SystemError> {
        if (old.bits() ^ new.bits()) & 0o111 != 0 {
            if let Some(state) = self.3.as_ref() {
                if state.lock().bits & F_SEAL_EXEC != 0 {
                    return Err(SystemError::EPERM);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Tmpfs {
    root_inode: Arc<LockedTmpfsInode>,
    writeback_domain: Option<Arc<PageCacheWritebackDomain>>,
    super_block: RwSem<SuperBlock>,
    size_limit: RwSem<Option<u64>>,
    current_size: AtomicU64,
    mount_mode: RwSem<InodeMode>,
    root_uid: usize,
    root_gid: usize,
    owner_user_ns: Arc<UserNamespace>,
}

#[derive(Debug)]
pub struct TmpfsShmemFile {
    file: Arc<File>,
}

impl TmpfsShmemFile {
    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    pub fn inode(&self) -> Arc<dyn IndexNode> {
        self.file.inode()
    }

    pub fn inode_id(&self) -> InodeId {
        // This wrapper is constructed only from a live internal tmpfs inode.
        self.file
            .metadata()
            .expect("internal shmem metadata")
            .inode_id
    }

    pub fn page_cache(&self) -> Arc<PageCache> {
        self.inode()
            .page_cache()
            .expect("internal shmem page cache")
    }

    pub fn set_locked(&self, locked: bool) -> (Arc<PageCache>, bool) {
        let page_cache = self.page_cache();
        let old_locked = page_cache.set_unevictable(locked);
        (page_cache, old_locked)
    }
}

#[derive(Debug)]
pub struct TmpfsInode {
    parent: Weak<LockedTmpfsInode>,
    self_ref: Weak<LockedTmpfsInode>,
    children: BTreeMap<DName, Arc<LockedTmpfsInode>>,
    page_cache: Option<Arc<PageCache>>,
    metadata: Metadata,
    fs: Weak<Tmpfs>,
    special_node: Option<SpecialNodeData>,
    inline_symlink: Option<String>,
    name: DName,
    /// Linux I_LINKABLE equivalent for a never-published O_TMPFILE inode.
    tmpfile_linkable: bool,
}

impl TmpfsInode {
    pub fn new() -> Self {
        Self {
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::Dir,
                mode: InodeMode::S_IRWXUGO,
                nlinks: 2,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
                flags: InodeFlags::empty(),
            },
            fs: Weak::default(),
            special_node: None,
            inline_symlink: None,
            name: Default::default(),
            tmpfile_linkable: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TmpfsMountData {
    mode: Option<InodeMode>,
    size_bytes: Option<u64>,
    uid: Option<usize>,
    gid: Option<usize>,
}

impl TmpfsMountData {
    /// Shared, side-effect-free parsing for initial mount and reconfiguration.
    fn parse_parameter(
        &mut self,
        key: &str,
        value: Option<&str>,
        owner: Option<&UserNamespace>,
    ) -> Result<(), SystemError> {
        let value = value.ok_or(SystemError::EINVAL)?;
        let value = if matches!(key, "uid" | "gid") {
            // kstrtouint permits one final newline, not surrounding spaces.
            value.strip_suffix('\n').unwrap_or(value)
        } else {
            value.trim()
        };
        match key {
            "mode" => {
                let mode = u32::from_str_radix(value, 8).map_err(|_| SystemError::EINVAL)?;
                self.mode = Some(InodeMode::from_bits_truncate(mode & 0o7777));
            }
            "uid" | "gid" => {
                let id = Self::parse_id(value)?;
                if let Some(owner) = owner {
                    let caller = ProcessManager::current_user_ns();
                    let global = if key == "uid" {
                        make_kuid(&caller, id)?.data()
                    } else {
                        make_kgid(&caller, id)?.data()
                    };
                    let inner = owner.inner.lock();
                    let map = if key == "uid" {
                        &inner.uid_map
                    } else {
                        &inner.gid_map
                    };
                    if map_id_up(map, global as u32).is_none() {
                        return Err(SystemError::EINVAL);
                    }
                    if key == "uid" {
                        self.uid = Some(global);
                    } else {
                        self.gid = Some(global);
                    }
                }
            }
            "size" => {
                let lower = value.to_lowercase();
                let (number, multiplier) = if let Some(s) = lower.strip_suffix('g') {
                    (s, 1u64 << 30)
                } else if let Some(s) = lower.strip_suffix('m') {
                    (s, 1u64 << 20)
                } else if let Some(s) = lower.strip_suffix('k') {
                    (s, 1u64 << 10)
                } else {
                    (lower.as_str(), 1u64)
                };
                let bytes = number
                    .parse::<u64>()
                    .map_err(|_| SystemError::EINVAL)?
                    .checked_mul(multiplier)
                    .ok_or(SystemError::EINVAL)?;
                self.size_bytes = Some(
                    bytes
                        .checked_add(MMArch::PAGE_SIZE as u64 - 1)
                        .ok_or(SystemError::EINVAL)?
                        & !(MMArch::PAGE_SIZE as u64 - 1),
                );
            }
            _ => return Err(SystemError::EINVAL),
        }
        Ok(())
    }

    /// Linux fsparam_u32 uses base 0, unlike Rust's decimal parse().
    fn parse_id(value: &str) -> Result<u32, SystemError> {
        let value = value.strip_prefix('+').unwrap_or(value);
        let (digits, radix) = if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            (hex, 16)
        } else if value.starts_with('0') {
            (value, 8)
        } else {
            (value, 10)
        };
        if !digits.chars().all(|c| c.is_digit(radix)) {
            return Err(SystemError::EINVAL);
        }
        let id = u32::from_str_radix(digits, radix).map_err(|_| SystemError::EINVAL)?;
        if id == u32::MAX {
            return Err(SystemError::EINVAL);
        }
        Ok(id)
    }

    fn parse(raw: Option<&str>, owner: Option<&UserNamespace>) -> Result<Self, SystemError> {
        let mut parsed = Self::default();

        if let Some(raw) = raw {
            for opt in raw.split(',').filter(|s| !s.is_empty()) {
                let (key, value) = opt.split_once('=').ok_or(SystemError::EINVAL)?;
                parsed.parse_parameter(key, Some(value), owner)?;
            }
        }

        Ok(parsed)
    }
}

impl FileSystemMakerData for TmpfsMountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileSystem for Tmpfs {
    fn cached_find_in_view(
        &self,
        inode: &Arc<dyn IndexNode>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        inode.cached_find(name)
    }

    fn vma_open(
        &self,
        file: &Arc<File>,
        _region: crate::mm::VirtRegion,
        vm_flags: crate::mm::VmFlags,
    ) -> super::vfs::VmaOpenRollback {
        let inode = file.inode();
        let Some(inode) = inode.as_any_ref().downcast_ref::<LockedTmpfsInode>() else {
            return super::vfs::VmaOpenRollback::NotRequired;
        };
        if inode.memfd_map_open(vm_flags) {
            super::vfs::VmaOpenRollback::Close
        } else {
            super::vfs::VmaOpenRollback::NotRequired
        }
    }

    fn vma_close(
        &self,
        file: &Arc<File>,
        _region: crate::mm::VirtRegion,
        vm_flags: crate::mm::VmFlags,
    ) {
        let inode = file.inode();
        if let Some(inode) = inode.as_any_ref().downcast_ref::<LockedTmpfsInode>() {
            inode.memfd_map_close(vm_flags);
        }
    }

    fn page_cache_writeback_domain(&self) -> Option<&Arc<PageCacheWritebackDomain>> {
        self.writeback_domain.as_ref()
    }

    fn supports_reliable_flush(&self) -> bool {
        // tmpfs has no crash-surviving backing state. A loop image stored here
        // disappears as a whole on power loss, so there is no partially
        // durable post-crash image for JBD2 to recover.
        true
    }

    unsafe fn fault(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
    ) -> crate::mm::VmFaultReason {
        // tmpfs 是纯 page-cache 后端，不应走 pread/磁盘路径。
        PageFaultHandler::pagecache_fault_zero(pfm)
    }

    unsafe fn page_mkwrite(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
    ) -> crate::mm::VmFaultReason {
        PageFaultHandler::filemap_page_mkwrite(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> crate::mm::VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: TMPFS_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "tmpfs"
    }

    fn supports_idmapped_mounts(&self) -> bool {
        true
    }

    fn mount_owner_user_ns(&self) -> Option<Arc<UserNamespace>> {
        Some(self.owner_user_ns.clone())
    }

    fn proc_show_mount_options(
        &self,
        _mount: &super::vfs::mount::MountFS,
        out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        let mode = *self.mount_mode.read();
        let mut separator = "";
        if mode != Self::default_root_mode() {
            write!(out, "mode={:03o}", mode.bits() & 0o7777).map_err(|_| SystemError::EINVAL)?;
            separator = ",";
        }
        if self.root_uid != 0 {
            write!(out, "{}uid={}", separator, self.root_uid).map_err(|_| SystemError::EINVAL)?;
            separator = ",";
        }
        if self.root_gid != 0 {
            write!(out, "{}gid={}", separator, self.root_gid).map_err(|_| SystemError::EINVAL)?;
        }
        Ok(())
    }

    fn super_block(&self) -> SuperBlock {
        let limit = self.size_limit.read();
        let mut sb = self.super_block.read().clone();
        if let Some(limit) = *limit {
            let current = self.current_size.load(Ordering::Acquire);
            let total_blocks = limit / TMPFS_BLOCK_SIZE;
            let used_blocks = Self::bytes_to_blocks_ceil(current);
            let free_blocks = total_blocks.saturating_sub(used_blocks);
            sb.blocks = total_blocks;
            sb.bfree = free_blocks;
            sb.bavail = free_blocks;
            sb.frsize = TMPFS_BLOCK_SIZE;
        }
        sb
    }

    fn support_readahead(&self) -> bool {
        // tmpfs 是内存文件系统，数据已经在 page_cache 中，不需要 readahead
        false
    }

    fn reconfigure(&self, request: FsReconfigureRequest<'_>) -> Result<MountFlags, SystemError> {
        // fsconfig validated identity options at SET time. They are not
        // reapplied on remount, nor reinterpreted under the commit caller.
        let owner = request.oldapi.then_some(self.owner_user_ns.as_ref());
        let parsed = TmpfsMountData::parse(request.raw_data, owner)?;

        if let Some(new_limit) = parsed.size_bytes {
            let mut limit = self.size_limit.write();
            let current = self.current_size.load(Ordering::Acquire);
            if new_limit < current {
                return Err(SystemError::EINVAL);
            }
            *limit = Some(new_limit);
        }

        Ok(request.sb_flags & request.sb_flags_mask)
    }

    fn validate_reconfigure_parameter(
        &self,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), SystemError> {
        TmpfsMountData::default().parse_parameter(key, value, Some(&self.owner_user_ns))
    }
}

impl Tmpfs {
    fn default_root_mode() -> InodeMode {
        InodeMode::S_IRWXUGO | InodeMode::S_ISVTX
    }
    #[inline]
    fn default_size_bytes() -> usize {
        // 与 /proc/meminfo 一致：从帧分配器获取物理内存总量。
        let total = unsafe { LockedFrameAllocator.usage() }.total().bytes();
        let half = total / 2;
        half.clamp(TMPFS_DEFAULT_MIN_SIZE_BYTES, TMPFS_DEFAULT_MAX_SIZE_BYTES)
    }

    #[inline]
    fn bytes_to_blocks_ceil(bytes: u64) -> u64 {
        bytes.div_ceil(TMPFS_BLOCK_SIZE)
    }

    pub fn new(mount_data: &TmpfsMountData) -> Arc<Self> {
        Self::new_in_context(mount_data, 0, 0, INIT_USER_NAMESPACE.clone())
    }

    fn new_in_context(
        mount_data: &TmpfsMountData,
        uid: usize,
        gid: usize,
        owner: Arc<UserNamespace>,
    ) -> Arc<Self> {
        // 若未指定 size=，使用默认容量策略（通常为物理内存的一半）。
        // 这样 busybox df -h（默认过滤 f_blocks==0）就能显示 /tmp。
        let size_limit = mount_data
            .size_bytes
            .or_else(|| Some(Self::default_size_bytes() as u64));
        Self::new_with_size_limit(
            mount_data.mode.unwrap_or_else(Self::default_root_mode),
            size_limit,
            Some(PageCacheWritebackDomain::new()),
            mount_data.uid.unwrap_or(uid),
            mount_data.gid.unwrap_or(gid),
            owner,
        )
    }

    fn new_with_size_limit(
        mode: InodeMode,
        size_limit: Option<u64>,
        writeback_domain: Option<Arc<PageCacheWritebackDomain>>,
        uid: usize,
        gid: usize,
        owner: Arc<UserNamespace>,
    ) -> Arc<Self> {
        let mut sb = SuperBlock::new(
            Magic::TMPFS_MAGIC,
            TMPFS_BLOCK_SIZE,
            TMPFS_MAX_NAMELEN as u64,
        );
        sb.frsize = TMPFS_BLOCK_SIZE;
        if let Some(size) = size_limit {
            let blocks = size / TMPFS_BLOCK_SIZE;
            sb.blocks = blocks;
            sb.bfree = blocks;
            sb.bavail = blocks;
        }

        let root: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new(TmpfsInode::new()));

        let result: Arc<Tmpfs> = Arc::new(Tmpfs {
            root_inode: root,
            writeback_domain,
            super_block: RwSem::new(sb),
            size_limit: RwSem::new(size_limit),
            current_size: AtomicU64::new(0),
            mount_mode: RwSem::new(mode),
            root_uid: uid,
            root_gid: gid,
            owner_user_ns: owner,
        });

        let mut root_guard: MutexGuard<TmpfsInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        root_guard.metadata.mode = mode;
        root_guard.metadata.uid = uid;
        root_guard.metadata.gid = gid;
        drop(root_guard);

        result
    }

    fn new_internal_shmem(mode: Option<InodeMode>) -> Arc<Self> {
        Self::new_with_size_limit(
            mode.unwrap_or(InodeMode::S_IRWXUGO),
            None,
            None,
            0,
            0,
            INIT_USER_NAMESPACE.clone(),
        )
    }

    /// 原子地增加文件系统使用的大小
    /// 返回Ok(())如果更新成功，Err(SystemError::ENOSPC)如果超过限制
    /// 使用compare_exchange_weak循环确保并发安全
    fn increase_size(&self, size_diff: u64) -> Result<(), SystemError> {
        let size_limit = self.size_limit.read();
        if let Some(limit) = *size_limit {
            // 使用compare_exchange_weak循环确保原子性
            loop {
                let current = self.current_size.load(Ordering::Acquire);
                let new_total = current.saturating_add(size_diff);

                if new_total > limit {
                    return Err(SystemError::ENOSPC);
                }

                // 原子地更新，如果current没有被其他线程修改，则更新成功
                match self.current_size.compare_exchange_weak(
                    current,
                    new_total,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,     // 更新成功
                    Err(_) => continue, // 被其他线程修改，重试
                }
            }
        }
        Ok(())
    }

    /// 原子地减少文件系统当前使用的大小（用于文件删除或缩小）
    /// 使用fetch_sub确保并发安全
    fn decrease_size(&self, size: usize) {
        let size_limit = self.size_limit.read();
        if size_limit.is_some() {
            let size_to_decrease = size as u64;
            loop {
                let current = self.current_size.load(Ordering::Acquire);
                let new = current.saturating_sub(size_to_decrease);
                if self
                    .current_size
                    .compare_exchange_weak(current, new, Ordering::Release, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
        }
    }

    fn available_pages(&self) -> Option<usize> {
        let size_limit = self.size_limit.read();
        (*size_limit).map(|limit| {
            let current = self.current_size.load(Ordering::Acquire);
            (limit.saturating_sub(current) / MMArch::PAGE_SIZE as u64) as usize
        })
    }

    fn create_unlinked_shmem_inode(
        self: &Arc<Self>,
        name: DName,
        mode: InodeMode,
        size: usize,
        memfd: Option<(Vec<u8>, u32)>,
    ) -> Result<Arc<TmpfsShmemFile>, SystemError> {
        if size > i64::MAX as usize {
            return Err(SystemError::EOVERFLOW);
        }
        // Logical size is not resident tmpfs quota. PageCache membership
        // reserves/releases actual pages, including creation failure rollback.
        let inode_id = generate_inode_id();
        let cred = ProcessManager::current_pcb().cred();
        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new_with_memfd(
            TmpfsInode {
                parent: Weak::default(),
                self_ref: Weak::default(),
                children: BTreeMap::new(),
                page_cache: None,
                metadata: Metadata {
                    dev_id: 0,
                    inode_id,
                    size: size as i64,
                    blk_size: TMPFS_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type: FileType::File,
                    mode,
                    flags: InodeFlags::empty(),
                    nlinks: 0,
                    uid: cred.fsuid.data(),
                    gid: cred.fsgid.data(),
                    raw_dev: DeviceNumber::default(),
                },
                fs: Arc::downgrade(self),
                special_node: None,
                inline_symlink: None,
                name,
                tmpfile_linkable: false,
            },
            memfd,
        ));

        result.0.lock().self_ref = Arc::downgrade(&result);
        let inode_dyn: Arc<dyn IndexNode> = result.clone();
        let backend = Arc::new(TmpfsPageCacheBackend::new(
            Arc::downgrade(&inode_dyn),
            Arc::downgrade(self),
        ));
        let pc = new_tmpfs_page_cache(Arc::downgrade(&inode_dyn), backend, &Arc::downgrade(self))?;
        result.0.lock().page_cache = Some(pc.clone());

        let file = Arc::new(File::new_pseudo(
            inode_dyn,
            FileFlags::O_RDWR | FileFlags::O_LARGEFILE,
        )?);
        Ok(Arc::new(TmpfsShmemFile { file }))
    }
}

lazy_static! {
    // Bare tmpfs inodes hold a Weak filesystem reference. Keep the private
    // mount alive independently of IPC namespaces, files and VMAs.
    static ref INTERNAL_SHMEM_TMPFS: Arc<Tmpfs> = Tmpfs::new_internal_shmem(Some(InodeMode::S_IRWXUGO));
}

/// Create a fixed-size, unlinked shmem object without publishing a path or fd.
/// The file/inode owns its PageCache; retaining just file() is sufficient.
/// `name` is diagnostic only and must be chosen by the kernel caller.
pub fn create_unlinked_shmem_file(
    name: &str,
    size: usize,
) -> Result<Arc<TmpfsShmemFile>, SystemError> {
    INTERNAL_SHMEM_TMPFS.create_unlinked_shmem_inode(
        DName::from(name),
        InodeMode::S_IFREG | InodeMode::S_IRWXUGO,
        size,
        None,
    )
}

/// Construct a user-visible memfd on the existing private shmem filesystem.
/// The caller owns fd allocation; a failed allocation simply drops the inode.
pub fn create_memfd_file(
    user_name: Vec<u8>,
    allow_sealing: bool,
    noexec_seal: bool,
) -> Result<Arc<File>, SystemError> {
    let initial_seals = if noexec_seal {
        F_SEAL_EXEC
    } else if allow_sealing {
        0
    } else {
        F_SEAL_SEAL
    };
    let mode = if noexec_seal {
        InodeMode::S_IFREG
            | InodeMode::S_IRUSR
            | InodeMode::S_IWUSR
            | InodeMode::S_IRGRP
            | InodeMode::S_IWGRP
            | InodeMode::S_IROTH
            | InodeMode::S_IWOTH
    } else {
        InodeMode::S_IFREG | InodeMode::S_IRWXUGO
    };
    let shmem = INTERNAL_SHMEM_TMPFS.create_unlinked_shmem_inode(
        DName::from("memfd"),
        mode,
        0,
        Some((user_name, initial_seals)),
    )?;
    Ok(shmem.file())
}

impl MountableFileSystem for Tmpfs {
    const SUPPORTS_USERNS_MOUNT: bool = true;
    const SUPPORTS_FSCONFIG_LEGACY_OPTIONS: bool = true;

    fn validate_fsconfig_parameter(key: &str, value: Option<&str>) -> Result<(), SystemError> {
        TmpfsMountData::default().parse_parameter(key, value, None)
    }

    fn prepare_fsconfig_string(
        key: &str,
        value: &str,
        previous: Option<&FsconfigPreparedData>,
        owner: &UserNamespace,
    ) -> Result<Option<FsconfigPreparedData>, SystemError> {
        let mut parsed = match previous {
            Some(data) => data
                .downcast_ref::<TmpfsMountData>()
                .ok_or(SystemError::EINVAL)?
                .clone(),
            None => TmpfsMountData::default(),
        };
        parsed.parse_parameter(key, Some(value), Some(owner))?;
        Ok(Some(Arc::new(parsed)))
    }

    fn make_mount_data_in_context(
        raw_data: Option<&str>,
        _source: &str,
        context: &FsCreationContext,
    ) -> Result<Option<Arc<dyn FileSystemMakerData>>, SystemError> {
        let parsed = match &context.fsconfig_prepared {
            Some(data) => data
                .downcast_ref::<TmpfsMountData>()
                .ok_or(SystemError::EINVAL)?
                .clone(),
            None => TmpfsMountData::parse(raw_data, Some(&context.cred.user_ns))?,
        };
        Ok(Some(Arc::new(parsed)))
    }

    fn make_fs_in_context(
        data: Option<&dyn FileSystemMakerData>,
        _flags: MountFlags,
        context: &FsCreationContext,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        let data = data
            .ok_or(SystemError::EINVAL)?
            .as_any()
            .downcast_ref::<TmpfsMountData>()
            .ok_or(SystemError::EINVAL)?;
        Ok(Self::new_in_context(
            data,
            context.cred.fsuid.data(),
            context.cred.fsgid.data(),
            context.cred.user_ns.clone(),
        ))
    }

    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let parsed = TmpfsMountData::parse(raw_data, Some(&INIT_USER_NAMESPACE))?;
        Ok(Some(Arc::new(parsed)))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let d = data
            .ok_or(SystemError::EINVAL)?
            .as_any()
            .downcast_ref::<TmpfsMountData>()
            .ok_or(SystemError::EINVAL)?;
        Ok(Tmpfs::new(d))
    }
}

register_mountable_fs!(Tmpfs, TMPFSMAKER, "tmpfs");

impl LockedTmpfsInode {
    fn unlink_with_operation_context(
        &self,
        name: &str,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<LinkRemovalOutcome, SystemError> {
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        crate::filesystem::vfs::permission::check_inode_delete(
            &inode.metadata,
            &deleted_inode.metadata,
            false,
            context,
        )?;

        drop(deleted_inode);

        let mut deleted_guard = to_delete.0.lock();
        deleted_guard.metadata.nlinks = deleted_guard
            .metadata
            .nlinks
            .checked_sub(1)
            .expect("tempfs nlinks underflow: filesystem corruption detected");
        let outcome = if deleted_guard.metadata.nlinks == 0 {
            LinkRemovalOutcome::LastLink
        } else {
            LinkRemovalOutcome::StillLinked
        };

        let now = PosixTimeSpec::now();
        deleted_guard.metadata.ctime = now;
        drop(deleted_guard);

        inode.children.remove(&name);
        tmpfs_touch_dir(&mut inode, now);

        Ok(outcome)
    }
}

impl LockedTmpfsInode {
    fn rmdir_with_operation_context(
        &self,
        name: &str,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<(), SystemError> {
        // 检查是否为 "." 或 ".."
        if name == "." {
            return Err(SystemError::EINVAL);
        }
        if name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        crate::filesystem::vfs::permission::check_inode_delete(
            &inode.metadata,
            &deleted_inode.metadata,
            true,
            context,
        )?;

        // 检查目录是否为空（排除 "." 和 ".."）
        if !deleted_inode.children.is_empty() {
            return Err(SystemError::ENOTEMPTY);
        }

        drop(deleted_inode);
        let now = PosixTimeSpec::now();
        let mut deleted_inode = to_delete.0.lock();
        deleted_inode.metadata.nlinks = 0;
        deleted_inode.metadata.ctime = now;
        drop(deleted_inode);
        inode.children.remove(&name);
        inode.metadata.nlinks -= 1;
        tmpfs_touch_dir(&mut inode, now);

        Ok(())
    }
}

impl LockedTmpfsInode {
    fn resize_with_operation(
        &self,
        len: usize,
        request: Option<(
            &Metadata,
            SetMetadataMask,
            &crate::filesystem::vfs::permission::InodeOpContext,
        )>,
    ) -> Result<SetMetadataMask, SystemError> {
        let _size_guard = self.1.write();
        let mut applied = SetMetadataMask::empty();
        let (old_size, new_size, page_cache) = {
            let mut inode = self.0.lock();
            if inode.metadata.file_type != FileType::File {
                return Err(SystemError::EINVAL);
            }

            let old_size = inode.metadata.size as usize;
            let new_size = len;
            let prepared = request
                .map(|(requested, mask, context)| {
                    crate::filesystem::vfs::vcore::prepare_backing_resize_metadata(
                        context,
                        &inode.metadata,
                        requested,
                        mask,
                        len,
                    )
                })
                .transpose()?;
            if let Some(state) = self.3.as_ref() {
                state.lock().check_resize(old_size, new_size)?;
            }

            if let Some((metadata, mask)) = prepared {
                if mask.contains(SetMetadataMask::MODE) {
                    self.check_exec_mode_change(inode.metadata.mode, metadata.mode)?;
                }
                crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, &metadata, mask);
                applied = mask;
            }

            // Linux truncate_setsize() writes the new i_size before truncating page cache.
            // Drop the inode lock before page-cache unmap/truncate so page faults do not
            // form an inode-lock/MM-lock ABBA with the truncate path.
            inode.metadata.size = len as i64;
            (old_size, new_size, inode.page_cache.clone())
        };

        if new_size < old_size {
            if let Some(pc) = page_cache {
                pc.manager().resize(len)?;
            }
        }

        Ok(applied)
    }
}

impl IndexNode for LockedTmpfsInode {
    fn link_mutation_coordinator(&self) -> Option<&LinkMutationCoordinator> {
        Some(&self.2)
    }

    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn append_lock_fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.fs())
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn prepare_mmap_file(
        &self,
        _file: &Arc<File>,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<(crate::mm::VmFlags, bool), SystemError> {
        self.prepare_memfd_mmap(vm_flags)
    }

    fn finish_mmap_prepare(&self) {
        self.finish_memfd_mmap_prepare()
    }

    fn mmap_file(
        &self,
        file: &Arc<File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<Arc<File>, SystemError> {
        self.mmap(start, len, offset)?;
        self.memfd_map_open(vm_flags);
        Ok(file.clone())
    }

    fn get_seals(&self) -> Result<u32, SystemError> {
        LockedTmpfsInode::get_seals(self)
    }

    fn add_seals(&self, seals: u32) -> Result<(), SystemError> {
        LockedTmpfsInode::add_seals(self, seals)
    }

    fn proc_fd_link_target(&self) -> Option<Vec<u8>> {
        self.4.as_ref().map(|name| {
            let mut target = Vec::with_capacity(7 + name.len() + 10);
            target.extend_from_slice(b"/memfd:");
            target.extend_from_slice(name);
            target.extend_from_slice(b" (deleted)");
            target
        })
    }

    fn begin_remote_write(&self) -> bool {
        self.pin_memfd_remote_write()
    }

    fn end_remote_write(&self) {
        self.unpin_memfd_remote_write()
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }
        drop(inode);
        // 复用 resize，保证扩展/收缩两侧逻辑一致
        self.resize(len)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn sync_file(
        &self,
        datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        match self.metadata()?.file_type {
            FileType::File | FileType::Dir => {
                if datasync {
                    self.datasync()
                } else {
                    self.sync()
                }
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        match self.metadata()?.file_type {
            FileType::File | FileType::Dir => {
                if let Some(page_cache) = self.page_cache() {
                    let start_index = start >> MMArch::PAGE_SHIFT;
                    let end_index = end >> MMArch::PAGE_SHIFT;
                    page_cache
                        .manager()
                        .writeback_range(start_index, end_index)?;
                }
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        let file_size = inode.metadata.size as usize;
        if let Some(target) = inode.inline_symlink.clone() {
            drop(inode);
            let read_len = if offset < target.len() {
                core::cmp::min(target.len() - offset, len)
            } else {
                0
            };
            if read_len > 0 {
                buf[..read_len].copy_from_slice(&target.as_bytes()[offset..offset + read_len]);
            }
            return Ok(read_len);
        }
        let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
        drop(inode);

        // 计算实际读取长度
        let read_len = if offset < file_size {
            core::cmp::min(file_size - offset, len)
        } else {
            0
        };

        if read_len == 0 {
            return Ok(0);
        }

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + read_len - 1) >> MMArch::PAGE_SHIFT;
        // 两阶段读取：
        // 1) 持有 page_cache 锁：只做“取页/建页 + 收集引用”，绝不触碰用户缓冲区
        // 2) 释放 page_cache 锁：再把页内容拷贝到用户缓冲区（并做 prefault）
        struct ReadItem {
            page: Option<Arc<Page>>,
            page_offset: usize,
            sub_len: usize,
        }

        let mut items: Vec<ReadItem> = Vec::new();
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let read_start = core::cmp::max(offset, page_start);
            let read_end = core::cmp::min(offset + read_len, page_end);
            let page_read_len = read_end.saturating_sub(read_start);
            if page_read_len == 0 {
                continue;
            }

            // Reading a sparse tmpfs hole returns zeroes without allocating a
            // page or consuming the mount's block quota.
            let page = page_cache.manager().peek_page(page_index);

            items.push(ReadItem {
                page,
                page_offset: read_start - page_start,
                sub_len: page_read_len,
            });
        }

        let mut dst_off = 0usize;
        for it in items {
            if it.sub_len == 0 {
                continue;
            }

            // prefault：避免在任何锁持有期间缺页（SelfRead 的关键）
            let v = volatile_read!(buf[dst_off]);
            volatile_write!(buf[dst_off], v);
            let v = volatile_read!(buf[dst_off + it.sub_len - 1]);
            volatile_write!(buf[dst_off + it.sub_len - 1], v);

            if let Some(page) = it.page {
                let page_guard = page.read();
                unsafe {
                    buf[dst_off..dst_off + it.sub_len].copy_from_slice(
                        &page_guard.as_slice()[it.page_offset..it.page_offset + it.sub_len],
                    );
                }
            } else {
                buf[dst_off..dst_off + it.sub_len].fill(0);
            }
            dst_off += it.sub_len;
        }

        Ok(read_len)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mut publish = || {};
        let mut stage = super::vfs::WritePrivilegeStage::new(&mut publish);
        self.write_at_with_privilege_stage(offset, len, buf, data, &mut stage)
    }

    fn write_at_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, SystemError> {
        self.write_at_with_inode_context(
            offset,
            len,
            buf,
            data,
            &super::vfs::permission::InodeOpContext::legacy(),
            stage,
        )
    }

    fn write_at_with_inode_context(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
        context: &super::vfs::permission::InodeOpContext,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, SystemError> {
        self.write_primary_with_inode_context(
            offset,
            len,
            super::vfs::PrimaryWriteSource::Kernel(buf),
            data,
            context,
            stage,
        )
    }

    fn write_primary_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: super::vfs::PrimaryWriteSource<'_, '_>,
        data: MutexGuard<FilePrivateData>,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, SystemError> {
        self.write_primary_with_inode_context(
            offset,
            len,
            source,
            data,
            &super::vfs::permission::InodeOpContext::legacy(),
            stage,
        )
    }

    fn write_primary_with_inode_context(
        &self,
        offset: usize,
        len: usize,
        source: super::vfs::PrimaryWriteSource<'_, '_>,
        _data: MutexGuard<FilePrivateData>,
        context: &super::vfs::permission::InodeOpContext,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, SystemError> {
        if let super::vfs::PrimaryWriteSource::Kernel(buffer) = &source {
            if buffer.len() < len {
                return Err(SystemError::EINVAL);
            }
        }
        if len == 0 {
            return Ok(0);
        }
        let content = self.1.read();
        let (page_cache, permitted_len) =
            self.prepare_primary_write_locked(offset, len, context, stage)?;
        source.with_buffer(permitted_len, |buffer| {
            self.write_data_locked(
                offset,
                permitted_len.min(buffer.len()),
                buffer,
                &page_cache,
                &content,
            )
        })
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let (mut metadata, cache) = {
            let inode = self.0.lock();
            (inode.metadata.clone(), inode.page_cache.clone())
        };
        if let Some(cache) = cache {
            metadata.blocks = cache
                .manager()
                .pages_count()?
                .checked_mul(MMArch::PAGE_SIZE / 512)
                .ok_or(SystemError::EOVERFLOW)?;
        }
        Ok(metadata)
    }

    fn cached_metadata(&self) -> Result<Metadata, SystemError> {
        let (mut metadata, cache) = {
            let inode = self
                .0
                .try_lock()
                .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
            (inode.metadata.clone(), inode.page_cache.clone())
        };
        if let Some(cache) = cache {
            metadata.blocks = cache
                .try_pages_count()?
                .checked_mul(MMArch::PAGE_SIZE / 512)
                .ok_or(SystemError::EOVERFLOW)?;
        }
        Ok(metadata)
    }

    fn cached_symlink_target(&self) -> Result<String, SystemError> {
        self.0
            .try_lock()
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?
            .inline_symlink
            .clone()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let _guard = self.1.write();
        let mut inode = self.0.lock();
        self.check_exec_mode_change(inode.metadata.mode, metadata.mode)?;
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;
        Ok(())
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        let _guard = self.1.write();
        let mut inode = self.0.lock();
        if mask.contains(SetMetadataMask::MODE) {
            self.check_exec_mode_change(inode.metadata.mode, metadata.mode)?;
        }
        crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, metadata, mask);
        Ok(())
    }

    fn update_metadata_masked(
        &self,
        update: &mut MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, SystemError> {
        let _guard = self.1.write();
        let current = self.0.lock().metadata.clone();
        let (requested, mask) = update(&current)?;
        let mut inode = self.0.lock();
        if mask.contains(SetMetadataMask::MODE) {
            self.check_exec_mode_change(inode.metadata.mode, requested.mode)?;
        }
        crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, &requested, mask);
        Ok(mask)
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.resize_with_operation(len, None).map(|_| ())
    }

    fn resize_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.resize_with_metadata_context(
            len,
            lock_owner,
            metadata,
            mask,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
        .map(|_| ())
    }

    fn resize_with_metadata_context(
        &self,
        len: usize,
        _lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<SetMetadataMask, SystemError> {
        self.resize_with_operation(len, Some((metadata, mask, context)))
    }

    fn resize_file_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.resize_file_with_metadata_context(
            len,
            lock_owner,
            data,
            metadata,
            mask,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
        .map(|_| ())
    }

    fn resize_file_with_metadata_context(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<SetMetadataMask, SystemError> {
        drop(data);
        self.resize_with_metadata_context(len, lock_owner, metadata, mask, context)
    }

    fn resize_open_truncate_with_inode_context(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        truncate: &crate::filesystem::vfs::OpenTruncateContext,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<SetMetadataMask, SystemError> {
        self.resize_file_with_metadata_context(
            len,
            lock_owner,
            data,
            &truncate.requested,
            truncate.mask,
            context,
        )
    }

    fn resize_with_metadata_result(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<SetMetadataMask, SystemError> {
        self.resize_with_metadata_context(
            len,
            lock_owner,
            metadata,
            mask,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn resize_file_with_metadata_result(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<SetMetadataMask, SystemError> {
        self.resize_file_with_metadata_context(
            len,
            lock_owner,
            data,
            metadata,
            mask,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn resize_open_truncate_result(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        truncate: &crate::filesystem::vfs::OpenTruncateContext,
    ) -> Result<SetMetadataMask, SystemError> {
        self.resize_open_truncate_with_inode_context(
            len,
            lock_owner,
            data,
            truncate,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        attrib: &mut crate::filesystem::vfs::AttribStageObserver<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.fallocate_file_with_inode_context(
            mode,
            offset,
            len,
            lock_owner,
            attrib,
            data,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn fallocate_file_with_inode_context(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        attrib: &mut crate::filesystem::vfs::AttribStageObserver<'_>,
        data: MutexGuard<FilePrivateData>,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<(), SystemError> {
        drop(data);
        const KEEP_SIZE: i32 = 0x01;
        const PUNCH_HOLE: i32 = 0x02;
        if mode != 0 && mode != KEEP_SIZE && mode != (KEEP_SIZE | PUNCH_HOLE) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if len == 0 {
            return Err(SystemError::EINVAL);
        }
        let end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        if end > isize::MAX as usize {
            return Err(SystemError::EFBIG);
        }
        crate::filesystem::vfs::vcore::check_file_size_limit(end)?;

        let _size_guard = self.1.write();
        if mode & PUNCH_HOLE != 0 {
            let (page_cache, size) = {
                let inode = self.0.lock();
                if let Some(state) = self.3.as_ref() {
                    if state.lock().bits & (seals::F_SEAL_WRITE | seals::F_SEAL_FUTURE_WRITE) != 0 {
                        return Err(SystemError::EPERM);
                    }
                }
                (
                    inode.page_cache.clone().ok_or(SystemError::EIO)?,
                    inode.metadata.size.max(0) as usize,
                )
            };
            if offset < size {
                page_cache.punch_hole(offset, end.min(size))?;
            }
            if tmpfs_fallocate_modified(&mut self.0.lock(), context) {
                attrib.commit();
            }
            let _ = lock_owner;
            return Ok(());
        }
        let (page_cache, fs) = {
            let inode = self.0.lock();
            if let Some(state) = self.3.as_ref() {
                if end > inode.metadata.size as usize && state.lock().bits & seals::F_SEAL_GROW != 0
                {
                    return Err(SystemError::EPERM);
                }
            }
            let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
            let fs = inode.fs.upgrade().ok_or(SystemError::EIO)?;
            (page_cache, fs)
        };
        let first = offset >> MMArch::PAGE_SHIFT;
        let last = (end - 1) >> MMArch::PAGE_SHIFT;
        let missing_pages = page_cache.manager().missing_pages_in_range(first, last)?;
        if fs
            .available_pages()
            .is_some_and(|available| missing_pages > available)
        {
            return Err(SystemError::ENOSPC);
        }
        page_cache.manager().preallocate_range(first, last)?;

        // Linux shmem commits file_modified() only after allocation succeeds.
        // Compute from the current metadata while holding the inode lock so a
        // concurrent chmod cannot be overwritten by a pre-allocation snapshot.
        let mode_changed = {
            let mut inode = self.0.lock();
            let effective_size = if mode & KEEP_SIZE != 0 {
                inode.metadata.size.max(0) as usize
            } else {
                core::cmp::max(inode.metadata.size.max(0) as usize, end)
            };
            inode.metadata.size = effective_size as i64;
            tmpfs_fallocate_modified(&mut inode, context)
        };
        if mode_changed {
            attrib.commit();
        }
        let _ = lock_owner;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.symlink_with_context(
            name,
            target,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn symlink_with_context(
        &self,
        name: &str,
        target: &str,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        const SHORT_SYMLINK_LEN: usize = 128;

        if target
            .len()
            .checked_add(1)
            .ok_or(SystemError::ENAMETOOLONG)?
            > MMArch::PAGE_SIZE
        {
            return Err(SystemError::ENAMETOOLONG);
        }

        let name = DName::from(name);
        let mut parent = self.0.lock();
        tmpfs_require_live_dir(&parent)?;
        if parent.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }
        crate::filesystem::vfs::permission::check_parent_create(&parent.metadata, context, false)?;
        let init = crate::filesystem::vfs::permission::child_inode_init_with_context(
            &parent.metadata,
            FileType::SymLink,
            InodeMode::S_IRWXUGO,
            context,
        )?;

        let now = PosixTimeSpec::now();
        let inline = target.len() < SHORT_SYMLINK_LEN;
        let result = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: parent.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: target.len() as i64,
                blk_size: TMPFS_BLOCK_SIZE as usize,
                blocks: 0, // Project the actual long-symlink cache allocation in metadata().
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type: FileType::SymLink,
                mode: init.mode,
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::default(),
            },
            fs: parent.fs.clone(),
            special_node: None,
            inline_symlink: inline.then(|| target.to_string()),
            name: name.clone(),
            tmpfile_linkable: false,
        }));
        result.0.lock().self_ref = Arc::downgrade(&result);

        if !inline {
            let inode_dyn: Arc<dyn IndexNode> = result.clone();
            let backend = Arc::new(TmpfsPageCacheBackend::new(
                Arc::downgrade(&inode_dyn),
                parent.fs.clone(),
            ));
            let page_cache = new_tmpfs_page_cache(Arc::downgrade(&inode_dyn), backend, &parent.fs)?;
            result.0.lock().page_cache = Some(page_cache.clone());
            let page = page_cache.manager().commit_overwrite(0)?;
            let mut page_guard = page.write();
            unsafe {
                page_guard.as_slice_mut()[..target.len()].copy_from_slice(target.as_bytes());
            }
            page_guard.add_flags(crate::mm::page::PageFlags::PG_DIRTY);
            if let Err(error) = page_cache.mark_page_dirty_page_locked(0, &page_guard) {
                page_guard.remove_flags(crate::mm::page::PageFlags::PG_DIRTY);
                return Err(error);
            }
        }

        parent.children.insert(name, result.clone());
        tmpfs_touch_dir(&mut parent, now);
        Ok(result)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.create_with_data_context(
            name,
            file_type,
            mode,
            data,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn create_with_context(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.create_with_data_context(name, file_type, mode, 0, context)
    }

    fn mkdir_with_context(
        &self,
        name: &str,
        mode: InodeMode,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.create_with_context(name, FileType::Dir, mode, context)
    }

    fn create_with_data_context(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let name = DName::from(name);
        let mut inode = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }
        crate::filesystem::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        let init = crate::filesystem::vfs::permission::child_inode_init_with_context(
            &inode.metadata,
            file_type,
            mode,
            context,
        )?;

        let now = PosixTimeSpec::now();
        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type,
                mode: init.mode,
                flags: InodeFlags::empty(),
                nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::from(data as u32),
            },
            fs: inode.fs.clone(),
            special_node: None,
            inline_symlink: None,
            name: name.clone(),
            tmpfile_linkable: false,
        }));

        result.0.lock().self_ref = Arc::downgrade(&result);

        // tmpfs 中：普通文件和符号链接都需要可读写的数据存储。
        // 目前 VFS 使用 read_at/write_at 来读写 symlink 内容（readlink/symlink 语义），
        // 因此 symlink 也必须有 page_cache 后端，否则会在 write_at/read_at 返回 EIO。
        if file_type == FileType::File || file_type == FileType::SymLink {
            let backend = Arc::new(TmpfsPageCacheBackend::new(
                Arc::downgrade(&result) as Weak<dyn IndexNode>,
                inode.fs.clone(),
            ));
            let pc = new_tmpfs_page_cache(
                Arc::downgrade(&result) as Weak<dyn IndexNode>,
                backend,
                &inode.fs,
            )?;
            result.0.lock().page_cache = Some(pc);
        }

        inode.children.insert(name, result.clone());
        if file_type == FileType::Dir {
            inode.metadata.nlinks += 1;
        }
        tmpfs_touch_dir(&mut inode, now);
        Ok(result)
    }

    fn tmpfile(
        &self,
        mode: InodeMode,
        flags: &FileFlags,
    ) -> Result<super::vfs::UnlinkedFile, SystemError> {
        self.tmpfile_with_context(
            mode,
            flags,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn tmpfile_with_context(
        &self,
        mode: InodeMode,
        flags: &FileFlags,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<super::vfs::UnlinkedFile, SystemError> {
        let parent = self.0.lock();
        // O_TMPFILE creates no child name. Linux permits it through a dirfd
        // whose directory was removed after the fd was opened.
        crate::filesystem::vfs::permission::check_parent_create(&parent.metadata, context, true)?;
        let init = crate::filesystem::vfs::permission::child_inode_init_with_context(
            &parent.metadata,
            FileType::File,
            mode,
            context,
        )?;
        let now = PosixTimeSpec::now();
        let inode_id = generate_inode_id();
        let fs = parent.fs.clone();
        let result = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: parent.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id,
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type: FileType::File,
                mode: init.mode,
                flags: InodeFlags::empty(),
                nlinks: 0,
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::default(),
            },
            fs: fs.clone(),
            special_node: None,
            inline_symlink: None,
            name: DName::from(format!("#{}", inode_id.data())),
            tmpfile_linkable: !flags.contains(FileFlags::O_EXCL),
        }));
        result.0.lock().self_ref = Arc::downgrade(&result);
        let inode_dyn: Arc<dyn IndexNode> = result.clone();
        let backend = Arc::new(TmpfsPageCacheBackend::new(
            Arc::downgrade(&inode_dyn),
            fs.clone(),
        ));
        let page_cache = new_tmpfs_page_cache(Arc::downgrade(&inode_dyn), backend, &fs)?;
        result.0.lock().page_cache = Some(page_cache);
        super::vfs::UnlinkedFile::new(inode_dyn)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.link_with_inode_context(
            name,
            other,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn link_with_inode_context(
        &self,
        name: &str,
        other: &Arc<dyn IndexNode>,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<(), SystemError> {
        // downcast 用于获取类型特定功能（跨文件系统检查已在 VFS 层完成）
        let other: &LockedTmpfsInode = other
            .downcast_ref::<LockedTmpfsInode>()
            .ok_or(SystemError::EINVAL)?;
        let name = DName::from(name);
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        let mut other_locked: MutexGuard<TmpfsInode> = other.0.lock();

        tmpfs_require_live_dir(&inode)?;
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        crate::filesystem::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        crate::filesystem::vfs::permission::check_inode_link_source(
            &other_locked.metadata,
            context,
        )?;

        if other_locked.metadata.nlinks == 0 && !other_locked.tmpfile_linkable {
            return Err(SystemError::ENOENT);
        }

        inode
            .children
            .insert(name, other_locked.self_ref.upgrade().unwrap());
        other_locked.metadata.nlinks += 1;
        other_locked.tmpfile_linkable = false;
        let now = PosixTimeSpec::now();
        other_locked.metadata.ctime = now;
        tmpfs_touch_dir(&mut inode, now);
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<LinkRemovalOutcome, SystemError> {
        self.unlink_with_operation_context(
            name,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn unlink_with_inode_context(
        &self,
        name: &str,
        mutation: &crate::filesystem::vfs::mount::DentryMutationContext<'_>,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<LinkRemovalOutcome, SystemError> {
        mutation.ensure_locked();
        self.unlink_with_operation_context(name, context)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.rmdir_with_operation_context(
            name,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn rmdir_with_inode_context(
        &self,
        name: &str,
        mutation: &crate::filesystem::vfs::mount::DentryMutationContext<'_>,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<(), SystemError> {
        mutation.ensure_locked();
        self.rmdir_with_operation_context(name, context)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<RenameOutcome, SystemError> {
        self.move_with_inode_context(
            old_name,
            target,
            new_name,
            flags,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn move_to_with_inode_context(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
        mutation: &crate::filesystem::vfs::mount::DentryMutationContext<'_>,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<RenameOutcome, SystemError> {
        mutation.ensure_locked();
        self.move_with_inode_context(old_name, target, new_name, flags, context)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => Ok(inode.self_ref.upgrade().ok_or(SystemError::ENOENT)?),
            ".." => Ok(inode.parent.upgrade().ok_or(SystemError::ENOENT)?),
            name => {
                let name = DName::from(name);
                Ok(inode
                    .children
                    .get(&name)
                    .ok_or(SystemError::ENOENT)?
                    .clone())
            }
        }
    }

    fn cached_find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self
            .0
            .try_lock()
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        match name {
            "" | "." => inode
                .self_ref
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            ".." => inode
                .parent
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            name => inode
                .children
                .get(&DName::from(name))
                .cloned()
                .map(|child| child as Arc<dyn IndexNode>)
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK),
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: MutexGuard<TmpfsInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match ino.into() {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            ino => {
                let mut key: Vec<String> = inode
                    .children
                    .iter()
                    .filter_map(|(k, v)| {
                        if v.0.lock().metadata.inode_id.into() == ino {
                            Some(k.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();

                match key.len() {
                    0 => Err(SystemError::ENOENT),
                    1 => Ok(key.remove(0)),
                    _ => Err(SystemError::EIO),
                }
            }
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(
            &mut self
                .0
                .lock()
                .children
                .keys()
                .map(|k| k.to_string())
                .collect(),
        );

        Ok(keys)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.mknod_with_context(
            filename,
            mode,
            dev_t,
            &crate::filesystem::vfs::permission::InodeOpContext::legacy(),
        )
    }

    fn mknod_with_context(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
        context: &crate::filesystem::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut inode = self.0.lock();
        tmpfs_require_live_dir(&inode)?;

        let file_type = FileType::from(mode);
        if unlikely(file_type == FileType::File) {
            // Regular file creation must not recurse while holding the directory lock,
            // otherwise self.create() will try to lock the same Mutex and deadlock.
            drop(inode);
            return self.create_with_context(filename, FileType::File, mode, context);
        }

        let filename = DName::from(filename);
        if inode.children.contains_key(&filename) {
            return Err(SystemError::EEXIST);
        }

        // 确定文件类型
        let file_type = match file_type {
            FileType::Pipe => FileType::Pipe,
            FileType::CharDevice => FileType::CharDevice,
            FileType::BlockDevice => FileType::BlockDevice,
            FileType::Socket => FileType::Socket,
            _ => return Err(SystemError::EINVAL),
        };
        crate::filesystem::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        let init = crate::filesystem::vfs::permission::child_inode_init_with_context(
            &inode.metadata,
            file_type,
            mode,
            context,
        )?;

        let now = PosixTimeSpec::now();
        let nod = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type,
                mode: init.mode,
                nlinks: 1,
                uid: init.uid,
                gid: init.gid,
                raw_dev: dev_t,
                flags: InodeFlags::empty(),
            },
            fs: inode.fs.clone(),
            special_node: None,
            inline_symlink: None,
            name: filename.clone(),
            tmpfile_linkable: false,
        }));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        // 对于 FIFO，需要创建实际的 pipe inode
        if mode.contains(InodeMode::S_IFIFO) {
            let pipe_inode = LockedPipeInode::new();
            pipe_inode.set_fifo();
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        }

        inode.children.insert(filename, nod.clone());
        tmpfs_touch_dir(&mut inode, now);
        Ok(nod)
    }

    fn special_node(&self) -> Option<super::vfs::SpecialNodeData> {
        self.0.lock().special_node.clone()
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().name.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0
            .lock()
            .parent
            .upgrade()
            .map(|item| item as Arc<dyn IndexNode>)
            .ok_or(SystemError::EINVAL)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.0.lock().page_cache.clone()
    }
}
