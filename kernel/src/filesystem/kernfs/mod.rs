use alloc::string::ToString;
use core::{
    cmp::min,
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use log::warn;
use system_error::SystemError;

use crate::libs::mutex::{Mutex, MutexGuard};
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::syscall::RenameFlags,
    libs::{casting::DowncastArc, rwsem::RwSem},
    time::PosixTimeSpec,
};

use self::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};

use super::vfs::{
    file::FileFlags, utils::DName, vcore::generate_inode_id, FilePrivateData, FileSystem, FileType,
    FsInfo, IndexNode, InodeFlags, InodeId, InodeMode, Magic, Metadata, OpenFileBehavior,
    PostWriteSyncPolicy, SuperBlock,
};

pub mod callback;
mod rename;

pub use rename::{KernFSRenameSpec, PreparedKernFSRename};

/// Stable identity used to distinguish same-named children in a
/// namespace-aware kernfs directory. It deliberately contains no namespace
/// object reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KernFSNamespaceTag(usize);

impl KernFSNamespaceTag {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KernFSChildKey {
    name: String,
    namespace: Option<KernFSNamespaceTag>,
}

impl KernFSChildKey {
    fn new(name: String, namespace: Option<KernFSNamespaceTag>) -> Self {
        Self { name, namespace }
    }
}

pub(crate) struct KernFSChildKeyRef<'a> {
    name: &'a str,
    namespace: Option<KernFSNamespaceTag>,
}

impl<'a> KernFSChildKeyRef<'a> {
    pub(crate) fn new(name: &'a str, namespace: Option<KernFSNamespaceTag>) -> Self {
        Self { name, namespace }
    }
}

#[derive(Debug, Default)]
pub(crate) struct KernFSChildren {
    buckets: HashMap<Option<KernFSNamespaceTag>, HashMap<String, Arc<KernFSInode>>>,
}

impl KernFSChildren {
    pub(crate) fn get(&self, key: &KernFSChildKeyRef<'_>) -> Option<&Arc<KernFSInode>> {
        self.buckets
            .get(&key.namespace)
            .and_then(|bucket| bucket.get(key.name))
    }

    pub(crate) fn contains_key(&self, key: &KernFSChildKeyRef<'_>) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn insert(
        &mut self,
        key: KernFSChildKey,
        inode: Arc<KernFSInode>,
    ) -> Option<Arc<KernFSInode>> {
        self.buckets
            .entry(key.namespace)
            .or_default()
            .insert(key.name, inode)
    }

    pub(crate) fn remove(&mut self, key: &KernFSChildKeyRef<'_>) -> Option<Arc<KernFSInode>> {
        let (removed, bucket_empty) = {
            let bucket = self.buckets.get_mut(&key.namespace)?;
            let removed = bucket.remove(key.name);
            (removed, bucket.is_empty())
        };
        if bucket_empty {
            self.buckets.remove(&key.namespace);
        }
        removed
    }

    /// Re-key an existing child without removing its namespace bucket. Rename
    /// commit relies on this operation being allocation-free after prepare.
    pub(crate) fn rekey(
        &mut self,
        old_key: &KernFSChildKeyRef<'_>,
        new_key: KernFSChildKey,
    ) -> Option<()> {
        if old_key.namespace != new_key.namespace {
            return None;
        }
        let bucket = self.buckets.get_mut(&old_key.namespace)?;
        let inode = bucket.remove(old_key.name)?;
        // Removing one entry before inserting its replacement guarantees that
        // the existing bucket has enough capacity; commit must not allocate.
        let replaced = bucket.insert(new_key.name, inode);
        debug_assert!(replaced.is_none());
        Some(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    fn find_name_by_inode(&self, ino: InodeId) -> Option<String> {
        self.buckets.values().find_map(|bucket| {
            bucket
                .iter()
                .find(|(_, inode)| inode.metadata().unwrap().inode_id == ino)
                .map(|(name, _)| name.clone())
        })
    }

    fn namespace_len(&self, namespace: Option<KernFSNamespaceTag>) -> usize {
        self.buckets.get(&namespace).map_or(0, HashMap::len)
    }

    fn extend_names(&self, namespace: Option<KernFSNamespaceTag>, names: &mut Vec<String>) {
        if let Some(bucket) = self.buckets.get(&namespace) {
            names.extend(bucket.keys().cloned());
        }
    }

    fn drain_values(&mut self) -> Vec<Arc<KernFSInode>> {
        self.buckets
            .drain()
            .flat_map(|(_, bucket)| bucket.into_values())
            .collect()
    }
}

#[derive(Debug)]
pub struct KernFS {
    root_inode: Arc<KernFSInode>,
    fsname: &'static str,
}

impl FileSystem for KernFS {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

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
            mode: InodeMode::from_bits_truncate(0o755),
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
            flags: InodeFlags::empty(),
        };
        let root_inode = Arc::new_cyclic(|self_ref| KernFSInode {
            inner: RwSem::new(InnerKernFSInode {
                name: String::from(""),
                parent: Weak::new(),
                metadata,
                symlink_target: None,
                symlink_target_absolute_path: None,
            }),
            self_ref: self_ref.clone(),
            fs: RwSem::new(Weak::new()),
            private_data: Mutex::new(None),
            callback: None,
            children: Mutex::new(KernFSChildren::default()),
            namespace: None,
            namespace_children: AtomicBool::new(false),
            child_mutation: Mutex::new(()),
            inode_type: KernInodeType::Dir,
            lazy_list: Mutex::new(HashMap::new()),
            lazy_build_lock: Mutex::new(()),
        });

        return root_inode;
    }
}

