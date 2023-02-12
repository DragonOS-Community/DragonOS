use core::any::Any;

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use crate::{
    include::bindings::bindings::{EBUSY, ENOTDIR},
    libs::spinlock::SpinLock,
};

use super::{FileSystem, FileType, IndexNode, InodeId};

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
        inner_fs: Arc<dyn FileSystem>,
        self_mountpoint: Option<Arc<MountFSInode>>,
    ) -> Arc<Self> {
        return MountFS {
            inner_filesystem: inner_fs,
            mountpoints: SpinLock::new(BTreeMap::new()),
            self_mountpoint: self_mountpoint,
            self_ref: Weak::default(),
        }
        .wrap();
    }

    /// @brief 用Arc指针包裹MountFS对象。
    /// 本函数的主要功能为，初始化MountFS对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let mount_fs: Arc<MountFS> = Arc::new(self);
        // 创建weak指针
        let weak: Weak<MountFS> = Arc::downgrade(&mount_fs);

        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut MountFS = Arc::into_raw(mount_fs) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            // 返回初始化好的MountFS对象
            return Arc::from_raw(ptr);
        }
    }

    /// @brief 获取挂载点的文件系统的root inode
    pub fn mountpoint_root_inode(&self) -> Arc<MountFSInode> {
        return MountFSInode {
            inner_inode: self.inner_filesystem.get_root_inode(),
            mount_fs: self.self_ref.upgrade().unwrap(),
            self_ref: Weak::default(),
        }
        .wrap();
    }
}

impl MountFSInode {
    /// @brief 用Arc指针包裹MountFSInode对象。
    /// 本函数的主要功能为，初始化MountFSInode对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let inode: Arc<MountFSInode> = Arc::new(self);
        // 创建Weak指针
        let weak: Weak<MountFSInode> = Arc::downgrade(&inode);
        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut MountFSInode = Arc::into_raw(inode) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;

            // 返回初始化好的MountFSInode对象
            return Arc::from_raw(ptr);
        }
    }

    /// @brief 判断当前inode是否为它所在的文件系统的root inode
    fn is_mountpoint_root(&self) -> Result<bool, i32> {
        return Ok(self.inner_inode.fs().get_root_inode().metadata()?.inode_id
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
}

impl IndexNode for MountFSInode {
    #[inline]
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        return self.inner_inode.read_at(offset, len, buf);
    }

    #[inline]
    fn write_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        return self.inner_inode.write_at(offset, len, buf);
    }

    #[inline]
    fn poll(&self) -> Result<super::PollStatus, i32> {
        return self.inner_inode.poll();
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
    fn metadata(&self) -> Result<super::Metadata, i32> {
        return self.inner_inode.metadata();
    }

    #[inline]
    fn set_metadata(&self, metadata: &super::Metadata) -> Result<(), i32> {
        return self.inner_inode.set_metadata(metadata);
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), i32> {
        return self.inner_inode.resize(len);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        return Ok(MountFSInode {
            inner_inode: self.inner_inode.create(name, file_type, mode)?,
            mount_fs: self.mount_fs.clone(),
            self_ref: Weak::default(),
        }
        .wrap());
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), i32> {
        return self.inner_inode.link(name, other);
    }

    /// @brief 在挂载文件系统中删除文件/文件夹
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), i32> {
        let inode_id = self.inner_inode.find(name)?.metadata()?.inode_id;

        // 先检查这个inode是否为一个挂载点，如果当前inode是一个挂载点，那么就不能删除这个inode
        if self.mount_fs.mountpoints.lock().contains_key(&inode_id) {
            return Err(-(EBUSY as i32));
        }
        // 调用内层的inode的方法来删除这个inode
        return self.inner_inode.unlink(name);
    }

    #[inline]
    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), i32> {
        return self.inner_inode.move_(old_name, target, new_name);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, i32> {
        match name {
            // 查找的是当前目录
            "" | "." => return Ok(self.self_ref.upgrade().unwrap()),
            // 往父级查找
            ".." => {
                if self.is_mountpoint_root()? {
                    // 当前inode是它所在的文件系统的root inode
                    match &self.mount_fs.self_mountpoint {
                        Some(inode) => {
                            return inode.find(name);
                        }
                        None => {
                            return Ok(self.self_ref.upgrade().unwrap());
                        }
                    }
                } else {
                    // 向上查找时，不会跨过文件系统的边界，因此直接调用当前inode所在的文件系统的find方法进行查找
                    return Ok(MountFSInode {
                        inner_inode: self.inner_inode.find(name)?,
                        mount_fs: self.mount_fs.clone(),
                        self_ref: Weak::default(),
                    }
                    .wrap());
                }
            }
            // 在当前目录下查找
            _ => {
                // 直接调用当前inode所在的文件系统的find方法进行查找
                // 由于向下查找可能会跨越文件系统的边界，因此需要尝试替换inode
                return Ok(MountFSInode {
                    inner_inode: self.inner_inode.find(name)?,
                    mount_fs: self.mount_fs.clone(),
                    self_ref: Weak::default(),
                }
                .wrap()
                .overlaid_inode());
            }
        }
    }
    
    #[inline]
    fn get_entry_name(&self, ino: InodeId) -> Result<alloc::string::String, i32> {
        return self.inner_inode.get_entry_name(ino);
    }

    #[inline]
    fn get_entry_name_and_metadata(
        &self,
        ino: InodeId,
    ) -> Result<(alloc::string::String, super::Metadata), i32> {
        return self.inner_inode.get_entry_name_and_metadata(ino);
    }

    #[inline]
    fn ioctl(&self, cmd: u32, data: usize) -> Result<usize, i32> {
        return self.inner_inode.ioctl(cmd, data);
    }

    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, i32> {
        return self.inner_inode.list();
    }

    /// @brief 在当前inode下，挂载一个文件系统
    ///
    /// @return Ok(Arc<MountFS>) 挂载成功，返回指向MountFS的指针
    fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, i32> {
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 为新的挂载点创建挂载文件系统
        let new_mount_fs: Arc<MountFS> = MountFS::new(fs, Some(self.self_ref.upgrade().unwrap()));
        // 将新的挂载点-挂载文件系统添加到父级的挂载树
        self.mount_fs
            .mountpoints
            .lock()
            .insert(metadata.inode_id, new_mount_fs.clone());
        return Ok(new_mount_fs);
    }
}

impl FileSystem for MountFS {
    fn get_root_inode(&self) -> Arc<dyn IndexNode> {
        match &self.self_mountpoint {
            Some(inode) => return inode.mount_fs.get_root_inode(),
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
}
