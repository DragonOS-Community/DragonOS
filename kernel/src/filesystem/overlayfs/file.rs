use super::inode::OvlInode;
use crate::filesystem::vfs::file::{File, FileFlags, FilePrivateData};
use crate::filesystem::vfs::{self, vcore, FileType};
use crate::libs::mutex::Mutex;
use crate::mm::VmFlags;
use alloc::sync::Arc;
use core::mem;
use system_error::SystemError;

#[derive(Debug, Clone)]
pub struct OverlayFilePrivateData {
    inner: Arc<Mutex<OverlayFilePrivateDataInner>>,
}

#[derive(Debug)]
struct OverlayFilePrivateDataInner {
    backing_file: Arc<File>,
    backing_is_upper: bool,
    flags: FileFlags,
}

impl OverlayFilePrivateData {
    fn new(backing_file: Arc<File>, backing_is_upper: bool, flags: FileFlags) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OverlayFilePrivateDataInner {
                backing_file,
                backing_is_upper,
                flags,
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
) -> Result<(), SystemError> {
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
) -> Result<usize, system_error::SystemError> {
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
    let (backing_file, _) = backing_file_for_io(data)?;
    backing_file.pread(offset, len, buf)
}

pub(super) fn write_at(
    _inode: &OvlInode,
    offset: usize,
    len: usize,
    buf: &[u8],
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<usize, SystemError> {
    let (backing_file, _) = backing_file_for_io(data)?;
    backing_file.pwrite(offset, len, buf)
}

pub(super) fn sync_file(
    _inode: &OvlInode,
    datasync: bool,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<(), SystemError> {
    let (backing_file, backing_is_upper) = backing_file_for_io(data)?;
    if backing_is_upper {
        backing_file.sync_range_and_check_wb_error(0, usize::MAX, datasync)
    } else {
        Ok(())
    }
}

pub(super) fn sync_file_range(
    _inode: &OvlInode,
    start: usize,
    end: usize,
    datasync: bool,
    data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
) -> Result<(), SystemError> {
    let (backing_file, backing_is_upper) = backing_file_for_io(data)?;
    if backing_is_upper {
        backing_file.sync_range_and_check_wb_error(start, end, datasync)
    } else {
        Ok(())
    }
}

pub(super) fn flush_file(
    _inode: &OvlInode,
    data: crate::libs::mutex::MutexGuard<FilePrivateData>,
    lock_owner: u64,
) -> Result<(), SystemError> {
    let (backing_file, _) = backing_file_for_io(data)?;
    backing_file.flush_for_close(lock_owner)
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
    _inode: &OvlInode,
    file: &Arc<File>,
    len: usize,
    offset: usize,
    vm_flags: VmFlags,
) -> Result<(), SystemError> {
    let (backing_file, _) = backing_file_for_io(file.private_data.lock())?;
    backing_file
        .inode()
        .check_mmap_file(&backing_file, len, offset, vm_flags)
}

pub(super) fn mmap_effective_file(
    _inode: &OvlInode,
    file: &Arc<File>,
) -> Result<Arc<File>, SystemError> {
    let (backing_file, _) = backing_file_for_io(file.private_data.lock())?;
    Ok(backing_file)
}

pub(super) fn mmap_file(
    _inode: &OvlInode,
    file: &Arc<File>,
    start: usize,
    len: usize,
    offset: usize,
    vm_flags: VmFlags,
) -> Result<(), SystemError> {
    let (backing_file, _) = backing_file_for_io(file.private_data.lock())?;
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
    flags.remove(FileFlags::O_CREAT | FileFlags::O_EXCL | FileFlags::O_NOCTTY | FileFlags::O_TRUNC);
    flags
}

fn open_backing_file(
    inode: &OvlInode,
    flags: FileFlags,
) -> Result<OverlayFilePrivateData, SystemError> {
    let needs_post_open_truncate = open_flags_need_copy_up(inode.file_type, &flags)
        && inode.copy_up_for_open(&flags)?.needs_post_open_truncate();

    let (backing_inode, backing_is_upper) = inode.current_realdata_inode()?;
    let backing_file = Arc::new(File::new(backing_inode, backing_open_flags(flags))?);
    if inode.file_type == FileType::File && backing_is_upper && needs_post_open_truncate {
        vcore::vfs_truncate_file(
            backing_file.inode(),
            0,
            vcore::current_file_lock_owner_id(),
            || backing_file.private_data.lock(),
        )?;
    }
    Ok(OverlayFilePrivateData::new(
        backing_file,
        backing_is_upper,
        flags,
    ))
}

fn backing_file_for_io(
    data: crate::libs::mutex::MutexGuard<FilePrivateData>,
) -> Result<(Arc<File>, bool), SystemError> {
    let FilePrivateData::Overlayfs(overlay_data) = &*data else {
        return Err(SystemError::EBADF);
    };
    let overlay_data = overlay_data.clone();
    drop(data);

    let inner = overlay_data.inner.lock();
    Ok((inner.backing_file.clone(), inner.backing_is_upper))
}
