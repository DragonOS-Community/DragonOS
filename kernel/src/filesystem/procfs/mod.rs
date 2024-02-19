use core::intrinsics::size_of;

use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::mm::LockedFrameAllocator,
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        core::{generate_inode_id, ROOT_INODE},
        FileType,
    },
    kerror, kinfo,
    libs::{
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::allocator::page_frame::FrameAllocator,
    process::{Pid, ProcessManager},
    time::TimeSpec,
};

use super::vfs::{
    file::{FileMode, FilePrivateData},
    syscall::ModeType,
    FileSystem, FsInfo, IndexNode, InodeId, Metadata,
};

pub mod kmsg;
pub mod log;
mod syscall;

/// @brief 进程文件类型
/// @usage 用于定义进程文件夹下的各类文件类型
#[derive(Debug)]
#[repr(u8)]
pub enum ProcFileType {
    ///展示进程状态信息
    ProcStatus = 0,
    /// meminfo
    ProcMeminfo = 1,
    /// kmsg
    ProcKmsg = 2,
    //todo: 其他文件类型
    ///默认文件类型
    Default,
}

impl From<u8> for ProcFileType {
    fn from(value: u8) -> Self {
        match value {
            0 => ProcFileType::ProcStatus,
            1 => ProcFileType::ProcMeminfo,
            2 => ProcFileType::ProcKmsg,
            _ => ProcFileType::Default,
        }
    }
}
/// @brief 节点私有信息结构体
/// @usage 用于传入各类文件所需的信息
#[derive(Debug)]
pub struct InodeInfo {
    ///进程的pid
    pid: Pid,
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

#[derive(Debug, Clone)]
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
    fn trim_string(&self, data: &mut Vec<u8>) {
        data.retain(|x| *x != 0);

        data.push(0);
    }
    // todo:其他数据获取函数实现

    /// @brief 打开status文件
    ///
    fn open_status(&self, pdata: &mut ProcfsFilePrivateData) -> Result<i64, SystemError> {
        // 获取该pid对应的pcb结构体
        let pid = self.fdata.pid;
        let pcb = ProcessManager::find(pid);
        let pcb = if pcb.is_none() {
            kerror!(
                "ProcFS: Cannot find pcb for pid {:?} when opening its 'status' file.",
                pid
            );
            return Err(SystemError::ESRCH);
        } else {
            pcb.unwrap()
        };
        // 传入数据
        let pdata: &mut Vec<u8> = &mut pdata.data;

        pdata.append(
            &mut format!("Name:\t{}", pcb.basic().name())
                .as_bytes()
                .to_owned(),
        );

        let sched_info_guard = pcb.sched_info();
        let state = sched_info_guard.inner_lock_read_irqsave().state();
        let cpu_id = sched_info_guard
            .on_cpu()
            .map(|cpu| cpu.data() as i32)
            .unwrap_or(-1);

        let priority = sched_info_guard.priority();
        let vrtime = sched_info_guard.virtual_runtime();

        pdata.append(&mut format!("\nState:\t{:?}", state).as_bytes().to_owned());
        pdata.append(
            &mut format!("\nPid:\t{}", pcb.pid().into())
                .as_bytes()
                .to_owned(),
        );
        pdata.append(
            &mut format!("\nPpid:\t{}", pcb.basic().ppid().into())
                .as_bytes()
                .to_owned(),
        );
        pdata.append(&mut format!("\ncpu_id:\t{}", cpu_id).as_bytes().to_owned());
        pdata.append(
            &mut format!("\npriority:\t{}", priority.data())
                .as_bytes()
                .to_owned(),
        );
        pdata.append(
            &mut format!("\npreempt:\t{}", pcb.preempt_count())
                .as_bytes()
                .to_owned(),
        );
        pdata.append(&mut format!("\nvrtime:\t{}", vrtime).as_bytes().to_owned());

        if let Some(user_vm) = pcb.basic().user_vm() {
            let address_space_guard = user_vm.read();
            // todo: 当前进程运行过程中占用内存的峰值
            let hiwater_vm: u64 = 0;
            // 进程代码段的大小
            let text = (address_space_guard.end_code - address_space_guard.start_code) / 1024;
            // 进程数据段的大小
            let data = (address_space_guard.end_data - address_space_guard.start_data) / 1024;
            drop(address_space_guard);
            pdata.append(
                &mut format!("\nVmPeak:\t{} kB", hiwater_vm)
                    .as_bytes()
                    .to_owned(),
            );
            pdata.append(&mut format!("\nVmData:\t{} kB", data).as_bytes().to_owned());
            pdata.append(&mut format!("\nVmExe:\t{} kB", text).as_bytes().to_owned());
        }

        pdata.append(
            &mut format!("\nflags: {:?}\n", pcb.flags().clone())
                .as_bytes()
                .to_owned(),
        );

        // 去除多余的\0
        self.trim_string(pdata);

        return Ok((pdata.len() * size_of::<u8>()) as i64);
    }

