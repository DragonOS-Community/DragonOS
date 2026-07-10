use alloc::{string::String, sync::Arc, vec::Vec};
use core::{mem::size_of, sync::atomic::Ordering};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::{
        page_cache::PageCache,
        vfs::{
            file::{FileFlags, FileMode},
            permission::PermissionMask,
            syscall::RenameFlags,
            utils::DName,
            FilePrivateData, FileSystem, FileType, IndexNode, InodeMode, Metadata, SetMetadataMask,
            XattrFlags,
        },
    },
    libs::{casting::DowncastArc, mutex::MutexGuard},
    mm::MemoryManagementArch,
};

use super::super::{
    private_data::FuseFilePrivateData,
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAccessIn, FuseAttrOut, FuseCreateIn, FuseEntryOut,
        FuseFallocateIn, FuseGetxattrIn, FuseGetxattrOut, FuseLinkIn, FuseMkdirIn, FuseMknodIn,
        FuseReadIn, FuseRename2In, FuseRenameIn, FuseSetattrIn, FuseSetxattrInCompat, FuseWriteIn,
        FuseWriteOut, FATTR_ATIME, FATTR_CTIME, FATTR_GID, FATTR_MODE, FATTR_MTIME, FATTR_SIZE,
        FATTR_UID, FOPEN_DIRECT_IO, FOPEN_NONSEEKABLE, FOPEN_STREAM, FUSE_ACCESS, FUSE_CREATE,
        FUSE_FALLOCATE, FUSE_FSYNC, FUSE_FSYNCDIR, FUSE_GETXATTR, FUSE_LINK, FUSE_LISTXATTR,
        FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD, FUSE_OPEN, FUSE_OPENDIR, FUSE_READDIR,
        FUSE_READDIRPLUS, FUSE_READLINK, FUSE_RELEASE, FUSE_RELEASEDIR, FUSE_REMOVEXATTR,
        FUSE_RENAME, FUSE_RENAME2, FUSE_RMDIR, FUSE_SETATTR, FUSE_SETXATTR, FUSE_SYMLINK,
        FUSE_UNLINK, FUSE_WRITE, FUSE_WRITE_LOCKOWNER,
    },
};
use super::FuseNode;

