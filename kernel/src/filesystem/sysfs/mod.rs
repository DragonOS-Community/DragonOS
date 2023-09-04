use super::vfs::{
    core::generate_inode_id, file::FileMode, FileSystem, FileType, FsInfo, IndexNode, Metadata,
    PollStatus,
};
use crate::{
    libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
    time::TimeSpec,
};
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::ptr::null_mut;

pub mod bus;
pub mod class;
pub mod devices;
pub mod fs;

const SYSFS_MAX_NAMELEN: usize = 64;

static mut __SYS_DEVICES_INODE: *mut Arc<dyn IndexNode> = null_mut();
static mut __SYS_BUS_INODE: *mut Arc<dyn IndexNode> = null_mut();
static mut __SYS_CLASS_INODE: *mut Arc<dyn IndexNode> = null_mut();
static mut __SYS_FS_INODE: *mut Arc<dyn IndexNode> = null_mut();

/// @brief 获取全局的sys/devices节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn SYS_DEVICES_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __SYS_DEVICES_INODE.as_ref().unwrap().clone();
    }
}

/// @brief 获取全局的sys/bus节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn SYS_BUS_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __SYS_BUS_INODE.as_ref().unwrap().clone();
    }
}

/// @brief 获取全局的sys/class节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn SYS_CLASS_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __SYS_CLASS_INODE.as_ref().unwrap().clone();
    }
}

/// @brief 获取全局的sys/fs节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn SYS_FS_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __SYS_FS_INODE.as_ref().unwrap().clone();
    }
}

/// @brief dev文件系统
#[derive(Debug)]
pub struct SysFS {
    // 文件系统根节点
    root_inode: Arc<LockedSysFSInode>,
}

impl FileSystem for SysFS {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> super::vfs::FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: SYSFS_MAX_NAMELEN,
        };
    }
}

impl SysFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedSysFSInode> = Arc::new(LockedSysFSInode(SpinLock::new(
            // /sys 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            SysFSInode::new(FileType::Dir, 0o755 as u32, 0),
        )));

        let sysfs: Arc<SysFS> = Arc::new(SysFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<SysFSInode> = sysfs.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&sysfs.root_inode);
        root_guard.self_ref = Arc::downgrade(&sysfs.root_inode);
        root_guard.fs = Arc::downgrade(&sysfs);
        // 释放锁
        drop(root_guard);

        // 创建文件夹
        let root: &Arc<LockedSysFSInode> = &sysfs.root_inode;
        match root.add_dir("devices") {
            Ok(devices) => unsafe {
                __SYS_DEVICES_INODE = Box::leak(Box::new(devices));
            },
            Err(_) => panic!("SysFS: Failed to create /sys/devices"),
        }

        match root.add_dir("bus") {
            Ok(bus) => unsafe {
                __SYS_BUS_INODE = Box::leak(Box::new(bus));
            },
            Err(_) => panic!("SysFS: Failed to create /sys/bus"),
        }

        match root.add_dir("class") {
            Ok(class) => unsafe {
                __SYS_CLASS_INODE = Box::leak(Box::new(class));
            },
            Err(_) => panic!("SysFS: Failed to create /sys/class"),
        }

        match root.add_dir("fs") {
            Ok(fs) => unsafe {
                __SYS_FS_INODE = Box::leak(Box::new(fs));
            },
            Err(_) => panic!("SysFS: Failed to create /sys/fs"),
        }
        // 初始化platform总线
        crate::driver::base::platform::platform_bus_init().expect("platform bus init failed");
        // 初始化串口
        crate::driver::uart::uart_device::uart_init().expect("initilize uart error");
        return sysfs;
    }
}

/// @brief sys文件i节点(锁)
#[derive(Debug)]
pub struct LockedSysFSInode(SpinLock<SysFSInode>);