    /// 打开 meminfo 文件
    fn open_meminfo(&self, pdata: &mut ProcfsFilePrivateData) -> Result<i64, SystemError> {
        // 获取内存信息
        let usage = unsafe { LockedFrameAllocator.usage() };

        // 传入数据
        let data: &mut Vec<u8> = &mut pdata.data;

        data.append(
            &mut format!("MemTotal:\t{} kB\n", usage.total().bytes() >> 10)
                .as_bytes()
                .to_owned(),
        );

        data.append(
            &mut format!("MemFree:\t{} kB\n", usage.free().bytes() >> 10)
                .as_bytes()
                .to_owned(),
        );

        // 去除多余的\0
        self.trim_string(data);

        return Ok((data.len() * size_of::<u8>()) as i64);
    }

    /// proc文件系统读取函数
    fn proc_read(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _pdata: &mut ProcfsFilePrivateData,
    ) -> Result<usize, SystemError> {
        let start = _pdata.data.len().min(offset);
        let end = _pdata.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(SystemError::ENOBUFS);
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
                    mode: ModeType::from_bits_truncate(0o555),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                fs: Weak::default(),
                fdata: InodeInfo {
                    pid: Pid::new(0),
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

        // 创建meminfo文件
        let inode = result.root_inode();
        let binding = inode.create(
            "meminfo",
            FileType::File,
            ModeType::from_bits_truncate(0o444),
        );
        if let Ok(meminfo) = binding {
            let meminfo_file = meminfo
                .as_any_ref()
                .downcast_ref::<LockedProcFSInode>()
                .unwrap();
            meminfo_file.0.lock().fdata.pid = Pid::new(0);
            meminfo_file.0.lock().fdata.ftype = ProcFileType::ProcMeminfo;
        } else {
            panic!("create meminfo error");
        }

        // 创建kmsg文件
        let binding = inode.create("kmsg", FileType::File, ModeType::from_bits_truncate(0o444));
        if let Ok(kmsg) = binding {
            let kmsg_file = kmsg
                .as_any_ref()
                .downcast_ref::<LockedProcFSInode>()
                .unwrap();
            kmsg_file.0.lock().fdata.pid = Pid::new(1);
            kmsg_file.0.lock().fdata.ftype = ProcFileType::ProcKmsg;
        } else {
            panic!("create ksmg error");
        }

        return result;
    }

    /// @brief 进程注册函数
    /// @usage 在进程中调用并创建进程对应文件
    pub fn register_pid(&self, pid: Pid) -> Result<(), SystemError> {
        // 获取当前inode
        let inode: Arc<dyn IndexNode> = self.root_inode();
        // 创建对应进程文件夹
        let pid_dir: Arc<dyn IndexNode> = inode.create(
            &pid.to_string(),
            FileType::Dir,
            ModeType::from_bits_truncate(0o555),
        )?;
        // 创建相关文件
        // status文件
        let binding: Arc<dyn IndexNode> = pid_dir.create(
            "status",
            FileType::File,
            ModeType::from_bits_truncate(0o444),
        )?;
        let status_file: &LockedProcFSInode = binding
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .unwrap();
        status_file.0.lock().fdata.pid = pid;
        status_file.0.lock().fdata.ftype = ProcFileType::ProcStatus;

        //todo: 创建其他文件

        return Ok(());
    }

    /// @brief 解除进程注册
    ///
    pub fn unregister_pid(&self, pid: Pid) -> Result<(), SystemError> {
        // 获取当前inode
        let proc: Arc<dyn IndexNode> = self.root_inode();
        // 获取进程文件夹
        let pid_dir: Arc<dyn IndexNode> = proc.find(&pid.to_string())?;
        // 删除进程文件夹下文件
        pid_dir.unlink("status")?;

        // 查看进程文件是否还存在
        // let pf= pid_dir.find("status").expect("Cannot find status");

        // 删除进程文件夹
        proc.unlink(&pid.to_string())?;

        return Ok(());
    }
}

impl IndexNode for LockedProcFSInode {
    fn open(&self, data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        // 加锁
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 如果inode类型为文件夹，则直接返回成功
        if let FileType::Dir = inode.metadata.file_type {
            return Ok(());
        }
        let mut private_data = ProcfsFilePrivateData::new();
        // 根据文件类型获取相应数据
        let file_size = match inode.fdata.ftype {
            ProcFileType::ProcStatus => inode.open_status(&mut private_data)?,
            ProcFileType::ProcMeminfo => inode.open_meminfo(&mut private_data)?,
            _ => {
                todo!()
            }
        };
        *data = FilePrivateData::Procfs(private_data);
        // 更新metadata里面的文件大小数值
        inode.metadata.size = file_size;

        return Ok(());
    }

