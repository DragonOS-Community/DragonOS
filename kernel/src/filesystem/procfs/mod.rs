use core::intrinsics::size_of;

use ::log::{error, info};
use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::mm::LockedFrameAllocator,
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        mount::{MountFlags, MountPath},
        syscall::RenameFlags,
        vcore::generate_inode_id,
        FileType,
    },
    libs::{
        once::Once,
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::allocator::page_frame::FrameAllocator,
    process::{ProcessManager, ProcessState, RawPid},
    time::PosixTimeSpec,
};

use super::vfs::{
    file::{FileMode, FilePrivateData},
    syscall::ModeType,
    utils::DName,
    FileSystem, FsInfo, IndexNode, InodeId, Magic, Metadata, SuperBlock,
};

pub mod kmsg;
pub mod log;
mod proc_cpuinfo;
mod proc_mounts;
mod proc_version;
mod syscall;

/// @brief 进程文件类型
/// @usage 用于定义进程文件夹下的各类文件类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcFileType {
    ///展示进程状态信息
    ProcStatus = 0,
    /// meminfo
    ProcMeminfo = 1,
    /// kmsg
    ProcKmsg = 2,
    /// 可执行路径
    ProcExe = 3,
    /// /proc/mounts
    ProcSelf = 4,
    ProcFdDir = 5,
    ProcFdFile = 6,
    ProcMounts = 7,
    /// /proc/version
    ProcVersion = 8,
    /// /proc/cpuinfo
    ProcCpuinfo = 9,
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
            3 => ProcFileType::ProcExe,
            4 => ProcFileType::ProcSelf,
            5 => ProcFileType::ProcFdDir,
            6 => ProcFileType::ProcFdFile,
            7 => ProcFileType::ProcMounts,
            8 => ProcFileType::ProcVersion,
            9 => ProcFileType::ProcCpuinfo,
            _ => ProcFileType::Default,
        }
    }
}
/// @brief 创建 ProcFS 文件的参数结构体
#[derive(Debug, Clone)]
pub struct ProcFileCreationParams<'a> {
    pub parent: Arc<dyn IndexNode>,
    pub name: &'a str,
    pub file_type: FileType,
    pub mode: ModeType,
    pub pid: Option<RawPid>,
    pub ftype: ProcFileType,
    pub data: Option<&'a str>,
}

impl<'a> ProcFileCreationParams<'a> {
    pub fn builder() -> ProcFileCreationParamsBuilder<'a> {
        ProcFileCreationParamsBuilder::default()
    }
}

/// @brief ProcFileCreationParams 的 Builder 模式实现
#[derive(Debug, Clone, Default)]
pub struct ProcFileCreationParamsBuilder<'a> {
    parent: Option<Arc<dyn IndexNode>>,
    name: Option<&'a str>,
    file_type: Option<FileType>,
    mode: Option<ModeType>,
    pid: Option<RawPid>,
    ftype: Option<ProcFileType>,
    data: Option<&'a str>,
}

#[allow(dead_code)]
impl<'a> ProcFileCreationParamsBuilder<'a> {
    pub fn parent(mut self, parent: Arc<dyn IndexNode>) -> Self {
        self.parent = Some(parent);
        self
    }

    pub fn name(mut self, name: &'a str) -> Self {
        self.name = Some(name);
        self
    }

    pub fn file_type(mut self, file_type: FileType) -> Self {
        self.file_type = Some(file_type);
        self
    }

    pub fn mode(mut self, mode: ModeType) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn pid(mut self, pid: RawPid) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn ftype(mut self, ftype: ProcFileType) -> Self {
        self.ftype = Some(ftype);
        self
    }

    pub fn data(mut self, data: &'a str) -> Self {
        self.data = Some(data);
        self
    }

    pub fn build(self) -> Result<ProcFileCreationParams<'a>, SystemError> {
        Ok(ProcFileCreationParams {
            parent: self.parent.ok_or(SystemError::EINVAL)?,
            name: self.name.ok_or(SystemError::EINVAL)?,
            file_type: self.file_type.ok_or(SystemError::EINVAL)?,
            mode: self.mode.unwrap_or(ModeType::S_IRUGO),
            pid: self.pid,
            ftype: self.ftype.ok_or(SystemError::EINVAL)?,
            data: self.data,
        })
    }
}

