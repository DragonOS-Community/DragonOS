use alloc::{string::{String, ToString}, sync::Arc, vec::Vec};
use core::any::Any;
use ::log::info;
use system_error::SystemError;

use crate::{
    libs::{casting::DowncastArc, once::Once},
    process::{ProcessManager, namespace::pid_namespace::PidNamespace},
    filesystem::{
        kernfs::{KernFS, KernFSInode},
        vfs::{FileSystem, IndexNode, FsInfo, SuperBlock, mount::MountFlags, syscall::ModeType, MountableFileSystem, FileSystemMakerData},
    },
};

use super::{
    data::{sys_config::init_system_configs, process_info::ProcessId},
    file::ProcFSKernPrivateData,
    registry::{proc_register_instance, proc_notify_pid_register, proc_notify_pid_exit},
    dynamic_pid_lookup::ProcFSDynamicPidLookup,
};
use crate::filesystem::procfs::proc_root_inode::ProcFSRootInode;

pub fn procfs_init() -> Result<(), SystemError> {
    // Linux风格：此处仅进行“注册/占位”，不做挂载，也不访问进程/挂载命名空间
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        info!("ProcFS: registered (no early mount; mount deferred to userspace/unshare)");
    });
    Ok(())
}



/// 在当前命名空间将 procfs 挂载到 /proc（按需调用的“后续挂载”逻辑）
/// - 若 /proc 已挂载则直接返回 Ok
/// - 将实例与当前 pidns 绑定，并登记到注册表以接收进程事件通知
pub fn mount_proc_current_ns() -> Result<(), SystemError> {
    use ::log::info;
    info!("mount_proc_current_ns: Starting proc mount");
    
    // 需要在进程子系统就绪后调用
    if !crate::process::ProcessManager::initialized() {
        info!("mount_proc_current_ns: ProcessManager not initialized");
        return Err(SystemError::EBUSY);
    }

    let mntns = crate::process::ProcessManager::current_mntns();
    let mount_point = mntns.get_mount_point("/proc");
    info!("mount_proc_current_ns: get_mount_point result: {:?}", mount_point.is_some());
    if let Some((_mp, rest, _)) = &mount_point {
        if rest.is_empty() {
            info!("mount_proc_current_ns: /proc already mounted (exact), skipping");
            return Ok(());
        }
    }
    
    // 检查 /proc 目录是否真的有内容
    let root_inode = mntns.root_inode();
    if let Ok(proc_dir) = root_inode.find("proc") {
        match proc_dir.list() {
            Ok(entries) => {
                info!("mount_proc_current_ns: /proc directory has {} entries: {:?}", entries.len(), entries);
                if !entries.is_empty() && entries.len() > 2 { // 不只是 . 和 ..
                    info!("mount_proc_current_ns: /proc already has content, skipping mount");
                    return Ok(());
                }
            }
            Err(e) => {
                info!("mount_proc_current_ns: Failed to list /proc directory: {:?}", e);
            }
        }
    }

    info!("mount_proc_current_ns: Creating ProcFS instance");
    // 绑定当前 pidns 创建 ProcFS 实例
    let pid_ns = crate::process::ProcessManager::current_pcb().active_pid_ns();
    let procfs = ProcFS::new_for_pid_ns(pid_ns);

    // 登记挂载实例，便于按 pidns 定向清理/更新 /proc/[pid]
    proc_register_instance(&procfs);

    info!("mount_proc_current_ns: Mounting procfs to /proc");
    // 在 / 上创建 proc 目录并挂载
    // 尝试找到现有的 proc 目录，如果不存在则创建
    let proc_dir = match root_inode.find("proc") {
        Ok(existing_dir) => {
            info!("mount_proc_current_ns: Found existing /proc directory");
            existing_dir
        }
        Err(_) => {
            info!("mount_proc_current_ns: Creating new /proc directory");
            root_inode.mkdir("proc", ModeType::from_bits_truncate(0o755))?
        }
    };
    
    match proc_dir.mount(procfs.clone() as alloc::sync::Arc<dyn FileSystem>, MountFlags::empty()) {
        Ok(_) => {
            info!("mount_proc_current_ns: Successfully mounted /proc");

            // 解决时序问题：重新通知所有现有进程进行注册
            let pid_ns = crate::process::ProcessManager::current_pcb().active_pid_ns();
            let current_pid = crate::process::ProcessManager::current_pcb().raw_pid();
            
            info!("mount_proc_current_ns: Current process PID {} in namespace level {}", 
                  current_pid.data(), pid_ns.level());
            
            // 当前进程目录将通过动态查找按需创建
            let current_pcb = crate::process::ProcessManager::current_pcb();
            let current_process_pid = current_pcb.pid();
            let current_ns_pid = current_process_pid.pid_nr_ns(&pid_ns);
            
            info!("mount_proc_current_ns: Current process global PID {}, ns PID {} - will be created dynamically when accessed", 
                  current_pid.data(), current_ns_pid.data());
            
            info!("mount_proc_current_ns: ProcFS mounted with pure dynamic lookup - no pre-registration of existing processes");

            // 验证挂载是否成功
            if let Ok(entries) = proc_dir.list() {
                info!("mount_proc_current_ns: After mount, /proc has {} entries: {:?}", entries.len(), entries);
            }
        }
        Err(e) => {
            info!("mount_proc_current_ns: Failed to mount /proc: {:?}", e);
            return Err(e);
        }
    }

    Ok(())
}