    fn close(&self, data: &mut FilePrivateData) -> Result<(), SystemError> {
        let guard: SpinLockGuard<ProcFSInode> = self.0.lock();
        // 如果inode类型为文件夹，则直接返回成功
        if let FileType::Dir = guard.metadata.file_type {
            return Ok(());
        }
        // 释放data
        *data = FilePrivateData::Procfs(ProcfsFilePrivateData::new());

        return Ok(());
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
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
            ProcFileType::ProcStatus => return inode.proc_read(offset, len, buf, private_data),
            ProcFileType::ProcMeminfo => return inode.proc_read(offset, len, buf, private_data),
            ProcFileType::ProcKmsg => (),
            ProcFileType::Default => (),
        };

        // 默认读取
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
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let inode = self.0.lock();
        let metadata = inode.metadata.clone();

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
                    raw_dev: DeviceNumber::from(data as u32),
                },
                fs: inode.fs.clone(),
                fdata: InodeInfo {
                    pid: Pid::new(0),
                    ftype: ProcFileType::Default,
                },
            })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(String::from(name), result.clone());

        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedProcFSInode = other
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(SystemError::EPERM)?;
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        let mut other_locked: SpinLockGuard<ProcFSInode> = other.0.lock();

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
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
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
        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(name);
        return Ok(());
    }

    fn move_(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();
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
                            .0
                            .lock()
                            .metadata
                            .inode_id
                            .into()
                            == ino
                    })
                    .cloned()
                    .collect();

                match key.len() {
                        0=>{return Err(SystemError::ENOENT);}
                        1=>{return Ok(key.remove(0));}
                        _ => panic!("Procfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
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

/// @brief 向procfs注册进程
pub fn procfs_register_pid(pid: Pid) -> Result<(), SystemError> {
    let procfs_inode = ROOT_INODE().find("proc")?;

    let procfs_inode = procfs_inode
        .downcast_ref::<LockedProcFSInode>()
        .expect("Failed to find procfs' root inode");
    let fs = procfs_inode.fs();
    let procfs: &ProcFS = fs.as_any_ref().downcast_ref::<ProcFS>().unwrap();

    // 调用注册函数
    procfs.register_pid(pid)?;

    return Ok(());
}

/// @brief 在ProcFS中,解除进程的注册
pub fn procfs_unregister_pid(pid: Pid) -> Result<(), SystemError> {
    // 获取procfs实例
    let procfs_inode: Arc<dyn IndexNode> = ROOT_INODE().find("proc")?;

    let procfs_inode: &LockedProcFSInode = procfs_inode
        .downcast_ref::<LockedProcFSInode>()
        .expect("Failed to find procfs' root inode");
    let fs: Arc<dyn FileSystem> = procfs_inode.fs();
    let procfs: &ProcFS = fs.as_any_ref().downcast_ref::<ProcFS>().unwrap();

    // 调用解除注册函数
    return procfs.unregister_pid(pid);
}

pub fn procfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        kinfo!("Initializing ProcFS...");
        // 创建 procfs 实例
        let procfs: Arc<ProcFS> = ProcFS::new();

        // procfs 挂载
        let _t = ROOT_INODE()
            .find("proc")
            .expect("Cannot find /proc")
            .mount(procfs)
            .expect("Failed to mount proc");
        kinfo!("ProcFS mounted.");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
