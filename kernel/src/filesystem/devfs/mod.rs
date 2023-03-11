/// 导出devfs的模块
pub mod null_dev;
pub mod zero_dev;

use super::vfs::{
    core::{generate_inode_id, ROOT_INODE},
    FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, Metadata, PollStatus,
};
use crate::{
    include::bindings::bindings::{EEXIST, EISDIR, ENOENT, ENOTDIR, ENOTSUP},
    kdebug, kerror,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::TimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

const DEVFS_MAX_NAMELEN: usize = 64;

/// @brief dev文件系统
#[derive(Debug)]
pub struct DevFS {
    // 文件系统根节点
    root_inode: Arc<LockedDevFSInode>,
}

impl FileSystem for DevFS {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> super::vfs::FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: DEVFS_MAX_NAMELEN,
        };
    }
}

impl DevFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(SpinLock::new(
            // /dev 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            DevFSInode::new(FileType::Dir, 0x755 as u32, 0),
        )));

        let devfs: Arc<DevFS> = Arc::new(DevFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<DevFSInode> = devfs.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&devfs.root_inode);
        root_guard.self_ref = Arc::downgrade(&devfs.root_inode);
        root_guard.fs = Arc::downgrade(&devfs);
        // 释放锁
        drop(root_guard);

        // 创建文件夹
        let root = &devfs.root_inode;
        root.add_dir("char")
            .expect("DevFS: Failed to create /dev/char");
        kdebug!("char init done");
        root.add_dir("block")
            .expect("DevFS: Failed to create /dev/block");

        return devfs;
    }
}

/// @brief dev文件i节点(锁)
#[derive(Debug)]
pub struct LockedDevFSInode(SpinLock<DevFSInode>);

/// @brief dev文件i节点(无锁)
#[derive(Debug)]
pub struct DevFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedDevFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedDevFSInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<dyn IndexNode>>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

impl DevFSInode {
    pub fn new(dev_type_: FileType, mode_: u32, data_: usize) -> Self {
        return Self::new_with_parent(Weak::default(), dev_type_, mode_, data_);
    }