pub fn procfs_register_pid(pid: ProcessId) -> Result<(), SystemError> {
    // Use notifier to create /proc/<pid> on all instances in the pidns
    proc_notify_pid_register(pid);
    Ok(())
}

pub fn procfs_unregister_pid(pid: ProcessId) -> Result<(), SystemError> {
    // Use notifier to remove /proc/<pid> on all instances in the pidns
    proc_notify_pid_exit(pid);
    Ok(())
}



#[derive(Debug)]
pub struct ProcFSInfo {
    pub pid_ns: Arc<PidNamespace>,   
}

impl ProcFSInfo {
    pub fn new(pid_ns: Arc<PidNamespace>) -> Self {
        ProcFSInfo { pid_ns }
    }
}

#[derive(Debug)]
pub struct ProcFS {
    /// 对外暴露的根 inode（包装器，提供纯动态视图）
    root_wrapper: Arc<ProcFSRootInode>,
    /// 底层 kernfs 根 inode（内部实际挂载点）
    root_inode: Arc<KernFSInode>,
    /// kernfs实例
    kernfs: Arc<KernFS>,
    info: Arc<ProcFSInfo>,
}

impl FileSystem for ProcFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // 返回包装器，让 list/find 走 ProcFSRootInode 以使用纯动态视图
        self.root_wrapper.clone()
    }

    fn info(&self) -> FsInfo {
        self.kernfs.info()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "proc"
    }

    fn super_block(&self) -> SuperBlock {
        self.kernfs.super_block()
    }
}

impl ProcFS {

    // new()仅用于系统初始化的全局procfs，语义上等同绑定到 init/active pidns
    #[allow(dead_code)] // 备用构造：不通过挂载路径使用，仅用于特殊初始化/调试
    pub fn new() -> Arc<Self> {
        let kernfs: Arc<KernFS> = KernFS::new("proc");
        let root_inode: Arc<KernFSInode> = kernfs.root_inode().downcast_arc().unwrap();
        
        let pid_ns = ProcessManager::current_pcb().active_pid_ns();
        let info = Arc::new(ProcFSInfo::new(pid_ns.clone()));

        // 先创建一个临时 Arc<Self> 占位以传入 Weak
        let procfs_temp = Arc::new_cyclic(|weak_self| {
            // 先构造最小壳，稍后填充 root_wrapper
            ProcFS {
                // 临时占位，稍后真正赋值
                root_wrapper: ProcFSRootInode::new(root_inode.clone(), Arc::new(ProcFSDynamicPidLookup::new(pid_ns.clone(), weak_self.clone()))),
                root_inode: root_inode.clone(),
                kernfs: kernfs.clone(),
                info: info.clone(),
            }
        });

        // 设置动态 PID 查找（挂在底层 kernfs 根）
        let dynamic_lookup = Arc::new(ProcFSDynamicPidLookup::new(pid_ns, Arc::downgrade(&procfs_temp)));
        procfs_temp.root_inode.set_dynamic_lookup(dynamic_lookup.clone());

        // 用相同 dynamic_lookup 构造真正的包装器
        let wrapper = ProcFSRootInode::new(procfs_temp.root_inode.clone(), dynamic_lookup);
        // 安全替换 root_wrapper 字段
        unsafe {
            let ptr = Arc::as_ptr(&procfs_temp) as *mut ProcFS;
            (*ptr).root_wrapper = wrapper;
        }

        init_system_configs();
        procfs_temp.init_directory_structure().expect("Failed to initialize ProcFS");
        
        procfs_temp
    }

