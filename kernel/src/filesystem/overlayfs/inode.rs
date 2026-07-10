use super::entry::OvlEntry;
use super::fs::OverlayFS;
use super::metadata::OvlOrigin;
use super::{dir, file, lookup, readdir, rename};
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::{File, FileFlags, FilePrivateData};
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::utils::DName;
use crate::filesystem::vfs::{
    self, FileSystem, FileType, IndexNode, InodeId, Metadata, SetMetadataMask, XattrFlags,
};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::Mutex;
use crate::mm::VmFlags;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug)]
pub struct OvlInode {
    pub(super) redirect: String, // Redirect path
    pub(super) file_type: FileType,
    #[allow(dead_code)]
    pub(super) flags: Mutex<u64>,
    pub(super) upper_inode: Mutex<Option<Arc<dyn IndexNode>>>, // Read-write layer (upper)
    pub(super) lower_inodes: Vec<Arc<dyn IndexNode>>, // Read-only layer (lower, supports multi-layer)
    pub(super) overlay_inode_id: Option<InodeId>,
    origin: Mutex<OriginState>,
    #[allow(dead_code)]
    pub(super) oe: Arc<OvlEntry>,
    pub(super) fs: Mutex<Weak<OverlayFS>>,
}

#[derive(Debug, Clone, Copy)]
enum OriginState {
    Unchecked,
    Checked(Option<OvlOrigin>),
}

impl OvlInode {
    pub fn new(
        redirect: String,
        file_type: FileType,
        upper: Option<Arc<dyn IndexNode>>,
        lower_inodes: Vec<Arc<dyn IndexNode>>,
        overlay_inode_id: Option<InodeId>,
    ) -> Self {
        Self {
            redirect,
            file_type,
            flags: Mutex::new(0),
            upper_inode: Mutex::new(upper),
            lower_inodes,
            overlay_inode_id,
            origin: Mutex::new(OriginState::Unchecked),
            oe: Arc::new(OvlEntry::new()),
            fs: Mutex::new(Weak::default()),
        }
    }

    pub(super) fn set_fs(&self, fs: Weak<OverlayFS>) {
        *self.fs.lock() = fs;
    }

    #[allow(dead_code)]
    pub fn ovl_lower_redirect(&self) -> Option<&str> {
        if !self.lower_inodes.is_empty()
            && (self.file_type == FileType::File || self.file_type == FileType::Dir)
        {
            Some(&self.redirect)
        } else {
            None
        }
    }

    pub(super) fn overlay_fs(&self) -> Result<Arc<OverlayFS>, SystemError> {
        self.fs.lock().upgrade().ok_or(SystemError::EINVAL)
    }

    pub(super) fn downcast_overlay_inode(
        inode: Arc<dyn IndexNode>,
    ) -> Result<Arc<OvlInode>, SystemError> {
        inode.downcast_arc::<OvlInode>().ok_or(SystemError::EXDEV)
    }

    pub(super) fn lookup_overlay_child(&self, name: &str) -> Result<Arc<OvlInode>, SystemError> {
        Self::downcast_overlay_inode(self.find(name)?)
    }

    pub(super) fn has_upper(&self) -> bool {
        self.upper_inode.lock().is_some()
    }

    pub(super) fn has_lower(&self) -> bool {
        !self.lower_inodes.is_empty()
    }

    pub(super) fn is_pure_upper(&self) -> bool {
        self.has_upper() && !self.has_lower()
    }

    pub(super) fn is_dir(&self) -> bool {
        self.file_type == FileType::Dir
    }

    pub(super) fn load_origin_once(&self) -> Result<(), SystemError> {
        if matches!(*self.origin.lock(), OriginState::Checked(_)) {
            return Ok(());
        }

        // Never hold the origin state while taking upper_inode: copy-up owns
        // upper_inode while publishing the checked origin state.
        let upper = self.upper_inode.lock().clone();
        let loaded = upper
            .as_ref()
            .map(|upper| super::metadata::load_origin(self, upper))
            .transpose()?
            .flatten();
        let mut state = self.origin.lock();
        if matches!(*state, OriginState::Unchecked) {
            *state = OriginState::Checked(loaded);
        }
        Ok(())
    }

    pub(super) fn set_origin(&self, origin: Option<OvlOrigin>) {
        *self.origin.lock() = OriginState::Checked(origin);
    }

