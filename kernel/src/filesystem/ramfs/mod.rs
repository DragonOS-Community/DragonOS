use core::any::Any;
use core::intrinsics::unlikely;

use crate::filesystem::vfs::{FileSystemMakerData, FSMAKER};
use crate::libs::rwlock::RwLock;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{core::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::PosixTimeSpec,
};

use alloc::string::ToString;
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::vfs::{
    file::FilePrivateData, syscall::ModeType, utils::DName, FileSystem, FileSystemMaker, FsInfo,
    IndexNode, InodeId, Metadata, SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::{Magic, SuperBlock};

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;
const RAMFS_BLOCK_SIZE: u64 = 512;
/// @brief 内存文件系统的Inode结构体
#[derive(Debug)]
pub struct LockedRamFSInode(pub SpinLock<RamFSInode>);

/// @brief 内存文件系统结构体
#[derive(Debug)]
pub struct RamFS {
    /// RamFS的root inode
    root_inode: Arc<LockedRamFSInode>,
    super_block: RwLock<SuperBlock>,
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
    children: BTreeMap<DName, Arc<LockedRamFSInode>>,
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<RamFS>,
    /// 指向特殊节点
    special_node: Option<SpecialNodeData>,

    name: DName,
}

impl RamFSInode {
    pub fn new() -> Self {
        Self {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                file_type: FileType::Dir,
                mode: ModeType::from_bits_truncate(0o777),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
            },
            fs: Weak::default(),
            special_node: None,
            name: Default::default(),
        }
    }
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

    fn name(&self) -> &str {
        "ramfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::RAMFS_MAGIC,
            RAMFS_BLOCK_SIZE,
            RAMFS_MAX_NAMELEN as u64,
        );
        // 初始化root inode
        let root: Arc<LockedRamFSInode> =
            Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode::new())));

        let result: Arc<RamFS> = Arc::new(RamFS {
            root_inode: root,
            super_block: RwLock::new(super_block),
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<RamFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
    }

    pub fn make_ramfs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let fs = RamFS::new();
        return Ok(fs);
    }
}
#[distributed_slice(FSMAKER)]
static RAMFSMAKER: FileSystemMaker = FileSystemMaker::new(
    "ramfs",
    &(RamFS::make_ramfs
        as fn(
            Option<&dyn FileSystemMakerData>,
        ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);

impl IndexNode for LockedRamFSInode {
    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock();

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

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
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
        _data: SpinLockGuard<FilePrivateData>,
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
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let name = DName::from(name);
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 如果有重名的，则返回
        if inode.children.contains_key(&name) {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                file_type,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            },
            fs: inode.fs.clone(),
            special_node: None,
            name: name.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(name, result.clone());

        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedRamFSInode = other
            .downcast_ref::<LockedRamFSInode>()
            .ok_or(SystemError::EPERM)?;
        let name = DName::from(name);
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
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        inode
            .children
            .insert(name, other_locked.self_ref.upgrade().unwrap());

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

        let name = DName::from(name);
        // 获得要删除的文件的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }
        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);
        return Ok(());
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let name = DName::from(name);
        let mut inode: SpinLockGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 获得要删除的文件夹的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);
        return Ok(());
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        let inode_to_move = self
            .find(old_name)?
            .downcast_arc::<LockedRamFSInode>()
            .ok_or(SystemError::EINVAL)?;

        let new_name = DName::from(new_name);

        inode_to_move.0.lock().name = new_name.clone();

        let target_id = target.metadata()?.inode_id;

        let mut self_inode = self.0.lock();
        // 判断是否在同一目录下, 是则进行重命名
        if target_id == self_inode.metadata.inode_id {
            self_inode.children.remove(&DName::from(old_name));
            self_inode.children.insert(new_name, inode_to_move);
            return Ok(());
        }
        drop(self_inode);

        // 修改其对父节点的引用
        inode_to_move.0.lock().parent = Arc::downgrade(
            &target
                .clone()
                .downcast_arc::<LockedRamFSInode>()
                .ok_or(SystemError::EINVAL)?,
        );

        // 在新的目录下创建一个硬链接
        target.link(new_name.as_ref(), &(inode_to_move as Arc<dyn IndexNode>))?;

        // 取消现有的目录下的这个硬链接
        if let Err(e) = self.unlink(old_name) {
            // 当操作失败时回退操作
            target.unlink(new_name.as_ref())?;
            return Err(e);
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
                let name = DName::from(name);
                return Ok(inode
                    .children
                    .get(&name)
                    .ok_or(SystemError::ENOENT)?
                    .clone());
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: SpinLockGuard<RamFSInode> = self.0.lock();
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
                    .iter()
                    .filter_map(|(k, v)| {
                        if v.0.lock().metadata.inode_id.into() == ino {
                            Some(k.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Ramfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
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
        keys.append(
            &mut self
                .0
                .lock()
                .children
                .keys()
                .map(|k| k.to_string())
                .collect(),
        );

        return Ok(keys);
    }

    fn mknod(
        &self,
        filename: &str,
        mode: ModeType,
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 判断需要创建的类型
        if unlikely(mode.contains(ModeType::S_IFREG)) {
            // 普通文件
            return self.create(filename, FileType::File, mode);
        }

        let filename = DName::from(filename);

        let nod = Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                file_type: FileType::Pipe,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
            },
            fs: inode.fs.clone(),
            special_node: None,
            name: filename.clone(),
        })));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        if mode.contains(ModeType::S_IFIFO) {
            nod.0.lock().metadata.file_type = FileType::Pipe;
            // 创建pipe文件
            let pipe_inode = LockedPipeInode::new();
            // 设置special_node
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        } else if mode.contains(ModeType::S_IFBLK) {
            nod.0.lock().metadata.file_type = FileType::BlockDevice;
            unimplemented!()
        } else if mode.contains(ModeType::S_IFCHR) {
            nod.0.lock().metadata.file_type = FileType::CharDevice;
            unimplemented!()
        }

        inode.children.insert(filename, nod.clone());
        Ok(nod)
    }

    fn special_node(&self) -> Option<super::vfs::SpecialNodeData> {
        return self.0.lock().special_node.clone();
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().name.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0
            .lock()
            .parent
            .upgrade()
            .map(|item| item as Arc<dyn IndexNode>)
            .ok_or(SystemError::EINVAL)
    }
}