#[derive(Debug)]
pub struct KernFSInode {
    inner: RwSem<InnerKernFSInode>,
    /// 指向当前Inode所属的文件系统的弱引用
    fs: RwSem<Weak<KernFS>>,
    /// 指向自身的弱引用
    self_ref: Weak<KernFSInode>,
    /// 私有数据
    private_data: Mutex<Option<KernInodePrivateData>>,
    /// 回调函数
    callback: Option<&'static dyn KernFSCallback>,
    /// 子Inode
    children: Mutex<KernFSChildren>,
    /// Namespace identity of this inode in its parent's child map.
    namespace: Option<KernFSNamespaceTag>,
    /// Whether direct children must carry a namespace tag.
    namespace_children: AtomicBool,
    /// Serializes every mutation of `children` and `lazy_list` for this
    /// directory. Readers keep using the map locks directly.
    child_mutation: Mutex<()>,
    /// Inode类型
    inode_type: KernInodeType,
    /// lazy list
    lazy_list: Mutex<HashMap<String, fn() -> KernFSInodeArgs>>,
    /// Serializes lazy entry materialization without holding entry maps.
    lazy_build_lock: Mutex<()>,
}

pub struct KernFSInodeArgs {
    pub mode: InodeMode,
    pub inode_type: KernInodeType,
    pub size: Option<usize>,
    pub private_data: Option<KernInodePrivateData>,
    pub callback: Option<&'static dyn KernFSCallback>,
}

#[derive(Debug)]
pub struct InnerKernFSInode {
    /// The name is changed together with its parent's map key by a prepared
    /// kernfs rename transaction.
    name: String,
    parent: Weak<KernFSInode>,

    /// 当前inode的元数据
    metadata: Metadata,
    /// 符号链接指向的inode（仅当inode_type为SymLink时有效）
    symlink_target: Option<Weak<KernFSInode>>,
    symlink_target_absolute_path: Option<String>,
}

impl IndexNode for KernFSInode {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        if let Some(callback) = self.callback {
            let callback_data = KernCallbackData::new(
                self.self_ref.upgrade().unwrap(),
                self.private_data.lock(),
                data,
            );
            return callback.open(callback_data);
        }