impl IndexNode for LockedSysFSInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: &mut super::vfs::FilePrivateData,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: &mut super::vfs::FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: &mut super::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut super::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn poll(&self) -> Result<super::vfs::PollStatus, SystemError> {
        // 加锁
        let inode: SpinLockGuard<SysFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        return Ok(PollStatus::READ | PollStatus::WRITE);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn get_entry_name(&self, ino: super::vfs::InodeId) -> Result<String, SystemError> {
        let inode: SpinLockGuard<SysFSInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match ino {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = inode
                    .children
                    .keys()
                    .filter(|k| inode.children.get(*k).unwrap().metadata().unwrap().inode_id == ino)
                    .cloned()
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Sysfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id}, to find={to_find}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                }
            }
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => {
                return Ok(inode.self_ref.upgrade().ok_or(SystemError::ENOENT)?);
            }
            ".." => {
                return Ok(inode.parent.upgrade().ok_or(SystemError::ENOENT)?);
            }
            name => {
                // 在子目录项中查找
                // match inode.children.get(name) {
                //     Some(_) => {}
                //     None => kdebug!("Sysfs find {} error", name),
                // }
                return Ok(inode.children.get(name).ok_or(SystemError::ENOENT)?.clone());
            }
        }
    }

    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(&mut self.0.lock().children.keys().cloned().collect());

        return Ok(keys);
    }
}

impl LockedSysFSInode {
    fn do_create_with_data(
        &self,
        mut guard: SpinLockGuard<SysFSInode>,
        _name: &str,
        _file_type: FileType,
        _mode: u32,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果有重名的，则返回
        if guard.children.contains_key(_name) {
            return Err(SystemError::EEXIST);
        }

        // 创建inode
        let result: Arc<LockedSysFSInode> = Arc::new(LockedSysFSInode(SpinLock::new(SysFSInode {
            parent: guard.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: _file_type,
                mode: _mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: _data,
            },
            fs: guard.fs.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        guard.children.insert(String::from(_name), result.clone());
        return Ok(result);
    }

    /// @brief 在当前目录下，创建一个目录
    /// @param name: 目录名
    /// @return 成功返回目录inode, 失败返回Err(错误码)
    #[inline]
    #[allow(dead_code)]
    pub fn add_dir(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let guard: SpinLockGuard<SysFSInode> = self.0.lock();

        if guard.children.contains_key(name) {
            return Err(SystemError::EEXIST);
        }

        match self.do_create_with_data(guard, name, FileType::Dir, 0o755 as u32, 0) {
            Ok(inode) => return Ok(inode),
            Err(err) => {
                return Err(err);
            }
        };
    }

    /// @brief 在当前目录下，创建一个二进制文件
    /// @param name: 文件名
    /// @return 成功返回Ok(()), 失败返回Err(错误码)
    #[inline]
    #[allow(dead_code)]
    pub fn add_file(&self, name: &str, file: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut this = self.0.lock();

        if this.children.contains_key(name) {
            return Err(SystemError::EEXIST);
        }

        this.children.insert(name.to_string(), file);
        return Ok(());
    }

    /// @brief 为该inode创建硬链接
    /// @param None
    /// @return 当前inode强引用
    #[inline]
    #[allow(dead_code)]
    pub fn link(&self) -> Arc<dyn IndexNode> {
        return self
            .0
            .lock()
            .self_ref
            .clone()
            .upgrade()
            .ok_or(SystemError::E2BIG)
            .unwrap();
    }

    pub fn remove(&self, name: &str) -> Result<(), SystemError> {
        let x = self
            .0
            .lock()
            .children
            .remove(name)
            .ok_or(SystemError::ENOENT)?;

        drop(x);
        return Ok(());
    }
}

/// @brief sys文件i节点(无锁)
#[derive(Debug)]
pub struct SysFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedSysFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedSysFSInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<dyn IndexNode>>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<SysFS>,
    /// INode 元数据
    metadata: Metadata,
}

impl SysFSInode {
    pub fn new(dev_type_: FileType, mode_: u32, data_: usize) -> Self {
        return Self::new_with_parent(Weak::default(), dev_type_, mode_, data_);
    }

    pub fn new_with_parent(
        parent: Weak<LockedSysFSInode>,
        dev_type_: FileType,
        mode_: u32,
        data_: usize,
    ) -> Self {
        return SysFSInode {
            parent: parent,
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: dev_type_, // 文件夹
                mode: mode_,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: data_,
            },
            fs: Weak::default(),
        };
    }
}
