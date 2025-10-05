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
            mount::MountFlags,
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
}

/// Cgroup file callback implementation
#[derive(Debug)]
struct CgroupFileCallback;

impl KernFSCallback for CgroupFileCallback {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        // cgroup 文件打开时不需要特殊处理
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let private_data = data.private_data();
        if let Some(KernInodePrivateData::CgroupFS(cgroup_data)) = private_data.as_ref() {
            match cgroup_data.file_type {
                CgroupFileType::Controllers => {
                    let content = "memory cpu\n";
                    let content_bytes = content.as_bytes();
                    if offset >= content_bytes.len() {
                        return Ok(0);
                    }
                    let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                    buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                    Ok(len)
                }
                CgroupFileType::Procs => {
                    let content = "1\n"; // 简化实现，显示 init 进程
                    let content_bytes = content.as_bytes();
                    if offset >= content_bytes.len() {
                        return Ok(0);
                    }
                    let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                    buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                    Ok(len)
                }
                _ => {
                    let content = "\n"; // 其他文件返回空内容
                    let content_bytes = content.as_bytes();
                    if offset >= content_bytes.len() {
                        return Ok(0);
                    }
                    let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                    buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                    Ok(len)
                }
            }
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        // 简化实现，暂不支持写入
        Err(SystemError::ENOSYS)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<crate::filesystem::vfs::PollStatus, SystemError> {
        // cgroup 文件默认可读
        Ok(crate::filesystem::vfs::PollStatus::READ)
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
            Some(&CGROUP_FILE_CALLBACK),
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

    fn create_temporary_entry(&self, _name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        // For cgroup files, we don't create temporary entries
        // Instead, we rely on the parent directory to create files when needed
        Ok(None)
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

    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        match self.file_type {
            CgroupFileType::Controllers => {
                let content = "memory cpu\n";
                let content_bytes = content.as_bytes();
                if offset >= content_bytes.len() {
                    return Ok(0);
                }
                let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                Ok(len)
            }
            CgroupFileType::Procs => {
                let content = "1\n"; // 简化实现，显示 init 进程
                let content_bytes = content.as_bytes();
                if offset >= content_bytes.len() {
                    return Ok(0);
                }
                let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                Ok(len)
            }
            _ => {
                let content = "\n"; // 其他文件返回空内容
                let content_bytes = content.as_bytes();
                if offset >= content_bytes.len() {
                    return Ok(0);
                }
                let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
                buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
                Ok(len)
            }
        }
    }

    pub fn callback_write(&self, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        // 简化实现，暂不支持写入
        Err(SystemError::ENOSYS)
    }
}



/// Initialize cgroup filesystem infrastructure
pub fn cgroup_fs_init() -> Result<(), SystemError> {
    // This will be called during kernel initialization
    // to set up the cgroup filesystem
    // Note: Core cgroup infrastructure should already be initialized
    // by cgroup_init_early() before this function is called
    // TODO: Register filesystem type
    // TODO: Create mount point
    // TODO: Mount cgroup filesystem
    
    log::info!("Cgroup filesystem infrastructure initialized");
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

/// Mount cgroup filesystem at /sys/fs/cgroup (similar to mount_proc_current_ns)
pub fn mount_cgroup_current_ns() -> Result<(), SystemError> {
    log::info!("mount_cgroup_current_ns: Starting cgroup mount");

    // Get the root mount filesystem to ensure we're working with MountFSInode
    let current_mntns = crate::process::ProcessManager::current_mntns();
    let root_mount_fs = current_mntns.root_mntfs();
    let root_inode = root_mount_fs.mountpoint_root_inode();
    
    // Create /sys directory if it doesn't exist
    let sys_dir = match root_inode.find("sys") {
        Ok(existing_dir) => {
            log::info!("mount_cgroup_current_ns: Found existing /sys directory");
            existing_dir
        }
        Err(_) => {
            log::info!("mount_cgroup_current_ns: Creating new /sys directory");
            root_inode.mkdir("sys", ModeType::from_bits_truncate(0o755))?
        }
    };

    // Create /sys/fs directory if it doesn't exist
    let fs_dir = match sys_dir.find("fs") {
        Ok(existing_dir) => {
            log::info!("mount_cgroup_current_ns: Found existing /sys/fs directory");
            existing_dir
        }
        Err(_) => {
            log::info!("mount_cgroup_current_ns: Creating new /sys/fs directory");
            sys_dir.mkdir("fs", ModeType::from_bits_truncate(0o755))?
        }
    };

    // Create /sys/fs/cgroup directory if it doesn't exist
    let cgroup_dir = match fs_dir.find("cgroup") {
        Ok(existing_dir) => {
            log::info!("mount_cgroup_current_ns: Found existing /sys/fs/cgroup directory");
            // Check if already mounted by looking for cgroup files
            if let Ok(entries) = existing_dir.list() {
                log::info!("mount_cgroup_current_ns: /sys/fs/cgroup has {} entries: {:?}", entries.len(), entries);
                if entries.iter().any(|e| e.starts_with("cgroup.")) {
                    log::info!("mount_cgroup_current_ns: /sys/fs/cgroup already has cgroup files, skipping mount");
                    return Ok(());
                }
            }
            existing_dir
        }
        Err(_) => {
            log::info!("mount_cgroup_current_ns: Creating new /sys/fs/cgroup directory");
            fs_dir.mkdir("cgroup", ModeType::from_bits_truncate(0o755))?
        }
    };

    log::info!("mount_cgroup_current_ns: Creating CgroupFS instance");
    let cgroupfs = match CgroupFS::new() {
        Ok(fs) => {
            log::info!("mount_cgroup_current_ns: CgroupFS instance created successfully");
            fs
        }
        Err(e) => {
            log::error!("mount_cgroup_current_ns: Failed to create CgroupFS instance: {:?}", e);
            return Err(e);
        }
    };

    log::info!("mount_cgroup_current_ns: Mounting cgroupfs to /sys/fs/cgroup");
    // Mount the filesystem on the directory
    match cgroup_dir.mount(cgroupfs, MountFlags::empty()) {
        Ok(mount_fs) => {
            log::info!("mount_cgroup_current_ns: Successfully mounted /sys/fs/cgroup");
            log::info!("mount_cgroup_current_ns: Mount filesystem: {:?}", mount_fs.name());
            
            // Verify mount by listing directory contents
            if let Ok(entries) = cgroup_dir.list() {
                log::info!("mount_cgroup_current_ns: After mount, /sys/fs/cgroup has {} entries: {:?}", entries.len(), entries);
            } else {
                log::warn!("mount_cgroup_current_ns: Failed to list /sys/fs/cgroup contents after mount");
            }
        }
        Err(e) => {
            log::error!("mount_cgroup_current_ns: Failed to mount /sys/fs/cgroup: {:?}", e);
            return Err(e);
        }
    }

    Ok(())
}


crate::register_mountable_fs!(CgroupFS, CGROUP_FS_MAKER, "cgroup2");