        return Ok(());
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
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
        _mode: InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 应当通过kernfs的其它方法来创建文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EROFS);
    }

    fn unlink(
        &self,
        _name: &str,
    ) -> Result<crate::filesystem::vfs::LinkRemovalOutcome, SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::ENOSYS);
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        _flags: RenameFlags,
    ) -> Result<crate::filesystem::vfs::RenameOutcome, SystemError> {
        // 处理重命名到自身的特殊情况
        // 如果源目录和目标目录是同一个 inode，且文件名相同，则直接返回成功
        // 这符合 Linux 的 rename 语义：重命名到自身是一个空操作
        if let Some(target_kernfs) = target.clone().downcast_arc::<KernFSInode>() {
            // 使用 Arc::ptr_eq 比较两个 Arc 是否指向同一个对象
            // 通过 self_ref.upgrade() 获取 Arc<KernFSInode>
            let self_arc = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let target_arc = target_kernfs
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;

            if Arc::ptr_eq(&self_arc, &target_arc) && old_name == new_name {
                return Ok(crate::filesystem::vfs::RenameOutcome::NoOp);
            }
        }

        // 其他情况返回 ENOSYS（sysfs/kernfs 不支持真正的重命名操作）
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
                if self.namespace_children.load(Ordering::Acquire) {
                    return Err(SystemError::ENOENT);
                }
                let key = KernFSChildKeyRef::new(name, None);
                if let Some(child) = self.children.lock().get(&key).cloned() {
                    return Ok(child);
                }

                return self
                    .materialize_lazy_child(name)
                    .map(|child| child as Arc<dyn IndexNode>);
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        if self.inode_type != KernInodeType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let children = self.children.lock();
        let r = children.find_name_by_inode(ino);

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
        _private_data: MutexGuard<FilePrivateData>,
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

        let children = self.children.lock();
        let mut keys = Vec::with_capacity(children.namespace_len(None) + 2);
        keys.push(String::from("."));
        keys.push(String::from(".."));
        if self.namespace_children.load(Ordering::Acquire) {
            return Err(SystemError::ENOENT);
        }
        children.extend_names(None, &mut keys);

        return Ok(keys);
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.name().into())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
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
        let callback_data = KernCallbackData::new(
            self.self_ref.upgrade().unwrap(),
            self.private_data.lock(),
            data,
        );
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
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if self.inode_type != KernInodeType::File {
            return Err(SystemError::EISDIR);
        }

        if self.callback.is_none() {
            return Err(SystemError::ENOSYS);
        }

        let callback_data = KernCallbackData::new(
            self.self_ref.upgrade().unwrap(),
            self.private_data.lock(),
            data,
        );
        return self
            .callback
            .as_ref()
            .unwrap()
            .write(callback_data, &buf[..len], offset);
    }
}

impl KernFSInode {
    pub fn namespace(&self) -> Option<KernFSNamespaceTag> {
        self.namespace
    }

    pub fn namespace_children_enabled(&self) -> bool {
        self.namespace_children.load(Ordering::Acquire)
    }

    /// Make direct children namespace-aware. Like Linux kernfs, this is an
    /// irreversible directory property and is only valid before children or
    /// lazy entries are published.
    pub fn enable_namespace_children(&self) -> Result<(), SystemError> {
        if self.inode_type != KernInodeType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let _mutation = self.child_mutation.lock();
        if !self.children.lock().is_empty() || !self.lazy_list.lock().is_empty() {
            return Err(SystemError::ENOTEMPTY);
        }
        self.namespace_children.store(true, Ordering::Release);
        Ok(())
    }

    pub fn find_ns(
        &self,
        name: &str,
        namespace: KernFSNamespaceTag,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name.len() > KernFS::MAX_NAMELEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        if self.inode_type != KernInodeType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        match name {
            "" | "." => self
                .self_ref
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            ".." => self
                .inner
                .read()
                .parent
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            name if self.namespace_children.load(Ordering::Acquire) => self
                .children
                .lock()
                .get(&KernFSChildKeyRef::new(name, Some(namespace)))
                .cloned()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            name => self.find(name),
        }
    }

    pub fn list_ns(&self, namespace: KernFSNamespaceTag) -> Result<Vec<String>, SystemError> {
        if self.inode_type != KernInodeType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if !self.namespace_children.load(Ordering::Acquire) {
            return self.list();
        }
        let children = self.children.lock();
        let mut names = Vec::with_capacity(children.namespace_len(Some(namespace)) + 2);
        names.push(".".to_string());
        names.push("..".to_string());
        children.extend_names(Some(namespace), &mut names);
        Ok(names)
    }

