use core::{intrinsics::size_of};

use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec, format,
};

use crate::{
    filesystem::vfs::{core::generate_inode_id, FileType},
    include::bindings::bindings::{
        process_find_pcb_by_pid, EEXIST, EINVAL, EISDIR, ENOBUFS, ENOENT, ENOTDIR, ENOTEMPTY,
        EPERM, ESRCH,
    },
    kdebug, kerror,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::TimeSpec,
};

use super::vfs::{
    file::FilePrivateData, FileSystem, FsInfo, IndexNode, InodeId, Metadata, PollStatus,
};

/// @brief 进程文件类型
/// @usage 用于定义进程文件夹下的各类文件类型
#[derive(Debug)]
#[repr(u8)]
pub enum ProcFileType {
    ///展示进程状态信息
    ProcStatus = 0,
    //todo: 其他文件类型
    ///默认文件类型
    Default,
}

impl From<u8> for ProcFileType {
    fn from(value: u8) -> Self {
        match value {
            0 => ProcFileType::ProcStatus,
            _ => ProcFileType::Default,
        }
    }
}
/// @brief 节点私有信息结构体
/// @usage 用于传入各类文件所需的信息
#[derive(Debug)]
pub struct InodeInfo {
    ///进程的pid
    pid: i64,
    ///文件类型
    ftype: ProcFileType,
    //其他需要传入的信息在此定义
}

/// @brief procfs的inode名称的最大长度
const PROCFS_MAX_NAMELEN: usize = 64;

/// @brief procfs文件系统的Inode结构体
#[derive(Debug)]
pub struct LockedProcFSInode(SpinLock<ProcFSInode>);

/// @brief procfs文件系统结构体
#[derive(Debug)]
pub struct ProcFS {
    /// procfs的root inode
    root_inode: Arc<LockedProcFSInode>,
}

#[derive(Debug)]
pub struct ProcfsFilePrivateData {
    data: Vec<u8>,
}

impl ProcfsFilePrivateData {
    pub fn new() -> Self {
        return ProcfsFilePrivateData { data: Vec::new() };
    }
}

/// @brief procfs文件系统的Inode结构体(不包含锁)
#[derive(Debug)]
pub struct ProcFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedProcFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedProcFSInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<LockedProcFSInode>>,
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<ProcFS>,
    /// 储存私有信息
    fdata: InodeInfo,
}

/// 对ProcFSInode实现获取各类文件信息的函数
impl ProcFSInode {

    /// @brief 去除Vec中所有的\0,并在结尾添加\0
    #[inline]
    fn trim_string(&self, data: &mut Vec<u8>){
        data.drain_filter(|x: &mut u8| *x == 0);
        data.push(0);
    }
    // todo:其他数据获取函数实现

