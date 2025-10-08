#![allow(dead_code)]
//! Cgroup v2 Filesystem Implementation
//! 
//! This module implements the cgroup v2 filesystem based on kernfs,
//! following the Linux kernel design patterns.

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{
            KernFS, KernFSInode, 
            dynamic::DynamicLookup,
            callback::{KernFSCallback, KernCallbackData, KernInodePrivateData},
        },
        vfs::{
            FileSystem, FileSystemMakerData, FsInfo, IndexNode, Magic, 
            SuperBlock, MountableFileSystem,
            syscall::ModeType,
            mount::{MountFlags},
        },
    },
    libs::{
        casting::DowncastArc,
        rwlock::RwLock,
        spinlock::SpinLock,
    },
};

use super::{
    CgroupId, CgroupFlags,
    mem_cgroup::MemCgroup,
    cpu_cgroup::CpuCgroup,
};

/// Cgroup filesystem magic number (same as Linux)
pub const CGROUP2_SUPER_MAGIC: Magic = Magic::from_bits_truncate(0x63677270);

/// Cgroup filesystem name
pub const CGROUP_FS_NAME: &str = "cgroup2";

/// Default cgroup mount point
pub const CGROUP_MOUNT_POINT: &str = "/sys/fs/cgroup";

/// Cgroup filesystem implementation
#[derive(Debug)]
pub struct CgroupFS {
    /// Root cgroup
    root_cgroup: Arc<RwLock<Cgroup>>,
    /// Kernfs instance
    kernfs: Arc<KernFS>,
    /// Filesystem info
    fs_info: FsInfo,
}

impl CgroupFS {
    /// Create a new cgroup filesystem
    pub fn new() -> Result<Arc<Self>, SystemError> {
        // Create kernfs instance
        let kernfs = KernFS::new(CGROUP_FS_NAME);
        
        // Create root cgroup
        let root_cgroup = Arc::new(RwLock::new(Cgroup::new_root()?));
        
        let fs_info = FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        };

        let cgroupfs = Arc::new(Self {
            root_cgroup: root_cgroup.clone(),
            kernfs,
            fs_info,
        });

        // Set up dynamic lookup for root cgroup directory
        cgroupfs.setup_dynamic_lookup()?;

        Ok(cgroupfs)
    }

    /// Set up dynamic lookup for the root cgroup directory
    fn setup_dynamic_lookup(&self) -> Result<(), SystemError> {
        let root_inode = self.kernfs.root_inode()
            .downcast_arc::<KernFSInode>()
            .ok_or(SystemError::EINVAL)?;

        // Create dynamic lookup provider for root cgroup
        let dynamic_lookup = Arc::new(CgroupDynamicLookup::new(
            Arc::downgrade(&self.root_cgroup),
            Arc::downgrade(&root_inode)
        ));

        // Set the dynamic lookup provider
        root_inode.set_dynamic_lookup(dynamic_lookup);

        Ok(())
    }



    /// Get root cgroup
    pub fn root_cgroup(&self) -> Arc<RwLock<Cgroup>> {
        self.root_cgroup.clone()
    }
}

impl FileSystem for CgroupFS {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: self.fs_info.blk_dev_id,
            max_name_len: self.fs_info.max_name_len,
        }
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.kernfs.root_inode()
    }

    fn name(&self) -> &str {
        CGROUP_FS_NAME
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            CGROUP2_SUPER_MAGIC,
            4096, // block size
            0,    // max file size (unlimited)
        )
    }
}

/// Cgroup structure
#[derive(Debug)]
pub struct Cgroup {
    /// Cgroup ID
    id: CgroupId,
    /// Cgroup flags
    flags: SpinLock<CgroupFlags>,
    /// Parent cgroup
    parent: Option<Weak<RwLock<Cgroup>>>,
    /// Child cgroups
    children: RwLock<Vec<Arc<RwLock<Cgroup>>>>,
    /// Cgroup name
    name: String,
    /// Associated kernfs inode
    kn: Option<Arc<KernFSInode>>,
    /// Memory controller
    mem_cgroup: Option<Arc<MemCgroup>>,
    /// CPU controller  
    cpu_cgroup: Option<Arc<CpuCgroup>>,
}

impl Cgroup {
    /// Create root cgroup
    pub fn new_root() -> Result<Self, SystemError> {
        static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        
        Ok(Self {
            id: NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            flags: SpinLock::new(CgroupFlags::NONE),
            parent: None,
            children: RwLock::new(Vec::new()),
            name: String::new(),
            kn: None,
            mem_cgroup: None, // TODO: implement proper root cgroup initialization
            cpu_cgroup: None, // TODO: implement proper root cgroup initialization
        })
    }

