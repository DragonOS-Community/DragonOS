mod utils;

use system_error::SystemError;

use core::{any::Any, intrinsics::unlikely};

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    driver::base::device::device_number::DeviceNumber,
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    time::PosixTimeSpec,
};

use super::vfs::{
    core::generate_inode_id,
    dcache::{DCache, DefaultDCache},
    file::FilePrivateData,
    syscall::ModeType,
    FileSystem, FileSystemMaker, FileType, FsInfo, IndexNode, Magic, Metadata, SpecialNodeData,
    SuperBlock, FSMAKER,
};

use self::utils::Keyer;

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;
const RAMFS_BLOCK_SIZE: u64 = 512;
#[derive(Debug)]
pub struct RamFS {
    root: Arc<LockedRamfsEntry>,
    cache: Arc<DefaultDCache>,
    super_block: RwLock<SuperBlock>,
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        let root = Arc::new(LockedRamfsEntry(SpinLock::new(RamfsEntry {
            name: String::new(),
            inode: Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode::new(
                FileType::Dir,
                ModeType::from_bits_truncate(0o777),
            )))),
            parent: Weak::new(),
            self_ref: Weak::new(),
            children: BTreeMap::new(),
            fs: Weak::new(),
            special_node: None,
        })));
        let ret = Arc::new(RamFS {
            root,
            cache: Arc::new(DefaultDCache::new(None)),
            super_block: RwLock::new(SuperBlock::new(
                Magic::RAMFS_MAGIC,
                RAMFS_BLOCK_SIZE,
                RAMFS_MAX_NAMELEN as u64,
            )),
        });
        {
            let mut entry = ret.root.0.lock();
            entry.parent = Arc::downgrade(&ret.root);
            entry.self_ref = Arc::downgrade(&ret.root);
            entry.fs = Arc::downgrade(&ret);
        }
        ret
    }

    pub fn make_ramfs() -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let fs = RamFS::new();
        return Ok(fs);
    }
}

#[distributed_slice(FSMAKER)]
static RAMFSMAKER: FileSystemMaker = FileSystemMaker::new(
    "ramfs",
    &(RamFS::make_ramfs as fn() -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);

impl FileSystem for RamFS {
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: RAMFS_MAX_NAMELEN,
        }
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "ramfs"
    }

    fn dcache(&self) -> Result<Arc<DefaultDCache>, SystemError> {
        Ok(self.cache.clone())
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }
}

#[derive(Debug)]
pub struct RamFSInode {
    /// 元数据
    metadata: Metadata,
    /// 数据块
    data: Vec<u8>,
}

#[derive(Debug)]
pub struct LockedRamFSInode(SpinLock<RamFSInode>);

#[derive(Debug)]
pub struct RamfsEntry {
    /// 目录名
    name: String,
    /// 文件节点
    inode: Arc<LockedRamFSInode>,
    /// 父目录
    parent: Weak<LockedRamfsEntry>,
    /// 自引用
    self_ref: Weak<LockedRamfsEntry>,
    /// 子目录
    children: BTreeMap<Keyer, Arc<LockedRamfsEntry>>,
    /// 目录所属文件系统
    fs: Weak<RamFS>,

    special_node: Option<SpecialNodeData>,
}

#[derive(Debug)]
pub struct LockedRamfsEntry(SpinLock<RamfsEntry>);

impl RamFSInode {
    pub fn new(file_type: FileType, mode: ModeType) -> RamFSInode {
        RamFSInode {
            data: Vec::new(),
            metadata: Metadata::new(file_type, mode),
        }
    }

    pub fn new_with_data(file_type: FileType, mode: ModeType, data: usize) -> RamFSInode {
        RamFSInode {
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
        }
    }
}

impl IndexNode for LockedRamfsEntry {
    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();