    /// @brief 打开status文件
    ///
    fn open_status(&self, pdata: &mut ProcfsFilePrivateData) -> Result<i64, i32> {
        // 获取该pid对应的pcb结构体
        let pid: &i64 = &self.fdata.pid;
        let pcb = unsafe { process_find_pcb_by_pid(*pid).as_mut() };
        let pcb = if pcb.is_none() {
            kerror!(
                "ProcFS: Cannot find pcb for pid {} when opening its 'status' file.",
                pid
            );
            return Err(-(ESRCH as i32));
        } else {
            pcb.ok_or(-(EPERM as i32))?
        };
        // 传入数据
        let pdata: &mut Vec<u8> = &mut pdata.data;
        kdebug!("pcb.name={:?}", pcb);
        let mut tmp_name: Vec<u8> = Vec::with_capacity(pcb.name.len());
        for val in pcb.name.iter() {
            tmp_name.push(*val as u8);
        }
        kdebug!("pcb.tmp_name={}", String::from_utf8(tmp_name.clone()).unwrap_or("NULL".to_string()));
        
        // pdata.append(&mut format!("Name:\t{}", String::from_utf8(tmp_name).unwrap_or("NULL".to_string())).as_bytes().to_owned());
        pdata.append(&mut format!("Name:\tunknow").as_bytes().to_owned());
        pdata.append(&mut format!("\nstate:\t{}", pcb.state).as_bytes().to_owned());
        pdata.append(&mut format!("\npid:\t{}", pcb.pid).as_bytes().to_owned());
        pdata.append(&mut format!("\nPpid:\t{}", unsafe { *pcb.parent_pcb }.pid).as_bytes().to_owned());
        pdata.append(&mut format!("\ncpu_id:\t{}", pcb.cpu_id).as_bytes().to_owned());
        pdata.append(&mut format!("\npriority:\t{}", pcb.priority).as_bytes().to_owned());
        pdata.append(&mut format!("\npreempt:\t{}", pcb.preempt_count).as_bytes().to_owned());
        pdata.append(&mut format!("\nvrtime:\t{}", pcb.virtual_runtime).as_bytes().to_owned());

        // 当前进程运行过程中占用内存的峰值
        let hiwater_vm: u64 =
            unsafe { *(*pcb.mm).vmas }.vm_end - unsafe { *(*pcb.mm).vmas }.vm_start;
        // 进程数据段的大小
        let text: u64 = unsafe { *pcb.mm }.code_addr_end - unsafe { *pcb.mm }.code_addr_start;
        // 进程代码的大小
        let data: u64 = unsafe { *pcb.mm }.data_addr_end - unsafe { *pcb.mm }.data_addr_start;

        pdata.append(&mut format!("\nVmPeak:\t{} kB", hiwater_vm).as_bytes().to_owned());
        pdata.append(&mut format!("\nVmData:\t{} kB", data).as_bytes().to_owned());
        pdata.append(&mut format!("\nVmExe:\t{} kB\n", text).as_bytes().to_owned());

        // 去除多余的\0
        self.trim_string(pdata);

        kdebug!("status got!");
        kdebug!("ProcfsFilePrivateData:{:?}", pdata);
        kdebug!("open_status success!");
        return Ok((pdata.len() * size_of::<u8>()) as i64);
    }

    /// status文件读取函数
    fn read_status(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _pdata: &mut ProcfsFilePrivateData,
    ) -> Result<usize, i32> {
        let start = _pdata.data.len().min(offset);
        let end = _pdata.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(-(ENOBUFS as i32));
        }

        // 拷贝数据
        let src = &_pdata.data[start..end];
        buf[0..src.len()].copy_from_slice(src);
        return Ok(src.len());
    }
}

impl FileSystem for ProcFS {
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: PROCFS_MAX_NAMELEN,
        };
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

impl ProcFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedProcFSInode> =
            Arc::new(LockedProcFSInode(SpinLock::new(ProcFSInode {
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
                fdata: InodeInfo {
                    pid: 0,
                    ftype: ProcFileType::Default,
                },
            })));

        let result: Arc<ProcFS> = Arc::new(ProcFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<ProcFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
    }

    /// @brief 进程注册函数
    /// @usage 在进程中调用并创建进程对应文件
    pub fn procfs_register_pid(&self, pid: i64) -> Result<(), i32> {
        // 获取当前inode
        let proc: Arc<dyn IndexNode> = self.root_inode();
        // 创建对应进程文件夹
        let pf: Arc<dyn IndexNode> = proc.create(&pid.to_string(), FileType::Dir, 0o777)?;
        // 创建相关文件
        // status文件
        let binding: Arc<dyn IndexNode> = pf.create("status", FileType::File, 0)?;
        let sf: &LockedProcFSInode = binding
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(-(EPERM as i32))?;
        sf.0.lock().fdata.pid = pid;
        sf.0.lock().fdata.ftype = ProcFileType::ProcStatus;

        //todo: 创建其他文件

        return Ok(());
    }

    /// @brief 解除进程注册
    /// 
    pub fn procfs_unregister_pid(&self, pid: i64)-> Result<(), i32> {
        // 获取当前inode
        let proc: Arc<dyn IndexNode> = self.root_inode();
        // 获取进程文件夹
        let pid_dir: Arc<dyn IndexNode> = proc.find(&format!("{}", pid)).expect("Cannot find this dir");
        // 删除进程文件夹下文件
        pid_dir.unlink("status")?;
        
        // 查看进程文件是否还存在
        // let pf= pid_dir.find("status").expect("Cannot find status");

        // 删除进程文件夹
        proc.unlink(&format!("{}", pid))?;
        
        return Ok(());
    }

}

