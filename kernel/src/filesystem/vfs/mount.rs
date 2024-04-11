use core::{
    any::Any,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{
    collections::BTreeMap, string::{String, ToString}, sync::{Arc, Weak}
};
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    libs::{casting::DowncastArc, rwlock::RwLock, spinlock::{SpinLock, SpinLockGuard}},
};

use super::{
    file::FileMode, syscall::ModeType, utils::DName, FilePrivateData, FileSystem, FileType, IndexNode, InodeId, Magic, SuperBlock
};

const MOUNTFS_BLOCK_SIZE: u64 = 512;
const MOUNTFS_MAX_NAMELEN: u64 = 64;
/// @brief 挂载文件系统
/// 挂载文件系统的时候，套了MountFS这一层，以实现文件系统的递归挂载
#[derive(Debug)]
pub struct MountFS {
    // MountFS内部的文件系统
    inner_filesystem: Arc<dyn FileSystem>,
    /// 用来存储InodeID->挂载点的MountFS的B树
    mountpoints: SpinLock<BTreeMap<InodeId, Arc<MountFS>>>,
    /// 当前文件系统挂载到的那个挂载点的Inode
    self_mountpoint: Option<Arc<MountFSInode>>,
    /// 指向当前MountFS的弱引用
    self_ref: Weak<MountFS>,
}

/// @brief MountFS的Index Node 注意，这个IndexNode只是一个中间层。它的目的是将具体文件系统的Inode与挂载机制连接在一起。
#[derive(Debug)]
#[cast_to([sync] IndexNode)]
pub struct MountFSInode {
    /// 当前挂载点对应到具体的文件系统的Inode
    inner_inode: Arc<dyn IndexNode>,
    /// 当前Inode对应的MountFS
    mount_fs: Arc<MountFS>,
    /// 指向自身的弱引用
    self_ref: Weak<MountFSInode>,
}

impl MountFS {
    pub fn new(
        inner_filesystem: Arc<dyn FileSystem>,
        self_mountpoint: Option<Arc<MountFSInode>>,
    ) -> Arc<Self> {
        return Arc::new_cyclic(|self_ref|
            MountFS {
                inner_filesystem,
                mountpoints: SpinLock::new(BTreeMap::new()),
                self_mountpoint,
                self_ref: self_ref.clone(),
            }
        );
    }

    /// @brief 用Arc指针包裹MountFS对象。
    /// 本函数的主要功能为，初始化MountFS对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let mount_fs: Arc<MountFS> = Arc::new(self);
        // 创建weak指针
        let weak: Weak<MountFS> = Arc::downgrade(&mount_fs);

        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut MountFS = mount_fs.as_ref() as *const Self as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            // 返回初始化好的MountFS对象
            return mount_fs;
        }
    }

    /// @brief 获取挂载点的文件系统的root inode
    pub fn mountpoint_root_inode(&self) -> Arc<MountFSInode> {
        return Arc::new_cyclic(|self_ref|
            MountFSInode {
                inner_inode: self.inner_filesystem.root_inode(),
                mount_fs: self.self_ref.upgrade().unwrap(),
                self_ref: self_ref.clone(),
            }
        );
    }

    pub fn inner_filesystem(&self) -> Arc<dyn FileSystem> {
        return self.inner_filesystem.clone();
    }

    pub fn self_ref(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    /// 卸载文件系统
    /// # Errors
    /// 如果当前文件系统是根文件系统，那么将会返回`EINVAL`
    pub fn umount(&self) -> Result<(), SystemError> {
        if let Some(parent) = &self.self_mountpoint {
            return parent.umount();
        } else {
            return Err(SystemError::EINVAL);
        }
    }
}