/// @brief 节点私有信息结构体
/// @usage 用于传入各类文件所需的信息
#[derive(Debug)]
pub struct InodeInfo {
    ///进程的pid
    pid: Option<RawPid>,
    ///文件类型
    ftype: ProcFileType,
    /// 文件描述符
    fd: i32,
    // 其他需要传入的信息在此定义
}

/// @brief procfs的inode名称的最大长度
const PROCFS_MAX_NAMELEN: usize = 64;
const PROCFS_BLOCK_SIZE: u64 = 512;
/// @brief procfs文件系统的Inode结构体
#[derive(Debug)]
pub struct LockedProcFSInode(SpinLock<ProcFSInode>);

/// @brief procfs文件系统结构体
#[derive(Debug)]
pub struct ProcFS {
    /// procfs的root inode
    root_inode: Arc<LockedProcFSInode>,
    super_block: RwLock<SuperBlock>,
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

impl Default for ProcfsFilePrivateData {
    fn default() -> Self {
        Self::new()
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
    children: BTreeMap<DName, Arc<LockedProcFSInode>>,
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<ProcFS>,
    /// 储存私有信息
    fdata: InodeInfo,
    /// 目录项
    dname: DName,
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
        let pid = self
            .fdata
            .pid
            .expect("ProcFS: pid is None when opening 'status' file.");
        let pcb = ProcessManager::find_task_by_vpid(pid);
        let pcb = if let Some(pcb) = pcb {
            pcb
        } else {
            error!(
                "ProcFS: Cannot find pcb for pid {:?} when opening its 'status' file.",
                pid
            );
            return Err(SystemError::ESRCH);
        };

        // ::log::debug!(
        //     "ProcFS: Opening 'status' file for pid {:?} (cnt: {})",
        //     pcb.raw_pid(),
        //     Arc::strong_count(&pcb)
        // );
        // 传入数据
        let pdata: &mut Vec<u8> = &mut pdata.data;
        // name
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

        let priority = sched_info_guard.policy();
        let vrtime = sched_info_guard.sched_entity.vruntime;
        let time = sched_info_guard.sched_entity.sum_exec_runtime;
        let start_time = sched_info_guard.sched_entity.exec_start;
        // State
        pdata.append(&mut format!("\nState:\t{:?}", state).as_bytes().to_owned());

        // Tgid
        pdata.append(
            &mut format!(
                "\nTgid:\t{}",
                pcb.task_tgid_vnr().unwrap_or(RawPid::new(0)).into()
            )
            .into(),
        );

        // pid
        pdata.append(
            &mut format!("\nPid:\t{}", pcb.task_pid_vnr().data())
                .as_bytes()
                .to_owned(),
        );

        // ppid
        pdata.append(
            &mut format!(
                "\nPpid:\t{}",
                pcb.parent_pcb()
                    .map(|p| p.task_pid_vnr().data() as isize)
                    .unwrap_or(-1)
            )
            .as_bytes()
            .to_owned(),
        );

        // fdsize
        if matches!(state, ProcessState::Exited(_)) {
            // 进程已经退出，fdsize为0
            pdata.append(&mut format!("\nFDSize:\t{}", 0).into());
        } else {
            pdata.append(
                &mut format!("\nFDSize:\t{}", pcb.fd_table().read().fd_open_count()).into(),
            );
        }

        // tty
        let name = if let Some(tty) = pcb.sig_info_irqsave().tty() {
            tty.core().name().clone()
        } else {
            "none".to_string()
        };
        pdata.append(&mut format!("\nTty:\t{}", name).as_bytes().to_owned());

        // 进程在cpu上的运行时间
        pdata.append(&mut format!("\nTime:\t{}", time).as_bytes().to_owned());
        // 进程开始运行的时间
        pdata.append(&mut format!("\nStime:\t{}", start_time).as_bytes().to_owned());
        // kthread
        pdata.append(&mut format!("\nKthread:\t{}", pcb.is_kthread() as usize).into());
        pdata.append(&mut format!("\ncpu_id:\t{}", cpu_id).as_bytes().to_owned());
        pdata.append(&mut format!("\npriority:\t{:?}", priority).as_bytes().to_owned());
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

    // 打开 exe 文件
    fn open_exe(&self, _pdata: &mut ProcfsFilePrivateData) -> Result<i64, SystemError> {
        // 这个文件是一个软链接，直接返回0即可
        let pcb = ProcessManager::current_pcb();
        let exe = pcb.execute_path();
        return Ok(exe.len() as _);
    }

    fn open_self(&self, _pdata: &mut ProcfsFilePrivateData) -> Result<i64, SystemError> {
        let pid = ProcessManager::current_pid().data();
        return Ok(pid.to_string().len() as _);
    }

    // 读取exe文件
    fn read_exe_link(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        // 判断是否有记录pid信息，有的话就是当前进程的exe文件，没有则是当前进程的exe文件
        let pcb = if let Some(pid) = self.fdata.pid {
            ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?
        } else {
            // 如果没有pid信息，则读取当前进程的exe文件
            ProcessManager::current_pcb()
        };
        let exe = pcb.execute_path();
        let exe_bytes = exe.as_bytes();
        if offset >= exe_bytes.len() {
            return Ok(0);
        }
        let len = buf.len().min(exe_bytes.len() - offset);
        buf[..len].copy_from_slice(&exe_bytes[offset..offset + len]);
        Ok(len)
    }

    /// read current task pid dynamically
    fn read_self_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let pid = ProcessManager::current_pid().data();
        let pid_bytes = pid.to_string();
        let len = pid_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&pid_bytes.as_bytes()[..len]);
        Ok(len)
    }