impl IndexNode for LockedProcFSInode {
    fn open(&self, data: &mut FilePrivateData) -> Result<(), i32> {
        kdebug!("open in!");
        // 加锁
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        let mut private_data = ProcfsFilePrivateData::new();
        // 根据文件类型获取相应数据
        let file_size = match inode.fdata.ftype {
            ProcFileType::ProcStatus => inode.open_status(&mut private_data)?,
            _ => {
                todo!()
            }
        };

        *data = FilePrivateData::Procfs(private_data);
        // 更新metadata里面的文件大小数值
        inode.metadata.size = file_size;
        kdebug!("open success!");
        return Ok(());
    }

    fn close(&self, data: &mut FilePrivateData) -> Result<(), i32> {
        // 获取数据信息
        let _private_data = match data {
            FilePrivateData::Procfs(p) => p,
            _ => {
                panic!("ProcFS: FilePrivateData mismatch!");
            }
        };
        // 释放资源
        drop(_private_data);
        return Ok(());
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }
        // 加锁
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        // 获取数据信息
        let private_data = match data {
            FilePrivateData::Procfs(p) => p,
            _ => {
                panic!("ProcFS: FilePrivateData mismatch!");
            }
        };

        // 根据文件类型读取相应数据
        match inode.fdata.ftype {
            ProcFileType::ProcStatus => return inode.read_status(offset, len, buf, private_data),
            ProcFileType::Default => (),
        };

        // 默认读取
        let start = inode.data.len().min(offset);
        let end = inode.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(-(ENOBUFS as i32));
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
        buf: &mut [u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }

        // 加锁
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
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

    fn poll(&self) -> Result<PollStatus, i32> {
        // 加锁
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        let inode = self.0.lock();
        let metadata = inode.metadata.clone();

        return Ok(metadata);
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

    fn resize(&self, len: usize) -> Result<(), i32> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        } else {
            return Err(-(EINVAL as i32));
        }
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }
        // 如果有重名的，则返回
        if inode.children.contains_key(name) {
            return Err(-(EEXIST as i32));
        }

        // 创建inode
        let result: Arc<LockedProcFSInode> =
            Arc::new(LockedProcFSInode(SpinLock::new(ProcFSInode {
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
                fdata: InodeInfo {
                    pid: 0,
                    ftype: ProcFileType::Default,
                },
            })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(String::from(name), result.clone());
        kdebug!("created file!");
        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), i32> {
        let other: &LockedProcFSInode = other
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(-(EPERM as i32))?;
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        let mut other_locked: SpinLockGuard<ProcFSInode> = other.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if inode.children.contains_key(name) {
            return Err(-(EEXIST as i32));
        }

        inode
            .children
            .insert(String::from(name), other_locked.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), i32> {
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(-(ENOTEMPTY as i32));
        }

        // 获得要删除的文件的inode
        let to_delete = inode.children.get(name).ok_or(-(ENOENT as i32))?;
        // 减少硬链接计数
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
    ) -> Result<(), i32> {
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

    fn get_entry_name(&self, ino: InodeId) -> Result<String, i32> {
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();
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
                    .filter(|k| inode.children.get(*k).unwrap().0.lock().metadata.inode_id == ino)
                    .cloned()
                    .collect();

                match key.len() {
                        0=>{return Err(-(ENOENT as i32));}
                        1=>{return Ok(key.remove(0));}
                        _ => panic!("Procfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id}, to find={to_find}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                    }
            }
        }
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
}
