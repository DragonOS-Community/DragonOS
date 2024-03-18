use super::vfs::{
    cache::DefaultCache, FileSystem, FsInfo,
    syscall::ModeType, FileType, 
    core::generate_inode_id, 
    file::{FilePrivateData, FileMode}, 
    IndexNode, Metadata, SpecialNodeData
};

use core::{
    cmp::Ordering, 
    intrinsics::unlikely,
    any::Any
};

use alloc::{
    string::String, 
    sync::{Arc, Weak},
    collections::BTreeMap,
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    ipc::pipe::LockedPipeInode,
    time::TimeSpec,
    libs::spinlock::SpinLock,
};
/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;

#[derive(Debug)]
pub struct RamFS {
    root: Arc<LockedEntry>,
    // To Add Cache
    cache: Arc<DefaultCache>,
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        let root =  Arc::new(LockedEntry(SpinLock::new(Entry{
            name: String::new(),
            inode: Arc::new(LockedInode(SpinLock::new(
                Inode::new(
                    FileType::Dir,  
                    ModeType::from_bits_truncate(0o777))))),
            parent: Weak::new(),
            self_ref: Weak::new(),
            children: BTreeMap::new(),
            fs: Weak::new(),
            special_node: None,
        })));
        let ret = Arc::new(RamFS{ 
            root, 
            cache: Arc::new(DefaultCache::new(None)),
        });
{
        let mut entry = ret.root.0.lock();
        entry.parent = Arc::downgrade(&ret.root);
        entry.self_ref = Arc::downgrade(&ret.root);
        entry.fs = Arc::downgrade(&ret);
}
        ret
    }
}

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

    fn cache(&self) -> Result<DefaultCache, SystemError> {
        Ok(self.cache.clone())
    }
}

#[derive(Debug)]
pub struct Keyer(Weak<LockedEntry>, Option<String>);

impl Keyer {
    fn from_str(key: &str) -> Self {
        Keyer(Weak::new(), Some(String::from(key)))
    }

    fn from_entry(entry: &Arc<LockedEntry>) -> Self {
        Keyer(Arc::downgrade(entry), None)
    }

    /// 获取name
    fn get(&self) -> Option<String> {
        if self.1.is_some() {
            return self.1.clone();
        }
        Some(self.0.upgrade()?.0.lock().name.clone())
    }   
}

// For Btree insertion
impl PartialEq for Keyer {
    fn eq(&self, other: &Self) -> bool {
        if self.0.ptr_eq(&other.0) {
            kdebug!("Compare itself!");
            return true;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Empty Both none!");
                panic!("Empty of both");
            }
            if opt1.is_none() || opt2.is_none() {
                return false;
            }
            return opt1.unwrap().0.lock().name == opt2.unwrap().0.lock().name;
        }

        if self.1.is_none() {
            let opt = self.0.upgrade();
            if opt.is_none() {
                kwarn!("depecated");
                return false;
            }

            return &opt.unwrap().0.lock().name == other.1.as_ref().unwrap();

        } else {
            let opt = other.0.upgrade();
            if opt.is_none() {
                kwarn!("depecated");
                return false;
            }
            
            return &opt.unwrap().0.lock().name == self.1.as_ref().unwrap();

        }
    }
}

impl Eq for Keyer {}

// Uncheck Stable
impl PartialOrd for Keyer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.0.ptr_eq(&other.0) { 
            kdebug!("Compare itself!");
            return Some(Ordering::Equal);
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Empty Both none!");
                panic!("All Keys None, compare error!");
            }
            if opt1.is_some() && opt2.is_some() {
                return Some(opt1.unwrap().0.lock().name.cmp(&opt2.unwrap().0.lock().name));
            } else {
                kwarn!("depecated");
                panic!("Empty Key!");
            }
        } else {
            if self.1.is_none() {
                let opt = self.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }
                return Some(opt.unwrap().0.lock().name.cmp(other.1.as_ref().unwrap()));

            } else {
                let opt = other.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }

                return Some(opt.unwrap().0.lock().name.cmp(self.1.as_ref().unwrap()));
            }
        }
    }
}

impl Ord for Keyer {
    fn cmp(&self, other: &Self) -> Ordering {
        // let mut ret: Ordering = Ordering::Equal;
        if self.0.ptr_eq(&other.0) {
            kdebug!("Compare itself!");
            return Ordering::Equal;
        }
        if self.1.is_none() && other.1.is_none() {
            // cmp between wrapper
            let opt1 = self.0.upgrade();
            let opt2 = other.0.upgrade();
            if opt1.is_none() && opt2.is_none() {
                kerror!("Both None!");
                panic!("All Keys None, compare error!");
            }
            if opt1.is_some() && opt2.is_some() {
                return opt1.unwrap().0.lock().name.cmp(&opt2.unwrap().0.lock().name);
            } else {
                kwarn!("depecated");
                panic!("Empty Key!");
            }
        } else {
            if self.1.is_none() {
                let opt = self.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }
                return opt.unwrap().0.lock().name.cmp(other.1.as_ref().unwrap());

            } else {
                let opt = other.0.upgrade();
                if opt.is_none() {
                    kwarn!("depecated");
                    panic!("Empty Key!");
                }

                return self.1.as_ref().unwrap().cmp(&opt.unwrap().0.lock().name);
            }
        }
    }
}

#[derive(Debug)]
pub struct Inode {
    /// 元数据
    metadata: Metadata,
    /// 数据块
    data: Vec<u8>,
}

#[derive(Debug)]
pub struct LockedInode(SpinLock<Inode>);

/// [WARN] [UNSAFE]
/// Every 
#[derive(Debug)]
pub struct Entry {