    pub fn new_with_parent(
        parent: Weak<LockedDevFSInode>,
        dev_type_: FileType,
        mode_: u32,
        data_: usize,
    ) -> Self {
        return DevFSInode {
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

impl LockedDevFSInode {
    pub fn add_dir(&self, name: &str) -> Result<(), i32> {
        match self.create(name, FileType::Dir, 0x755 as u32) {
            Ok(inode) => inode,
            Err(err) => {
                return Err(err);
            }
        };

        return Ok(());
    }

    pub fn add_dev(&self, name: &str, dev: Arc<dyn IndexNode>) -> Result<(), i32> {
        let mut this = self.0.lock();

        if this.children.contains_key(name) {
            return Err(-(EEXIST as i32));
        }

        this.children.insert(name.to_string(), dev);
        return Ok(());
    }

    pub fn remove(&self, name: &str) -> Result<(), i32> {
        self.0
            .lock()
            .children
            .remove(name)
            .ok_or(-(ENOENT as i32))?;
        return Ok(());
    }
}

impl IndexNode for LockedDevFSInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: u32,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 如果有重名的，则返回
        if inode.children.contains_key(_name) {
            return Err(-(EEXIST as i32));
        }

        // 创建inode
        let result: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(SpinLock::new(DevFSInode {
            parent: inode.self_ref.clone(),
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
            fs: inode.fs.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(String::from(_name), result.clone());

        return Ok(result);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, i32> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        match name {
            "" | "." => {
                return Ok(inode.self_ref.upgrade().ok_or(-(ENOENT as i32))?);
            }
            ".." => {
                return Ok(inode.parent.upgrade().ok_or(-(ENOENT as i32))?);
            }
            name => {
                // 在子目录项中查找
                return Ok(inode.children.get(name).ok_or(-(ENOENT as i32))?.clone());
            }
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn get_entry_name(&self, ino: super::vfs::InodeId) -> Result<String, i32> {
        let inode: SpinLockGuard<DevFSInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
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
                    0=>{return Err(-(ENOENT as i32));}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Devfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id}, to find={to_find}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                }
            }
        }
    }

    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, i32> {
        Err(-(ENOTSUP as i32))
    }

    fn list(&self) -> Result<Vec<String>, i32> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(&mut self.0.lock().children.keys().cloned().collect());

        return Ok(keys);
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), i32> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn poll(&self) -> Result<super::vfs::PollStatus, i32> {
        // 加锁
        let inode: SpinLockGuard<DevFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: &mut super::vfs::file::FilePrivateData,
    ) -> Result<usize, i32> {
        Err(-(ENOTSUP as i32))
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut super::vfs::file::FilePrivateData,
    ) -> Result<usize, i32> {
        Err(-(ENOTSUP as i32))
    }
}

/// @brief 所有的设备INode都需要额外实现这个trait
pub trait DeviceINode: IndexNode {
    fn set_fs(&self, fs: Weak<DevFS>);
}

/// @brief devfs的设备注册函数
pub fn devfs_register<T: DeviceINode>(name: &str, device: Arc<T>) -> Result<(), i32> {
    let devfs = ROOT_INODE().find("dev");
    if let Err(e) = devfs {
        kerror!("failed to register device name = {}, error = {}", name, e);
        return Err(-(ENOENT as i32));
    }
    let devfs = devfs.unwrap();

    match device.metadata().unwrap().file_type {
        // 字节设备挂载在 /dev/char
        FileType::CharDevice => {
            if let Err(_) = devfs.find("char") {
                devfs.create("char", FileType::Dir, 0x755)?;
            }

            let any_char_inode = devfs.find("char")?;
            let dev_char_inode: &LockedDevFSInode = any_char_inode
                .as_any_ref()
                .downcast_ref::<LockedDevFSInode>()
                .unwrap();

            device.set_fs(dev_char_inode.0.lock().fs.clone());
            dev_char_inode.add_dev(name, device)?;
        }
        FileType::BlockDevice => {
            if let Err(_) = devfs.find("block") {
                devfs.create("block", FileType::Dir, 0x755)?;
            }

            let any_block_inode = devfs.find("block")?;
            let dev_block_inode: &LockedDevFSInode = any_block_inode
                .as_any_ref()
                .downcast_ref::<LockedDevFSInode>()
                .unwrap();

            device.set_fs(dev_block_inode.0.lock().fs.clone());
            dev_block_inode.add_dev(name, device)?;
        }
        _ => {
            return Err(-(ENOTSUP as i32));
        }
    }

    return Ok(());
}

/// @brief devfs的设备卸载函数
pub fn devfs_unregister<T: DeviceINode>(name: &str, device: Arc<T>) -> Result<(), i32> {
    let devfs = ROOT_INODE().find("dev").unwrap();

    match device.metadata().unwrap().file_type {
        // 字节设备挂载在 /dev/char
        FileType::CharDevice => {
            if let Err(_) = devfs.find("char") {
                return Err(-(ENOENT as i32));
            }

            let any_char_inode = devfs.find("char")?;
            let dev_char_inode = any_char_inode
                .as_any_ref()
                .downcast_ref::<LockedDevFSInode>()
                .unwrap();

            dev_char_inode.remove(name)?;
        }
        FileType::BlockDevice => {
            if let Err(_) = devfs.find("block") {
                return Err(-(ENOENT as i32));
            }

            let any_block_inode = devfs.find("block")?;
            let dev_block_inode = any_block_inode
                .as_any_ref()
                .downcast_ref::<LockedDevFSInode>()
                .unwrap();

            dev_block_inode.remove(name)?;
        }
        _ => {
            return Err(-(ENOTSUP as i32));
        }
    }

    return Ok(());
}

/// @brief 注册系统内部自带的设备
pub fn register_bultinin_device() {
    use null_dev::LockedNullInode;
    use zero_dev::LockedZeroInode;
    devfs_register::<LockedNullInode>("null", LockedNullInode::new()).unwrap();
    devfs_register::<LockedZeroInode>("zero", LockedZeroInode::new()).unwrap();
}

pub fn __test_dev() {
    let dev = ROOT_INODE().find("dev").unwrap();
    kdebug!("ls /dev = {:?}", dev.list().unwrap());
    let block = dev.find("block").unwrap();
    kdebug!("ls /dev/block = {:?}", block.list().unwrap());
    let char = dev.find("char").unwrap();
    kdebug!("ls /dev/char = {:?}", char.list().unwrap());

    // __test_keyboard();
}

pub fn __test_keyboard() {
    let mut buf = [0 as u8; 100 as usize];
    let dev = ROOT_INODE()
        .find("dev")
        .unwrap()
        .find("char")
        .unwrap()
        .find("ps2_keyboard")
        .unwrap();
    while let Ok(c) = dev.read_at(0, 1, &mut buf, &mut FilePrivateData::Unused) {
        kdebug!("print = {} | {}", c, buf[0]);
    }
}
