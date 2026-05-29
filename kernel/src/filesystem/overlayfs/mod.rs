#![allow(dead_code, unused_variables, unused_imports)]
pub mod copy_up;
pub mod entry;

use super::ramfs::{LockedRamFSInode, RamFSInode};
use super::vfs::utils::DName;
use super::vfs::FSMAKER;
use super::vfs::{
    self, FileSystem, FileType, FsInfo, IndexNode, Metadata, MountableFileSystem, SuperBlock,
};
use crate::driver::base::device::device_number::DeviceNumber;
use crate::driver::base::device::device_number::Major;
use crate::filesystem::vfs::{FileSystemMaker, FileSystemMakerData};
use crate::libs::mutex::Mutex;
use crate::process::ProcessManager;
use crate::register_mountable_fs;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use entry::{OvlEntry, OvlLayer};
use linkme::distributed_slice;
use system_error::SystemError;

const WHITEOUT_MODE: u64 = 0o020000 | 0o600; // whiteout字符设备文件模式与权限
const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0); // Whiteout 文件设备号
const WHITEOUT_FLAG: u64 = 0x1;
type LowerRoot = (String, Arc<dyn IndexNode>);

#[derive(Debug)]
pub struct OverlayMountData {
    upper_dir: String,
    lower_dirs: Vec<String>,
    work_dir: String,
}

impl OverlayMountData {
    pub fn from_raw(raw_data: Option<&str>) -> Result<Self, SystemError> {
        if raw_data.is_none() {
            return Err(SystemError::EINVAL);
        }
        let raw_str = raw_data.unwrap();
        let mut data = OverlayMountData {
            upper_dir: String::new(),
            lower_dirs: Vec::new(),
            work_dir: String::new(),
        };

        for pair in raw_str.split(',') {
            let mut parts = pair.split('=');
            let key = parts.next().ok_or(SystemError::EINVAL)?;
            let value = parts.next().ok_or(SystemError::EINVAL)?;

            match key {
                "upperdir" => data.upper_dir = value.into(),
                "lowerdir" => data.lower_dirs = value.split(':').map(|s| s.into()).collect(),
                "workdir" => data.work_dir = value.into(),
                _ => return Err(SystemError::EINVAL),
            }
        }
        Ok(data)
    }
}
impl FileSystemMakerData for OverlayMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
#[derive(Debug)]
pub struct OvlSuperBlock {
    super_block: SuperBlock,
    pseudo_dev: DeviceNumber, // 虚拟设备号
    is_lower: bool,
}

#[derive(Debug)]
struct OverlayFS {
    numlayer: usize,
    numfs: u32,
    numdatalayer: usize,
    layers: Vec<OvlLayer>, // 第0层为读写层，后面是只读层
    workdir: Arc<OvlInode>,
    root_inode: Arc<OvlInode>,
    super_block: SuperBlock,
}

#[derive(Debug)]
pub struct OvlInode {
    redirect: String, // 重定向路径
    file_type: FileType,
    flags: Mutex<u64>,
    upper_inode: Mutex<Option<Arc<dyn IndexNode>>>, // 读写层
    lower_inodes: Vec<Arc<dyn IndexNode>>,          // 只读层（支持多层）
    oe: Arc<OvlEntry>,
    fs: Mutex<Weak<OverlayFS>>,
}
impl OvlInode {
    pub fn new(
        redirect: String,
        file_type: FileType,
        upper: Option<Arc<dyn IndexNode>>,
        lower_inodes: Vec<Arc<dyn IndexNode>>,
    ) -> Self {
        Self {
            redirect,
            file_type,
            flags: Mutex::new(0),
            upper_inode: Mutex::new(upper),
            lower_inodes,
            oe: Arc::new(OvlEntry::new()),
            fs: Mutex::new(Weak::default()),
        }
    }

    fn set_fs(&self, fs: Weak<OverlayFS>) {
        *self.fs.lock() = fs;
    }
}

impl FileSystem for OverlayFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "overlayfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }
}

impl OverlayFS {
    pub fn ovl_upper_mnt(&self) -> Arc<OvlInode> {
        self.layers[0].mnt.clone()
    }
}

