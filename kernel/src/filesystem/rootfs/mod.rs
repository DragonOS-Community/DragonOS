use core::{
    any::Any,
    ptr::null_mut,
    sync::atomic::{AtomicI8, Ordering},
};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};

use crate::{include::bindings::bindings::EPERM, kerror};

use super::vfs::{
    file::FilePrivateData, mount::MountFS, FileSystem, FileType, FsInfo, IndexNode, InodeId,
    Metadata, PollStatus,
};

/// RootFS内部的具体的文件系统
static mut __INNER_MOUNT_FS: *mut Arc<dyn FileSystem> = null_mut();

/// ROOTFS实例计数
static __ROOT_FS_COUNT: AtomicI8 = AtomicI8::new(0);

/// @brief 根文件系统
/// 该文件系统用于支持ROOT_INODE的迁移。因此，它只能有1个实例，并且，这个实例只能够有1个inode
#[derive(Debug)]
pub struct RootFS {
    /// 指向自身的弱引用
    self_ref: Weak<RootFS>,
    root_inode: Arc<RootFSInode>,
}

/// @brief RootFS的Index Node
/// 请注意，它只能有1个实例
#[derive(Debug)]
pub struct RootFSInode {
    /// 指向自身的弱引用
    self_ref: Weak<RootFSInode>,
}

impl RootFS {
    pub fn new(inner_fs: Arc<dyn FileSystem>) -> Result<Arc<Self>, i32> {
        let mount_fs: Arc<MountFS> = MountFS::new(inner_fs, None);
        // 先检查是否有多于1个实例
        let prev_cnt = __ROOT_FS_COUNT.fetch_add(1, Ordering::SeqCst);
        if prev_cnt > 0 {
            kerror!("Attempting to create RootFS instance twice!");
            return Err(-(EPERM as i32));
        }

        unsafe {
            // let x: &mut Arc<dyn IndexNode> = Box::leak(Box::new(inner_fs.root_inode()));
            // __INNER_ROOT_INODE = x;
            let f: &mut Arc<dyn FileSystem> = Box::leak(Box::new(mount_fs));
            __INNER_MOUNT_FS = f;
        }
        let root_inode: Arc<RootFSInode> = RootFSInode {
            self_ref: Weak::default(),
        }
        .wrap();
        let rootfs: Arc<RootFS> = RootFS {
            self_ref: Weak::default(),
            root_inode: root_inode,
        }
        .wrap();

        return Ok(rootfs);
    }

    /// @brief 用Arc指针包裹MountFS对象。
    /// 本函数的主要功能为，初始化MountFS对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let mount_fs: Arc<RootFS> = Arc::new(self);
        // 创建weak指针
        let weak: Weak<RootFS> = Arc::downgrade(&mount_fs);

        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut RootFS = Arc::into_raw(mount_fs) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;
            // 返回初始化好的MountFS对象
            return Arc::from_raw(ptr);
        }
    }
}

impl RootFSInode {
    /// @brief 用Arc指针包裹MountFSInode对象。
    /// 本函数的主要功能为，初始化MountFSInode对象中的自引用Weak指针
    /// 本函数只应在构造器中被调用
    fn wrap(self) -> Arc<Self> {
        // 创建Arc指针
        let inode: Arc<RootFSInode> = Arc::new(self);
        // 创建Weak指针
        let weak: Weak<RootFSInode> = Arc::downgrade(&inode);
        // 将Arc指针转为Raw指针并对其内部的self_ref字段赋值
        let ptr: *mut RootFSInode = Arc::into_raw(inode) as *mut Self;
        unsafe {
            (*ptr).self_ref = weak;

            // 返回初始化好的MountFSInode对象
            return Arc::from_raw(ptr);
        }
    }

    #[inline]
    fn inner_inode(&self) -> Arc<dyn IndexNode> {
        unsafe {
            return __INNER_MOUNT_FS.as_ref().unwrap().root_inode();
        }
    }
}

