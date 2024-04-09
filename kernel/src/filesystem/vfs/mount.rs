pub mod utils;
pub mod dcache;

use core::{
    any::Any,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use hashbrown::HashSet;
use path_base::PathBuf;
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    libs::{rwlock::RwLock, spinlock::{SpinLock, SpinLockGuard}, casting::DowncastArc},
};

use super::{
    file::FileMode, syscall::ModeType, utils::Key, DCache, FilePrivateData, FileSystem, FileType, IndexNode, InodeId, Magic, SuperBlock, ROOT_INODE
};

use self::utils::{MountList, MountNameCmp};

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

    self_root_inode: Weak<MountFSInode>,

    pub dcache: DCache,
}

/// # Behavior
/// - parent 指向父节点，倘若当前节点为当前文件系统的目录根，则为Weak::new()
/// - name 目录项名称，在create/move_to 时 创建/更改，若当前节点为当前文件系统的目录根，则为String::new()
/// - children 在create/find 时 创建/更改
/// - key 当获取节点时，对于挂载点，以父文件系统的挂载点名字，内容为挂载点下文件系统具体内容
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

    name: Arc<String>,

    parent: Weak<MountFSInode>,

    children: Arc<RwLock<HashSet<Key<MountNameCmp>>>>,
}

impl MountFS {
    pub fn new(
        inner_fs: Arc<dyn FileSystem>,
        self_mountpoint: Option<Arc<MountFSInode>>,
    ) -> Arc<Self> {
        let mut ret = Arc::new_cyclic(|fs_ref|
            MountFS {
                inner_filesystem: inner_fs.clone(),
                mountpoints: SpinLock::new(BTreeMap::new()),
                self_mountpoint,
                self_ref: fs_ref.clone(),
                self_root_inode: Weak::new(),
                dcache: DCache::new(),
            }
        );
        let root = Arc::new_cyclic( |node_self| {
            MountFSInode {
                inner_inode: inner_fs.root_inode(),
                mount_fs: ret.clone(),
                self_ref: node_self.clone(),
                name: Arc::default(),
                parent: Weak::new(),
                children: Arc::new(RwLock::new(HashSet::new())),
            }
        });
        ret.dcache.put(&root);
        unsafe { Arc::get_mut_unchecked(&mut ret).self_root_inode = Arc::downgrade(&root); }
        return ret;
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
        return self.self_root_inode.upgrade().unwrap();
    }

    pub fn inner_filesystem(&self) -> Arc<dyn FileSystem> {
        return self.inner_filesystem.clone();
    }

