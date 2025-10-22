use alloc::string::ToString;
use core::{cmp::min, fmt::Debug, intrinsics::unlikely};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use log::warn;
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    libs::{
        casting::DowncastArc,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{ProcessManager, namespace::mnt::MountPropagation},
    time::PosixTimeSpec,
};

use self::{
    callback::{KernCallbackData, KernFSCallback, KernInodePrivateData},
    dynamic::DynamicLookup,
};

use super::vfs::{
    file::FileMode, syscall::ModeType, vcore::generate_inode_id, FilePrivateData, FileSystem,
    FileType, FsInfo, IndexNode, InodeId, Magic, Metadata, SuperBlock,
    mount::{MountFS, MountFlags, MountPath},
};

pub mod callback;
pub mod dynamic;

#[derive(Debug)]
pub struct KernFS {
    root_inode: Arc<KernFSInode>,
    fsname: &'static str,
}

impl FileSystem for KernFS {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: KernFS::MAX_NAMELEN,
        };
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        return self.root_inode.clone();
    }

    fn name(&self) -> &str {
        self.fsname
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::KER_MAGIC,
            KernFS::KERNFS_BLOCK_SIZE,
            KernFS::MAX_NAMELEN as u64,
        )
    }
}

impl KernFS {
    pub const MAX_NAMELEN: usize = 4096;
    pub const KERNFS_BLOCK_SIZE: u64 = 512;

    #[inline(never)]
    pub fn new(fsname: &'static str) -> Arc<Self> {
        let root_inode = Self::create_root_inode();
        let fs = Arc::new(Self {
            root_inode: root_inode.clone(),
            fsname,
        });

        root_inode.inner.write().parent = Arc::downgrade(&root_inode);
        *root_inode.fs.write() = Arc::downgrade(&fs);
        return fs;
    }

    fn create_root_inode() -> Arc<KernFSInode> {
        let metadata = Metadata {
            size: 0,
            mode: ModeType::from_bits_truncate(0o755),
            uid: 0,
            gid: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::new(0, 0),
            mtime: PosixTimeSpec::new(0, 0),
            ctime: PosixTimeSpec::new(0, 0),
            btime: PosixTimeSpec::new(0, 0),
            dev_id: 0,
            inode_id: generate_inode_id(),
            file_type: FileType::Dir,
            nlinks: 1,
            raw_dev: DeviceNumber::default(),
        };
        let root_inode = Arc::new_cyclic(|self_ref| KernFSInode {
            name: String::from(""),
            inner: RwLock::new(InnerKernFSInode {
                parent: Weak::new(),
                metadata,
                symlink_target: None,
                symlink_target_absolute_path: None,
            }),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::new()),
            private_data: SpinLock::new(None),
            callback: None,
            children: SpinLock::new(HashMap::new()),
            inode_type: KernInodeType::Dir,
            lazy_list: SpinLock::new(HashMap::new()),
            dynamic_lookup: RwLock::new(None),
            is_temporary: false,
        });

        return root_inode;
    }
}

#[derive(Debug)]
pub struct KernFSInode {
    inner: RwLock<InnerKernFSInode>,
    /// 指向当前Inode所属的文件系统的弱引用
    fs: RwLock<Weak<KernFS>>,
    /// 指向自身的弱引用
    self_ref: Weak<KernFSInode>,
    /// 私有数据
    private_data: SpinLock<Option<KernInodePrivateData>>,
    /// 回调函数
    callback: Option<&'static dyn KernFSCallback>,
    /// 子Inode
    children: SpinLock<HashMap<String, Arc<KernFSInode>>>,
    /// Inode类型
    inode_type: KernInodeType,
    /// Inode名称
    name: String,
    /// lazy list
    lazy_list: SpinLock<HashMap<String, fn() -> KernFSInodeArgs>>,
    /// 动态查找提供者（可选）
    dynamic_lookup: RwLock<Option<Arc<dyn DynamicLookup>>>,
    /// 是否为临时节点（不会被添加到父目录的children中）
    is_temporary: bool,
}

