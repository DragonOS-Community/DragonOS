use core::{cmp::min, fmt::Debug, intrinsics::unlikely};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    libs::{
        casting::DowncastArc,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    time::TimeSpec,
};

use self::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};

use super::vfs::{
    core::generate_inode_id, file::FileMode, syscall::ModeType, FilePrivateData, FileSystem,
    FileType, FsInfo, IndexNode, InodeId, Metadata,
};

pub mod callback;

#[derive(Debug)]
pub struct KernFS {
    root_inode: Arc<KernFSInode>,
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
}

impl KernFS {
    pub const MAX_NAMELEN: usize = 4096;

    #[allow(dead_code)]
    pub fn new() -> Arc<Self> {
        let root_inode = Self::create_root_inode();
        let fs = Arc::new(Self {
            root_inode: root_inode.clone(),
        });

        {
            let ptr = root_inode.as_ref() as *const KernFSInode as *mut KernFSInode;
            unsafe {
                (*ptr).self_ref = Arc::downgrade(&root_inode);
            }
        }
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
            atime: TimeSpec::new(0, 0),
            mtime: TimeSpec::new(0, 0),
            ctime: TimeSpec::new(0, 0),
            dev_id: 0,
            inode_id: generate_inode_id(),
            file_type: FileType::Dir,
            nlinks: 1,
            raw_dev: DeviceNumber::default(),
        };
        let root_inode = Arc::new(KernFSInode {
            name: String::from(""),
            inner: RwLock::new(InnerKernFSInode {
                parent: Weak::new(),
                metadata,
                symlink_target: None,
                symlink_target_absolute_path: None,
            }),
            self_ref: Weak::new(),
            fs: RwLock::new(Weak::new()),
            private_data: SpinLock::new(None),
            callback: None,
            children: SpinLock::new(HashMap::new()),
            inode_type: KernInodeType::Dir,
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

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        if let Some(callback) = self.callback {
            let callback_data =
                KernCallbackData::new(self.self_ref.upgrade().unwrap(), self.private_data.lock());
            return callback.open(callback_data);
        }

        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.inner.read().metadata.clone());
    }

    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn move_(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
                return Ok(self
                    .children
                    .lock()
                    .get(name)
                    .ok_or(SystemError::ENOENT)?
                    .clone());
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
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        // 应当通过kernfs的其它方法来操作文件，而不能从用户态直接调用此方法。
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
        self.children
            .lock()
            .keys()
            .into_iter()
            .for_each(|x| keys.push(x.clone()));

        return Ok(keys);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
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
            kwarn!("kernfs: callback is none");
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

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
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if self.inode_type != KernInodeType::File {
            return Err(SystemError::EISDIR);
        }

        if self.callback.is_none() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let callback_data =
            KernCallbackData::new(self.self_ref.upgrade().unwrap(), self.private_data.lock());
        return self
            .callback
            .as_ref()
            .unwrap()
            .write(callback_data, &buf[..len], offset);
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

    fn inner_create(
        &self,
        name: String,
        file_type: KernInodeType,
        mode: ModeType,
        mut size: usize,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
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
            atime: TimeSpec::new(0, 0),
            mtime: TimeSpec::new(0, 0),
            ctime: TimeSpec::new(0, 0),
            dev_id: 0,
            inode_id: generate_inode_id(),
            file_type: file_type.into(),
            nlinks: 1,
            raw_dev: DeviceNumber::default(),
        };

        let new_inode: Arc<KernFSInode> = Self::new(
            Some(self.self_ref.upgrade().unwrap()),
            name.clone(),
            metadata,
            file_type,
            private_data,
            callback,
        );

        self.children.lock().insert(name, new_inode.clone());

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
        // kdebug!("kernfs add link: name:{name}, target path={target_absolute_path}");
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

    pub fn private_data_mut(&self) -> SpinLockGuard<Option<KernInodePrivateData>> {
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
    #[allow(dead_code)]
    pub fn remove_inode_include_self(&self) {
        let parent = self.parent();
        if let Some(parent) = parent {
            parent.children.lock().remove(self.name());
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

impl Into<FileType> for KernInodeType {
    fn into(self) -> FileType {
        match self {
            KernInodeType::Dir => FileType::Dir,
            KernInodeType::File => FileType::File,
            KernInodeType::SymLink => FileType::SymLink,
        }
    }
}