impl MountableFileSystem for OverlayFS {
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<OverlayMountData>())
            .ok_or(SystemError::EINVAL)?;
        let root_inode = ProcessManager::current_mntns().root_inode();
        let upper_inode = root_inode
            .lookup(&mount_data.upper_dir)
            .map_err(|_| SystemError::EINVAL)?;
        let upper_file_type = upper_inode.metadata()?.file_type;
        let upper_layer = OvlLayer {
            mnt: Arc::new(OvlInode::new(
                mount_data.upper_dir.clone(),
                upper_file_type,
                Some(upper_inode.clone()),
                Vec::new(),
            )),
            index: 0,
            fsid: 0,
        };

        let lower_roots: Result<Vec<LowerRoot>, SystemError> = mount_data
            .lower_dirs
            .iter()
            .map(|dir| {
                let lower_inode = ProcessManager::current_mntns()
                    .root_inode()
                    .lookup(dir)
                    .map_err(|_| SystemError::EINVAL)?;
                Ok((dir.clone(), lower_inode))
            })
            .collect();

        let lower_roots = lower_roots?;

        let lower_layers: Result<Vec<OvlLayer>, SystemError> = lower_roots
            .iter()
            .enumerate()
            .map(|(i, (dir, lower_inode))| {
                let lower_file_type = lower_inode.metadata()?.file_type;
                Ok(OvlLayer {
                    mnt: Arc::new(OvlInode::new(
                        dir.clone(),
                        lower_file_type,
                        None,
                        vec![lower_inode.clone()],
                    )),
                    index: (i + 1) as u32,
                    fsid: (i + 1) as u32,
                })
            })
            .collect();

        let lower_layers = lower_layers?;

        let workdir = Arc::new(OvlInode::new(
            mount_data.work_dir.clone(),
            FileType::Dir,
            None,
            Vec::new(),
        ));

        if lower_roots.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let mut layers = Vec::new();
        layers.push(upper_layer);
        layers.extend(lower_layers);

        let root_inode = Arc::new(OvlInode::new(
            String::new(),
            upper_file_type,
            Some(upper_inode),
            lower_roots
                .iter()
                .map(|(_, lower_inode)| lower_inode.clone())
                .collect(),
        ));

        let super_block = SuperBlock::new(vfs::Magic::OVERLAYFS_MAGIC, 4096, 255);
        let fs = Arc::new_cyclic(|weak_fs| {
            for layer in &layers {
                layer.mnt.set_fs(weak_fs.clone());
            }
            workdir.set_fs(weak_fs.clone());
            root_inode.set_fs(weak_fs.clone());

            OverlayFS {
                numlayer: layers.len(),
                numfs: 1,
                numdatalayer: lower_roots.len(),
                layers,
                workdir,
                root_inode,
                super_block: super_block.clone(),
            }
        });
        Ok(fs)
    }

    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = OverlayMountData::from_raw(raw_data).map_err(|e| {
            log::error!("Failed to create overlay mount data: {:?}", e);
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
    }
}

register_mountable_fs!(OverlayFS, OVERLAYFSMAKER, "overlay");

impl OvlInode {
    pub fn ovl_lower_redirect(&self) -> Option<&str> {
        if !self.lower_inodes.is_empty()
            && (self.file_type == FileType::File || self.file_type == FileType::Dir)
        {
            Some(&self.redirect)
        } else {
            None
        }
    }

    fn overlay_fs(&self) -> Result<Arc<OverlayFS>, SystemError> {
        self.fs.lock().upgrade().ok_or(SystemError::EINVAL)
    }

    fn upper_root_inode(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let upper_mnt = self.overlay_fs()?.ovl_upper_mnt();
        let upper_inode = upper_mnt.upper_inode.lock();
        upper_inode.clone().ok_or(SystemError::EROFS)
    }

    fn child_redirect(&self, name: &str) -> String {
        if self.redirect.is_empty() {
            String::from(name)
        } else {
            let mut redirect = self.redirect.clone();
            redirect.push('/');
            redirect.push_str(name);
            redirect
        }
    }