    name: String,

    inode: Arc<LockedInode>,

    parent: Weak<LockedEntry>,

    self_ref: Weak<LockedEntry>,

    children: BTreeMap<Keyer, Arc<LockedEntry>>,

    fs: Weak<RamFS>,

    special_node: Option<SpecialNodeData>,
}

#[derive(Debug)]
pub struct LockedEntry(SpinLock<Entry>);

impl Inode {
    pub fn new(file_type: FileType, mode: ModeType) -> Inode {
        Inode {
            data: Vec::new(),
            metadata: Metadata::new(file_type, mode),
        }
    }

    pub fn from(file_type: FileType, mode: ModeType, data: usize) -> Inode {
        Inode {
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
                file_type,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            }
        }
    }
}

// impl trait for LockedEntry

impl IndexNode for LockedEntry {
    fn truncate(&self, len: usize) -> Result<(), SystemError> 
    {
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

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> 
    {
        Ok(())
    }

    fn open(
        &self,
        _data: &mut FilePrivateData,
        _mode: &FileMode,
    ) -> Result<(), SystemError> 
    {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> 
    {
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
        Ok(src.len())
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> 
    {
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

        Ok(len)
    }

    fn fs(&self) -> Arc<dyn FileSystem> 
    {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any 
    {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> 
    {

        let entry = self.0.lock();
        let inode = entry.inode.0.lock();
        let mut metadata = inode.metadata.clone();

        metadata.size = inode.data.len() as i64;
        
        drop(inode); drop(entry);

        Ok(metadata)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> 
    {
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> 
    {
        let entry = self.0.lock();
        let mut inode = entry.inode.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        }
        Err(SystemError::EINVAL)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> 
    {
        kdebug!("Call Ramfs create.");
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
        let result = Arc::new(LockedEntry(SpinLock::new(Entry {
            parent: entry.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            inode: Arc::new(LockedInode(SpinLock::new(Inode::from(file_type, mode, data)))),
            fs: entry.fs.clone(),
            special_node: None,
            name: String::from(name),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        entry.children.insert(Keyer::from_entry(&result), result.clone());

        Ok(result)
    }

    /// Not Stable, waiting for improvement
    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> 
    {
        let other: &LockedEntry = other
            .downcast_ref::<LockedEntry>()
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
        let to_insert = Arc::new(LockedEntry(SpinLock::new(Entry {
            name: String::from(name),
            inode: other_entry.inode.clone(),
            parent: entry.self_ref.clone(),
            self_ref: Weak::new(),
            children: BTreeMap::new(),  // File should not have children
            fs: other_entry.fs.clone(),
            special_node: other_entry.special_node.clone(),
        })));
        entry.children.insert(Keyer::from_entry(&to_insert), to_insert);

        // 增加硬链接计数
        other_inode.metadata.nlinks += 1;

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> 
    {
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
        let to_del_entry = entry.children.get(&Keyer::from_str(name))
            .ok_or(SystemError::ENOENT)?.0.lock();
        let mut to_del_node = to_del_entry.inode.0.lock();

        if to_del_node.metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }
        // 减少硬链接计数
        to_del_node.metadata.nlinks -= 1;
}
        // 在当前目录中删除这个子目录项
        entry.children.remove(&Keyer::from_str(name));

        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> 
    {
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
        let to_del_ent = entry.children
            .get(&keyer)
            .ok_or(SystemError::ENOENT)?
            .0.lock();
        let mut to_del_nod = to_del_ent.inode.0.lock();
        if to_del_nod.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        to_del_nod.metadata.nlinks -= 1;
}
        // 在当前目录中删除这个子目录项
        entry.children.remove(&keyer);
        Ok(())
    }

    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> 
    {
        let old_inode: Arc<dyn IndexNode> = self.find(old_name)?;

        // 在新的目录下创建一个硬链接
        target.link(new_name, &old_inode)?;
        // 取消现有的目录下的这个硬链接
        if let Err(err) = self.unlink(old_name) {
            // 如果取消失败，那就取消新的目录下的硬链接
            target.unlink(new_name)?;
            return Err(err);
        }
        Ok(())
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> 
    {
        let entry = self.0.lock();
        let inode = entry.inode.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => {
                Ok(entry.self_ref.upgrade().ok_or(SystemError::ENOENT)?)
            }

            ".." => {
                Ok(entry.parent.upgrade().ok_or(SystemError::ENOENT)?)
            }
            name => {
                // 在子目录项中查找
                Ok(entry.children.get(&Keyer::from_str(name)).ok_or(SystemError::ENOENT)?.clone())
            }
        }
    }

    /// Potential panic
    fn get_entry_name(&self, ino: crate::filesystem::vfs::InodeId) 
        -> Result<String, SystemError> 
    {
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

    fn list(&self) -> Result<Vec<String>, SystemError> 
    {
        // kinfo!("Call Ramfs::list");
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(&mut self.0.lock().children.keys()
            .map(|k|{k.get().unwrap_or(
                String::from("[unknown_filename]"))})
            .collect());

        Ok(keys)
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
            return Ok(self.create(filename, FileType::File, mode)?);
        }

        let nod = Arc::new(LockedEntry(SpinLock::new(Entry{
            parent: entry.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            inode: Arc::new(LockedInode(SpinLock::new(Inode::new(FileType::Pipe, mode)))),
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

        entry.children
            .insert(
                Keyer::from_str(String::from(filename)
                                            .to_uppercase().as_str()), 
                nod.clone());
        Ok(nod)
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

    fn self_ref(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        Ok(self.0.lock().self_ref.upgrade().ok_or(SystemError::ENOENT)?)
    }
}