    pub(super) fn origin(&self) -> Option<OvlOrigin> {
        match *self.origin.lock() {
            OriginState::Unchecked => None,
            OriginState::Checked(origin) => origin,
        }
    }
}

impl IndexNode for OvlInode {
    fn open(
        &self,
        data: crate::libs::mutex::MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        file::open(self, data, flags)
    }

    fn truncate_before_open(&self, _flags: &FileFlags) -> bool {
        false
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        file::read_at(self, offset, len, buf, data)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        file::write_at(self, offset, len, buf, data)
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        file::sync_file(self, datasync, data)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        file::sync_file_range(self, start, end, datasync, data)
    }

    fn flush_file(
        &self,
        data: crate::libs::mutex::MutexGuard<FilePrivateData>,
        lock_owner: u64,
    ) -> Result<(), SystemError> {
        file::flush_file(self, data, lock_owner)
    }

    fn close(
        &self,
        data: crate::libs::mutex::MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        file::close(data)
    }

    fn check_mmap_file(
        &self,
        file: &Arc<File>,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        file::check_mmap_file(self, file, len, offset, vm_flags)
    }

    fn mmap_effective_file(&self, file: &Arc<File>) -> Result<Arc<File>, SystemError> {
        file::mmap_effective_file(self, file)
    }

    fn mmap_file(
        &self,
        file: &Arc<File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: VmFlags,
    ) -> Result<(), SystemError> {
        file::mmap_file(self, file, start, len, offset, vm_flags)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        None
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.lock().upgrade().unwrap()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.load_origin_once()?;
        super::metadata::metadata(self)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        super::metadata::set_metadata_masked(
            self,
            metadata,
            SetMetadataMask::MODE
                | SetMetadataMask::UID
                | SetMetadataMask::GID
                | SetMetadataMask::ATIME
                | SetMetadataMask::MTIME
                | SetMetadataMask::CTIME,
        )
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        super::metadata::set_metadata_masked(self, metadata, mask)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        super::metadata::resize_with_lock_owner(self, len, 0)
    }

    fn resize_with_lock_owner(&self, len: usize, lock_owner: u64) -> Result<(), SystemError> {
        super::metadata::resize_with_lock_owner(self, len, lock_owner)
    }

    fn resize_file(
        &self,
        len: usize,
        lock_owner: u64,
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        super::metadata::resize_file(self, len, lock_owner, data)
    }

    fn resize_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        super::metadata::resize_with_metadata(self, len, lock_owner, metadata, mask)
    }

    fn resize_file_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        super::metadata::resize_file_with_metadata(self, len, lock_owner, data, metadata, mask)
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        super::metadata::getxattr(self, name, buf)
    }

    fn setxattr(&self, name: &str, value: &[u8], flags: XattrFlags) -> Result<usize, SystemError> {
        super::metadata::setxattr(self, name, value, flags)
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        super::metadata::listxattr(self, buf)
    }

    fn removexattr(&self, name: &str) -> Result<usize, SystemError> {
        super::metadata::removexattr(self, name)
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from(
            self.redirect
                .rsplit('/')
                .next()
                .unwrap_or(&self.redirect)
                .to_string(),
        ))
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.overlay_fs()?;
        let Some(parent_redirect) = self.parent_redirect() else {
            return Ok(fs.root_inode.clone());
        };

        if parent_redirect.is_empty() {
            return Ok(fs.root_inode.clone());
        }

        let root: Arc<dyn IndexNode> = fs.root_inode.clone();
        root.lookup(parent_redirect)
    }

    fn list(&self) -> Result<Vec<String>, system_error::SystemError> {
        readdir::list(self)
    }

    fn mkdir(
        &self,
        name: &str,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        dir::mkdir(self, name, mode)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        dir::rmdir(self, name)
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        dir::unlink(self, name)
    }

    fn link(
        &self,
        name: &str,
        other: &Arc<dyn IndexNode>,
    ) -> Result<(), system_error::SystemError> {
        dir::link(self, name, other)
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        dir::create(self, name, file_type, mode)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        rename::move_to(self, old_name, target, new_name, flags)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        lookup::find(self, name)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: vfs::InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        dir::mknod(self, filename, mode, dev_t)
    }
}