    fn ensure_upper_dir_path(&self, path: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut current = self.upper_root_inode()?;
        if path.is_empty() {
            return Ok(current);
        }

        let mut current_path = String::new();
        for component in path.split('/').filter(|component| !component.is_empty()) {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(component);

            match current.find(component) {
                Ok(next) => current = next,
                Err(SystemError::ENOENT) => {
                    let mode = self.lower_dir_mode(&current_path)?;
                    current = current.mkdir(component, mode)?;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(current)
    }

    fn lower_dir_mode(&self, path: &str) -> Result<vfs::InodeMode, SystemError> {
        let fs = self.overlay_fs()?;
        for layer in fs.layers.iter().skip(1) {
            if let Some(lower_root) = layer.mnt.lower_inodes.first() {
                if let Ok(inode) = lower_root.lookup(path) {
                    return Ok(inode.metadata()?.mode);
                }
            }
        }

        Ok(vfs::InodeMode::S_IRWXUGO)
    }

    fn is_whiteout_inode(inode: &Arc<dyn IndexNode>) -> bool {
        inode
            .metadata()
            .map(|metadata| {
                metadata.file_type == FileType::CharDevice && metadata.raw_dev == WHITEOUT_DEV
            })
            .unwrap_or(false)
    }

    pub fn create_whiteout(&self, name: &str) -> Result<(), SystemError> {
        let whiteout_mode = vfs::InodeMode::S_IFCHR;
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }

        self.copy_up()?;
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
            return Ok(());
        }
        Err(SystemError::EROFS)
    }

    fn is_whiteout(&self) -> bool {
        self.file_type == FileType::CharDevice
            && self
                .metadata()
                .map(|metadata| metadata.raw_dev == WHITEOUT_DEV)
                .unwrap_or(false)
    }

    fn has_whiteout(&self, name: &str) -> bool {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            if let Ok(inode) = upper_inode.find(name) {
                return Self::is_whiteout_inode(&inode);
            }
        }
        false
    }

    fn remove_whiteout_if_present(&self, name: &str) -> Result<bool, SystemError> {
        let upper_inode = self.upper_inode.lock().clone().ok_or(SystemError::EROFS)?;
        match upper_inode.find(name) {
            Ok(inode) => {
                if Self::is_whiteout_inode(&inode) {
                    upper_inode.unlink(name)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(SystemError::ENOENT) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn is_dot_entry(name: &str) -> bool {
        name == "." || name == ".."
    }

    fn is_dir_empty(inode: &Arc<dyn IndexNode>) -> Result<bool, SystemError> {
        Ok(inode.list()?.iter().all(|entry| Self::is_dot_entry(entry)))
    }
}

impl IndexNode for OvlInode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.read_at(offset, len, buf, data);
        }

        let mut lower_inodes = self.lower_inodes.iter();
        if let Some(lower_inode) = lower_inodes.next() {
            match lower_inode.read_at(offset, len, buf, data) {
                Ok(read_len) => return Ok(read_len),
                Err(mut err) => {
                    for lower_inode in lower_inodes {
                        let lock = Mutex::new(vfs::FilePrivateData::Unused);
                        match lower_inode.read_at(offset, len, buf, lock.lock()) {
                            Ok(read_len) => return Ok(read_len),
                            Err(next_err) => err = next_err,
                        }
                    }
                    return Err(err);
                }
            }
        }

        Err(SystemError::ENOENT)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if (*self.upper_inode.lock()).is_none() {
            self.copy_up()?;
        }
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.write_at(offset, len, buf, data);
        }

        Err(SystemError::EROFS)
    }

    fn sync_file(
        &self,
        datasync: bool,
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.sync_file(datasync, _data);
        }

        if !self.lower_inodes.is_empty() {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.sync_file_range(start, end, datasync, data);
        }

        if !self.lower_inodes.is_empty() {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.lock().upgrade().unwrap()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.metadata();
        }