    /// Create a new KernFSInode with a parent.
    /// Uses Arc::new_cyclic to safely initialize self_ref without unsafe code.
    /// After construction, sets the fs reference from parent if available.
    pub fn new_with_parent(
        parent: Option<Arc<KernFSInode>>,
        name: String,
        metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Arc<KernFSInode> {
        Self::new_with_parent_ns(
            parent,
            name,
            metadata,
            inode_type,
            private_data,
            callback,
            None,
        )
    }

    fn new_with_parent_ns(
        parent: Option<Arc<KernFSInode>>,
        name: String,
        mut metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
        namespace: Option<KernFSNamespaceTag>,
    ) -> Arc<KernFSInode> {
        metadata.file_type = inode_type.into();

        let inode = Arc::new_cyclic(|self_ref| KernFSInode {
            inner: RwSem::new(InnerKernFSInode {
                name,
                parent: parent.as_ref().map_or(Weak::new(), Arc::downgrade),
                metadata,
                symlink_target: None,
                symlink_target_absolute_path: None,
            }),
            self_ref: self_ref.clone(),
            fs: RwSem::new(Weak::new()),
            private_data: Mutex::new(private_data),
            callback,
            children: Mutex::new(KernFSChildren::default()),
            namespace,
            namespace_children: AtomicBool::new(false),
            child_mutation: Mutex::new(()),
            inode_type,
            lazy_list: Mutex::new(HashMap::new()),
            lazy_build_lock: Mutex::new(()),
        });

        // Set fs reference from parent if available
        // This is done after construction to work within Arc::new_cyclic constraints
        if let Some(ref parent) = parent {
            if let Some(kernfs) = parent.fs().downcast_arc::<KernFS>() {
                *inode.fs.write() = Arc::downgrade(&kernfs);
            }
        }

        inode
    }

    /// Create a new KernFSInode (legacy interface).
    /// This is an alias for new_with_parent.
    #[deprecated(note = "Use new_with_parent instead")]
    pub fn new(
        parent: Option<Arc<KernFSInode>>,
        name: String,
        metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Arc<KernFSInode> {
        Self::new_with_parent(parent, name, metadata, inode_type, private_data, callback)
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
        mode: InodeMode,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        return self.inner_create(name, KernInodeType::Dir, mode, 0, private_data, callback);
    }

    pub fn add_dir_ns(
        &self,
        name: String,
        mode: InodeMode,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
        namespace: KernFSNamespaceTag,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        self.inner_create_ns(
            name,
            KernFSInodeArgs {
                mode,
                inode_type: KernInodeType::Dir,
                size: Some(0),
                private_data,
                callback,
            },
            Some(namespace),
        )
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
        mode: InodeMode,
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
        let _mutation = self.child_mutation.lock();
        if self.namespace_children.load(Ordering::Acquire) {
            return Err(SystemError::EINVAL);
        }
        let children = self.children.lock();
        let mut lazy_list = self.lazy_list.lock();
        if children.contains_key(&KernFSChildKeyRef::new(&name, None))
            || lazy_list.contains_key(&name)
        {
            return Err(SystemError::EEXIST);
        }

        lazy_list.insert(name, provider);
        Ok(())
    }

    fn materialize_lazy_child(&self, name: &str) -> Result<Arc<KernFSInode>, SystemError> {
        let _build_guard = self.lazy_build_lock.lock();

        if let Some(child) = self
            .children
            .lock()
            .get(&KernFSChildKeyRef::new(name, None))
            .cloned()
        {
            return Ok(child);
        }

        let provider = self
            .lazy_list
            .lock()
            .get(name)
            .copied()
            .ok_or(SystemError::ENOENT)?;

        let args = provider();
        let inode = self.new_child_inode(name.to_string(), args, None);

        let _mutation = self.child_mutation.lock();
        let mut children = self.children.lock();
        if let Some(child) = children.get(&KernFSChildKeyRef::new(name, None)).cloned() {
            return Ok(child);
        }

        let mut lazy_list = self.lazy_list.lock();
        if lazy_list.remove(name).is_none() {
            return children
                .get(&KernFSChildKeyRef::new(name, None))
                .cloned()
                .ok_or(SystemError::ENOENT);
        }

        children.insert(KernFSChildKey::new(name.to_string(), None), inode.clone());
        Ok(inode)
    }

    fn inner_create(
        &self,
        name: String,
        file_type: KernInodeType,
        mode: InodeMode,
        size: usize,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        self.inner_create_ns(
            name,
            KernFSInodeArgs {
                mode,
                inode_type: file_type,
                size: Some(size),
                private_data,
                callback,
            },
            None,
        )
    }

    fn inner_create_ns(
        &self,
        name: String,
        args: KernFSInodeArgs,
        namespace: Option<KernFSNamespaceTag>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let _mutation = self.child_mutation.lock();
        if self.namespace_children.load(Ordering::Acquire) != namespace.is_some() {
            return Err(SystemError::EINVAL);
        }
        let mut children = self.children.lock();
        let lazy_list = self.lazy_list.lock();
        let borrowed_key = KernFSChildKeyRef::new(&name, namespace);
        if children.contains_key(&borrowed_key) || lazy_list.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        let new_inode = self.new_child_inode(name.clone(), args, namespace);

        children.insert(KernFSChildKey::new(name, namespace), new_inode.clone());

        return Ok(new_inode);
    }

    fn new_child_inode(
        &self,
        name: String,
        args: KernFSInodeArgs,
        namespace: Option<KernFSNamespaceTag>,
    ) -> Arc<KernFSInode> {
        let size = match args.inode_type {
            KernInodeType::Dir | KernInodeType::SymLink => 0,
            _ => args.size.unwrap_or(4096),
        };

        let metadata = Metadata {
            size: size as i64,
            mode: args.mode,
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
            file_type: args.inode_type.into(),
            nlinks: 1,
            raw_dev: DeviceNumber::default(),
            flags: InodeFlags::empty(),
        };

        Self::new_with_parent_ns(
            Some(self.self_ref.upgrade().unwrap()),
            name,
            metadata,
            args.inode_type,
            args.private_data,
            args.callback,
            namespace,
        )
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
        self.remove_ns(name, None)
    }

    pub fn remove_in_namespace(
        &self,
        name: &str,
        namespace: KernFSNamespaceTag,
    ) -> Result<(), SystemError> {
        self.remove_ns(name, Some(namespace))
    }

    fn remove_ns(
        &self,
        name: &str,
        namespace: Option<KernFSNamespaceTag>,
    ) -> Result<(), SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }
        if self.namespace_children.load(Ordering::Acquire) != namespace.is_some() {
            return Err(SystemError::EINVAL);
        }

        let _mutation = self.child_mutation.lock();
        let mut children = self.children.lock();
        let key = KernFSChildKeyRef::new(name, namespace);
        let inode = children.get(&key).ok_or(SystemError::ENOENT)?;
        if inode.children.lock().is_empty() {
            children.remove(&key);
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
        let namespace = if self.namespace_children.load(Ordering::Acquire) {
            Some(target.namespace().ok_or(SystemError::EINVAL)?)
        } else {
            None
        };
        self.add_link_ns(name, target, target_absolute_path, namespace)
    }

    fn add_link_ns(
        &self,
        name: String,
        target: &Arc<KernFSInode>,
        target_absolute_path: String,
        namespace: Option<KernFSNamespaceTag>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let inode = self.inner_create_ns(
            name,
            KernFSInodeArgs {
                mode: InodeMode::S_IFLNK | InodeMode::S_IRWXUGO,
                inode_type: KernInodeType::SymLink,
                size: Some(0),
                private_data: None,
                callback: None,
            },
            namespace,
        )?;

        inode.inner.write().symlink_target = Some(Arc::downgrade(target));
        inode.inner.write().symlink_target_absolute_path = Some(target_absolute_path);
        return Ok(inode);
    }

    pub fn name(&self) -> String {
        self.inner.read().name.clone()
    }

    /// Borrows the inode name while holding its read guard. This keeps
    /// comparisons and fallible path construction allocation-free at the
    /// call site without exposing the mutable inode internals.
    pub(crate) fn with_name<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        let inner = self.inner.read();
        f(&inner.name)
    }