    fn read_fd_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fd = self.fdata.fd;
        let fd_table = ProcessManager::current_pcb().fd_table();
        let fd_table = fd_table.read();
        let file = fd_table.get_file_by_fd(fd);
        if let Some(file) = file {
            let inode = file.inode();
            let path = inode.absolute_path().unwrap();
            let len = path.len().min(buf.len());
            buf[..len].copy_from_slice(&path.as_bytes()[..len]);
            Ok(len)
        } else {
            return Err(SystemError::EBADF);
        }
    }

    /// proc文件系统读取函数
    fn proc_read(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<usize, SystemError> {
        let start = pdata.data.len().min(offset);
        let end = pdata.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &pdata.data[start..end];
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
    fn name(&self) -> &str {
        "procfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }
}

impl ProcFS {
    #[inline(never)]
    fn create_proc_file(&self, params: ProcFileCreationParams) -> Result<(), SystemError> {
        let binding = params
            .parent
            .create(params.name, params.file_type, params.mode)?;
        let proc_file = binding
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .unwrap();
        let mut proc_file_guard = proc_file.0.lock();
        proc_file_guard.fdata.pid = params.pid;
        proc_file_guard.fdata.ftype = params.ftype;
        if let Some(data_content) = params.data {
            proc_file_guard.data = data_content.to_string().as_bytes().to_vec();
        }
        drop(proc_file_guard);
        Ok(())
    }

    #[inline(never)]
    pub fn new() -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::PROC_MAGIC,
            PROCFS_BLOCK_SIZE,
            PROCFS_MAX_NAMELEN as u64,
        );
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
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: ModeType::from_bits_truncate(0o555),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                fs: Weak::default(),
                fdata: InodeInfo {
                    pid: None,
                    ftype: ProcFileType::Default,
                    fd: -1,
                },
                dname: DName::default(),
            })));

        let result: Arc<ProcFS> = Arc::new(ProcFS {
            root_inode: root,
            super_block: RwLock::new(super_block),
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<ProcFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        // 创建meminfo文件
        let meminfo_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("meminfo")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcMeminfo)
            .build()
            .unwrap();
        result
            .create_proc_file(meminfo_params)
            .unwrap_or_else(|_| panic!("create meminfo error"));

        // 创建kmsg文件
        let kmsg_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("kmsg")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcKmsg)
            .build()
            .unwrap();
        result
            .create_proc_file(kmsg_params)
            .unwrap_or_else(|_| panic!("create kmsg error"));
        // 这个文件是用来欺骗Aya框架识别内核版本
        /* On Ubuntu LINUX_VERSION_CODE doesn't correspond to info.release,
         * but Ubuntu provides /proc/version_signature file, as described at
         * https://ubuntu.com/kernel, with an example contents below, which we
         * can use to get a proper LINUX_VERSION_CODE.
         *
         *   Ubuntu 5.4.0-12.15-generic 5.4.8
         *
         * In the above, 5.4.8 is what kernel is actually expecting, while
         * uname() call will return 5.4.0 in info.release.
         */
        let version_signature_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("version_signature")
            .file_type(FileType::File)
            .ftype(ProcFileType::Default)
            .data("DragonOS 6.0.0-generic 6.0.0\n")
            .build()
            .unwrap();
        result
            .create_proc_file(version_signature_params)
            .unwrap_or_else(|_| panic!("create version_signature error"));

        let mounts_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("mounts")
            .file_type(FileType::File)
            .ftype(ProcFileType::ProcMounts)
            .build()
            .unwrap();
        result
            .create_proc_file(mounts_params)
            .unwrap_or_else(|_| panic!("create mounts error"));

        // 创建 version 文件
        let version_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("version")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcVersion)
            .build()
            .unwrap();
        result
            .create_proc_file(version_params)
            .unwrap_or_else(|_| panic!("create version error"));

        // 创建 cpuinfo 文件
        let cpuinfo_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("cpuinfo")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcCpuinfo)
            .build()
            .unwrap();
        result
            .create_proc_file(cpuinfo_params)
            .unwrap_or_else(|_| panic!("create cpuinfo error"));

        let self_params = ProcFileCreationParams::builder()
            .parent(result.root_inode())
            .name("self")
            .file_type(FileType::SymLink)
            .mode(ModeType::from_bits_truncate(0o555))
            .ftype(ProcFileType::ProcSelf)
            .build()
            .unwrap();
        result
            .create_proc_file(self_params)
            .unwrap_or_else(|_| panic!("create self error"));

        return result;
    }

    /// @brief 进程注册函数
    /// @usage 在进程中调用并创建进程对应文件
    pub fn register_pid(&self, pid: RawPid) -> Result<(), SystemError> {
        // 获取当前inode
        let inode = self.root_inode();
        // 创建对应进程文件夹
        let pid_dir = inode.create(
            &pid.to_string(),
            FileType::Dir,
            ModeType::from_bits_truncate(0o555),
        )?;
        // 创建相关文件
        // status文件
        let status_binding = pid_dir.create(
            "status",
            FileType::File,
            ModeType::from_bits_truncate(0o444),
        )?;
        let status_file: &LockedProcFSInode = status_binding
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .unwrap();
        status_file.0.lock().fdata.pid = Some(pid);
        status_file.0.lock().fdata.ftype = ProcFileType::ProcStatus;

        // exe文件
        let exe_binding = pid_dir.create_with_data(
            "exe",
            FileType::SymLink,
            ModeType::from_bits_truncate(0o444),
            0,
        )?;
        let exe_file = exe_binding
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .unwrap();
        exe_file.0.lock().fdata.pid = Some(pid);
        exe_file.0.lock().fdata.ftype = ProcFileType::ProcExe;

        // fd dir
        let fd = pid_dir.create("fd", FileType::Dir, ModeType::from_bits_truncate(0o555))?;
        let fd = fd.as_any_ref().downcast_ref::<LockedProcFSInode>().unwrap();
        fd.0.lock().fdata.ftype = ProcFileType::ProcFdDir;
        //todo: 创建其他文件

        return Ok(());
    }

    /// @brief 解除进程注册
    ///
    pub fn unregister_pid(&self, pid: RawPid) -> Result<(), SystemError> {
        // 获取当前inode
        let proc: Arc<dyn IndexNode> = self.root_inode();
        // 获取进程文件夹
        let pid_dir: Arc<dyn IndexNode> = proc.find(&pid.to_string())?;
        // 删除进程文件夹下文件
        pid_dir.unlink("status")?;
        pid_dir.unlink("exe")?;
        pid_dir.rmdir("fd")?;

        // 查看进程文件是否还存在
        // let pf= pid_dir.find("status").expect("Cannot find status");

        // 删除进程文件夹
        proc.unlink(&pid.to_string())?;

        return Ok(());
    }
}