        for lower_inode in &self.lower_inodes {
            if let Ok(metadata) = lower_inode.metadata() {
                return Ok(metadata);
            }
        }
        Ok(Metadata::default())
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from(self.redirect.clone()))
    }

    fn list(&self) -> Result<Vec<String>, system_error::SystemError> {
        let mut entries: Vec<String> = Vec::new();
        let mut hidden_entries: Vec<String> = Vec::new();
        let upper_entries = if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.list()?
        } else {
            Vec::new()
        };

        for entry in upper_entries {
            if !self.has_whiteout(&entry) {
                entries.push(entry);
            }
        }

        for lower_inode in &self.lower_inodes {
            let lower_entries = lower_inode.list()?;
            for entry in lower_entries {
                if entries.contains(&entry) || hidden_entries.contains(&entry) {
                    continue;
                }
                if Self::is_dot_entry(&entry) {
                    entries.push(entry);
                    continue;
                }
                if self.has_whiteout(&entry) {
                    hidden_entries.push(entry);
                    continue;
                }
                match lower_inode.find(&entry) {
                    Ok(inode) => {
                        if Self::is_whiteout_inode(&inode) {
                            hidden_entries.push(entry);
                            continue;
                        }
                    }
                    Err(SystemError::ENOENT) => continue,
                    Err(err) => return Err(err),
                }
                entries.push(entry);
            }
        }

        Ok(entries)
    }

    fn mkdir(
        &self,
        name: &str,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.mkdir(name, mode)
        } else {
            Err(SystemError::EROFS)
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            match upper_inode.rmdir(name) {
                Ok(()) => return Ok(()),
                Err(SystemError::ENOENT) => {}
                Err(err) => return Err(err),
            }
        }

        match self.find(name) {
            Ok(inode) => {
                if inode.metadata()?.file_type != FileType::Dir {
                    return Err(SystemError::ENOTDIR);
                }
                if !Self::is_dir_empty(&inode)? {
                    return Err(SystemError::ENOTEMPTY);
                }
                return self.create_whiteout(name);
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }

        Err(SystemError::ENOENT)
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            match upper_inode.unlink(name) {
                Ok(()) => return Ok(()),
                Err(SystemError::ENOENT) => {}
                Err(err) => return Err(err),
            }
        }

        match self.find(name) {
            Ok(inode) => {
                if inode.metadata()?.file_type == FileType::Dir {
                    return Err(SystemError::EISDIR);
                }
                return self.create_whiteout(name);
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }

        Err(SystemError::ENOENT)
    }

    fn link(
        &self,
        name: &str,
        other: &Arc<dyn IndexNode>,
    ) -> Result<(), system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.link(name, other)
        } else {
            Err(SystemError::EROFS)
        }
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        let upper_inode = self.upper_inode.lock().clone().ok_or(SystemError::EROFS)?;
        self.remove_whiteout_if_present(name)?;
        upper_inode.create(name, file_type, mode)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        let mut upper_inode = None;
        let mut upper_file_type = None;
        if let Some(ref upper) = *self.upper_inode.lock() {
            match upper.find(name) {
                Ok(inode) => {
                    if Self::is_whiteout_inode(&inode) {
                        return Err(SystemError::ENOENT);
                    }
                    upper_file_type = Some(inode.metadata()?.file_type);
                    upper_inode = Some(inode);
                }
                Err(SystemError::ENOENT) => {}
                Err(err) => return Err(err),
            }
        }

        if self.has_whiteout(name) {
            return Err(SystemError::ENOENT);
        }

        let mut lower_inodes = Vec::new();
        if matches!(upper_file_type, None | Some(FileType::Dir)) {
            let mut merge_dirs = upper_file_type == Some(FileType::Dir);
            for lower in &self.lower_inodes {
                match lower.find(name) {
                    Ok(inode) => {
                        if Self::is_whiteout_inode(&inode) {
                            if upper_inode.is_none() {
                                return Err(SystemError::ENOENT);
                            }
                            break;
                        }
                        let lower_file_type = inode.metadata()?.file_type;
                        if merge_dirs {
                            if lower_file_type == FileType::Dir {
                                lower_inodes.push(inode);
                                continue;
                            }
                            break;
                        }

                        lower_inodes.push(inode);
                        if lower_file_type == FileType::Dir {
                            merge_dirs = true;
                        } else {
                            break;
                        }
                    }
                    Err(SystemError::ENOENT) => {}
                    Err(err) => return Err(err),
                }
            }
        }

        if upper_inode.is_none() && lower_inodes.is_empty() {
            return Err(SystemError::ENOENT);
        }

        let file_type = if let Some(file_type) = upper_file_type {
            file_type
        } else {
            lower_inodes[0].metadata()?.file_type
        };

        let inode = Arc::new(OvlInode::new(
            self.child_redirect(name),
            file_type,
            upper_inode,
            lower_inodes,
        ));
        inode.set_fs(self.fs.lock().clone());

        Ok(inode)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: vfs::InodeMode,
        dev_t: crate::driver::base::device::device_number::DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref inode) = *upper_inode {
            inode.mknod(filename, mode, dev_t)
        } else {
            Err(SystemError::EROFS)
        }
    }
}
