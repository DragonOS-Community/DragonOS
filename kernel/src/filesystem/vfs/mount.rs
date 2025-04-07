use core::{
    any::Any,
    fmt::Debug,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{page_cache::PageCache, vfs::ROOT_INODE},
    libs::{
        casting::DowncastArc,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{fault::PageFaultMessage, VmFaultReason},
};

use super::{
    file::FileMode, syscall::ModeType, utils::DName, FilePrivateData, FileSystem, FileType,
    IndexNode, InodeId, Magic, PollableInode, SuperBlock,
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
        return Arc::new_cyclic(|self_ref| MountFS {
            inner_filesystem,
            mountpoints: SpinLock::new(BTreeMap::new()),
            self_mountpoint,
            self_ref: self_ref.clone(),
        });
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
        return Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: self.inner_filesystem.root_inode(),
            mount_fs: self.self_ref.upgrade().unwrap(),
            self_ref: self_ref.clone(),
        });
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
    pub fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        self.self_mountpoint
            .as_ref()
            .ok_or(SystemError::EINVAL)?
            .do_umount()
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

    fn do_find(&self, name: &str) -> Result<Arc<MountFSInode>, SystemError> {
        // 直接调用当前inode所在的文件系统的find方法进行查找
        // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
        let inner_inode = self.inner_inode.find(name)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        })
        .overlaid_inode());
    }

    pub(super) fn do_parent(&self) -> Result<Arc<MountFSInode>, SystemError> {
        if self.is_mountpoint_root()? {
            // 当前inode是它所在的文件系统的root inode
            match &self.mount_fs.self_mountpoint {
                Some(inode) => {
                    let inner_inode = inode.parent()?;
                    return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
                        inner_inode,
                        mount_fs: self.mount_fs.clone(),
                        self_ref: self_ref.clone(),
                    }));
                }
                None => {
                    return Ok(self.self_ref.upgrade().unwrap());
                }
            }
        } else {
            let inner_inode = self.inner_inode.parent()?;
            // 向上查找时，不会跨过文件系统的边界，因此直接调用当前inode所在的文件系统的find方法进行查找
            return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
                inner_inode,
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
            }));
        }
    }

    /// 移除挂载点下的文件系统
    fn do_umount(&self) -> Result<Arc<MountFS>, SystemError> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        return self
            .mount_fs
            .mountpoints
            .lock()
            .remove(&self.inner_inode.metadata()?.inode_id)
            .ok_or(SystemError::ENOENT);
    }

    fn do_absolute_path(&self) -> Result<String, SystemError> {
        let mut path_parts = Vec::new();
        let mut current = self.self_ref.upgrade().unwrap();

        while current.metadata()?.inode_id != ROOT_INODE().metadata()?.inode_id {
            let name = current.dname()?;
            path_parts.push(name.0);
            current = current.do_parent()?;
        }

        // 由于我们从叶子节点向上遍历到根节点，所以需要反转路径部分
        path_parts.reverse();

        // 构建最终的绝对路径字符串
        let mut absolute_path = String::with_capacity(
            path_parts.iter().map(|s| s.len()).sum::<usize>() + path_parts.len(),
        );
        for part in path_parts {
            absolute_path.push('/');
            absolute_path.push_str(&part);
        }

        Ok(absolute_path)
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
            .create_with_data(name, file_type, mode, data)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
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

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner_inode.read_direct(offset, len, buf, data)
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner_inode.write_direct(offset, len, buf, data)
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
        let inner_inode = self.inner_inode.create(name, file_type, mode)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
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
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            // 往父级查找
            ".." => self.parent(),
            // 在当前目录下查找
            // 直接调用当前inode所在的文件系统的find方法进行查找
            // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
            _ => self.do_find(name).map(|inode| inode as Arc<dyn IndexNode>),
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

    fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        if self.is_mountpoint_root()? {
            return Err(SystemError::EBUSY);
        }

        // 若已有挂载系统，保证MountFS只包一层
        let to_mount_fs = fs
            .clone()
            .downcast_arc::<MountFS>()
            .map(|it| it.inner_filesystem())
            .unwrap_or(fs);
        let new_mount_fs = MountFS::new(to_mount_fs, Some(self.self_ref.upgrade().unwrap()));
        self.mount_fs
            .mountpoints
            .lock()
            .insert(metadata.inode_id, new_mount_fs.clone());

        let mount_path = self.absolute_path();

        MOUNT_LIST().insert(mount_path?, new_mount_fs.clone());
        return Ok(new_mount_fs);
    }

    fn mount_from(&self, from: Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.metadata()?;
        if from.metadata()?.file_type != FileType::Dir || metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if self.is_mountpoint_root()? {
            return Err(SystemError::EBUSY);
        }
        // debug!("from {:?}, to {:?}", from, self);
        let new_mount_fs = from.umount()?;
        self.mount_fs
            .mountpoints
            .lock()
            .insert(metadata.inode_id, new_mount_fs.clone());

        // MOUNT_LIST().remove(from.absolute_path()?);
        // MOUNT_LIST().insert(self.absolute_path()?, new_mount_fs.clone());
        return Ok(new_mount_fs);
    }

    fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        if !self.is_mountpoint_root()? {
            return Err(SystemError::EINVAL);
        }
        return self.mount_fs.umount();
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        self.do_absolute_path()
    }

    #[inline]
    fn mknod(
        &self,
        filename: &str,
        mode: ModeType,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.mknod(filename, mode, dev_t)?;
        return Ok(Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
        }));
    }

    #[inline]
    fn special_node(&self) -> Option<super::SpecialNodeData> {
        self.inner_inode.special_node()
    }

    /// 若不支持，则调用第二种情况来从父目录获取文件名
    /// # Performance
    /// 应尽可能引入DName，
    /// 在默认情况下，性能非常差！！！
    fn dname(&self) -> Result<DName, SystemError> {
        if self.is_mountpoint_root()? {
            if let Some(inode) = &self.mount_fs.self_mountpoint {
                return inode.inner_inode.dname();
            }
        }
        return self.inner_inode.dname();
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_parent().map(|inode| inode as Arc<dyn IndexNode>);
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.inner_inode.page_cache()
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        self.inner_inode.as_pollable_inode()
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

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        self.inner_filesystem.fault(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        self.inner_filesystem.map_pages(pfm, start_pgoff, end_pgoff)
    }
}