impl LockedProcFSInode {
    fn dynamical_find_fd(&self, fd: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // ::log::info!("ProcFS: Dynamically opening fd files for current process.");
        let fd = fd.parse::<i32>().map_err(|_| SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();
        let fd_table = pcb.fd_table();
        let fd_table = fd_table.read();
        let file = fd_table.get_file_by_fd(fd);
        if file.is_some() {
            let _ = self.unlink(&fd.to_string());
            let fd_file = self.create(
                &fd.to_string(),
                FileType::SymLink,
                ModeType::from_bits_truncate(0o444),
            )?;
            let fd_file_proc = fd_file
                .as_any_ref()
                .downcast_ref::<LockedProcFSInode>()
                .unwrap();
            fd_file_proc.0.lock().fdata.fd = fd;
            fd_file_proc.0.lock().fdata.ftype = ProcFileType::ProcFdFile;
            return Ok(fd_file);
        } else {
            return Err(SystemError::ENOENT);
        }
    }

    fn dynamical_list_fd(&self) -> Result<Vec<String>, SystemError> {
        // ::log::info!("ProcFS: Dynamically listing fd files for current process");
        let pcb = ProcessManager::current_pcb();
        let fd_table = pcb.fd_table();
        let fd_table = fd_table.read();
        let res = fd_table.iter().map(|(fd, _)| fd.to_string()).collect();
        return Ok(res);
    }
}

impl IndexNode for LockedProcFSInode {
    fn open(
        &self,
        mut data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        // 加锁
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        let proc_ty = inode.fdata.ftype;

        // 如果inode类型为文件夹，则直接返回成功
        if let FileType::Dir = inode.metadata.file_type {
            return Ok(());
        }
        let mut private_data = ProcfsFilePrivateData::new();
        // 根据文件类型获取相应数据
        let file_size = match proc_ty {
            ProcFileType::ProcStatus => inode.open_status(&mut private_data)?,
            ProcFileType::ProcMeminfo => inode.open_meminfo(&mut private_data)?,
            ProcFileType::ProcExe => inode.open_exe(&mut private_data)?,
            ProcFileType::ProcMounts => inode.open_mounts(&mut private_data)?,
            ProcFileType::ProcVersion => inode.open_version(&mut private_data)?,
            ProcFileType::ProcCpuinfo => inode.open_cpuinfo(&mut private_data)?,
            ProcFileType::Default => inode.data.len() as i64,
            ProcFileType::ProcSelf => inode.open_self(&mut private_data)?,
            _ => 0,
        };
        *data = FilePrivateData::Procfs(private_data);
        // 更新metadata里面的文件大小数值
        inode.metadata.size = file_size;
        drop(inode);
        return Ok(());
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut guard = self.0.lock();
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let name = DName::from(name);
        guard.children.remove(&name);
        return Ok(());
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.0.lock().parent.upgrade().ok_or(SystemError::ENOENT)?;
        return Ok(parent as Arc<dyn IndexNode>);
    }

