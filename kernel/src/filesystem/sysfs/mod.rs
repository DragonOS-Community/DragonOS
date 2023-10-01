use core::fmt::Debug;

use self::{dir::SysKernDirPriv, file::SysKernFilePriv};

use super::{
    kernfs::{callback::KernInodePrivateData, KernFS, KernFSInode, KernInodeType},
    vfs::{
        core::generate_inode_id, file::FileMode, syscall::ModeType, FileSystem, FileType, FsInfo,
        IndexNode, Metadata, PollStatus,
    },
};
use crate::{
    driver::base::{kobject::KObject, platform::platform_bus_init},
    filesystem::{sysfs::bus::sys_bus_init, vfs::ROOT_INODE},
    kdebug, kinfo, kwarn,
    libs::{
        casting::DowncastArc,
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
    },
    syscall::SystemError,
    time::TimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

pub mod bus;
pub mod class;
pub mod devices;
mod dir;
mod file;
pub mod fs;
mod group;

const SYSFS_MAX_NAMELEN: usize = 64;

static mut __SYS_DEVICES_INODE: Option<Arc<dyn IndexNode>> = None;
static mut __SYS_BUS_INODE: Option<Arc<dyn IndexNode>> = None;
static mut __SYS_CLASS_INODE: Option<Arc<dyn IndexNode>> = None;
static mut __SYS_FS_INODE: Option<Arc<dyn IndexNode>> = None;

/// 全局的sysfs实例
pub(self) static mut SYSFS_INSTANCE: Option<SysFS> = None;

#[inline(always)]
pub fn sysfs_instance() -> &'static SysFS {
    unsafe {
        return &SYSFS_INSTANCE.as_ref().unwrap();
    }
}

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
pub struct OldSysFS {
    // 文件系统根节点
    root_inode: Arc<LockedSysFSInode>,
}

impl FileSystem for OldSysFS {
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

impl OldSysFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedSysFSInode> = Arc::new(LockedSysFSInode(SpinLock::new(
            // /sys 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            SysFSInode::new(FileType::Dir, ModeType::from_bits_truncate(0o755), 0),
        )));

        let sysfs: Arc<OldSysFS> = Arc::new(OldSysFS { root_inode: root });

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
                __SYS_DEVICES_INODE = Some(devices);
            },
            Err(_) => panic!("SysFS: Failed to create /sys/devices"),
        }

        match root.add_dir("bus") {
            Ok(bus) => unsafe {
                __SYS_BUS_INODE = Some(bus);
            },
            Err(_) => panic!("SysFS: Failed to create /sys/bus"),
        }

        match root.add_dir("class") {
            Ok(class) => unsafe {
                __SYS_CLASS_INODE = Some(class);
            },
            Err(_) => panic!("SysFS: Failed to create /sys/class"),
        }

        match root.add_dir("fs") {
            Ok(fs) => unsafe {
                __SYS_FS_INODE = Some(fs);
            },
            Err(_) => panic!("SysFS: Failed to create /sys/fs"),
        }

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

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
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

        match ino.into() {
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
                    .filter(|k| {
                        inode
                            .children
                            .get(*k)
                            .unwrap()
                            .metadata()
                            .unwrap()
                            .inode_id
                            .into()
                            == ino
                    })
                    .cloned()
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Sysfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
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
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果有重名的，则返回
        if guard.children.contains_key(name) {
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
                file_type,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: data,
            },
            fs: guard.fs.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        guard.children.insert(String::from(name), result.clone());
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

        match self.do_create_with_data(
            guard,
            name,
            FileType::Dir,
            ModeType::from_bits_truncate(0o755),
            0,
        ) {
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
    fs: Weak<OldSysFS>,
    /// INode 元数据
    metadata: Metadata,
}

impl SysFSInode {
    pub fn new(file_type: FileType, mode: ModeType, data_: usize) -> Self {
        return Self::new_with_parent(Weak::default(), file_type, mode, data_);
    }

    pub fn new_with_parent(
        parent: Weak<LockedSysFSInode>,
        file_type: FileType,
        mode: ModeType,
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
                file_type,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: data_,
            },
            fs: Weak::default(),
        };
    }
}

pub fn sysfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        kinfo!("Initializing SysFS...");

        // 创建 sysfs 实例
        // let sysfs: Arc<OldSysFS> = OldSysFS::new();
        let sysfs = SysFS::new();
        unsafe { SYSFS_INSTANCE = Some(sysfs) };

        // sysfs 挂载
        let _t = ROOT_INODE()
            .find("sys")
            .expect("Cannot find /sys")
            .mount(sysfs_instance().fs().clone())
            .expect("Failed to mount sysfs");
        kinfo!("SysFS mounted.");

        // // 初始化platform总线
        // platform_bus_init().expect("platform bus init failed");

        // sys_bus_init(&SYS_BUS_INODE()).unwrap_or_else(|err| {
        //     panic!("sys_bus_init failed: {:?}", err);
        // });

        // kdebug!("sys_bus_init result: {:?}", SYS_BUS_INODE().list());
        result = Some(Ok(()));
    });

    return result.unwrap();
}

/// SysFS在KernFS的inode中的私有信息
#[allow(dead_code)]
#[derive(Debug)]
pub enum SysFSKernPrivateData {
    Dir(SysKernDirPriv),
    File(SysKernFilePriv),
}

/// sysfs文件目录的属性组
pub trait AttributeGroup: Debug + Send + Sync {
    /// 属性组的名称
    ///
    /// 如果属性组的名称为None，则所有的属性都会被添加到父目录下，而不是创建一个新的目录
    fn name(&self) -> Option<&str>;
    /// 属性组的属性列表
    fn attrs(&self) -> &[&'static dyn Attribute];

    /// 属性在当前属性组内的权限（该方法可选）
    ///
    /// 如果返回None，则使用Attribute的mode()方法返回的权限
    ///
    /// 如果返回Some，则使用返回的权限。
    /// 如果要标识属性不可见，则返回Some(ModeType::empty())
    fn is_visible(&self, kobj: Arc<dyn KObject>, attr: &dyn Attribute) -> Option<ModeType>;
}

/// sysfs文件的属性
pub trait Attribute: Debug + Send + Sync {
    fn name(&self) -> &str;
    fn mode(&self) -> ModeType;
    
    fn support(&self) -> SysFSOpsSupport;
    
    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

pub trait SysFSOps {
    /// 获取当前文件的支持的操作
    fn support(&self, attr:&dyn Attribute) -> SysFSOpsSupport;

    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError>;

    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError>;
}

bitflags! {
    pub struct SysFSOpsSupport: u8{
        const SHOW = 1 << 0;
        const STORE = 1 << 1;
    }
}

#[derive(Debug)]
pub struct SysFS {
    root_inode: Arc<KernFSInode>,
    kernfs: Arc<KernFS>,
}

impl SysFS {
    pub fn new() -> Self {
        let kernfs: Arc<KernFS> = KernFS::new();

        let root_inode: Arc<KernFSInode> = kernfs.root_inode().downcast_arc().unwrap();

        let sysfs = SysFS { root_inode, kernfs };

        return sysfs;
    }

    pub fn root_inode(&self) -> &Arc<KernFSInode> {
        return &self.root_inode;
    }

    pub fn fs(&self) -> &Arc<KernFS> {
        return &self.kernfs;
    }

    /// 警告：重复的sysfs entry
    pub(self) fn warn_duplicate(&self, parent: &Arc<KernFSInode>, name: &str) {
        let path = self.kernfs_path(parent);
        kwarn!("duplicate sysfs entry: {path}/{name}");
    }
}