        //如果是文件夹，则报错
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }

        //当前文件长度大于_len才进行截断，否则不操作
        if inode.data.len() > len {
            inode.data.resize(len, 0);
        }

        Ok(())
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
        let entry = self.0.lock();

        let inode = entry.inode.0.lock();

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
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();

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
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let entry = self.0.lock();
        let inode = entry.inode.0.lock();
        let mut metadata = inode.metadata.clone();

        metadata.size = inode.data.len() as i64;

        drop(inode);
        drop(entry);

        return Ok(metadata);
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        }
        return Err(SystemError::EINVAL);
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 获取当前inode
        let mut entry = self.0.lock();
        {
            let inode = entry.inode.0.lock();
            // 如果当前inode不是文件夹，则返回
            if inode.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
        }

        // 如果有重名的，则返回
        if entry.children.contains_key(&Keyer::from_str(name)) {
            return Err(SystemError::EEXIST);
        }

        // 创建Entry-inode
        let result = Arc::new(LockedRamfsEntry(SpinLock::new(RamfsEntry {
            parent: entry.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            inode: Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode::new_with_data(
                file_type, mode, data,
            )))),
            fs: entry.fs.clone(),
            special_node: None,
            name: String::from(name),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        entry
            .children
            .insert(Keyer::from_entry(&result), result.clone());
        return Ok(result);
    }

    /// Not Stable, waiting for improvement
    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedRamfsEntry = other
            .downcast_ref::<LockedRamfsEntry>()
            .ok_or(SystemError::EPERM)?;

        let mut entry = self.0.lock();
        let other_entry = other.0.lock();
        let mut other_inode = other_entry.inode.0.lock();
        {
            let inode = entry.inode.0.lock();

            // 如果当前inode不是文件夹，那么报错
            if inode.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
        }
        // 如果另一个inode是文件夹，那么也报错
        if other_inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if entry.children.contains_key(&Keyer::from_str(name)) {
            return Err(SystemError::EEXIST);
        }

        // 创建新Entry指向other inode
        // 并插入子目录序列
        let to_insert = Arc::new(LockedRamfsEntry(SpinLock::new(RamfsEntry {
            name: String::from(name),
            inode: other_entry.inode.clone(),
            parent: entry.self_ref.clone(),
            self_ref: Weak::new(),
            children: BTreeMap::new(), // File should not have children
            fs: other_entry.fs.clone(),
            special_node: other_entry.special_node.clone(),
        })));
        entry
            .children
            .insert(Keyer::from_entry(&to_insert), to_insert);

        // 增加硬链接计数
        other_inode.metadata.nlinks += 1;

        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut entry = self.0.lock();
        {
            let inode = entry.inode.0.lock();
            // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
            if inode.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }
        {
            // 获得要删除的文件的inode
            let to_del_entry = entry
                .children
                .get(&Keyer::from_str(name))
                .ok_or(SystemError::ENOENT)?
                .0
                .lock();
            let mut to_del_node = to_del_entry.inode.0.lock();

            if to_del_node.metadata.file_type == FileType::Dir {
                return Err(SystemError::EPERM);
            }
            // 减少硬链接计数
            to_del_node.metadata.nlinks -= 1;
        }
        // 在当前目录中删除这个子目录项
        entry.children.remove(&Keyer::from_str(name));

        return Ok(());
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut entry = self.0.lock();
        {
            let inode = entry.inode.0.lock();

            // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
            if inode.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
        }
        // Gain keyer
        let keyer = Keyer::from_str(name);
        {
            // 获得要删除的文件夹的inode
            let to_del_ent = entry
                .children
                .get(&keyer)
                .ok_or(SystemError::ENOENT)?
                .0
                .lock();
            let mut to_del_nod = to_del_ent.inode.0.lock();
            if to_del_nod.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }

            to_del_nod.metadata.nlinks -= 1;
        }
        // 在当前目录中删除这个子目录项
        entry.children.remove(&keyer);
        return Ok(());
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        let inode: Arc<dyn IndexNode> = self.find(old_name)?;
        // 修改其对父节点的引用
        inode
            .downcast_ref::<LockedRamfsEntry>()
            .ok_or(SystemError::EPERM)?
            .0
            .lock()
            .parent = Arc::downgrade(
            &target
                .clone()
                .downcast_arc::<LockedRamfsEntry>()
                .ok_or(SystemError::EPERM)?,
        );

        // 在新的目录下创建一个硬链接
        target.link(new_name, &inode)?;

        // 取消现有的目录下的这个硬链接
        if let Err(e) = self.unlink(old_name) {
            // 当操作失败时回退操作
            target.unlink(new_name)?;
            return Err(e);
        }

        return Ok(());
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let entry = self.0.lock();
        let inode = entry.inode.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => Ok(entry.self_ref.upgrade().ok_or(SystemError::ENOENT)?),

            ".." => Ok(entry.parent.upgrade().ok_or(SystemError::ENOENT)?),
            name => {
                // 在子目录项中查找
                Ok(entry
                    .children
                    .get(&Keyer::from_str(name))
                    .ok_or(SystemError::ENOENT)?
                    .clone())
            }
        }
    }

    /// Potential panic
    fn get_entry_name(&self, ino: crate::filesystem::vfs::InodeId) -> Result<String, SystemError> {
        let entry = self.0.lock();
        let inode = entry.inode.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match ino.into() {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            ino => {
                let mut key: Vec<String> = entry
                    .children
                    .iter()
                    .filter_map(|(_, value)| {
                        if value.0.lock().inode.0.lock().metadata.inode_id.into() == ino {
                            return Some(value.0.lock().name.clone());
                        }
                        None
                    })
                    .collect();

                match key.len() {
                    0 => Err(SystemError::ENOENT),
                    1 => Ok(key.remove(0)),
                    _ => panic!("Ramfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                }
            }
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        // kinfo!("Call Ramfs::list");
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
                .map(|k| k.get().unwrap_or(String::from("[unknown_filename]")))
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
        let mut entry = self.0.lock();
        {
            let inode = entry.inode.0.lock();
            if inode.metadata.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
        }
        // 判断需要创建的类型
        if unlikely(mode.contains(ModeType::S_IFREG)) {
            // 普通文件
            return self.create(filename, FileType::File, mode);
        }

        let nod = Arc::new(LockedRamfsEntry(SpinLock::new(RamfsEntry {
            parent: entry.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            inode: Arc::new(LockedRamFSInode(SpinLock::new(RamFSInode::new(
                FileType::Pipe,
                mode,
            )))),
            fs: entry.fs.clone(),
            special_node: None,
            name: String::from(filename),
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
            unimplemented!() // Todo
        } else if mode.contains(ModeType::S_IFCHR) {
            nod.0.lock().inode.0.lock().metadata.file_type = FileType::CharDevice;
            unimplemented!() // Todo
        }

        entry.children.insert(
            Keyer::from_str(String::from(filename).to_uppercase().as_str()),
            nod.clone(),
        );
        return Ok(nod);
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        self.0.lock().special_node.clone()
    }

    fn key(&self) -> Result<String, SystemError> {
        Ok(self.0.lock().name.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        match self.0.lock().parent.upgrade() {
            Some(pptr) => Ok(pptr.clone()),
            None => Err(SystemError::ENOENT),
        }
    }

    /// # 用于重命名内存中的文件或目录
    fn rename(&self, _old_name: &str, _new_name: &str) -> Result<(), SystemError> {
        let old_inode: Arc<dyn IndexNode> = self.find(_old_name)?;
        // 在新的目录下创建一个硬链接
        self.link(_new_name, &old_inode)?;

        // 取消现有的目录下的这个硬链接
        if let Err(err) = self.unlink(_old_name) {
            // 如果取消失败，那就取消新的目录下的硬链接
            self.unlink(_new_name)?;
            return Err(err);
        }

        return Ok(());
    }
}