impl MountFSInode {
    /// @brief 用Arc指针包裹MountFSInode对象。
    /// 本函数的主要功能为，初始化MountFSInode对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    #[allow(dead_code)]
    #[deprecated]
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let inode: Arc<MountFSInode> = Arc::new(self);
        // 创建Weak指针
        let weak: Weak<MountFSInode> = Arc::downgrade(&inode);
        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        compiler_fence(Ordering::SeqCst);
        let ptr: *mut MountFSInode = inode.as_ref() as *const Self as *mut Self;
        compiler_fence(Ordering::SeqCst);
        unsafe {
            (*ptr).self_ref = weak;
            compiler_fence(Ordering::SeqCst);

            // 返回初始化好的MountFSInode对象
            return inode;
        }
    }

    /// @brief 判断当前inode是否为它所在的文件系统的root inode
    fn is_mountpoint_root(&self) -> Result<bool, SystemError> {
        return Ok(self.inner_inode.fs().root_inode().metadata()?.inode_id
            == self.inner_inode.metadata()?.inode_id);
    }

    /// @brief 在挂载树上进行inode替换。
    /// 如果当前inode是父MountFS内的一个挂载点，那么，本函数将会返回挂载到这个挂载点下的文件系统的root inode.
    /// 如果当前inode在父MountFS内，但不是挂载点，那么说明在这里不需要进行inode替换，因此直接返回当前inode。
    ///
    /// @return Arc<MountFSInode>
    fn overlaid_inode(&self) -> Arc<MountFSInode> {
        let inode_id = self.metadata().unwrap().inode_id;

        if let Some(sub_mountfs) = self.mount_fs.mountpoints.lock().get(&inode_id) {
            return sub_mountfs.mountpoint_root_inode();
        } else {
            return self.self_ref.upgrade().unwrap();
        }
    }

    /// 将新的挂载点-挂载文件系统添加到父级的挂载树
    pub(super) fn do_mount(
        &self,
        inode_id: InodeId,
        new_mount_fs: Arc<MountFS>,
    ) -> Result<(), SystemError> {
        let mut guard = self.mount_fs.mountpoints.lock();
        if guard.contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        guard.insert(inode_id, new_mount_fs);

        return Ok(());
    }

    pub(super) fn inode_id(&self) -> InodeId {
        self.metadata().map(|x| x.inode_id).unwrap()
    }

    fn umount(&self) -> Result<(), SystemError> {
        // 移除挂载点
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        self.mount_fs
            .mountpoints
            .lock()
            .remove(&self.inner_inode.metadata()?.inode_id);
        return Ok(());
    }

    pub(super) fn _find(&self, name: &str) -> Result<Arc<MountFSInode>, SystemError> {
        // 直接调用当前inode所在的文件系统的find方法进行查找
        // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
        let inner_inode = self.inner_inode.find(name)?;
        return Ok(Arc::new_cyclic(|self_ref|
            MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }
        ).overlaid_inode());
    }

    pub(super) fn _parent(&self) -> Result<Arc<MountFSInode>, SystemError> {
        if self.is_mountpoint_root()? {
            // 当前inode是它所在的文件系统的root inode
            match &self.mount_fs.self_mountpoint {
                Some(inode) => {
                    return inode._find("..");
                }
                None => {
                    return Ok(self.self_ref.upgrade().unwrap());
                }
            }
        } else {
            let inner_inode = self.inner_inode.find("..")?;
            // 向上查找时，不会跨过文件系统的边界，因此直接调用当前inode所在的文件系统的find方法进行查找
            return Ok(Arc::new_cyclic(|self_ref| 
                MountFSInode {
                    inner_inode,
                    mount_fs: self.mount_fs.clone(),
                    self_ref: self_ref.clone(),
                }
            ));
        }
    }
}

impl IndexNode for MountFSInode {
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        mode: &FileMode,
    ) -> Result<(), SystemError> {
        return self.inner_inode.open(data, mode);
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return self.inner_inode.close(data);
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self
            .inner_inode
            .create_with_data(name, file_type, mode,data)?;
        return Ok(Arc::new_cyclic(|self_ref|
            MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }
        ));
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        return self.inner_inode.truncate(len);
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.read_at(offset, len, buf, data);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.write_at(offset, len, buf, data);
    }

    #[inline]
    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.mount_fs.clone();
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self.inner_inode.as_any_ref();
    }

    #[inline]
    fn metadata(&self) -> Result<super::Metadata, SystemError> {
        return self.inner_inode.metadata();
    }

    #[inline]
    fn set_metadata(&self, metadata: &super::Metadata) -> Result<(), SystemError> {
        return self.inner_inode.set_metadata(metadata);
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), SystemError> {
        return self.inner_inode.resize(len);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self
            .inner_inode
            .create(name, file_type, mode)?;
        return Ok(Arc::new_cyclic(|self_ref|
            MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }
        ));
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        return self.inner_inode.link(name, other);
    }

    /// @brief 在挂载文件系统中删除文件/文件夹
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // 调用内层的inode的方法来删除这个inode
        return self.inner_inode.unlink(name);
    }

    #[inline]
    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // 调用内层的rmdir的方法来删除这个inode
        let r = self.inner_inode.rmdir(name);

        return r;
    }

    #[inline]
    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        return self.inner_inode.move_to(old_name, target, new_name);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            // 查找的是当前目录
            "" | "." => self
                .self_ref
                .upgrade()
                .map(|inode| 
                    inode as Arc<dyn IndexNode>
                )
                .ok_or(SystemError::ENOENT),
            // 往父级查找
            ".." => self.dparent(),
            // 在当前目录下查找
            // 直接调用当前inode所在的文件系统的find方法进行查找
            // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
            _ => self
                ._find(name)
                .map(|inode|
                    inode as Arc<dyn IndexNode>
                ),
        }
    }

    #[inline]
    fn get_entry_name(&self, ino: InodeId) -> Result<alloc::string::String, SystemError> {
        return self.inner_inode.get_entry_name(ino);
    }

    #[inline]
    fn get_entry_name_and_metadata(
        &self,
        ino: InodeId,
    ) -> Result<(alloc::string::String, super::Metadata), SystemError> {
        return self.inner_inode.get_entry_name_and_metadata(ino);
    }

    #[inline]
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        return self.inner_inode.ioctl(cmd, data, private_data);
    }

    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return self.inner_inode.list();
    }

    /// @brief 在当前inode下，挂载一个文件系统
    /// 
    /// 传入一个裸的具体文件系统，包装Mount信息并挂载
    ///
    /// @return Ok(Arc<MountFS>) 挂载成功，返回指向MountFS的指针
    fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 为新的挂载点创建挂载文件系统
        // let new_mount_fs: Arc<MountFS> = MountFS::new(fs, Some(self.self_ref.upgrade().unwrap()));
        let new_mount_fs = fs.clone()
            .downcast_arc::<MountFS>()
            .unwrap_or(MountFS::new(fs, Some(self.self_ref.upgrade().unwrap())));
        self.do_mount(metadata.inode_id, new_mount_fs.clone())?;
        return Ok(new_mount_fs);
    }

    fn absolute_path(&self, len: usize) -> Result<String, SystemError> {
        let parent = self.dparent()?;
        if self.metadata()?.inode_id == parent.metadata()?.inode_id {
            return Ok(String::with_capacity(len));
        }
        let name = self.dname()?;
        return Ok(parent.absolute_path(len + name.0.lock().len())? + &name.0.lock());
    }

    #[inline]
    fn mknod(
        &self,
        filename: &str,
        mode: ModeType,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.mknod(filename, mode, dev_t)?;
        return Ok(Arc::new_cyclic(|self_ref| 
            MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }
        ));
    }

    #[inline]
    fn special_node(&self) -> Option<super::SpecialNodeData> {
        self.inner_inode.special_node()
    }

    #[inline]
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        self.inner_inode.poll(private_data)
    }

    fn dname(&self) -> Result<DName, SystemError> {
        return self
            .inner_inode
            .dname()
            .or(
                self
                    .dparent()?
                    .get_entry_name(
                        self.metadata()?.inode_id
                    ).map(|name| DName::from(name))
            );
    }

    fn dparent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self._parent().map(|inode| inode as Arc<dyn IndexNode>);
    }
}