/// MountList
/// ```rust
/// use alloc::collection::BTreeSet;
/// let map = BTreeSet::from([
///     "/sys", "/dev", "/", "/bin", "/proc"
/// ]);
/// assert_eq!(format!("{:?}", map), "{\"/\", \"/bin\", \"/dev\", \"/proc\", \"/sys\"}");
/// // {"/", "/bin", "/dev", "/proc", "/sys"}
/// ```
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
            // 深度一样时反序来排
            // 根目录和根目录下的文件的绝对路径都只有一个'/'
            other.0.cmp(&self.0)
        } else {
            // 根据深度，深度
            othe_dep.cmp(&self_dep)
        }
    }
}

// 维护一个挂载点的记录，以支持特定于文件系统的索引
pub struct MountList(RwLock<BTreeMap<MountPath, Arc<MountFS>>>);
// pub struct MountList(Option<Arc<MountListInner>>);
static mut __MOUNTS_LIST: Option<Arc<MountList>> = None;

/// # init_mountlist - 初始化挂载列表
///
/// 此函数用于初始化系统的挂载列表。挂载列表记录了系统中所有的文件系统挂载点及其属性。
///
/// ## 参数
///
/// - 无
///
/// ## 返回值
///
/// - 无
#[inline(always)]
pub fn init_mountlist() {
    unsafe {
        __MOUNTS_LIST = Some(Arc::new(MountList(RwLock::new(BTreeMap::new()))));
    }
}

/// # MOUNT_LIST - 获取全局挂载列表
///
/// 该函数用于获取一个对全局挂载列表的引用。全局挂载列表是系统中所有挂载点的集合。
///
/// ## 返回值
/// - &'static Arc<MountList>: 返回全局挂载列表的引用。
#[inline(always)]
#[allow(non_snake_case)]
pub fn MOUNT_LIST() -> &'static Arc<MountList> {
    unsafe {
        return __MOUNTS_LIST.as_ref().unwrap();
    }
}

impl MountList {
    /// # insert - 将文件系统挂载点插入到挂载表中
    ///
    /// 将一个新的文件系统挂载点插入到挂载表中。如果挂载点已经存在，则会更新对应的文件系统。
    ///
    /// 此函数是线程安全的，因为它使用了RwLock来保证并发访问。
    ///
    /// ## 参数
    ///
    /// - `path`: &str, 挂载点的路径。这个路径会被转换成`MountPath`类型。
    /// - `fs`: Arc<MountFS>, 共享的文件系统实例。
    ///
    /// ## 返回值
    ///
    /// - 无
    #[inline]
    pub fn insert<T: AsRef<str>>(&self, path: T, fs: Arc<MountFS>) {
        self.0.write().insert(MountPath::from(path.as_ref()), fs);
    }

    /// # get_mount_point - 获取挂载点的路径
    ///
    /// 这个函数用于查找给定路径的挂载点。它搜索一个内部映射，找到与路径匹配的挂载点。
    ///
    /// ## 参数
    ///
    /// - `path: T`: 这是一个可转换为字符串的引用，表示要查找其挂载点的路径。
    ///
    /// ## 返回值
    ///
    /// - `Option<(String, String, Arc<MountFS>)>`:
    ///   - `Some((mount_point, rest_path, fs))`: 如果找到了匹配的挂载点，返回一个包含挂载点路径、剩余路径和挂载文件系统的元组。
    ///   - `None`: 如果没有找到匹配的挂载点，返回 None。
    #[inline]
    #[allow(dead_code)]
    pub fn get_mount_point<T: AsRef<str>>(
        &self,
        path: T,
    ) -> Option<(String, String, Arc<MountFS>)> {
        self.0
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

    /// # remove - 移除挂载点
    ///
    /// 从挂载点管理器中移除一个挂载点。
    ///
    /// 此函数用于从挂载点管理器中移除一个已经存在的挂载点。如果挂载点不存在，则不进行任何操作。
    ///
    /// ## 参数
    ///
    /// - `path: T`: `T` 实现了 `Into<MountPath>`  trait，代表要移除的挂载点的路径。
    ///
    /// ## 返回值
    ///
    /// - `Option<Arc<MountFS>>`: 返回一个 `Arc<MountFS>` 类型的可选值，表示被移除的挂载点，如果挂载点不存在则返回 `None`。
    #[inline]
    pub fn remove<T: Into<MountPath>>(&self, path: T) -> Option<Arc<MountFS>> {
        self.0.write().remove(&path.into())
    }
}

impl Debug for MountList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(MOUNT_LIST().0.read().iter()).finish()
    }
}
