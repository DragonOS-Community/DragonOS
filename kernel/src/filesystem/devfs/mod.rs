/// 导出devfs的模块
pub mod null_dev;
pub mod zero_dev;

use super::vfs::{
    core::generate_inode_id, FileSystem, FileType, FsInfo, IndexNode, Metadata, PollStatus,
};
use crate::{
    include::bindings::bindings::{EEXIST, EISDIR, ENOENT, ENOTDIR, ENOTSUP},
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::TimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

const DevFS_MAX_NAMELEN: usize = 64;

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
            max_name_len: DevFS_MAX_NAMELEN,
        };
    }
}

impl DevFS {
    fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(SpinLock::new(
            // /dev 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            DevFSInode::new(FileType::Dir, 0x755 as u32, 0),
        )));

        let result: Arc<DevFS> = Arc::new(DevFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<DevFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
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
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: dev_type_, // 文件夹，block设备，char设备
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
    pub fn add_dir(&self, name: String) -> Result<(), i32> {
        let mut this = self.0.lock();

        if this.children.contains_key(&name) {
            return Err(-(EEXIST as i32));
        }

        let inode = match self.create(name.clone().as_str(), FileType::Dir, 0x755 as u32) {
            Ok(inode) => inode,
            Err(err) => {
                return Err(err);
            }
        };

        this.children.insert(name, inode);

        return Ok(());
    }

    pub fn add_dev(&self, name: String, dev: Arc<dyn IndexNode>) -> Result<(), i32> {
        let mut this = self.0.lock();

        if this.children.contains_key(&name) {
            return Err(-(EEXIST as i32));
        }

        this.children.insert(name, dev);
        return Ok(());
    }

    pub fn remove(&self, name: String) -> Result<(), i32> {
        self.0
            .lock()
            .children
            .remove(&name)
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

    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), i32> {
        let mut inode = self.0.lock();
        inode.metadata.atime = _metadata.atime;
        inode.metadata.mtime = _metadata.mtime;
        inode.metadata.ctime = _metadata.ctime;
        inode.metadata.mode = _metadata.mode;
        inode.metadata.uid = _metadata.uid;
        inode.metadata.gid = _metadata.gid;

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
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut super::vfs::file::FilePrivateData,
    ) -> Result<usize, i32> {

        Err(-(ENOTSUP as i32))
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut super::vfs::file::FilePrivateData,
    ) -> Result<usize, i32> {
        Err(-(ENOTSUP as i32))
    }
}
