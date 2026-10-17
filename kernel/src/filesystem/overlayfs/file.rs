use super::cred::CredOverrideGuard;
use super::inode::OvlInode;
use crate::filesystem::vfs::file::{File, FileFlags, FilePrivateData};
use crate::filesystem::vfs::{self, FileType, Metadata, SetMetadataMask};
use crate::libs::mutex::Mutex;
use crate::mm::VmFlags;
use crate::process::Cred;
use alloc::sync::Arc;
use core::mem;
use system_error::SystemError;

#[derive(Debug, Clone)]
pub struct OverlayFilePrivateData {
    inner: Arc<Mutex<OverlayFilePrivateDataInner>>,
}

#[derive(Debug)]
struct OverlayFilePrivateDataInner {
    _initial_backing_file: Arc<File>,
    active_real_inode: Arc<dyn vfs::IndexNode>,
    backing_file: Arc<File>,
    backing_is_upper: bool,
    flags: FileFlags,
    backing_cred: Arc<Cred>,
}

impl OverlayFilePrivateData {
    fn new(
        active_real_inode: Arc<dyn vfs::IndexNode>,
        backing_file: Arc<File>,
        backing_is_upper: bool,
        flags: FileFlags,
        backing_cred: Arc<Cred>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OverlayFilePrivateDataInner {
                _initial_backing_file: backing_file.clone(),
                active_real_inode,
                backing_file,
                backing_is_upper,
                flags,
                backing_cred,
            })),
        }
    }

    pub fn set_flags(&mut self, flags: FileFlags) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        inner.backing_file.set_flags(backing_open_flags(flags))?;
        inner.flags = flags;
        Ok(())
    }
}

pub(super) fn open(
    inode: &OvlInode,
    mut data: crate::libs::mutex::MutexGuard<FilePrivateData>,
    flags: &FileFlags,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let overlay_data = open_backing_file(inode, *flags)?;
    *data = FilePrivateData::Overlayfs(overlay_data);
    Ok(())
}

pub(super) fn read_at(
    inode: &OvlInode,
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
    if inode.file_type == FileType::SymLink {
        drop(data);
        let (backing_inode, _) = inode.current_realdata_inode()?;
        return backing_inode.read_at(
            offset,
            len,
            buf,
            crate::libs::mutex::Mutex::new(FilePrivateData::Unused).lock(),
        );
    }
    let (backing_file, _) = backing_file_for_io(inode, data)?;
    backing_file.try_pread(offset, len, buf)
}

pub(super) fn write_at(
    inode: &OvlInode,
    offset: usize,
    len: usize,
    buf: &[u8],
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
    let result = write_at_with_sync(inode, offset, len, buf, vfs::WriteSyncIntent::None, data)?;
    result.sync_result?;
    Ok(result.written_len)
}

pub(super) fn write_at_with_sync(
    inode: &OvlInode,
    offset: usize,
    len: usize,
    buf: &[u8],
    sync_intent: vfs::WriteSyncIntent,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<vfs::DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
    let mut publish = || {};
    let mut stage = vfs::WritePrivilegeStage::new(&mut publish);
    write_primary_with_sync(
        inode,
        offset,
        len,
        vfs::PrimaryWriteSource::Kernel(buf),
        sync_intent,
        data,
        &mut stage,
    )
}

pub(super) fn write_primary_with_sync(
    inode: &OvlInode,
    offset: usize,
    len: usize,
    source: vfs::PrimaryWriteSource<'_, '_>,
    sync_intent: vfs::WriteSyncIntent,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    stage: &mut vfs::WritePrivilegeStage<'_>,
) -> Result<vfs::DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
    inode.overlay_fs()?.require_upper()?;
    if len == 0 {
        let (backing_file, _) = backing_file_for_io(inode, data)?;
        return backing_file.pwrite_with_sync_intent(offset, 0, &[], sync_intent);
    }

    let caller = crate::process::ProcessManager::current_pcb().cred();
    let _privilege_guard = inode.content_privilege_lock.lock();
    let (backing_file, _) = backing_file_for_io(inode, data)?;
    let fs = inode.overlay_fs()?;
    let _content_guard = fs.content_lock(&backing_file.inode())?.lock();
    super::metadata::prepare_primary_write_locked(
        &backing_file.inode(),
        &fs.backing_cred,
        &caller,
        stage,
    )?;
    // Overlay removes caller privileges before the backing write checks.
    // Admit the backing file-size limit before touching the user payload;
    // the backing File rechecks it when committing the staged buffer.
    let len = backing_file.limit_write_len_by_fsize(vfs::FileType::File, offset, len)?;
    // Keep user faults under the caller credentials. Only the completed
    // staging buffer enters the backing credential override, as before.
    source.with_buffer(len, |buffer| {
        let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
        backing_file.pwrite_with_sync_intent(offset, len.min(buffer.len()), buffer, sync_intent)
    })
}

pub(super) fn sync_file(
    inode: &OvlInode,
    datasync: bool,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, backing_is_upper) = backing_file_for_io(inode, data)?;
    if backing_is_upper {
        backing_file.try_sync_range_and_check_wb_error(0, usize::MAX, datasync)
    } else {
        Ok(())
    }
}

pub(super) fn sync_file_range(
    inode: &OvlInode,
    start: usize,
    end: usize,
    datasync: bool,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, backing_is_upper) = backing_file_for_io(inode, data)?;
    if backing_is_upper {
        backing_file.try_sync_range_and_check_wb_error(start, end, datasync)
    } else {
        Ok(())
    }
}

pub(super) fn flush_file(
    inode: &OvlInode,
    data: crate::libs::mutex::MutexGuard<FilePrivateData>,
    lock_owner: u64,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, _) = backing_file_for_io(inode, data)?;
    backing_file.try_flush_for_close(lock_owner)
}