impl IndexNode for FuseNode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _ = (offset, buf);
        self.check_not_stale()?;
        self.ensure_regular()?;
        Err(SystemError::ENOSYS)
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _ = (offset, buf);
        self.check_not_stale()?;
        self.ensure_regular()?;
        Err(SystemError::ENOSYS)
    }

    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        let _ = (start, len, offset);
        self.check_not_stale()?;
        self.ensure_regular()?;
        self.ensure_page_cache()?;
        Ok(())
    }

    fn check_mmap_file(
        &self,
        file: &Arc<crate::filesystem::vfs::file::File>,
        len: usize,
        offset: usize,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<(), SystemError> {
        let _ = (len, offset);
        self.check_not_stale()?;
        if file.file_type() != FileType::File {
            return Err(SystemError::EINVAL);
        }

        let fopen_flags = {
            let data = file.private_data.lock();
            let FilePrivateData::Fuse(FuseFilePrivateData::File(p)) = &*data else {
                return Err(SystemError::EINVAL);
            };
            p.fopen_flags
        };

        if (fopen_flags & FOPEN_DIRECT_IO) != 0
            && vm_flags.contains(crate::mm::VmFlags::VM_MAYSHARE)
        {
            return Err(SystemError::ENODEV);
        }

        Ok(())
    }

    fn mmap_file(
        &self,
        file: &Arc<crate::filesystem::vfs::file::File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<(), SystemError> {
        let _ = (start, len, offset);
        self.check_not_stale()?;
        if file.file_type() != FileType::File {
            return Err(SystemError::EINVAL);
        }

        let fopen_flags = {
            let data = file.private_data.lock();
            let FilePrivateData::Fuse(FuseFilePrivateData::File(p)) = &*data else {
                return Err(SystemError::EINVAL);
            };
            p.fopen_flags
        };

        if (fopen_flags & FOPEN_DIRECT_IO) != 0 {
            if vm_flags.contains(crate::mm::VmFlags::VM_MAYSHARE) {
                return Err(SystemError::ENODEV);
            }
            self.invalidate_clean_page_cache()?;
        }

        self.ensure_page_cache()?;
        Ok(())
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if self.conn.no_xattr(FUSE_GETXATTR) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let requested = core::cmp::min(buf.len(), Self::XATTR_SIZE_MAX);
        let inarg = FuseGetxattrIn {
            size: requested as u32,
            padding: 0,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
        let payload = match self.conn().request(FUSE_GETXATTR, self.nodeid, &payload_in) {
            Ok(payload) => payload,
            Err(SystemError::ENOSYS) => return Err(self.fuse_xattr_unsupported(FUSE_GETXATTR)),
            Err(err) => return Err(err),
        };

        if buf.is_empty() {
            let out: FuseGetxattrOut = fuse_read_struct(&payload)?;
            return Ok(core::cmp::min(out.size as usize, Self::XATTR_SIZE_MAX));
        }
        if payload.len() > buf.len() {
            return Err(SystemError::ERANGE);
        }
        if payload.len() > Self::XATTR_SIZE_MAX {
            return Err(SystemError::E2BIG);
        }
        buf[..payload.len()].copy_from_slice(&payload);
        Ok(payload.len())
    }

    fn setxattr(&self, name: &str, value: &[u8], flags: XattrFlags) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if self.conn.no_xattr(FUSE_SETXATTR) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if value.len() > Self::XATTR_SIZE_MAX {
            return Err(SystemError::E2BIG);
        }
        let inarg = FuseSetxattrInCompat {
            size: value.len() as u32,
            flags: flags.bits() as u32,
        };
        let mut payload_in = Self::pack_struct_and_name_payload(&inarg, name);
        payload_in.extend_from_slice(value);
        match self.conn().request(FUSE_SETXATTR, self.nodeid, &payload_in) {
            Ok(_) => Ok(0),
            Err(SystemError::ENOSYS) => Err(self.fuse_xattr_unsupported(FUSE_SETXATTR)),
            Err(err) => Err(err),
        }
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if self.conn.no_xattr(FUSE_LISTXATTR) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let requested = core::cmp::min(buf.len(), Self::XATTR_LIST_MAX);
        let inarg = FuseGetxattrIn {
            size: requested as u32,
            padding: 0,
        };
        let payload =
            match self
                .conn()
                .request(FUSE_LISTXATTR, self.nodeid, fuse_pack_struct(&inarg))
            {
                Ok(payload) => payload,
                Err(SystemError::ENOSYS) => return Err(self.fuse_xattr_unsupported(FUSE_LISTXATTR)),
                Err(err) => return Err(err),
            };

        if buf.is_empty() {
            let out: FuseGetxattrOut = fuse_read_struct(&payload)?;
            return Ok(core::cmp::min(out.size as usize, Self::XATTR_LIST_MAX));
        }
        if payload.len() > buf.len() {
            return Err(SystemError::ERANGE);
        }
        if payload.len() > Self::XATTR_LIST_MAX {
            return Err(SystemError::E2BIG);
        }
        Self::verify_xattr_list(&payload)?;
        buf[..payload.len()].copy_from_slice(&payload);
        Ok(payload.len())
    }

    fn removexattr(&self, name: &str) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if self.conn.no_xattr(FUSE_REMOVEXATTR) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        match self.request_name(FUSE_REMOVEXATTR, self.nodeid, name) {
            Ok(_) => Ok(0),
            Err(SystemError::ENOSYS) => Err(self.fuse_xattr_unsupported(FUSE_REMOVEXATTR)),
            Err(err) => Err(err),
        }
    }

    fn truncate_before_open(&self, flags: &FileFlags) -> bool {
        flags.contains(FileFlags::O_TRUNC)
            && !self
                .conn
                .has_init_flag(super::super::protocol::FUSE_ATOMIC_O_TRUNC)
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let md = self.cached_or_fetch_metadata()?;
        match md.file_type {
            FileType::Dir => self.open_common(FUSE_OPENDIR, &mut data, flags),
            FileType::File => self.open_common(FUSE_OPEN, &mut data, flags),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn adjust_file_mode_after_open(&self, data: &FilePrivateData, mode: &mut FileMode) {
        let fopen_flags = match data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(p))
            | FilePrivateData::Fuse(FuseFilePrivateData::Dir(p)) => p.fopen_flags,
            _ => return,
        };

        if (fopen_flags & FOPEN_STREAM) != 0 {
            mode.remove(
                FileMode::FMODE_LSEEK
                    | FileMode::FMODE_PREAD
                    | FileMode::FMODE_PWRITE
                    | FileMode::FMODE_ATOMIC_POS,
            );
            mode.insert(FileMode::FMODE_STREAM);
        } else if (fopen_flags & FOPEN_NONSEEKABLE) != 0 {
            mode.remove(FileMode::FMODE_LSEEK | FileMode::FMODE_PREAD | FileMode::FMODE_PWRITE);
        }
    }

    fn flush_file(
        &self,
        data: MutexGuard<FilePrivateData>,
        lock_owner: u64,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => return Ok(()),
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => self.flush_open_file(&p, lock_owner),
            FuseFilePrivateData::Dir(_) | FuseFilePrivateData::Dev(_) => Ok(()),
        }
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // `IndexNode::close()` is called from `File::drop()`, i.e. after the
        // last `Arc<File>` reference is gone.  User-visible FUSE_FLUSH errors
        // are handled by `flush_file()` on fd close; this final close only
        // drains dirty mappings and sends RELEASE.
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => return Ok(()),
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                let writeback_result = if p.writeback_handle.is_some() {
                    self.sync_dirty_cached_pages()
                } else {
                    Ok(())
                };
                if let Some(handle) = &p.writeback_handle {
                    self.unregister_writeback_handle(handle);
                }
                if p.no_open {
                    return writeback_result;
                }
                self.release_common(FUSE_RELEASE, p.fh, p.open_flags, 0);
                writeback_result
            }
            FuseFilePrivateData::Dir(p) => {
                if p.no_open {
                    Ok(())
                } else {
                    self.release_common(FUSE_RELEASEDIR, p.fh, p.open_flags, 0);
                    Ok(())
                }
            }
            FuseFilePrivateData::Dev(_) => Ok(()),
        }
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type == FileType::SymLink {
            if offset != 0 {
                return Ok(0);
            }
            let payload = self.conn().request(FUSE_READLINK, self.nodeid, &[])?;
            let n = core::cmp::min(payload.len(), len);
            buf[..n].copy_from_slice(&payload[..n]);
            return Ok(n);
        }
        self.ensure_regular()?;
        let private_data = Self::fuse_file_private_snapshot(&data)?;
        drop(data);
        let fh = private_data.fh;
        let file_flags = private_data.open_flags;
        let fopen_flags = private_data.fopen_flags;

        if (fopen_flags & FOPEN_DIRECT_IO) != 0 || (file_flags & FileFlags::O_DIRECT.bits()) != 0 {
            self.prepare_direct_io_range(offset, len, &private_data, false)?;
            let lock_owner = crate::filesystem::vfs::vcore::current_file_lock_owner_id();
            return self.read_direct_with_open(offset, len, buf, fh, file_flags, lock_owner);
        }

        self.read_cached_with_open(offset, len, buf, fh, file_flags)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        self.ensure_regular()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        if len > 0 {
            offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        }
        let private_data = Self::fuse_file_private_snapshot(&data)?;
        drop(data);
        let fh = private_data.fh;
        let file_flags = private_data.open_flags;
        let fopen_flags = private_data.fopen_flags;
        let max_write = core::cmp::min(self.conn().max_write(), self.max_pages_bytes());
        if max_write == 0 {
            return Err(SystemError::EIO);
        }
        let cached_write =
            (fopen_flags & FOPEN_DIRECT_IO) == 0 && (file_flags & FileFlags::O_DIRECT.bits()) == 0;
        let lock_owner = if cached_write {
            0
        } else {
            crate::filesystem::vfs::vcore::current_file_lock_owner_id()
        };
        let _direct_write_guard = if cached_write {
            None
        } else {
            Some(self.direct_io_lock.lock())
        };
        let mut total_written = 0usize;
        if !cached_write {
            self.prepare_direct_io_range(offset, len, &private_data, true)?;
        }
        let cached_page_cache = if cached_write {
            self.cached_page_cache()
        } else {
            None
        };
        let _cached_write_guard = cached_page_cache
            .as_ref()
            .map(|page_cache| page_cache.invalidate_write());
        if let Some(page_cache) = cached_page_cache.as_ref() {
            // Serialize ordinary cached writes against page-cache writeback so an older
            // dirty mmap page cannot be written back after the daemon sees this write.
            self.prepare_cached_write_range(page_cache, offset, len)?;
        }

        while total_written < len {
            let chunk = core::cmp::min(max_write, len - total_written);
            let chunk_offset = offset
                .checked_add(total_written)
                .ok_or(SystemError::EOVERFLOW)?;

            let write_in = FuseWriteIn {
                fh,
                offset: chunk_offset as u64,
                size: chunk as u32,
                write_flags: if lock_owner != 0 {
                    FUSE_WRITE_LOCKOWNER
                } else {
                    0
                },
                lock_owner,
                flags: file_flags,
                padding: 0,
            };
            let mut payload_in = Vec::with_capacity(size_of::<FuseWriteIn>() + chunk);
            payload_in.extend_from_slice(fuse_pack_struct(&write_in));
            payload_in.extend_from_slice(&buf[total_written..total_written + chunk]);
            let payload = self.conn().request(FUSE_WRITE, self.nodeid, &payload_in)?;
            let out: FuseWriteOut = fuse_read_struct(&payload)?;
            if out.size as usize > chunk {
                return if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(SystemError::EIO)
                };
            }
            let wrote = out.size as usize;
            self.note_successful_write(chunk_offset, wrote)?;
            let cache_result = if cached_write {
                if let Some(page_cache) = cached_page_cache.as_ref() {
                    self.update_cached_pages_after_write(
                        page_cache,
                        chunk_offset,
                        &buf[total_written..total_written + wrote],
                    )
                } else {
                    Ok(())
                }
            } else {
                self.invalidate_cached_pages_after_direct_write(chunk_offset, wrote)
            };
            total_written += wrote;
            if cache_result.is_err() {
                break;
            }
            if wrote < chunk {
                break;
            }
        }

        Ok(total_written)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.check_not_stale()?;
        self.cached_or_fetch_metadata()
    }

    fn check_access(&self, mask: PermissionMask) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let inarg = FuseAccessIn {
            mask: mask.bits() & PermissionMask::MAY_RWX.bits(),
            padding: 0,
        };
        let _ = self
            .conn()
            .request(FUSE_ACCESS, self.nodeid, fuse_pack_struct(&inarg))?;
        Ok(())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let old = self.cached_or_fetch_metadata()?;
        let mut valid = 0u32;
        if metadata.mode != old.mode {
            valid |= FATTR_MODE;
        }
        if metadata.uid != old.uid {
            valid |= FATTR_UID;
        }
        if metadata.gid != old.gid {
            valid |= FATTR_GID;
        }
        if metadata.size != old.size {
            valid |= FATTR_SIZE;
        }
        if metadata.atime != old.atime {
            valid |= FATTR_ATIME;
        }
        if metadata.mtime != old.mtime {
            valid |= FATTR_MTIME;
        }
        if metadata.ctime != old.ctime {
            valid |= FATTR_CTIME;
        }
        if valid == 0 {
            return Ok(());
        }

        let inarg = FuseSetattrIn {
            valid,
            padding: 0,
            fh: 0,
            size: metadata.size as u64,
            lock_owner: 0,
            atime: metadata.atime.tv_sec as u64,
            mtime: metadata.mtime.tv_sec as u64,
            ctime: metadata.ctime.tv_sec as u64,
            atimensec: metadata.atime.tv_nsec as u32,
            mtimensec: metadata.mtime.tv_nsec as u32,
            ctimensec: metadata.ctime.tv_nsec as u32,
            mode: metadata.mode.bits(),
            unused4: 0,
            uid: metadata.uid as u32,
            gid: metadata.gid as u32,
            unused5: 0,
        };
        let payload = self
            .conn()
            .request(FUSE_SETATTR, self.nodeid, fuse_pack_struct(&inarg))?;
        let out: FuseAttrOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&out.attr);
        let new_size = md.size.max(0) as usize;
        self.set_cached_metadata_with_valid(md, out.attr_valid, out.attr_valid_nsec);
        if (valid & FATTR_SIZE) != 0 {
            self.truncate_page_cache(new_size)?;
        }
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.setattr_size(len, None, None, None)
    }

    fn resize_with_lock_owner(&self, len: usize, lock_owner: u64) -> Result<(), SystemError> {
        self.setattr_size(len, Some(lock_owner), None, None)
    }

    fn resize_file(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.resize_with_lock_owner(len, lock_owner);
            }
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                self.setattr_size(len, Some(lock_owner), Some(p.fh), None)
            }
            _ => self.resize_with_lock_owner(len, lock_owner),
        }
    }

    fn resize_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        self.setattr_size(len, Some(lock_owner), None, Some((metadata, mask)))
    }

    fn resize_file_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.resize_with_metadata(len, lock_owner, metadata, mask);
            }
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                self.setattr_size(len, Some(lock_owner), Some(p.fh), Some((metadata, mask)))
            }
            _ => self.resize_with_metadata(len, lock_owner, metadata, mask),
        }
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        _lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        if mode != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        self.check_not_stale()?;
        let new_size = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        if self.conn().no_fallocate() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let fuse_data = match &*data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(data)) => data.clone(),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        drop(data);

        let md = self.metadata()?;
        if new_size > md.size.max(0) as usize {
            crate::filesystem::vfs::vcore::check_file_size_limit(new_size)?;
        }

        let in_arg = FuseFallocateIn {
            fh: fuse_data.fh,
            offset: offset as u64,
            length: len as u64,
            mode: mode as u32,
            padding: 0,
        };
        match self
            .conn()
            .request(FUSE_FALLOCATE, self.nodeid, fuse_pack_struct(&in_arg))
        {
            Ok(_) => {
                if let Some(md) = self.cached_metadata.lock().as_mut() {
                    if new_size > md.size.max(0) as usize {
                        md.size = new_size as i64;
                    }
                }
                self.cached_metadata_deadline_ns.store(0, Ordering::Relaxed);
                Ok(())
            }
            Err(SystemError::ENOSYS) => {
                self.conn().mark_no_fallocate();
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&self) -> Result<(), SystemError> {
        self.fsync_common(false)
    }

    fn datasync(&self) -> Result<(), SystemError> {
        self.fsync_common(true)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.cached_page_cache()
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.fsync_common(datasync);
            }
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                let sync_result = self.sync_cached_pages();
                let wb_error_result = self.check_and_advance_open_wb_error(&p);
                sync_result?;
                wb_error_result?;
                self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync)
            }
            FuseFilePrivateData::Dir(p) => self.fsync_with_fh(FUSE_FSYNCDIR, p.fh, datasync),
            FuseFilePrivateData::Dev(_) => self.fsync_common(datasync),
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.fsync_common(datasync);
            }
        };
        drop(data);

        if let FuseFilePrivateData::File(_) = &fuse_data {
            if let Some(page_cache) = self.cached_page_cache() {
                let start_index = start >> MMArch::PAGE_SHIFT;
                let end_index = end >> MMArch::PAGE_SHIFT;
                page_cache
                    .manager()
                    .writeback_range(start_index, end_index)?;
            }
        }

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                self.check_and_advance_open_wb_error(&p)?;
                self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync)
            }
            FuseFilePrivateData::Dir(p) => self.fsync_with_fh(FUSE_FSYNCDIR, p.fh, datasync),
            FuseFilePrivateData::Dev(_) => self.fsync_common(datasync),
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;

        // OPENDIR
        let mut pdata = FilePrivateData::Unused;
        let flags = FileFlags::O_RDONLY;
        self.open_common(FUSE_OPENDIR, &mut pdata, &flags)?;
        let FilePrivateData::Fuse(FuseFilePrivateData::Dir(dir_p)) = &pdata else {
            return Err(SystemError::EINVAL);
        };
        let fh = dir_p.fh;
        let open_flags = dir_p.open_flags;
        let mut use_readdirplus = self.conn.use_readdirplus();
        // Linux issues uncached READDIR with a single PAGE_SIZE output page. DragonOS keeps the
        // existing larger batch when the connection supports it, but must not exceed either the
        // mount's max_read value or the negotiated max_pages descriptor bound.
        let readdir_size = self.readdir_request_size();

        let mut names: Vec<String> = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let read_in = FuseReadIn {
                fh,
                offset,
                size: readdir_size,
                read_flags: 0,
                lock_owner: 0,
                flags: open_flags,
                padding: 0,
            };
            let opcode = if use_readdirplus {
                FUSE_READDIRPLUS
            } else {
                FUSE_READDIR
            };
            let payload = match self
                .conn()
                .request(opcode, self.nodeid, fuse_pack_struct(&read_in))
            {
                Ok(v) => v,
                Err(SystemError::ENOSYS) if use_readdirplus => {
                    self.conn.disable_readdirplus();
                    use_readdirplus = false;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if payload.is_empty() {
                break;
            }

            let mut last_off: u64 = offset;
            if use_readdirplus {
                last_off = self.parse_readdirplus_payload(&payload, &mut names, last_off)?;
            } else {
                last_off = Self::parse_readdir_payload(&payload, &mut names, last_off)?;
            }

            if last_off == offset {
                // Avoid infinite loop if userspace doesn't advance offsets.
                break;
            }
            offset = last_off;
        }

        // RELEASEDIR (best-effort)
        if !dir_p.no_open {
            self.release_common(FUSE_RELEASEDIR, fh, open_flags, 0);
        }
        Ok(names)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        if name == "." {
            let this = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            return Ok(this);
        }
        if name == ".." {
            return self.parent();
        }

        if let Some(child) = self.lookup_cached_child(name) {
            return Ok(child);
        }

        let payload = match self.request_name(FUSE_LOOKUP, self.nodeid, name) {
            Ok(payload) => payload,
            Err(err) => {
                self.invalidate_lookup_cache(name);
                return Err(err);
            }
        };
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let md = Self::metadata_from_valid_entry(&entry, SystemError::ENOENT, None).inspect_err(
            |_| {
                if entry.nodeid != 0 {
                    let _ = self.conn.queue_forget(entry.nodeid, 1);
                }
            },
        )?;
        if entry.nodeid == self.nodeid {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
            return Err(SystemError::EIO);
        }

        let mut consumed = false;
        let result = (|| {
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
                1,
            )?;
            consumed = true;
            Ok(child)
        })();
        let child = match result {
            Ok(child) => child,
            Err(err) => {
                if entry.nodeid != 0 && !consumed {
                    let _ = self.conn.queue_forget(entry.nodeid, 1);
                }
                return Err(err);
            }
        };
        child.set_dname(name);
        child
            .lookup_attr_flags
            .store(entry.attr.flags, Ordering::Relaxed);
        child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
        self.cache_lookup_child(
            name,
            &child,
            entry.generation,
            entry.entry_valid,
            entry.entry_valid_nsec,
        );
        Ok(child)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        if let Some(parent) = self.parent.lock().clone() {
            return Ok(parent);
        }
        let parent_nodeid = *self.parent_nodeid.lock();
        if parent_nodeid == self.nodeid {
            return Ok(fs.root_node());
        }
        Err(SystemError::ESTALE)
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        if file_type != FileType::File {
            return self.create_with_data(name, file_type, mode, 0);
        }

        let inarg = FuseCreateIn {
            flags: FileFlags::O_RDONLY.bits(),
            mode: (InodeMode::S_IFREG | mode).bits(),
            umask: 0,
            open_flags: 0,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);

        let payload = match self.conn().request(FUSE_CREATE, self.nodeid, &payload_in) {
            Ok(v) => v,
            Err(SystemError::ENOSYS) => return self.create_with_data(name, file_type, mode, 0),
            Err(e) => return Err(e),
        };
        let (entry, open_out) = Self::parse_create_reply(&payload)?;
        if entry.nodeid != 0 {
            self.release_common_for_node(
                FUSE_RELEASE,
                entry.nodeid,
                open_out.fh,
                FileFlags::O_RDONLY.bits(),
                0,
            );
        }
        self.create_node_from_entry(&entry, Some(name), FileType::File)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;

        match file_type {
            FileType::Dir => {
                let inarg = FuseMkdirIn {
                    mode: (InodeMode::S_IFDIR | mode).bits(),
                    umask: 0,
                };
                let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
                let payload = self.conn().request(FUSE_MKDIR, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::Dir)
            }
            FileType::File => {
                let inarg = FuseMknodIn {
                    mode: (InodeMode::S_IFREG | mode).bits(),
                    rdev: 0,
                    umask: 0,
                    padding: 0,
                };
                let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
                let payload = self.conn().request(FUSE_MKNOD, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::File)
            }
            FileType::SymLink => {
                let mut payload_in = Vec::with_capacity(name.len() + 2);
                payload_in.push(0);
                payload_in.extend_from_slice(name.as_bytes());
                payload_in.push(0);
                let payload = self
                    .conn()
                    .request(FUSE_SYMLINK, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::SymLink)
            }
            _ => Err(SystemError::ENOSYS),
        }
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let payload_in = Self::pack_two_names_payload(target, name);
        let payload = self
            .conn()
            .request(FUSE_SYMLINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        self.create_node_from_entry(&entry, Some(name), FileType::SymLink)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let target = other
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;
        let expected_type = target.cached_or_fetch_metadata()?.file_type;
        let inarg = FuseLinkIn {
            oldnodeid: target.nodeid,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
        let payload = self.conn().request(FUSE_LINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let mut consumed = false;
        let result = (|| {
            let md =
                Self::metadata_from_valid_entry(&entry, SystemError::EIO, Some(expected_type))?;
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_for_link(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
                1,
            )?;
            consumed = true;
            child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
            Ok(())
        })();
        if result.is_err() && entry.nodeid != 0 && !consumed {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
        }
        if result.is_ok() {
            self.invalidate_lookup_cache(name);
        }
        result
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let _ = self.request_name(FUSE_UNLINK, self.nodeid, name)?;
        self.invalidate_child_name(name);
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let _ = self.request_name(FUSE_RMDIR, self.nodeid, name)?;
        self.invalidate_child_name(name);
        Ok(())
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flag: RenameFlags,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let target_any = target
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;

        let mut payload_in = Vec::new();
        let opcode = if flag.is_empty() {
            let inarg = FuseRenameIn {
                newdir: target_any.nodeid,
            };
            payload_in.extend_from_slice(fuse_pack_struct(&inarg));
            FUSE_RENAME
        } else {
            let inarg = FuseRename2In {
                newdir: target_any.nodeid,
                flags: flag.bits(),
                padding: 0,
            };
            payload_in.extend_from_slice(fuse_pack_struct(&inarg));
            FUSE_RENAME2
        };
        payload_in.extend_from_slice(old_name.as_bytes());
        payload_in.push(0);
        payload_in.extend_from_slice(new_name.as_bytes());
        payload_in.push(0);
        let cached_old = self.lookup_cached_child(old_name).or_else(|| {
            self.find(old_name)
                .ok()
                .and_then(|inode| inode.downcast_arc::<FuseNode>())
        });
        let cached_new = target_any.lookup_cached_child(new_name).or_else(|| {
            target_any
                .find(new_name)
                .ok()
                .and_then(|inode| inode.downcast_arc::<FuseNode>())
        });
        let r = self.conn().request(opcode, self.nodeid, &payload_in);
        if opcode == FUSE_RENAME2 && matches!(r, Err(SystemError::ENOSYS)) {
            return Err(SystemError::EINVAL);
        }
        let _ = r?;
        self.invalidate_lookup_cache(old_name);
        target_any.invalidate_lookup_cache(new_name);
        if let Some(node) = cached_old {
            node.set_parent_nodeid(target_any.nodeid);
            node.set_parent(Some(
                target_any.self_ref.upgrade().ok_or(SystemError::ENOENT)?,
            ));
            node.set_dname(new_name);
        }
        if let Some(node) = cached_new {
            if flag.contains(RenameFlags::EXCHANGE) {
                node.set_parent_nodeid(self.nodeid);
                node.set_parent(Some(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?));
                node.set_dname(old_name);
            } else {
                node.clear_dname_if(new_name);
            }
        }
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(format!("fuse:{}", self.nodeid))
    }

    fn dname(&self) -> Result<DName, SystemError> {
        self.name
            .lock()
            .as_ref()
            .map(|name| DName(Arc::new(name.clone())))
            .ok_or(SystemError::ENOENT)
    }
}