    pub fn new_for_pid_ns(pid_ns: Arc<PidNamespace>) -> Arc<Self> {
        let kernfs: Arc<KernFS> = KernFS::new("proc");
        let root_inode: Arc<KernFSInode> = kernfs.root_inode().downcast_arc().unwrap();
        let info = Arc::new(ProcFSInfo::new(pid_ns.clone()));

        // 先创建一个临时 Arc<Self> 占位以传入 Weak
        let procfs_temp = Arc::new_cyclic(|weak_self| {
            // 先构造最小壳，稍后填充 root_wrapper
            ProcFS {
                root_wrapper: ProcFSRootInode::new(root_inode.clone(), Arc::new(ProcFSDynamicPidLookup::new(pid_ns.clone(), weak_self.clone()))),
                root_inode: root_inode.clone(),
                kernfs: kernfs.clone(),
                info: info.clone(),
            }
        });

        // 设置动态 PID 查找（挂在底层 kernfs 根）
        let dynamic_lookup = Arc::new(ProcFSDynamicPidLookup::new(pid_ns, Arc::downgrade(&procfs_temp)));
        procfs_temp.root_inode.set_dynamic_lookup(dynamic_lookup.clone());

        // 将挂载实例上下文注入根inode的私有数据，供所有kernfs回调获取per-mount语义
        {
            let weak_info = Arc::downgrade(&procfs_temp.info);
            *procfs_temp.root_inode.private_data_mut() =
                Some(crate::filesystem::kernfs::callback::KernInodePrivateData::ProcFS(
                    ProcFSKernPrivateData::MountContext(weak_info),
                ));
        }

        // 用相同 dynamic_lookup 构造真正的包装器
        let wrapper = ProcFSRootInode::new(procfs_temp.root_inode.clone(), dynamic_lookup);
        unsafe {
            let ptr = Arc::as_ptr(&procfs_temp) as *mut ProcFS;
            (*ptr).root_wrapper = wrapper;
        }

        init_system_configs();
        procfs_temp.init_directory_structure().expect("Failed to initialize ProcFS");

        procfs_temp
    }    

    pub fn pid_namespace(&self) -> &Arc<PidNamespace> {
        &self.info.pid_ns
    }

    #[allow(dead_code)]
    pub fn root_inode(&self) -> &Arc<KernFSInode> {
        &self.root_inode
    }
    #[allow(dead_code)]
    fn fs(&self) -> &Arc<KernFS> {
        &self.kernfs
    }