impl IndexNode for RootFSInode {
    #[inline]
    fn open(&self, data: &mut FilePrivateData) -> Result<(), i32> {
        return self.inner_inode().open(data);
    }

    #[inline]
    fn close(&self, data: &mut FilePrivateData) -> Result<(), i32> {
        return self.inner_inode().close(data);
    }

    #[inline]
    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        return self
            .inner_inode()
            .create_with_data(name, file_type, mode, data);
    }

    #[inline]
    fn truncate(&self, len: usize) -> Result<(), i32> {
        return self.inner_inode().truncate(len);
    }

    #[inline]
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        return self
            .inner_inode()
            .read_at(offset, len, buf, &mut FilePrivateData::Unused);
    }

    #[inline]
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        return self
            .inner_inode()
            .write_at(offset, len, buf, &mut FilePrivateData::Unused);
    }

    #[inline]
    fn poll(&self) -> Result<PollStatus, i32> {
        return self.inner_inode().poll();
    }

    #[inline]
    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.inner_inode().fs();
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    #[inline]
    fn metadata(&self) -> Result<Metadata, i32> {
        return self.inner_inode().metadata();
    }

    #[inline]
    fn set_metadata(&self, metadata: &Metadata) -> Result<(), i32> {
        return self.inner_inode().set_metadata(metadata);
    }

    #[inline]
    fn resize(&self, len: usize) -> Result<(), i32> {
        return self.inner_inode().resize(len);
    }

    #[inline]
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        return self.inner_inode().create(name, file_type, mode);
    }

    #[inline]
    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), i32> {
        return self.inner_inode().link(name, other);
    }

    /// @brief 在挂载文件系统中删除文件/文件夹
    #[inline]
    fn unlink(&self, name: &str) -> Result<(), i32> {
        return self.inner_inode().unlink(name);
    }

    #[inline]
    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), i32> {
        return self.inner_inode().move_(old_name, target, new_name);
    }

    #[inline]
    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, i32> {
        return self.inner_inode().find(name);
    }

    #[inline]
    fn get_entry_name(&self, ino: InodeId) -> Result<alloc::string::String, i32> {
        return self.inner_inode().get_entry_name(ino);
    }

    #[inline]
    fn get_entry_name_and_metadata(
        &self,
        ino: InodeId,
    ) -> Result<(alloc::string::String, Metadata), i32> {
        return self.inner_inode().get_entry_name_and_metadata(ino);
    }

    #[inline]
    fn ioctl(&self, cmd: u32, data: usize) -> Result<usize, i32> {
        return self.inner_inode().ioctl(cmd, data);
    }

    #[inline]
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, i32> {
        return self.inner_inode().list();
    }

    /// @brief 替换Root fs下的具体文件系统
    /// 由于mount方法的返回参数限制，我们约定ROOT_INODE.mount()返回Err(0)时，表示执行成功。
    #[inline]
    fn mount(&self, fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, i32> {
        let fs = MountFS::new(fs, None) as Arc<dyn FileSystem>;
        unsafe {
            if !__INNER_MOUNT_FS.is_null() {
                let f: Box<Arc<dyn FileSystem>> = Box::from_raw(__INNER_MOUNT_FS);
                drop(f);
                __INNER_MOUNT_FS = null_mut();
            }

            __INNER_MOUNT_FS = Box::leak(Box::new(fs));
        }
        return Err(0);
    }
}

impl FileSystem for RootFS {
    #[inline]
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        return self.root_inode.clone();
    }

    #[inline]
    fn info(&self) -> FsInfo {
        unsafe {
            return __INNER_MOUNT_FS.as_ref().unwrap().info();
        }
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    #[inline]
    fn as_any_ref(&self) -> &dyn Any {
        unsafe { return __INNER_MOUNT_FS.as_mut().unwrap().as_any_ref() };
    }
}