    /// Create child cgroup
    pub fn new_child(
        parent: Arc<RwLock<Cgroup>>,
        name: String,
    ) -> Result<Arc<RwLock<Self>>, SystemError> {
        static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        
        let child = Arc::new(RwLock::new(Self {
            id: NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            flags: SpinLock::new(CgroupFlags::NONE),
            parent: Some(Arc::downgrade(&parent)),
            children: RwLock::new(Vec::new()),
            name,
            kn: None,
            mem_cgroup: None, // Will be initialized if needed
            cpu_cgroup: None, // Will be initialized if needed
        }));

        // Add to parent's children
        parent.write().children.write().push(child.clone());

        Ok(child)
    }

    /// Get cgroup ID
    pub fn id(&self) -> CgroupId {
        self.id
    }

    /// Get cgroup name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Check if cgroup has flag
    pub fn has_flag(&self, flag: CgroupFlags) -> bool {
        self.flags.lock().contains(flag)
    }

    /// Set cgroup flag
    pub fn set_flag(&self, flag: CgroupFlags) {
        self.flags.lock().insert(flag);
    }

    /// Clear cgroup flag
    pub fn clear_flag(&self, flag: CgroupFlags) {
        self.flags.lock().remove(flag);
    }
}

/// Cgroup file types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupFileType {
    Controllers,
    SubtreeControl,
    Procs,
    Threads,
    Type,
    MemoryCurrent,
    MemoryMax,
    CpuWeight,
    CpuMax,
    CgroupDir, // 用于 cgroup 目录本身
}

/// Cgroup file callback implementation
/// 
/// This is a simple callback that delegates to the private_data's callback_read/write methods.
/// Similar to ProcFS implementation pattern.
#[derive(Debug)]
struct CgroupFileCallback;

impl KernFSCallback for CgroupFileCallback {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        // 委托给 private_data 的 callback_read 方法
        data.callback_read(buf, offset)
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        // 委托给 private_data 的 callback_write 方法
        data.callback_write(buf, offset)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<crate::filesystem::vfs::PollStatus, SystemError> {
        // cgroup 文件支持读写
        Ok(crate::filesystem::vfs::PollStatus::READ | crate::filesystem::vfs::PollStatus::WRITE)
    }
}

static CGROUP_FILE_CALLBACK: CgroupFileCallback = CgroupFileCallback;

/// Cgroup dynamic lookup provider
/// 
/// This implements the DynamicLookup trait to provide dynamic file creation
/// for cgroup directories, similar to Linux kernel's cgroup v2 implementation.
#[derive(Debug)]
pub struct CgroupDynamicLookup {
    /// Reference to the cgroup this lookup provider serves
    cgroup: Weak<RwLock<Cgroup>>,
    /// Reference to the parent kernfs inode for creating temporary files
    parent_inode: Weak<KernFSInode>,
}

impl CgroupDynamicLookup {
    pub fn new(cgroup: Weak<RwLock<Cgroup>>, parent_inode: Weak<KernFSInode>) -> Self {
        Self { cgroup, parent_inode }
    }

    /// Get the list of files that should exist in this cgroup directory
    fn get_available_files(&self) -> Vec<&'static str> {
        let mut files = vec![
            "cgroup.controllers",
            "cgroup.procs", 
            "cgroup.threads",
        ];

        // Add cgroup.subtree_control and cgroup.type for non-root cgroups
        if let Some(cgroup) = self.cgroup.upgrade() {
            let cgroup_guard = cgroup.read();
            let is_root = cgroup_guard.parent.is_none();
            if !is_root {
                files.push("cgroup.subtree_control");
                files.push("cgroup.type");
            }
            
            // Add controller-specific files based on enabled controllers
            if cgroup_guard.mem_cgroup.is_some() {
                files.push("memory.current");
                files.push("memory.max");
            }
            if cgroup_guard.cpu_cgroup.is_some() {
                files.push("cpu.weight");
                files.push("cpu.max");
            }
        }