    fn close(&self, mut data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
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
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let inode = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 根据文件类型读取相应数据
        match inode.fdata.ftype {
            ProcFileType::ProcStatus
            | ProcFileType::ProcMeminfo
            | ProcFileType::ProcMounts
            | ProcFileType::ProcVersion
            | ProcFileType::ProcCpuinfo => {
                // 获取数据信息
                let mut private_data = match &*data {
                    FilePrivateData::Procfs(p) => p.clone(),
                    _ => {
                        panic!("ProcFS: FilePrivateData mismatch!");
                    }
                };
                return inode.proc_read(offset, len, buf, &mut private_data);
            }
            ProcFileType::ProcExe => return inode.read_exe_link(buf, offset),
            ProcFileType::ProcSelf => return inode.read_self_link(buf),
            ProcFileType::ProcFdFile => return inode.read_fd_link(buf),

            _ => (),
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
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
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
        inode.metadata.btime = metadata.btime;
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
        let name = DName::from(name);
        // 如果有重名的，则返回
        if inode.children.contains_key(&name) {
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
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type,
                    mode,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::from(data as u32),
                },
                fs: inode.fs.clone(),
                fdata: InodeInfo {
                    pid: None,
                    ftype: ProcFileType::Default,
                    fd: -1,
                },
                dname: name.clone(),
            })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(name, result.clone());

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
        let name = DName::from(name);
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
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
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

        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;

        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);

