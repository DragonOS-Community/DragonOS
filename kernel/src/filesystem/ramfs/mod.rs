use core::any::Any;
use core::intrinsics::unlikely;

use alloc::collections::LinkedList;
use alloc::{
    // collections::{BTreeMap, BTreeSet},
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{core::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::TimeSpec,
};

use super::vfs::{
    file::FilePrivateData, syscall::ModeType, FileSystem, FsInfo, IndexNode, InodeId, Metadata,
    SpecialNodeData,
    cache::DCache,
};

use core::hash::{Hash,Hasher};

// use alloc::collections::BinaryHeap;

#[derive(Debug)]
pub struct DEntry {
    name: String,
    parent: Weak<LockedDEntry>,
    self_ref: Weak<LockedDEntry>,
    children: LinkedList<Arc<LockedDEntry>>,
    inode: Arc<LockedInode>,
    /// 指向特殊节点
    special_node: Option<SpecialNodeData>,
    fs: Weak<RamFS>,
}

#[derive(Debug)]
pub struct INode {
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<RamFS>,
}

/// @brief 内存文件系统的Inode结构体
#[derive(Debug)]
struct LockedInode(SpinLock<INode>);

#[derive(Debug)]
struct LockedDEntry(SpinLock<DEntry>);

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;

/// @brief 内存文件系统结构体
/// act as superblock
#[derive(Debug)]
pub struct RamFS {
    /// RamFS的root
    root: Arc<LockedDEntry>,
    // Dentry cache
    cache: DCache<LockedDEntry>,
}

impl FileSystem for RamFS {
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root.clone();
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
        let root: Arc<LockedDEntry> = Arc::new(LockedDEntry(SpinLock::new( DEntry {
            name: String::new(),
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: LinkedList::new(),
            inode: Arc::new(LockedInode(SpinLock::new( INode { 
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
                    mode: ModeType::from_bits_truncate(0o777),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                fs: Weak::default(),
            }))),
            special_node: None,
            fs: Weak::default(),
        })));

        let result: Arc<RamFS> = Arc::new(RamFS { 
            root,
            cache: DCache::new(),
        });

        {
            // 对root inode加锁，并继续完成初始化工作
            let mut root_guard = result.root.0.lock();
            root_guard.parent = Arc::downgrade(&result.root);
            root_guard.self_ref = Arc::downgrade(&result.root);
            root_guard.fs = Arc::downgrade(&result);
            root_guard.inode.0.lock().fs = Arc::downgrade(&result);
        }
        // auto drop root_guard

        result
    }

    // fn cache(&self) -> Option<DCache>{
    //     Some(self.cache)
    // }
}

impl DEntry {
    fn get(&self, name: &str) -> Option<Arc<LockedDEntry>> {
        self.children.iter().find(|entry| {
            entry.0.lock().name == name
        }).cloned()
    }
}

impl Hash for DEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl IndexNode for LockedDEntry {
    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock().inode.0.lock();