pub struct KernFSInodeArgs {
    pub mode: ModeType,
    pub inode_type: KernInodeType,
    pub size: Option<usize>,
    pub private_data: Option<KernInodePrivateData>,
    pub callback: Option<&'static dyn KernFSCallback>,
}

#[derive(Debug)]
pub struct InnerKernFSInode {
    parent: Weak<KernFSInode>,

    /// 当前inode的元数据
    metadata: Metadata,
    /// 符号链接指向的inode（仅当inode_type为SymLink时有效）
    symlink_target: Option<Weak<KernFSInode>>,
    symlink_target_absolute_path: Option<String>,
}

impl IndexNode for KernFSInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        if let Some(callback) = self.callback {
            let callback_data =
                KernCallbackData::new(self.self_ref.upgrade().unwrap(), self.private_data.lock());
            return callback.open(callback_data);
        }

        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.inner.read().metadata.clone());
    }

    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }

    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: ModeType,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 应当通过kernfs的其它方法来创建文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        if unlikely(name.len() > KernFS::MAX_NAMELEN) {
            return Err(SystemError::ENAMETOOLONG);
        }
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }
        match name {
            "" | "." => {
                return Ok(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?);
            }

            ".." => {
                return Ok(self
                    .inner
                    .read()
                    .parent
                    .upgrade()
                    .ok_or(SystemError::ENOENT)?);
            }
            name => {
                // 在子目录项中查找
                let child = self.children.lock().get(name).cloned();
                if let Some(child) = child {
                    return Ok(child);
                }
                let lazy_list = self.lazy_list.lock();
                if let Some(provider) = lazy_list.get(name) {
                    // 如果存在lazy list，则调用提供者函数创建
                    let args = provider();
                    let inode = self.inner_create(
                        name.to_string(),
                        args.inode_type,
                        args.mode,
                        args.size.unwrap_or(4096),
                        args.private_data,
                        args.callback,
                    )?;
                    return Ok(inode);
                }
                
                // 尝试动态查找
                if let Some(provider) = self.dynamic_lookup.read().as_ref() {
                    match provider.dynamic_find(name)? {
                        Some(inode) => return Ok(inode),
                        None => {} // 继续返回 ENOENT
                    }
                }
                
                Err(SystemError::ENOENT)
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        if self.inode_type != KernInodeType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let children = self.children.lock();
        let r = children
            .iter()
            .find(|(_, v)| v.metadata().unwrap().inode_id == ino)
            .map(|(k, _)| k.clone());

        return r.ok_or(SystemError::ENOENT);
    }

    fn get_entry_name_and_metadata(&self, ino: InodeId) -> Result<(String, Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn sync(&self) -> Result<(), SystemError> {
        return Ok(());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.fs.read().upgrade().unwrap();
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        
        // 检查是否有动态查找提供者
        if let Some(provider) = self.dynamic_lookup.read().as_ref() {
            // 添加所有静态子目录（包括非PID目录如cpuinfo, meminfo等）
            for child_name in self.children.lock().keys() {
                keys.push(child_name.clone());
            }
            
            // 添加动态条目（PID目录应该完全通过这里提供）
            let mut dynamic_entries = provider.dynamic_list()?;
            
            // 去重并合并（动态条目优先级更高）
            for entry in dynamic_entries.drain(..) {
                if !keys.contains(&entry) {
                    keys.push(entry);
                }
            }
            
            // 排序以获得一致的输出
            keys.sort();
        } else {
            // 没有动态查找提供者，直接添加所有静态子目录
            self.children
                .lock()
                .keys()
                .for_each(|x| keys.push(x.clone()));
        }

        return Ok(keys);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if self.inode_type == KernInodeType::SymLink {
            let inner = self.inner.read();
            if offset >= inner.symlink_target_absolute_path.as_ref().unwrap().len() {
                return Ok(0);
            }
            let len = min(len, buf.len());
            let len = min(
                len,
                inner.symlink_target_absolute_path.as_ref().unwrap().len() - offset,
            );
            buf[0..len].copy_from_slice(
                &inner
                    .symlink_target_absolute_path
                    .as_ref()
                    .unwrap()
                    .as_bytes()[offset..offset + len],
            );
            return Ok(len);
        }
        if self.inode_type != KernInodeType::File {
            return Err(SystemError::EISDIR);
        }

        if self.callback.is_none() {
            warn!("kernfs: callback is none");
            return Err(SystemError::ENOSYS);
        }
        // release the private data lock before calling the callback
        drop(data);

        let callback_data =
            KernCallbackData::new(self.self_ref.upgrade().unwrap(), self.private_data.lock());
        return self
            .callback
            .as_ref()
            .unwrap()
            .read(callback_data, &mut buf[..len], offset);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if self.inode_type != KernInodeType::File {
            return Err(SystemError::EISDIR);
        }

        if self.callback.is_none() {
            return Err(SystemError::ENOSYS);
        }

        // release the private data lock before calling the callback
        drop(data);

        let callback_data =
            KernCallbackData::new(self.self_ref.upgrade().unwrap(), self.private_data.lock());
        return self
            .callback
            .as_ref()
            .unwrap()
            .write(callback_data, &buf[..len], offset);
    }

    fn mount(
        &self,
        fs: Arc<dyn FileSystem>,
        mount_flags: MountFlags,
    ) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 创建 MountFS 来处理挂载
        let new_mount_fs = MountFS::new(
            fs,
            None, // KernFS 节点没有父挂载点
            MountPropagation::new_private(),
            Some(&ProcessManager::current_mntns()),
            mount_flags,
        );

        // KernFSInode 不应该调用 absolute_path()，因为该方法只为 MountFS 设计
        // 对于 KernFS 挂载，我们采用更直接的方式：
        // 1. 如果是特定已知路径（如 cgroup），使用预定义路径
        // 2. 否则返回错误，要求调用者使用其他方式挂载
        let mount_path = self.get_known_mount_path().ok_or_else(|| {
            log::error!("KernFSInode::mount: Cannot determine mount path for KernFS inode. Consider using MountFSInode for mounting.");
            SystemError::ENOSYS
        })?;
        let mount_path = Arc::new(MountPath::from(mount_path));
        ProcessManager::current_mntns().add_mount(
            Some(metadata.inode_id),
            mount_path,
            new_mount_fs.clone(),
        )?;

        Ok(new_mount_fs)
    }
}

