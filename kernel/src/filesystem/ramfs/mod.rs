use core::any::Any;

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    filesystem::vfs::{core::generate_inode_id, FileType},
    libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
    time::TimeSpec,
};

use super::vfs::{
    file::FilePrivateData, FileSystem, FsInfo, IndexNode, InodeId, Metadata, PollStatus,
};

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;

/// @brief 内存文件系统的Inode结构体
#[derive(Debug)]
struct LockedRamFSInode(SpinLock<RamFSInode>);

/// @brief 内存文件系统结构体
#[derive(Debug)]
pub struct RamFS {
    /// RamFS的root inode
    root_inode: Arc<LockedRamFSInode>,
}

/// @brief 内存文件系统的Inode结构体(不包含锁)
#[derive(Debug)]
pub struct RamFSInode {
    // parent变量目前只在find函数中使用到
    // 所以只有当inode是文件夹的时候，parent才会生效
    // 对于文件来说，parent就没什么作用了
    // 关于parent的说明: 目录不允许有硬链接
    /// 指向父Inode的弱引用
    parent: Weak<LockedRamFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedRamFSInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<LockedRamFSInode>>,
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<RamFS>,
}

impl FileSystem for RamFS {
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: RAMFS_MAX_NAMELEN,
        };
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedRamFSInode> = Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode {
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            data: Vec::new(),
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::Dir,
                mode: 0o777,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0,
            },
            fs: Weak::default(),
        })));

        let result: Arc<RamFS> = Arc::new(RamFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<RamFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
    }
}

impl IndexNode for LockedRamFSInode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let inode: SpinLockGuard<RamFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        let start = inode.data.len().min(offset);
        let end = inode.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &inode.data[start..end];
        buf[0..src.len()].copy_from_slice(src);
        return Ok(src.len());
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        // 加锁
        let mut inode: SpinLockGuard<RamFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        let data: &mut Vec<u8> = &mut inode.data;

        // 如果文件大小比原来的大，那就resize这个数组
        if offset + len > data.len() {
            data.resize(offset + len, 0);
        }

        let target = &mut data[offset..offset + len];
        target.copy_from_slice(&buf[0..len]);
        return Ok(len);
    }

    fn poll(&self) -> Result<PollStatus, SystemError> {
        // 加锁
        let inode: SpinLockGuard<RamFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        return Ok(PollStatus::READ | PollStatus::WRITE);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let inode = self.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 如果有重名的，则返回
        if inode.children.contains_key(name) {
            return Err(SystemError::EEXIST);
        }

        // 创建inode
        let result: Arc<LockedRamFSInode> = Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            data: Vec::new(),
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: file_type,
                mode: mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: data,
            },
            fs: inode.fs.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(String::from(name), result.clone());

        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedRamFSInode = other
            .downcast_ref::<LockedRamFSInode>()
            .ok_or(SystemError::EPERM)?;
        let mut inode: SpinLockGuard<RamFSInode> = self.0.lock();
        let mut other_locked: SpinLockGuard<RamFSInode> = other.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if inode.children.contains_key(name) {
            return Err(SystemError::EEXIST);
        }

        inode
            .children
            .insert(String::from(name), other_locked.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut inode: SpinLockGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        // 获得要删除的文件的inode
        let to_delete = inode.children.get(name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }
        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(name);
        return Ok(());
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut inode: SpinLockGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 获得要删除的文件夹的inode
        let to_delete = inode.children.get(name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(name);
        return Ok(());
    }

    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        let old_inode: Arc<dyn IndexNode> = self.find(old_name)?;

        // 在新的目录下创建一个硬链接
        target.link(new_name, &old_inode)?;
        // 取消现有的目录下的这个硬链接
        if let Err(err) = self.unlink(old_name) {
            // 如果取消失败，那就取消新的目录下的硬链接
            target.unlink(new_name)?;
            return Err(err);
        }
        return Ok(());
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
                return Ok(inode.children.get(name).ok_or(SystemError::ENOENT)?.clone());
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: SpinLockGuard<RamFSInode> = self.0.lock();
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
                    .filter(|k| inode.children.get(*k).unwrap().0.lock().metadata.inode_id == ino)
                    .cloned()
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Ramfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id}, to find={to_find}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                }
            }
        }
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