        //如果是文件夹，则报错
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }

        //当前文件长度大于_len才进行截断，否则不操作
        if inode.data.len() > len {
            inode.data.resize(len, 0);
        }
        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn open(
        &self,
        _data: &mut FilePrivateData,
        _mode: &super::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

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
        let inode = self.0.lock().inode.0.lock();

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
        let mut inode = self.0.lock().inode.0.lock();

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

        Ok(len)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let inode = self.0.lock().inode.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        Ok(metadata)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock().inode.0.lock();

        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock().inode.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 获取当前inode
        let mut dentry = self.0.lock();
        let mut inode = dentry.inode.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 重名则返回
        if dentry.get(name).is_some() {
            return Err(SystemError::EEXIST);
        }

        // 创建inode
        let result: Arc<LockedDEntry> = Arc::new(LockedDEntry(SpinLock::new(DEntry {
            name: String::new(),
            parent: dentry.self_ref.clone(),
            self_ref: Weak::default(),
            children: LinkedList::new(),
            inode: Arc::new(LockedInode(SpinLock::new( INode { 
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
                    mode: ModeType::from_bits_truncate(0o777),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                fs: dentry.fs.clone(),
            }))),
            special_node: None,
            fs: dentry.fs.clone(),
        })));

        // 初始化inode的自引用的weak指针    
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        dentry.children.push_back(result.clone());

        Ok(result)
    }  

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedDEntry = other
            .downcast_ref::<LockedDEntry>()
            .ok_or(SystemError::EPERM)?;
        let mut dentry: SpinLockGuard<DEntry> = self.0.lock();
        let other_dentry: SpinLockGuard<DEntry> = other.0.lock();

        let inode: SpinLockGuard<INode> = dentry.inode.0.lock();
        let mut other_locked: SpinLockGuard<INode> = other_dentry.inode.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if dentry.get(name).is_some() {
            return Err(SystemError::EEXIST);
        }

        // // 如果当前文件夹下硬连接重复，报错。
        // if self.get_entry_name(other.0.lock().metadata.inode_id).is_ok() {
        //     return Err(SystemError::EEXIST);
        // }

        dentry.children
            .push_back(other_dentry.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut dentry: SpinLockGuard<DEntry> = self.0.lock();
        let mut inode: SpinLockGuard<INode> = dentry.inode.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let mut cur = dentry.children.cursor_front_mut();
        loop {
            if let Some(x) = cur.peek_next() {
                if x.0.lock().name == name {
                    if FileType::Dir ==
                        x.0.lock().inode.0.lock().metadata.file_type {
                        return Err(SystemError::EPERM);
                    }
                    cur.move_next();
                    cur.remove_current();
                    return Ok(());
                }
                cur.move_next();
            } else {
                return Err(SystemError::ENOENT);
            }
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut dentry: SpinLockGuard<DEntry> = self.0.lock();
        let mut inode: SpinLockGuard<INode> = dentry.inode.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 获得要删除的文件夹的inode
        let to_delete = dentry.get(name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().inode.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        to_delete.0.lock().inode.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        let mut cur = dentry.children.cursor_front_mut();
        loop {
            if let Some(x) = cur.peek_next() {
                if x.0.lock().name == name {
                    // 当前文件夹非空：
                    if !x.0.lock().children.is_empty() {
                        return Err(SystemError::EPERM);
                    }
                    cur.move_next();
                    cur.remove_current();
                    return Ok(());
                }
                cur.move_next();
            } else {
                return Err(SystemError::ENOENT);
            }
        }
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

        if inode.inode.0.lock().metadata.file_type != FileType::Dir {
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
                return Ok(inode.get(name).ok_or(SystemError::ENOENT)?.clone());
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: SpinLockGuard<DEntry> = self.0.lock();
        if inode.inode.0.lock().metadata.file_type != FileType::Dir {
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
                let mut key: Vec<String> = inode
                    .children.iter().filter(|entry| {
                        entry.0.lock().inode.0.lock().metadata.inode_id.into() == ino
                    })
                    .map(|entry| entry.0.lock().name.clone())
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    // shouldn't panic but return Vec<String>
                    // or just return String.concat together
                    _ => panic!("Ramfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.inode.0.lock().metadata.inode_id, to_find=ino)
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
        // keys.append(&mut self.0.lock().children.keys().cloned().collect());
        keys.append(&mut
            self.0.lock().children.iter()
                .map(|entry| entry.0.lock().name.clone())
                .collect()
        );

        return Ok(keys);
    }

    fn mknod(
        &self,
        filename: &str,
        mode: ModeType,
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut dentry = self.0.lock();
        let mut inode = dentry.inode.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 判断需要创建的类型
        if unlikely(mode.contains(ModeType::S_IFREG)) {
            // 普通文件
            return Ok(self.create(filename, FileType::File, mode)?);
        }

        let nod = Arc::new(LockedDEntry(SpinLock::new(DEntry {
            name: String::new(),
            parent: dentry.self_ref.clone(),
            self_ref: Weak::default(),
            children: LinkedList::new(),
            inode: Arc::new(LockedInode(SpinLock::new( INode { 
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
                    mode: ModeType::from_bits_truncate(0o777),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                fs: dentry.fs.clone(),
            }))),
            fs: dentry.fs.clone(),
            special_node: None,
        })));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        if mode.contains(ModeType::S_IFIFO) {
            nod.0.lock().inode.0.lock().metadata.file_type = FileType::Pipe;
            // 创建pipe文件
            let pipe_inode = LockedPipeInode::new();
            // 设置special_node
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        } else if mode.contains(ModeType::S_IFBLK) {
            nod.0.lock().inode.0.lock().metadata.file_type = FileType::BlockDevice;
            unimplemented!()
        } else if mode.contains(ModeType::S_IFCHR) {
            nod.0.lock().inode.0.lock().metadata.file_type = FileType::CharDevice;
            unimplemented!()
        }

        dentry
            .children
            .push_back(nod.clone());
        Ok(nod)
    }

    fn special_node(&self) -> Option<super::vfs::SpecialNodeData> {
        return self.0.lock().special_node.clone();
    }
}