    #[allow(dead_code)] // 预留：遍历命名空间内当前活动进程列表
    fn get_current_process_list(&self) -> Result<Vec<ProcessId>, SystemError> {
        if !ProcessManager::initialized() {
            info!("Process management system not initialized yet, returning empty process list");
            return Ok(Vec::new());
        }

        // 这里原语义返回的是 ProcessId，但我们需要遵循绑定的 pidns。
        // 当前系统的 ProcessId 与 pidns.RawPid 等价使用方式不明确，维持接口类型不变：
        // 将 ns 内的 RawPid 按 ProcessId::new 包装返回，仅用于排序/显示目录名。
        let ns_pids = self.info.pid_ns.get_all_pids();

        let mut list = Vec::new();
        for raw in ns_pids {
            // 仅当该 ns 中记录的 pid 对应的进程当前仍然存在且未退出才显示
            if let Some(pid) = self.info.pid_ns.find_pid_in_ns(raw) {
                if let Some(task) = pid.pid_task(crate::process::pid::PidType::PID) {
                    // 检查进程是否已经退出
                    let state = task.sched_info().inner_lock_read_irqsave().state();
                    if !state.is_exited() {
                        // 注意：这里只是为了复用后续排序/显示逻辑，将 RawPid 转为 ProcessId
                        // ProcessId::new 接受 u32（或等价类型），这里 raw.data() 为 usize，做安全转换
                        let n = usize::try_from(raw.data()).unwrap();
                        list.push(ProcessId::new(n));
                    }
                }
            }
        }

        list.sort_by_key(|pid| pid.data());
        info!("Found {} active processes in PID namespace", list.len());
        Ok(list)
    }


 
    fn init_directory_structure(&self) -> Result<(), SystemError> {
        let root = &self.root_inode;
        
        self.create_system_files(root)?;
        self.create_sys_directory(root)?;
        
        Ok(())
    }

}

// 让 ProcFS 支持通过 VFS 注册/挂载（Linux 风格：注册时不挂载，mount 时绑定当前 pidns）
impl MountableFileSystem for ProcFS {
    fn make_mount_data(_raw_data: Option<&str>, _source: &str)
        -> Result<Option<alloc::sync::Arc<dyn FileSystemMakerData + 'static>>, system_error::SystemError>
    {
        // 暂不解析 mount data（如 hidepid/gid 等），保持最小实现
        Ok(None)
    }

    fn make_fs(_data: Option<&dyn FileSystemMakerData>)
        -> Result<alloc::sync::Arc<dyn FileSystem + 'static>, system_error::SystemError>
    {
        let pid_ns = crate::process::ProcessManager::current_pcb().active_pid_ns();
        let inst = ProcFS::new_for_pid_ns(pid_ns);
        // 登记实例
        proc_register_instance(&inst);
        Ok(inst as alloc::sync::Arc<dyn FileSystem>)
    }
}



impl ProcFS {


    #[allow(dead_code)] // 预留：自定义根目录分页读取
    fn readdir_root(&self, offset: usize) -> Result<Vec<String>, SystemError> {
        let mut entries = Vec::new();

        // 固定静态项：保持在前
        let static_entries = vec![
            ".", "..", "version", "cpuinfo", "meminfo", "uptime",
            "loadavg", "stat", "interrupts", "devices", "filesystems",
            "mounts", "cmdline", "sys"
        ];
        // 先处理静态项的分页
        for (i, entry) in static_entries.iter().enumerate() {
            if i >= offset {
                entries.push(entry.to_string());
            }
        }
        // 计算进程项的偏移
        let process_offset = if offset > static_entries.len() {
            offset - static_entries.len()
        } else {
            0
        };

        // 使用绑定 pidns 的 ns 内 PID（从 1 起），作为目录名
        let mut ns_pids: Vec<_> = self.info.pid_ns.get_all_pids();
        ns_pids.sort_by_key(|raw| raw.data());

        // 进程项分页输出（仅添加现存且未退出的 nspid；子项创建失败将在 lookup 阶段被容错）
        for (i, raw) in ns_pids.iter().enumerate() {
            if i < process_offset {
                continue;
            }
            if let Some(pid) = self.info.pid_ns.find_pid_in_ns(*raw) {
                if let Some(task) = pid.pid_task(crate::process::pid::PidType::PID) {
                    // 检查进程是否已经退出
                    let state = task.sched_info().inner_lock_read_irqsave().state();
                    if !state.is_exited() {
                        entries.push(raw.data().to_string());
                    }
                }
            }
        }

        Ok(entries)
    }
}