pub(super) fn resize_file_with_metadata(
    inode: &OvlInode,
    data: crate::libs::mutex::MutexGuard<FilePrivateData>,
    len: usize,
    lock_owner: u64,
    metadata: &Metadata,
    mask: SetMetadataMask,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, backing_is_upper) = backing_file_for_io(inode, data)?;
    if !backing_is_upper {
        return Err(SystemError::EIO.into());
    }
    backing_file.inode().resize_file_with_metadata(
        len,
        lock_owner,
        backing_file.private_data.lock(),
        metadata,
        mask,
    )
}

pub(super) fn close(
    mut data: crate::libs::mutex::MutexGuard<FilePrivateData>,
) -> Result<(), SystemError> {
    let old = mem::replace(&mut *data, FilePrivateData::Unused);
    drop(data);
    if let FilePrivateData::Overlayfs(overlay_data) = old {
        drop(overlay_data);
    }
    Ok(())
}

pub(super) fn check_mmap_file(
    inode: &OvlInode,
    file: &Arc<File>,
    len: usize,
    offset: usize,
    vm_flags: VmFlags,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, _) = backing_file_for_io(inode, file.private_data.lock())?;
    backing_file
        .inode()
        .check_mmap_file(&backing_file, len, offset, vm_flags)
}

pub(super) fn mmap_effective_file(
    inode: &OvlInode,
    file: &Arc<File>,
) -> Result<Arc<File>, crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, _) = backing_file_for_io(inode, file.private_data.lock())?;
    Ok(backing_file)
}

pub(super) fn mmap_file(
    inode: &OvlInode,
    file: &Arc<File>,
    start: usize,
    len: usize,
    offset: usize,
    vm_flags: VmFlags,
) -> Result<Arc<File>, crate::mm::allocation::MemoryAllocationError> {
    let (backing_file, _) = backing_file_for_io(inode, file.private_data.lock())?;
    backing_file
        .inode()
        .mmap_file(&backing_file, start, len, offset, vm_flags)
}

fn open_flags_need_copy_up(file_type: FileType, flags: &FileFlags) -> bool {
    if file_type != FileType::File {
        return false;
    }

    let access = flags.access_flags();
    access == FileFlags::O_WRONLY
        || access == FileFlags::O_RDWR
        || flags.contains(FileFlags::O_TRUNC)
}

fn backing_open_flags(mut flags: FileFlags) -> FileFlags {
    flags.remove(
        FileFlags::O_CREAT
            | FileFlags::O_EXCL
            | FileFlags::O_NOCTTY
            | FileFlags::O_TRUNC
            | FileFlags::O_SYNC
            | FileFlags::O_DSYNC,
    );
    flags
}

fn open_backing_file(
    inode: &OvlInode,
    flags: FileFlags,
) -> Result<OverlayFilePrivateData, crate::mm::allocation::MemoryAllocationError> {
    if open_flags_need_copy_up(inode.file_type, &flags) {
        inode.copy_up_for_open(&flags)?;
    }

    let (backing_inode, backing_is_upper) = inode.current_realdata_inode()?;
    let backing_cred = inode.overlay_fs()?.backing_cred.clone();
    let backing_file = open_real_file_with_cred(
        backing_inode.clone(),
        backing_open_flags(flags),
        backing_cred.clone(),
    )?;
    Ok(OverlayFilePrivateData::new(
        backing_inode,
        backing_file,
        backing_is_upper,
        flags,
        backing_cred,
    ))
}

fn open_real_file_with_cred(
    inode: Arc<dyn vfs::IndexNode>,
    flags: FileFlags,
    cred: Arc<Cred>,
) -> Result<Arc<File>, crate::mm::allocation::MemoryAllocationError> {
    let _cred_guard = CredOverrideGuard::new(cred)?;
    Ok(Arc::new(File::new(inode, flags)?))
}

fn backing_file_for_io(
    inode: &OvlInode,
    data: crate::libs::mutex::MutexGuard<FilePrivateData>,
) -> Result<(Arc<File>, bool), crate::mm::allocation::MemoryAllocationError> {
    let FilePrivateData::Overlayfs(overlay_data) = &*data else {
        return Err(SystemError::EBADF.into());
    };
    let overlay_data = overlay_data.clone();
    drop(data);

    loop {
        let (current_real_inode, current_is_upper) = inode.current_realdata_inode()?;
        let (flags, cred) = {
            let inner = overlay_data.inner.lock();
            if inner.backing_is_upper == current_is_upper
                && Arc::ptr_eq(&inner.active_real_inode, &current_real_inode)
            {
                return Ok((inner.backing_file.clone(), inner.backing_is_upper));
            }
            (inner.flags, inner.backing_cred.clone())
        };
        let candidate =
            open_real_file_with_cred(current_real_inode.clone(), backing_open_flags(flags), cred)?;
        let (real_now, upper_now) = inode.current_realdata_inode()?;
        let mut inner = overlay_data.inner.lock();
        if inner.flags != flags
            || upper_now != current_is_upper
            || !Arc::ptr_eq(&real_now, &current_real_inode)
        {
            drop(inner);
            drop(candidate);
            continue;
        }
        if inner.backing_is_upper == current_is_upper
            && Arc::ptr_eq(&inner.active_real_inode, &current_real_inode)
        {
            let result = (inner.backing_file.clone(), inner.backing_is_upper);
            drop(inner);
            drop(candidate);
            return Ok(result);
        }
        inner.active_real_inode = current_real_inode;
        inner.backing_is_upper = current_is_upper;
        let retired = mem::replace(&mut inner.backing_file, candidate);
        let result = (inner.backing_file.clone(), current_is_upper);
        drop(inner);
        drop(retired);
        return Ok(result);
    }
}