impl FileSystem for MountFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        match &self.self_mountpoint {
            Some(inode) => return inode.mount_fs.root_inode(),
            // 当前文件系统是rootfs
            None => self.mountpoint_root_inode(),
        }
    }

    fn info(&self) -> super::FsInfo {
        return self.inner_filesystem.info();
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "mountfs"
    }
    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::MOUNT_MAGIC, MOUNTFS_BLOCK_SIZE, MOUNTFS_MAX_NAMELEN)
    }
}

/// MountList
/// 

#[derive(PartialEq, Eq, Debug)]
pub struct MountPath(String);

impl From<&str> for MountPath {
    fn from(value: &str) -> Self {
        Self(String::from(value))
    }
}

impl From<String> for MountPath {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for MountPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialOrd for MountPath {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MountPath {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let self_dep = self.0.chars().filter(|c| *c == '/').count();
        let othe_dep = other.0.chars().filter(|c| *c == '/').count();
        if self_dep == othe_dep {
            self.0.cmp(&other.0)
        } else {
            othe_dep.cmp(&self_dep)
        }
    }
}

// 维护一个挂载点的记录，以支持特定于文件系统的索引
type MountListType = Option<Arc<RwLock<BTreeMap<MountPath, Arc<MountFS>>>>>;
pub struct MountList(MountListType);
static mut __MOUNTS_LIST: MountList = MountList(None);

impl MountList {
    /// 初始化挂载点
    pub fn init() {
        unsafe {
            __MOUNTS_LIST = MountList(Some(Arc::new(RwLock::new(BTreeMap::new()))));
        }
    }

    fn instance() -> &'static Arc<RwLock<BTreeMap<MountPath, Arc<MountFS>>>> {
        unsafe {
            if __MOUNTS_LIST.0.is_none() {
                MountList::init();
            }
            return __MOUNTS_LIST.0.as_ref().unwrap();
        }
    }

    /// 在 **路径`path`** 下挂载 **文件系统`fs`**
    pub fn insert<T: AsRef<str>>(path: T, fs: Arc<MountFS>) {
        MountList::instance()
            .write()
            .insert(MountPath::from(path.as_ref()), fs);
    }

    /// 获取挂载点信息，返回
    ///
    /// - `最近的挂载点`
    /// - `挂载点下的路径`
    /// - `文件系统`
    /// # None
    /// 未找到挂载点
    pub fn get<T: AsRef<str>>(path: T) -> Option<(String, String, Arc<MountFS>)> {
        MountList::instance()
            .upgradeable_read()
            .iter()
            .filter_map(|(key, fs)| {
                let strkey = key.as_ref();
                if let Some(rest) = path.as_ref().strip_prefix(strkey) {
                    return Some((strkey.to_string(), rest.to_string(), fs.clone()));
                }
                None
            })
            .next()
    }

    pub fn remove<T: Into<MountPath>>(path: T) -> Option<Arc<MountFS>> {
        MountList::instance()
            .write()
            .remove(&path.into())
    }
}