        files
    }

    /// Create a cgroup file dynamically
    fn create_cgroup_file(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        let _cgroup = self.cgroup.upgrade().ok_or(SystemError::ENOENT)?;
        let parent_inode = self.parent_inode.upgrade().ok_or(SystemError::ENOENT)?;
        
        // Determine file type from name
        let file_type = match name {
            "cgroup.controllers" => CgroupFileType::Controllers,
            "cgroup.subtree_control" => CgroupFileType::SubtreeControl,
            "cgroup.procs" => CgroupFileType::Procs,
            "cgroup.threads" => CgroupFileType::Threads,
            "cgroup.type" => CgroupFileType::Type,
            "memory.current" => CgroupFileType::MemoryCurrent,
            "memory.max" => CgroupFileType::MemoryMax,
            "cpu.weight" => CgroupFileType::CpuWeight,
            "cpu.max" => CgroupFileType::CpuMax,
            _ => return Ok(None),
        };

        // Create temporary file using kernfs
        let private_data = CgroupKernPrivateData::new(self.cgroup.clone(), file_type);
        let temp_file = parent_inode.create_temporary_file(
            name,
            ModeType::from_bits_truncate(0o644),
            Some(4096),
            Some(KernInodePrivateData::CgroupFS(private_data)),
            Some(&CGROUP_FILE_CALLBACK), // 使用通用 callback，委托给 private_data
        )?;

        Ok(Some(temp_file))
    }
}

impl DynamicLookup for CgroupDynamicLookup {
    fn dynamic_find(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        // Check if this is a valid cgroup file name
        if self.is_valid_entry(name) {
            self.create_cgroup_file(name)
        } else {
            Ok(None)
        }
    }

    fn dynamic_list(&self) -> Result<Vec<String>, SystemError> {
        Ok(self.get_available_files()
            .into_iter()
            .map(|s| s.to_string())
            .collect())
    }

    fn is_valid_entry(&self, name: &str) -> bool {
        self.get_available_files().contains(&name)
    }

    fn create_temporary_entry(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        // 检查是否是有效的 cgroup 名称
        if !self.is_valid_entry(name) {
            return Ok(None);
        }

        // 获取父 inode（应该是 CgroupFS 的根目录）
        let parent_inode = match self.parent_inode.upgrade() {
            Some(inode) => inode,
            None => {
                log::warn!("CgroupDynamicLookup::create_temporary_entry: parent inode not available");
                return Ok(None);
            }
        };

        log::debug!("CgroupDynamicLookup::create_temporary_entry: Creating temporary cgroup directory '{}'", name);

        // 使用 kernfs 的 create_temporary_dir 方法创建临时目录
        let temp_dir = parent_inode.create_temporary_dir(
            name,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o755),
            Some(KernInodePrivateData::CgroupFS(
                CgroupKernPrivateData::new(self.cgroup.clone(), CgroupFileType::Type)
            )),
        )?;

        // 在临时目录中创建基本的 cgroup 控制文件
        let basic_files = [
            "cgroup.controllers",
            "cgroup.subtree_control", 
            "cgroup.procs",
            "cgroup.threads",
            "cgroup.type",
        ];
        
        for file_name in basic_files.iter() {
            if let Some(_file_node) = self.create_cgroup_file(file_name)? {
                // 文件创建成功，可以进行额外的初始化操作
                log::debug!("Created cgroup file: {}", file_name);
            }
        }

        log::debug!("CgroupDynamicLookup::create_temporary_entry: Successfully created temporary cgroup directory '{}'", name);
        Ok(Some(temp_dir as Arc<dyn IndexNode>))
    }
}

/// Cgroup private data for kernfs
#[derive(Debug)]
pub struct CgroupKernPrivateData {
    /// File type
    pub file_type: CgroupFileType,
}

impl CgroupKernPrivateData {
    pub fn new(_cgroup: Weak<RwLock<Cgroup>>, file_type: CgroupFileType) -> Self {
        Self { file_type }
    }

    pub fn file_type(&self) -> CgroupFileType {
        self.file_type
    }

    /// 统一的内容读取函数，消除重复代码
    fn read_content_helper(content: &str, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        let content_bytes = content.as_bytes();
        if offset >= content_bytes.len() {
            return Ok(0);
        }
        let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
        buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
        Ok(len)
    }

    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        let content = match self.file_type {
            CgroupFileType::Controllers => "memory cpu\n",
            CgroupFileType::Procs => "1\n", // 简化实现，显示 init 进程
            CgroupFileType::SubtreeControl => "memory cpu\n", // 显示可用的控制器
            CgroupFileType::Type => "domain\n", // cgroup v2 默认类型
            CgroupFileType::Threads => "1\n", // 简化实现，显示 init 线程
            CgroupFileType::MemoryCurrent => "0\n", // 简化实现，显示当前内存使用
            CgroupFileType::MemoryMax => "max\n", // 简化实现，显示内存限制
            CgroupFileType::CpuWeight => "100\n", // 简化实现，显示 CPU 权重
            CgroupFileType::CpuMax => "max 100000\n", // 简化实现，显示 CPU 限制
            CgroupFileType::CgroupDir => return Err(SystemError::EISDIR), // 目录不能读取
        };
        