    pub fn self_ref(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
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

    pub fn _find(&self, name: &str) -> Result<Arc<MountFSInode>, SystemError> {
        match name {
            // 查找的是当前目录
            "" | "." => return Ok(self.self_ref.upgrade().unwrap()),
            // 往父级查找
            ".." => {
                return self._parent();
            }
            // 在当前目录下查找
            _ => {
                // 直接调用当前inode所在的文件系统的find方法进行查找
                // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
                let inner_inode = self.inner_inode.find(name)?;
                let ret = Arc::new_cyclic(|self_ref| MountFSInode {
                    inner_inode,
                    mount_fs: self.mount_fs.clone(),
                    self_ref: self_ref.clone(),
                    name: Arc::new(String::from(name)),
                    parent: self.self_ref.clone(),
                    children: Arc::new(RwLock::new(HashSet::new())),
                }).overlaid_inode();
                // 按道理加入挂载点后，这里不需要替换inode
                self.children.write();
                self.mount_fs.dcache.put(&ret);
                return Ok(ret);
            }
        }
    }

    /// 退化到文件系统中递归查找文件, 并将找到的目录项缓存到dcache中, 返回找到的节点
    ///
    /// - rest_path: 剩余路径
    /// - max_follow_times: 最大跟随次数
    /// ## Err
    /// - SystemError::ENOENT: 未找到
    pub(super) fn lookup_walk(
        &self,
        rest_path: &path_base::Path,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // kdebug!("walking though {:?}", rest_path);
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let child = rest_path.iter().next().unwrap();
        // kdebug!("Lookup {:?}", child);
        match self._find(child) {
            Ok(child_node) => {
                if child_node.metadata()?.file_type == FileType::SymLink && max_follow_times > 0 {
                    // symlink wrapping problem
                    return ROOT_INODE().lookup_follow_symlink(
                        child_node.convert_symlink()?.join(rest_path),
                        max_follow_times - 1,
                    );
                }

                self.mount_fs.dcache.put(&child_node);

                if let Ok(rest) = rest_path.strip_prefix(child) {
                    if rest.iter().next().is_some() {
                        // kdebug!("Rest {:?}", rest);
                        return child_node.lookup_walk(rest, max_follow_times);
                    }
                }
                // kdebug!("return node {:?}", child_node);

                return Ok(child_node);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    /// 将符号链接转换为路径
    fn convert_symlink(&self) -> Result<PathBuf, SystemError> {
        let mut content = [0u8; 256];
        let len = self.read_at(
            0,
            256,
            &mut content,
            SpinLock::new(FilePrivateData::Unused).lock(),
        )?;

        return Ok(PathBuf::from(
            ::core::str::from_utf8(&content[..len]).map_err(|_| SystemError::ENOTDIR)?,
        ));
    }

    fn _parent(&self) -> Result<Arc<MountFSInode>, SystemError> {
        if self.is_mountpoint_root()? {
            match &self.mount_fs.self_mountpoint {
                Some(inode) => {
                    return inode._parent();
                }
                None => {
                    return self.self_ref.upgrade().ok_or(SystemError::ENOENT);
                }
            }
        }

        return Ok(self.parent.upgrade().unwrap_or(self.self_ref.upgrade().unwrap()));
    }

    fn _create(
        &self,
        name: Arc<String>,
        inner: Arc<dyn IndexNode>,
    ) -> Arc<MountFSInode> {
        let ret = Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode: inner.clone(),
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
            name: name.clone(),
            parent: self.self_ref.clone(),
            children: Arc::new(RwLock::new(HashSet::new())),
        });

        self.children.write().insert(Key::Inner(MountNameCmp(Arc::downgrade(&ret))));
        self.mount_fs.dcache.put(&ret);

        return ret;
    }

    fn _link(&self, name: Arc<String>, other: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        if let Some(target) = other.downcast_arc::<MountFSInode>() {

            let to_link = Arc::new_cyclic(|self_ref| MountFSInode {
                inner_inode: target.inner_inode.clone(),
                mount_fs: self.mount_fs.clone(),
                self_ref: self_ref.clone(),
                name: name.clone(),
                parent: self.self_ref.clone(),
                children: target.children.clone(),
            });

            self.children.write().insert(Key::Inner(MountNameCmp(Arc::downgrade(&to_link))));
            self.mount_fs.dcache.put(&target);

            return Ok(());
        }
        return Err(SystemError::EINVAL);
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
        let inner_inode = self.inner_inode.create_with_data(name, file_type, mode, data)?;
        let ret = Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
            name: Arc::new(String::from(name)),
            parent: self.self_ref.clone(),
            children: Arc::new(RwLock::new(HashSet::new())),
        });

        self.children.write().insert(Key::Inner(MountNameCmp(Arc::downgrade(&ret))));
        self.mount_fs.dcache.put(&ret);

        return Ok(ret);
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
        let inner_inode = self.inner_inode.create(name, file_type, mode)?;
        let ret = Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
            name: Arc::new(String::from(name)),
            parent: self.self_ref.clone(),
            children: Arc::new(RwLock::new(HashSet::new())),
        });

        self.children.write().insert(Key::Inner(MountNameCmp(Arc::downgrade(&ret))));
        self.mount_fs.dcache.put(&ret);

        return Ok(ret);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.inner_inode.link(name, other)?;
        self._link(Arc::new(String::from(name)), other.clone())?;
        return Ok(());
    }

    /// @brief 在挂载文件系统中删除文件/文件夹
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        // kdebug!("Call Mountfs unlink: Item {}", name);
        let inode_id = self.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }

        // 调用内层的inode的方法来删除这个inode
        if let Err(err) = self.inner_inode.unlink(name) {
            return Err(err);
        }

        self.children.write().remove(&Key::Cmp(Arc::new(String::from(name))));

        return Ok(());
    }

    #[inline]
    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(SystemError::EBUSY);
        }
        // 调用内层的rmdir的方法来删除这个inode
        if let Err(e) = self.inner_inode.rmdir(name) {
            return Err(e);
        }

        self.children.write().remove(&Key::Cmp(Arc::new(String::from(name))));

        return Ok(());
    }

    #[inline]
    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        self.children.write().remove(&Key::Cmp(Arc::new(String::from(old_name))));
        let to_move = self.find(old_name)?;
        if let Some(target1) = target.clone().downcast_arc::<MountFSInode>() {

            target1._link(Arc::new(String::from(new_name)), to_move)?;
            self.children.write().remove(&Key::Cmp(Arc::new(String::from(old_name))));

            self.inner_inode.move_to(old_name, target, new_name)?;
            return Ok(());
        }
        return Err(SystemError::EINVAL);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        return Ok(self._find(name)?);
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

    // Todo: 缓存当前目录下的目录项
    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        // let mut ret: Vec<String> = Vec::new();
        // ret.push(String::from("."));
        // ret.push(String::from(".."));
        // let mut ext = self
        //     .children
        //     .read()
        //     .iter()
        //     .map(|item| {
        //         (*item.unwrap()).clone()
        //     })
        //     .collect::<Vec<String>>();
        // ext.sort();
        // ret.append(&mut ext);
        // kdebug!("{:?}", ret);
        return self.inner_inode.list();
    }

    /// @brief 在当前inode下，挂载一个文件系统
    ///
    /// @return Ok(Arc<MountFS>) 挂载成功，返回指向MountFS的指针
    fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, SystemError> {
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 为新的挂载点创建挂载文件系统
        let new_mount_fs: Arc<MountFS> = MountFS::new(fs, Some(self.self_ref.upgrade().unwrap()));
        self.do_mount(metadata.inode_id, new_mount_fs.clone())?;

        MountList::insert(self.abs_path()?, &new_mount_fs);

        return Ok(new_mount_fs);
    }

    #[inline]
    fn mknod(
        &self,
        filename: &str,
        mode: ModeType,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inner_inode = self.inner_inode.mknod(filename, mode, dev_t)?;
        let ret = Arc::new_cyclic(|self_ref| MountFSInode {
            inner_inode,
            mount_fs: self.mount_fs.clone(),
            self_ref: self_ref.clone(),
            name: Arc::new(String::from(filename)),
            parent: self.self_ref.clone(),
            children: Arc::new(RwLock::new(HashSet::new())),
        });
        self.children.write().insert(Key::Inner(MountNameCmp(Arc::downgrade(&ret))));
        self.mount_fs.dcache.put(&ret);
        return Ok(ret);
    }

    #[inline]
    fn special_node(&self) -> Option<super::SpecialNodeData> {
        self.inner_inode.special_node()
    }

    #[inline]
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        self.inner_inode.poll(private_data)
    }

    #[inline]
    fn entry_name(&self) -> Result<String, SystemError> {
        self.inner_inode.entry_name()
    }

    fn abs_path(&self) -> Result<PathBuf, SystemError> {
        let mut path_stack = Vec::new();
        // kdebug!("Inner: {:?}", self.inner_inode.entry_name());
        path_stack.push(self.name.as_ref().clone());

        let init = self._parent()?;
        if self.metadata()?.inode_id == init.metadata()?.inode_id {
            return Ok(PathBuf::from("/"));
        }

        let mut inode = init;
        path_stack.push(inode.name.as_ref().clone());

        while inode.metadata()?.inode_id != ROOT_INODE().metadata()?.inode_id {
            let tmp = inode._parent()?;
            if inode.metadata()?.inode_id == tmp.metadata()?.inode_id {
                break;
            }
            inode = tmp;
            path_stack.push(inode.name.as_ref().clone());
        }

        path_stack.reverse();
        let mut path = PathBuf::from("/");
        path.extend(path_stack);
        return Ok(path);
    }

}

impl FileSystem for MountFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        return match &self.self_mountpoint {
            Some(inode) => {
                // kdebug!("Mount point at {:?}", inode._abs_path());
                inode.mount_fs.root_inode()
            }
            // 当前文件系统是rootfs
            None => {
                // kdebug!("Root fs");
                self.mountpoint_root_inode()
            }
        };
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
