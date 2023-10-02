use core::{fmt::Debug, intrinsics::unlikely};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;

use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    syscall::SystemError,
    time::TimeSpec,
};

use self::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};

use super::vfs::{
    core::generate_inode_id, file::FileMode, syscall::ModeType, FilePrivateData, FileSystem,
    FileType, FsInfo, IndexNode, InodeId, Metadata, PollStatus,
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
        root_inode.inner.lock().parent = Arc::downgrade(&root_inode);
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
            raw_dev: 0,
        };
        let root_inode = Arc::new(KernFSInode {
            inner: SpinLock::new(InnerKernFSInode {
                parent: Weak::new(),
                metadata,
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
    inner: SpinLock<InnerKernFSInode>,
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
}

#[derive(Debug)]
pub struct InnerKernFSInode {
    parent: Weak<KernFSInode>,

    /// 当前inode的元数据
    metadata: Metadata,
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
        return Ok(self.inner.lock().metadata.clone());
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
        let x: Arc<KernFSInode> = self
            .children
            .lock()
            .get(name)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        return Ok(x);
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

    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, SystemError> {
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
        let mut list = Vec::new();
        for (name, _) in self.children.lock().iter() {
            list.push(name.clone());
        }
        return Ok(list);
    }

    fn poll(&self) -> Result<PollStatus, SystemError> {
        // todo: 根据inode的具体attribute，返回PollStatus
        return Ok(PollStatus::READ | PollStatus::WRITE);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
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

    fn special_nod(&self) -> Option<Arc<dyn IndexNode>> {
        return None;
    }

    fn set_special_nod(&self, _nod: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

impl KernFSInode {
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

        return self.inner_create(name, KernInodeType::Dir, mode, private_data, callback);
    }

    /// 在当前inode下增加文件
    ///
    /// ## 参数
    ///
    /// - `name`：文件名称
    /// - `mode`：文件权限
    /// - `private_data`：文件私有数据
    /// - `callback`：文件回调函数
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
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        if unlikely(self.inode_type != KernInodeType::Dir) {
            return Err(SystemError::ENOTDIR);
        }

        return self.inner_create(name, KernInodeType::File, mode, private_data, callback);
    }

    fn inner_create(
        &self,
        name: String,
        file_type: KernInodeType,
        mode: ModeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let metadata = Metadata {
            size: 0,
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
            raw_dev: 0,
        };

        let new_inode: Arc<KernFSInode> = Self::new(
            self.self_ref.upgrade().unwrap(),
            metadata,
            KernInodeType::Dir,
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

    pub(self) fn new(
        parent: Arc<KernFSInode>,
        metadata: Metadata,
        inode_type: KernInodeType,
        private_data: Option<KernInodePrivateData>,
        callback: Option<&'static dyn KernFSCallback>,
    ) -> Arc<KernFSInode> {
        let inode = Arc::new(KernFSInode {
            inner: SpinLock::new(InnerKernFSInode {
                parent: Arc::downgrade(&parent),
                metadata,
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
        *inode.fs.write() = Arc::downgrade(
            parent
                .fs()
                .as_any_ref()
                .downcast_ref()
                .expect("KernFSInode::new: parent is not a KernFS instance"),
        );
        return inode;
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(self) enum KernInodeType {
    Dir,
    File,
}

impl Into<FileType> for KernInodeType {
    fn into(self) -> FileType {
        match self {
            KernInodeType::Dir => FileType::Dir,
            KernInodeType::File => FileType::File,
        }
    }
}