        Self::read_content_helper(content, buf, offset)
    }

    pub fn callback_write(&self, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        // 简化实现，暂不支持写入
        // 未来可以根据 file_type 实现不同的写入逻辑
        match self.file_type {
            CgroupFileType::Procs | 
            CgroupFileType::Threads |
            CgroupFileType::SubtreeControl |
            CgroupFileType::MemoryMax |
            CgroupFileType::CpuWeight |
            CgroupFileType::CpuMax => {
                // 这些文件理论上应该支持写入，但目前简化实现
                Err(SystemError::ENOSYS)
            }
            _ => {
                // 只读文件
                Err(SystemError::EACCES)
            }
        }
    }
}



/// Initialize cgroup filesystem infrastructure
pub fn cgroup_fs_init() -> Result<(), SystemError> {
    // Called during kernel initialization to set up the cgroup filesystem.
    // Core cgroup infrastructure should already be initialized by cgroup_init_early().
    // Perform the actual mount of cgroup2 at /sys/fs/cgroup.
    if let Err(e) = mount_cgroup_current_ns() {
        log::error!("cgroup_fs_init: failed to mount cgroup2 at /sys/fs/cgroup: {:?}", e);
        return Err(e);
    }

    log::info!("Cgroup filesystem infrastructure initialized and mounted at /sys/fs/cgroup");
    Ok(())
}

/// Implementation of MountableFileSystem for CgroupFS
impl MountableFileSystem for CgroupFS {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        // Cgroup filesystem doesn't need special mount data
        Ok(None)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let cgroupfs = CgroupFS::new()?;
        Ok(cgroupfs as Arc<dyn FileSystem>)
    }
}

/// Mount cgroup filesystem at /sys/fs/cgroup using kernfs dynamic directory creation
pub fn mount_cgroup_current_ns() -> Result<(), SystemError> {
    log::info!("mount_cgroup_current_ns: Starting cgroup mount");

    // Create directory hierarchy using the existing function
    let cgroup_dir = create_cgroup_mount_point()?;

    log::info!("mount_cgroup_current_ns: Creating CgroupFS instance");
    let cgroupfs = CgroupFS::new().map_err(|e| {
        log::error!("mount_cgroup_current_ns: Failed to create CgroupFS instance: {:?}", e);
        e
    })?;

    log::info!("mount_cgroup_current_ns: Mounting cgroupfs to /sys/fs/cgroup");
    
    // Mount the filesystem using the KernFSInode mount method
    cgroup_dir.mount(cgroupfs as Arc<dyn FileSystem>, MountFlags::empty()).map_err(|e| {
        log::error!("mount_cgroup_current_ns: Failed to mount /sys/fs/cgroup: {:?}", e);
        e
    })?;

    Ok(())
}

/// Create the cgroup mount point directory hierarchy using kernfs
fn create_cgroup_mount_point() -> Result<Arc<KernFSInode>, SystemError> {
    log::info!("create_cgroup_mount_point: Creating /sys/fs/cgroup mount point");

    // 获取 /sys/fs 的 KSet 实例（由 fs_sysfs_init 创建）
    let fs_kset = crate::filesystem::sys_fs::sys_fs_kset();
    log::debug!("create_cgroup_mount_point: Retrieved /sys/fs KSet instance");

    // 从 KSet 中获取底层的 KernFSInode
    let fs_kern_inode = fs_kset.inode()
        .ok_or_else(|| {
            log::error!("create_cgroup_mount_point: /sys/fs KSet has no associated KernFSInode");
            SystemError::ENOENT
        })?;

    // 使用 KernFSInode 的 create_temporary_dir 创建 cgroup 目录
    let cgroup_dir = fs_kern_inode.create_temporary_dir(
        "cgroup",
        ModeType::S_IFDIR | ModeType::from_bits_truncate(0o755),
        Some(KernInodePrivateData::CgroupFS(
            CgroupKernPrivateData::new(Weak::new(), CgroupFileType::CgroupDir)
        )),
    )?;
    
    log::debug!("create_cgroup_mount_point: Created cgroup directory using KernFS");
    
    // 为创建的 cgroup 目录设置动态查找功能，让它能够动态创建子目录和文件
    let dynamic_lookup = Arc::new(CgroupDynamicLookup::new(
        Weak::new(),
        Arc::downgrade(&cgroup_dir)
    ));
    
    cgroup_dir.set_dynamic_lookup(dynamic_lookup);
    log::info!("create_cgroup_mount_point: Successfully created cgroup mount point with dynamic lookup");

    Ok(cgroup_dir)
}


crate::register_mountable_fs!(CgroupFS, CGROUP_FS_MAKER, "cgroup2");