    pub(crate) fn try_name(&self) -> Result<String, SystemError> {
        self.with_name(|name| {
            let mut result = String::new();
            result
                .try_reserve_exact(name.len())
                .map_err(|_| SystemError::ENOMEM)?;
            result.push_str(name);
            Ok(result)
        })
    }

    pub fn parent(&self) -> Option<Arc<KernFSInode>> {
        return self.inner.read().parent.upgrade();
    }

    pub fn private_data_mut(&self) -> MutexGuard<'_, Option<KernInodePrivateData>> {
        return self.private_data.lock();
    }

    #[allow(dead_code)]
    pub fn symlink_target(&self) -> Option<Arc<KernFSInode>> {
        return self.inner.read().symlink_target.as_ref()?.upgrade();
    }

    /// remove a kernfs_node recursively
    pub fn remove_recursive(&self) {
        let mut children = {
            let _mutation = self.child_mutation.lock();
            self.children.lock().drain_values()
        };
        while let Some(child) = children.pop() {
            let mut descendants = {
                let _mutation = child.child_mutation.lock();
                child.children.lock().drain_values()
            };
            children.append(&mut descendants);
        }
    }

    /// 删除当前的inode（包括其自身、子目录和子文件）
    #[allow(dead_code)]
    pub fn remove_inode_include_self(&self) {
        let parent = self.parent();
        if let Some(parent) = parent {
            let name = self.name();
            let _mutation = parent.child_mutation.lock();
            parent
                .children
                .lock()
                .remove(&KernFSChildKeyRef::new(&name, self.namespace));
        }
        self.remove_recursive();
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
