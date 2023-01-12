use core::ops::DerefMut;

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use crate::{libs::spinlock::RawSpinlock, include::bindings::bindings::ENOTDIR};

use super::{FileSystem, IndexNode, InodeId, FileType};

/// @brief 挂载文件系统
/// 挂载文件系统的时候，套了MountFS这一层，以实现文件系统的递归挂载
#[derive(Debug)]
pub struct MountFS {
    // MountFS内部的文件系统
    inner_filesystem: Arc<dyn FileSystem>,
    /// mountpoints B树的锁
    mountpoints_lock: RawSpinlock,
    /// 用来存储InodeID->挂载点的MountFS的B树
    mountpoints: BTreeMap<InodeId, Arc<MountFS>>,
    /// 当前挂载点的Inode
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
    pub fn new(inner_fs: Arc<dyn FileSystem>, self_mountpoint: Option<Arc<MountFSInode>>) -> Arc<Self> {
        return MountFS {
            inner_filesystem: inner_fs,
            mountpoints_lock: RawSpinlock::INIT,
            mountpoints: BTreeMap::new(),
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

    /// @brief 在当前inode下，挂载一个文件系统
    /// 
    /// @return Ok(Arc<MountFS>) 挂载成功，返回指向
    pub fn mount(&mut self, fs: Arc<dyn FileSystem>)->Result<Arc<MountFS>, i32>{
        let metadata = self.inner_inode.metadata()?;
        if metadata.file_type != FileType::Dir{
            return Err(-(ENOTDIR as i32));
        }

        let new_mount_fs = MountFS::new(fs, Some(self.self_ref.upgrade().unwrap()));

        self.mount_fs.mountpoints_lock.lock();
        self.mount_fs.mountpoints_lock.unlock();
        return Ok(new_mount_fs);
    }
}