        return Ok(());
    }

    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
        _flag: RenameFlags,
    ) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        if self.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => {
                return Ok(self
                    .0
                    .lock()
                    .self_ref
                    .upgrade()
                    .ok_or(SystemError::ENOENT)?);
            }

            ".." => {
                return Ok(self.0.lock().parent.upgrade().ok_or(SystemError::ENOENT)?);
            }

            name => {
                if self.0.lock().fdata.ftype == ProcFileType::ProcFdDir {
                    return self.dynamical_find_fd(name);
                }
                // 在子目录项中查找
                return Ok(self
                    .0
                    .lock()
                    .children
                    .get(&DName::from(name))
                    .ok_or(SystemError::ENOENT)?
                    .clone());
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
                    0 => {
                        return Err(SystemError::ENOENT);
                    }
                    1 => {
                        return Ok(key.remove(0));
                    }
                    _ => panic!(
                        "Procfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}",
                        key_len = key.len(),
                        inode_id = inode.metadata.inode_id,
                        to_find = ino
                    ),
                }
            }
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));

        if self.0.lock().fdata.ftype == ProcFileType::ProcFdDir {
            return self.dynamical_list_fd();
        }

        keys.append(
            &mut self
                .0
                .lock()
                .children
                .keys()
                .map(ToString::to_string)
                .collect(),
        );

        return Ok(keys);
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }
}

/// @brief 向procfs注册进程
#[inline(never)]
pub fn procfs_register_pid(pid: RawPid) -> Result<(), SystemError> {
    let root_inode = ProcessManager::current_mntns().root_inode();
    let procfs_inode = root_inode.find("proc")?;

    let procfs_inode = procfs_inode
        .downcast_ref::<LockedProcFSInode>()
        .expect("Failed to find procfs' root inode");
    let fs = procfs_inode.fs();
    let procfs: &ProcFS = fs.as_any_ref().downcast_ref::<ProcFS>().unwrap();

    // 调用注册函数
    procfs.register_pid(pid)?;

    Ok(())
}

/// @brief 在ProcFS中,解除进程的注册
#[inline(never)]
pub fn procfs_unregister_pid(pid: RawPid) -> Result<(), SystemError> {
    let root_inode = ProcessManager::current_mntns().root_inode();
    // 获取procfs实例
    let procfs_inode: Arc<dyn IndexNode> = root_inode.find("proc")?;

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
        info!("Initializing ProcFS...");
        // 创建 procfs 实例
        let procfs: Arc<ProcFS> = ProcFS::new();
        let root_inode = ProcessManager::current_mntns().root_inode();
        // procfs 挂载
        let mntfs = root_inode
            .mkdir("proc", ModeType::from_bits_truncate(0o755))
            .expect("Unabled to find /proc")
            .mount(procfs, MountFlags::empty())
            .expect("Failed to mount at /proc");
        let ino = root_inode.metadata().unwrap().inode_id;
        let mount_path = Arc::new(MountPath::from("/proc"));
        ProcessManager::current_mntns()
            .add_mount(Some(ino), mount_path, mntfs)
            .expect("Failed to add mount for /proc");
        info!("ProcFS mounted.");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