impl KernFSInode {
    pub fn new(
        parent: Option<Arc<KernFSInode>>,
        name: String,
        mut metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Arc<KernFSInode> {
        Self::new_with_temporary(
            parent,
            name,
            metadata,
            inode_type,
            private_data,
            callback,
            false, // 不是临时节点
        )
    }

    pub fn new_with_temporary(
        parent: Option<Arc<KernFSInode>>,
        name: String,
        mut metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
        is_temporary: bool,
    ) -> Arc<KernFSInode> {
        metadata.file_type = inode_type.into();
        let parent: Weak<KernFSInode> = parent.map(|x| Arc::downgrade(&x)).unwrap_or_default();

        let inode = Arc::new(KernFSInode {
            name,
            inner: RwLock::new(InnerKernFSInode {
                parent: parent.clone(),
                metadata,
                symlink_target: None,
                symlink_target_absolute_path: None,
            }),
            self_ref: Weak::new(),
            fs: RwLock::new(Weak::new()),
            private_data: SpinLock::new(private_data),
            callback,
            children: SpinLock::new(HashMap::new()),
            inode_type,
            lazy_list: SpinLock::new(HashMap::new()),
            dynamic_lookup: RwLock::new(None),
            is_temporary,
        });

        {
            let ptr = inode.as_ref() as *const KernFSInode as *mut KernFSInode;
            unsafe {
                (*ptr).self_ref = Arc::downgrade(&inode);
            }
        }
        if parent.strong_count() > 0 {
            let kernfs = parent
                .upgrade()
                .unwrap()
                .fs()
                .downcast_arc::<KernFS>()
                .expect("KernFSInode::new: parent is not a KernFS instance");
            *inode.fs.write() = Arc::downgrade(&kernfs);
        }
        return inode;
    }

    /// 在当前inode下增加子目录
    ///
    /// ## 参数
    ///
    /// - `name`：子目录名称
    /// - `mode`：子目录权限
    /// - `private_data`：子目录私有数据
    /// - `callback`：子目录回调函数
    ///
    /// ## 返回值
    ///
    /// - 成功：子目录inode
    /// - 失败：错误码
    #[allow(dead_code)]
    #[inline]
    pub fn add_dir(
        &self,
        name: String,
        mode: ModeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        return self.inner_create(name, KernInodeType::Dir, mode, 0, private_data, callback);
    }

    /// 在当前inode下增加文件
    ///
    /// ## 参数
    ///
    /// - `name`：文件名称
    /// - `mode`：文件权限
    /// - `size`：文件大小(如果不指定，则默认为4096)
    /// - `private_data`：文件私有数据
    /// - `callback`：文件回调函数
    ///
    ///
    /// ## 返回值
    ///
    /// - 成功：文件inode
    /// - 失败：错误码
    #[allow(dead_code)]
    #[inline]
    pub fn add_file(
        &self,
        name: String,
        mode: ModeType,
        size: Option<usize>,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        let size = size.unwrap_or(4096);
        return self.inner_create(
            name,
            KernInodeType::File,
            mode,
            size,
            private_data,
            callback,
        );
    }

    pub fn add_file_lazy(
        &self,
        name: String,
        provider: fn() -> KernFSInodeArgs,
    ) -> Result<(), SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }
        self.lazy_list.lock().insert(name, provider);
        Ok(())
    }

    /// 在当前inode下增加临时目录
    ///
    /// 临时目录不会被添加到父目录的children中，适用于动态内容
    ///
    /// ## 参数
    ///
    /// - `name`：临时目录名称
    /// - `mode`：临时目录权限
    /// - `private_data`：临时目录私有数据
    /// - `callback`：临时目录回调函数
    ///
    /// ## 返回值
    ///
    /// - 成功：临时目录inode
    /// - 失败：错误码
    #[allow(dead_code)]
    #[inline]
    pub fn create_temporary_dir(
        &self,
        name: &str,
        mode: ModeType,
        private_data: Option<KernInodePrivateData>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        return self.inner_create_with_temporary(
            name.to_string(),
            KernInodeType::Dir,
            mode,
            0,
            private_data,
            None,
            true, // 是临时节点
        );
    }

    /// 在当前inode下增加临时文件
    ///
    /// 临时文件不会被添加到父目录的children中，适用于动态内容
    ///
    /// ## 参数
    ///
    /// - `name`：临时文件名称
    /// - `mode`：临时文件权限
    /// - `size`：临时文件大小(如果不指定，则默认为4096)
    /// - `private_data`：临时文件私有数据
    /// - `callback`：临时文件回调函数
    ///
    /// ## 返回值
    ///
    /// - 成功：临时文件inode
    /// - 失败：错误码
    #[allow(dead_code)]
    #[inline]
    pub fn create_temporary_file(
        &self,
        name: &str,
        mode: ModeType,
        size: Option<usize>,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        let size = size.unwrap_or(4096);
        return self.inner_create_with_temporary(
            name.to_string(),
            KernInodeType::File,
            mode,
            size,
            private_data,
            callback,
            true, // 是临时节点
        );
    }

    fn inner_create(
        &self,
        name: String,
        file_type: KernInodeType,
        mode: ModeType,
        mut size: usize,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        self.inner_create_with_temporary(
            name,
            file_type,
            mode,
            size,
            private_data,
            callback,
            false, // 不是临时节点
        )
    }

    fn inner_create_with_temporary(
        &self,
        name: String,
        file_type: KernInodeType,
        mode: ModeType,
        mut size: usize,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
        is_temporary: bool,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        match file_type {
            KernInodeType::Dir | KernInodeType::SymLink => {
                size = 0;
            }
            _ => {}
        }

        let metadata = Metadata {
            size: size as i64,
            mode,
            uid: 0,
            gid: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::new(0, 0),
            mtime: PosixTimeSpec::new(0, 0),
            ctime: PosixTimeSpec::new(0, 0),
            btime: PosixTimeSpec::new(0, 0),
            dev_id: 0,
            inode_id: generate_inode_id(),
            file_type: file_type.into(),
            nlinks: 1,
            raw_dev: DeviceNumber::default(),
        };

        let new_inode: Arc<KernFSInode> = Self::new_with_temporary(
            Some(self.self_ref.upgrade().unwrap()),
            name.clone(),
            metadata,
            file_type,
            private_data,
            callback,
            is_temporary,
        );

        // 只有非临时节点才被添加到父目录的children中
        if !is_temporary {
            self.children.lock().insert(name, new_inode.clone());
        }

        return Ok(new_inode);
    }

    /// 在当前inode下删除子目录或者文件
    ///
    /// 如果要删除的是子目录，且子目录不为空，则返回ENOTEMPTY
    ///
    /// ## 参数
    ///
    /// - `name`：子目录或者文件名称
    ///
    /// ## 返回值
    ///
    /// - 成功：()
    /// - 失败：错误码
    #[allow(dead_code)]
    pub fn remove(&self, name: &str) -> Result<(), SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        let mut children = self.children.lock();
        let inode = children.get(name).ok_or(SystemError::ENOENT)?;
        if inode.children.lock().is_empty() {
            children.remove(name);
            return Ok(());
        } else {
            return Err(SystemError::ENOTEMPTY);
        }
    }

    

    /// add_link - create a symlink in kernfs
    ///
    /// ## 参数
    ///
    /// - `parent`: directory to create the symlink in
    /// - `name`: name of the symlink
    /// - `target`: target node for the symlink to point to
    ///
    /// Returns the created node on success
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/kernfs/symlink.c#25
    pub fn add_link(
        &self,
        name: String,
        target: &Arc<KernFSInode>,
        target_absolute_path: String,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        // debug!("kernfs add link: name:{name}, target path={target_absolute_path}");
        let inode = self.inner_create(
            name,
            KernInodeType::SymLink,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777),
            0,
            None,
            None,
        )?;

        inode.inner.write().symlink_target = Some(Arc::downgrade(target));
        inode.inner.write().symlink_target_absolute_path = Some(target_absolute_path);
        return Ok(inode);
    }

    pub fn name(&self) -> &str {
        return &self.name;
    }

    pub fn parent(&self) -> Option<Arc<KernFSInode>> {
        return self.inner.read().parent.upgrade();
    }

    pub fn private_data_mut(&self) -> SpinLockGuard<'_, Option<KernInodePrivateData>> {
        return self.private_data.lock();
    }

    #[allow(dead_code)]
    pub fn symlink_target(&self) -> Option<Arc<KernFSInode>> {
        return self.inner.read().symlink_target.as_ref()?.upgrade();
    }

    /// remove a kernfs_node recursively
    pub fn remove_recursive(&self) {
        let mut children = self.children.lock().drain().collect::<Vec<_>>();
        while let Some((_, child)) = children.pop() {
            children.append(&mut child.children.lock().drain().collect::<Vec<_>>());
        }
    }

    /// 删除当前的inode（包括其自身、子目录和子文件）
    /// 
    /// 这个方法会：
    /// 1. 递归删除所有子节点
    /// 2. 从父目录中移除自己
    /// 3. 清理相关资源
    #[allow(dead_code)]
    pub fn remove_inode_include_self(&self) {
        // 如果是目录，先递归删除所有子节点
        if self.inode_type == KernInodeType::Dir {
            let children_names: Vec<String> = self.children.lock().keys().cloned().collect();
            for child_name in children_names {
                if let Ok(child_inode) = self.find(&child_name) {
                    if let Some(kernfs_child) = child_inode.downcast_arc::<KernFSInode>() {
                        kernfs_child.remove_inode_include_self();
                    }
                }
            }
        }
        
        // 从父节点的children中移除自己
        if let Some(parent) = self.parent() {
            let name = self.name().to_string();
            parent.children.lock().remove(&name);
        }
        
        ::log::debug!("remove_inode_include_self: removed inode '{}'", self.name());
    }

    /// 设置动态查找提供者
    pub fn set_dynamic_lookup(&self, provider: Arc<dyn DynamicLookup>) {
        *self.dynamic_lookup.write() = Some(provider);
    }

    /// 获取动态查找提供者
    pub fn dynamic_lookup(&self) -> Option<Arc<dyn DynamicLookup>> {
        self.dynamic_lookup.read().clone()
    }

    /// 扩展的查找方法，支持动态查找
    pub fn find_extended(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 首先尝试静态查找
        match self.find(name) {
            Ok(inode) => return Ok(inode),
            Err(SystemError::ENOENT) => {
                // 如果静态查找失败且有动态查找提供者，尝试动态查找
                if let Some(provider) = self.dynamic_lookup.read().as_ref() {
                    match provider.dynamic_find(name)? {
                        Some(inode) => return Ok(inode),
                        None => {} // 继续返回 ENOENT
                    }
                }
                Err(SystemError::ENOENT)
            }
            Err(e) => Err(e),
        }
    }

    /// 扩展的列表方法，支持动态列表
    pub fn list_extended(&self) -> Result<Vec<String>, SystemError> {
        let mut entries = self.list()?;
        
        // 如果有动态查找提供者，添加动态条目
        if let Some(provider) = self.dynamic_lookup.read().as_ref() {
            let mut dynamic_entries = provider.dynamic_list()?;
            
            // 去重并合并
            for entry in dynamic_entries.drain(..) {
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            
            // 排序以获得一致的输出
            entries.sort();
        }
        
        Ok(entries)
    }

    /// 检查当前节点是否为临时节点
    pub fn is_temporary(&self) -> bool {
        self.is_temporary
    }

    /// 获取已知的挂载路径
    /// 
    /// 这个方法为特定的 KernFSInode 返回预定义的挂载路径。
    /// 主要用于处理特殊用途的 KernFS 节点，如 cgroup 目录。
    fn get_known_mount_path(&self) -> Option<String> {
        // 检查节点的私有数据，确定是否为已知的挂载点
        if let Some(private_data) = self.private_data.lock().as_ref() {
            match private_data {
                KernInodePrivateData::CgroupFS(_) => {
                    // 这是一个 cgroup 相关的节点，返回 cgroup 挂载点路径
                    log::debug!("KernFSInode::get_known_mount_path: Identified cgroup mount point for node '{}'", self.name);
                    return Some("/sys/fs/cgroup".to_string());
                }
                _ => {
                    log::debug!("KernFSInode::get_known_mount_path: Node '{}' has non-cgroup private data", self.name);
                }
            }
        } else {
            log::debug!("KernFSInode::get_known_mount_path: Node '{}' has no private data", self.name);
        }

        // 对于没有私有数据或非已知类型的节点，返回 None
        None
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernInodeType {
    Dir,
    File,
    SymLink,
}

impl From<KernInodeType> for FileType {
    fn from(val: KernInodeType) -> Self {
        match val {
            KernInodeType::Dir => FileType::Dir,
            KernInodeType::File => FileType::File,
            KernInodeType::SymLink => FileType::SymLink,
        }
    }
}
