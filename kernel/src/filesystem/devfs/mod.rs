/// 导出devfs的模块
pub mod full_dev;
pub mod null_dev;
pub mod random_dev;
pub mod zero_dev;

use super::{
    devpts::{DevPtsFs, LockedDevPtsFSInode},
    vfs::{
        file::FileFlags, utils::DName, vcore::generate_inode_id, FilePrivateData, FileSystem,
        FileSystemMakerData, FileType, FsInfo, IndexNode, InodeFlags, InodeMode, Magic, Metadata,
        MountableFileSystem, SuperBlock, FSMAKER,
    },
};

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        devfs::zero_dev::LockedZeroInode,
        vfs::{mount::MountFlags, produce_fs},
    },
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
        once::Once,
    },
    mm::{
        fault::{PageFaultHandler, PageFaultMessage},
        VmFaultReason,
    },
    process::ProcessManager,
    register_mountable_fs,
    time::PosixTimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use linkme::distributed_slice;
use log::{error, info, warn};
use system_error::SystemError;

const DEVFS_BLOCK_SIZE: u64 = 4096;
const DEVFS_MAX_NAMELEN: usize = 255;

static DEVFS_INIT: Once = Once::new();
static mut DEVFS_INSTANCE: Option<Arc<DevFS>> = None;

fn devfs_global_instance() -> Arc<DevFS> {
    DEVFS_INIT.call_once(|| unsafe {
        DEVFS_INSTANCE = Some(DevFS::new());
    });

    unsafe { DEVFS_INSTANCE.as_ref().unwrap().clone() }
}

#[derive(Debug)]
struct DevNodePath {
    dirs: Vec<DName>,
    basename: DName,
}

impl DevNodePath {
    fn parse(name: &str) -> Result<Self, SystemError> {
        let normalized = name.replace('!', "/");
        let normalized = normalized.trim_start_matches('/');
        if normalized.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let mut components = Vec::new();
        for component in normalized.split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(SystemError::EINVAL);
            }
            if component.len() > DEVFS_MAX_NAMELEN {
                return Err(SystemError::ENAMETOOLONG);
            }
            components.push(DName::from(component));
        }

        let basename = components.pop().ok_or(SystemError::EINVAL)?;
        Ok(Self {
            dirs: components,
            basename,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DeviceIndexKind {
    Char,
    Block,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeviceIndexKey {
    kind: DeviceIndexKind,
    dev_t: DeviceNumber,
}

impl DeviceIndexKey {
    fn from_file_type(file_type: FileType, dev_t: DeviceNumber) -> Option<Self> {
        let kind = match file_type {
            FileType::BlockDevice => DeviceIndexKind::Block,
            FileType::CharDevice | FileType::KvmDevice | FileType::FramebufferDevice => {
                DeviceIndexKind::Char
            }
            _ => return None,
        };

        Some(Self { kind, dev_t })
    }
}

/// @brief dev文件系统
#[derive(Debug)]
pub struct DevFS {
    // 文件系统根节点
    root_inode: Arc<LockedDevFSInode>,
    super_block: SuperBlock,
    operation_lock: Mutex<()>,
    device_by_devnum: Mutex<BTreeMap<DeviceIndexKey, Vec<Arc<dyn IndexNode>>>>,
}

fn is_zero_inode(pfm: &PageFaultMessage) -> bool {
    let vma = pfm.vma();
    let vma_guard = vma.lock();
    match vma_guard.vm_file() {
        Some(file) => {
            let inode = file.inode();
            inode
                .as_any_ref()
                .downcast_ref::<LockedZeroInode>()
                .is_some()
        }
        None => false,
    }
}

impl FileSystem for DevFS {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> super::vfs::FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: DEVFS_MAX_NAMELEN,
        };
    }

    fn name(&self) -> &str {
        "devtmpfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        if !is_zero_inode(pfm) {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        PageFaultHandler::zero_fault(pfm)
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        if !is_zero_inode(pfm) {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        VmFaultReason::empty()
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        if !is_zero_inode(pfm) {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        PageFaultHandler::zero_map_pages(pfm, start_pgoff, end_pgoff)
    }
}

impl DevFS {
    pub fn new() -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::TMPFS_MAGIC,
            DEVFS_BLOCK_SIZE,
            DEVFS_MAX_NAMELEN as u64,
        );
        // 初始化root inode
        let root: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(Mutex::new(
            // /dev 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            DevFSInode::new(FileType::Dir, InodeMode::from_bits_truncate(0o755), 0),
        )));

        // panic!("devfs root inode id: {:?}", root.0.lock().metadata.inode_id);

        let devfs: Arc<DevFS> = Arc::new(DevFS {
            root_inode: root,
            super_block,
            operation_lock: Mutex::new(()),
            device_by_devnum: Mutex::new(BTreeMap::new()),
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: MutexGuard<DevFSInode> = devfs.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&devfs.root_inode);
        root_guard.self_ref = Arc::downgrade(&devfs.root_inode);
        root_guard.fs = Arc::downgrade(&devfs);
        // 释放锁
        drop(root_guard);

        let root: &Arc<LockedDevFSInode> = &devfs.root_inode;
        // Linux 用户态会通过 /dev/fd/N[/path] 重新访问 fd 派生的可见路径。
        // DragonOS 的真实对象解析能力在 /proc/self/fd，因此这里补兼容入口。
        root.add_dev_symlink("/proc/self/fd", "fd")
            .expect("DevFS: Failed to create /dev/fd");
        devfs.register_bultinin_device();

        // debug!("ls /dev: {:?}", root.list());
        return devfs;
    }

    /// @brief 注册系统内部自带的设备
    fn register_bultinin_device(&self) {
        use crate::driver::{
            base::device::{
                device_number::{DeviceNumber, Major},
                IdTable,
            },
            tty::tty_device::{PtyType, TtyDevice, TtyType},
        };
        use crate::filesystem::fuse::dev::LockedFuseDevInode;
        use full_dev::LockedFullInode;
        use null_dev::LockedNullInode;
        use random_dev::LockedRandomInode;
        use zero_dev::LockedZeroInode;

        self.register_builtin_root_device("null", LockedNullInode::new())
            .expect("DevFS: Failed to register /dev/null");
        self.register_builtin_root_device("zero", LockedZeroInode::new())
            .expect("DevFS: Failed to register /dev/zero");
        self.register_builtin_root_device("full", LockedFullInode::new())
            .expect("DevFS: Failed to register /dev/full");
        self.register_builtin_root_device(
            "random",
            LockedRandomInode::new("random", DeviceNumber::new(Major::new(1), 8)),
        )
        .expect("DevFS: Failed to register /dev/random");
        self.register_builtin_root_device(
            "urandom",
            LockedRandomInode::new("urandom", DeviceNumber::new(Major::new(1), 9)),
        )
        .expect("DevFS: Failed to register /dev/urandom");
        self.register_builtin_root_device("fuse", LockedFuseDevInode::new())
            .expect("DevFS: Failed to register /dev/fuse");

        let ptmx_devnum = DeviceNumber::new(Major::TTYAUX_MAJOR, 2);
        let ptmx = TtyDevice::new(
            "ptmx".to_string(),
            IdTable::new("ptmx".to_string(), Some(ptmx_devnum)),
            TtyType::Pty(PtyType::Ptm),
        );
        let mut metadata = ptmx
            .metadata()
            .expect("DevFS: Failed to read /dev/ptmx metadata");
        metadata.mode = InodeMode::from_bits_truncate(0o666) | InodeMode::S_IFCHR;
        metadata.raw_dev = ptmx_devnum;
        ptmx.set_metadata(&metadata)
            .expect("DevFS: Failed to set /dev/ptmx metadata");
        self.register_builtin_root_device("ptmx", ptmx)
            .expect("DevFS: Failed to register /dev/ptmx");
    }

    fn register_builtin_root_device<T: DeviceINode + 'static>(
        &self,
        name: &str,
        device: Arc<T>,
    ) -> Result<(), SystemError> {
        let dev_root = self.root_inode.clone();
        device.set_fs(dev_root.0.lock().fs.clone());
        device.set_parent(Arc::downgrade(&dev_root));
        let device_inode: Arc<dyn IndexNode> = device;
        dev_root.add_dev_or_same(name, device_inode.clone())?;
        self.remember_device_node(device_inode)?;
        Ok(())
    }

    fn remember_device_node(&self, device: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let metadata = device.metadata()?;
        let raw_dev = metadata.raw_dev;
        if raw_dev != DeviceNumber::default() {
            let Some(key) = DeviceIndexKey::from_file_type(metadata.file_type, raw_dev) else {
                return Ok(());
            };

            let mut devices = self.device_by_devnum.lock();
            let entries = devices.entry(key).or_default();
            if !entries
                .iter()
                .any(|registered| Arc::ptr_eq(registered, &device))
            {
                entries.push(device);
            }
        }
        Ok(())
    }

    fn forget_device_node_if_same(&self, device: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let metadata = device.metadata()?;
        let raw_dev = metadata.raw_dev;
        if raw_dev == DeviceNumber::default() {
            return Ok(());
        }
        let Some(key) = DeviceIndexKey::from_file_type(metadata.file_type, raw_dev) else {
            return Ok(());
        };

        let mut devices = self.device_by_devnum.lock();
        if let Some(entries) = devices.get_mut(&key) {
            entries.retain(|registered| !Arc::ptr_eq(registered, device));
            if entries.is_empty() {
                devices.remove(&key);
            }
        }
        Ok(())
    }

    fn lookup_device_by_devnum(
        &self,
        file_type: FileType,
        dev_t: DeviceNumber,
    ) -> Option<Arc<dyn IndexNode>> {
        let key = DeviceIndexKey::from_file_type(file_type, dev_t)?;
        self.device_by_devnum
            .lock()
            .get(&key)
            .and_then(|entries| entries.first().cloned())
    }

    /// @brief 在devfs内注册设备
    ///
    /// @param name 设备名称
    /// @param device 设备节点的结构体
    pub fn register_device<T: DeviceINode + 'static>(
        &self,
        name: &str,
        device: Arc<T>,
    ) -> Result<(), SystemError> {
        self.register_device_dyn(name, device).map(|_| ())
    }

    /// @brief Register a dynamic device node in devfs
    ///
    /// This interface is used by the device core layer to register a node after
    /// dynamic conversion from `Arc<dyn Device>`.
    pub fn register_device_dyn(
        &self,
        name: &str,
        device: Arc<dyn DeviceINode>,
    ) -> Result<bool, SystemError> {
        let _guard = self.operation_lock.lock();
        let dev_root_inode = self.root_inode.clone();
        let metadata = device.metadata()?;
        if !matches!(
            metadata.file_type,
            FileType::CharDevice
                | FileType::BlockDevice
                | FileType::KvmDevice
                | FileType::FramebufferDevice
        ) {
            return Err(SystemError::ENOSYS);
        }

        let path = DevNodePath::parse(name)?;
        let parent = dev_root_inode.ensure_kernel_parent_dir(&path)?;
        device.set_fs(parent.0.lock().fs.clone());
        device.set_parent(Arc::downgrade(&parent));
        let device_inode: Arc<dyn IndexNode> = device;
        let inserted = parent.add_dev_or_same(path.basename.as_ref(), device_inode.clone())?;
        self.remember_device_node(device_inode)?;
        Ok(inserted)
    }

    /// @brief 卸载设备
    pub fn unregister_device<T: DeviceINode + 'static>(
        &self,
        name: &str,
        device: Arc<T>,
    ) -> Result<(), SystemError> {
        self.unregister_device_dyn(name, device)
    }

    /// @brief Unregister a dynamic device node
    pub fn unregister_device_dyn(
        &self,
        name: &str,
        device: Arc<dyn DeviceINode>,
    ) -> Result<(), SystemError> {
        let _guard = self.operation_lock.lock();
        let dev_root_inode: Arc<LockedDevFSInode> = self.root_inode.clone();
        let expected: Arc<dyn IndexNode> = device.clone();
        self.forget_device_node_if_same(&expected)?;
        let path = DevNodePath::parse(name)?;
        let Some(parent) = dev_root_inode.find_parent_dir_if_exists(&path)? else {
            return Ok(());
        };

        parent.remove_if_same(path.basename.as_ref(), &expected)?;
        dev_root_inode.prune_kernel_dirs(&path)?;
        Ok(())
    }
}

/// @brief dev文件i节点(锁)
#[derive(Debug)]
pub struct LockedDevFSInode(Mutex<DevFSInode>);

/// @brief dev文件i节点(无锁)
#[derive(Debug)]
pub struct DevFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedDevFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedDevFSInode>,
    /// 子Inode的B树
    children: BTreeMap<DName, Arc<dyn IndexNode>>,
    /// Whether this devfs-owned inode was created by kernel device management.
    kernel_managed: bool,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    /// 目录名
    dname: DName,
    /// 当前inode的数据部分(仅供symlink使用)
    data: Vec<u8>,
}

impl DevFSInode {
    pub fn new(dev_type_: FileType, mode: InodeMode, data_: usize) -> Self {
        return Self::new_with_parent(Weak::default(), dev_type_, mode, data_);
    }

    pub fn new_with_parent(
        parent: Weak<LockedDevFSInode>,
        dev_type_: FileType,
        mode: InodeMode,
        data_: usize,
    ) -> Self {
        return DevFSInode {
            parent,
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            kernel_managed: false,
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: dev_type_, // 文件夹
                mode,
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data_ as u32),
            },
            fs: Weak::default(),
            dname: DName::default(),
            data: Vec::new(),
        };
    }
}

impl LockedDevFSInode {
    fn add_dev_or_same(&self, name: &str, dev: Arc<dyn IndexNode>) -> Result<bool, SystemError> {
        let mut this = self.0.lock();
        let name = DName::from(name);
        if let Some(existing) = this.children.get(&name) {
            if Arc::ptr_eq(existing, &dev) {
                return Ok(false);
            }
            return Err(SystemError::EEXIST);
        }

        this.children.insert(name, dev);
        return Ok(true);
    }

    fn ensure_kernel_parent_dir(
        &self,
        path: &DevNodePath,
    ) -> Result<Arc<LockedDevFSInode>, SystemError> {
        let mut current = self.0.lock().self_ref.upgrade().ok_or(SystemError::EIO)?;

        for component in &path.dirs {
            let next = match current.find(component.as_ref()) {
                Ok(inode) => inode
                    .downcast_arc::<LockedDevFSInode>()
                    .ok_or(SystemError::ENOTDIR)?,
                Err(SystemError::ENOENT) => {
                    let inode = current.create_with_data(
                        component.as_ref(),
                        FileType::Dir,
                        InodeMode::from_bits_truncate(0o755),
                        0,
                    )?;
                    let inode = inode
                        .downcast_arc::<LockedDevFSInode>()
                        .ok_or(SystemError::EIO)?;
                    inode.0.lock().kernel_managed = true;
                    inode
                }
                Err(e) => return Err(e),
            };

            if next.metadata()?.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }
            current = next;
        }

        Ok(current)
    }

    fn find_parent_dir_if_exists(
        &self,
        path: &DevNodePath,
    ) -> Result<Option<Arc<LockedDevFSInode>>, SystemError> {
        let mut current = self.0.lock().self_ref.upgrade().ok_or(SystemError::EIO)?;

        for component in &path.dirs {
            let inode = match current.find(component.as_ref()) {
                Ok(inode) => inode,
                Err(SystemError::ENOENT) => return Ok(None),
                Err(e) => return Err(e),
            };

            let Some(next) = inode.downcast_arc::<LockedDevFSInode>() else {
                return Ok(None);
            };
            if next.metadata()?.file_type != FileType::Dir {
                return Ok(None);
            }
            current = next;
        }

        Ok(Some(current))
    }

    fn remove_kernel_dir_if_empty(&self, name: &DName) -> Result<bool, SystemError> {
        let child = {
            let inode = self.0.lock();
            inode.children.get(name).cloned()
        };

        let Some(child) = child else {
            return Ok(false);
        };
        let Some(child) = child.as_any_ref().downcast_ref::<LockedDevFSInode>() else {
            return Ok(false);
        };

        let should_remove = {
            let child = child.0.lock();
            child.kernel_managed
                && child.metadata.file_type == FileType::Dir
                && child.children.is_empty()
        };
        if !should_remove {
            return Ok(false);
        }

        self.0.lock().children.remove(name);
        Ok(true)
    }

    fn prune_kernel_dirs(&self, path: &DevNodePath) -> Result<(), SystemError> {
        let mut current = self.0.lock().self_ref.upgrade().ok_or(SystemError::EIO)?;
        let mut chain: Vec<(Arc<LockedDevFSInode>, DName)> = Vec::new();

        for component in &path.dirs {
            let inode = match current.find(component.as_ref()) {
                Ok(inode) => inode,
                Err(SystemError::ENOENT) => return Ok(()),
                Err(e) => return Err(e),
            };
            let Some(next) = inode.downcast_arc::<LockedDevFSInode>() else {
                return Ok(());
            };
            chain.push((current.clone(), component.clone()));
            current = next;
        }

        for (parent, name) in chain.into_iter().rev() {
            if !parent.remove_kernel_dir_if_empty(&name)? {
                break;
            }
        }

        Ok(())
    }

    /// # 在devfs中添加一个符号链接
    ///
    /// ## 参数
    /// - `path`: 符号链接指向的路径
    /// - `symlink_name`: 符号链接的名称
    pub fn add_dev_symlink(&self, path: &str, symlink_name: &str) -> Result<(), SystemError> {
        let new_inode =
            self.create_with_data(symlink_name, FileType::SymLink, InodeMode::S_IRWXUGO, 0)?;

        let buf = path.as_bytes();
        let len = buf.len();
        let devfs_inode = new_inode.downcast_ref::<LockedDevFSInode>().unwrap();
        devfs_inode.write_at(0, len, buf, Mutex::new(FilePrivateData::Unused).lock())?;
        devfs_inode.0.lock().kernel_managed = true;
        Ok(())
    }

    fn remove_if_same(&self, name: &str, expected: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        let dname = DName::from(name);
        let should_remove = inode
            .children
            .get(&dname)
            .is_some_and(|child| Arc::ptr_eq(child, expected));

        if should_remove {
            inode.children.remove(&dname);
        }

        Ok(())
    }

    fn do_create_with_data(
        &self,
        mut guard: MutexGuard<DevFSInode>,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        dev: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let name = DName::from(name);
        // 如果有重名的，则返回
        if guard.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        // 创建inode
        let result: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(Mutex::new(DevFSInode {
            parent: guard.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            kernel_managed: false,
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
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(dev as u32),
            },
            fs: guard.fs.clone(),
            dname: name.clone(),
            data: Vec::new(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        guard.children.insert(name, result.clone());
        return Ok(result);
    }
}

impl IndexNode for LockedDevFSInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 获取当前inode
        let guard: MutexGuard<DevFSInode> = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        return self.do_create_with_data(guard, name, file_type, mode, data);
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
                return Ok(inode
                    .children
                    .get(&DName::from(name))
                    .ok_or(SystemError::ENOENT)?
                    .clone());
            }
        }
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        if name.is_empty() {
            return Err(SystemError::ENOENT);
        }
        if name == "." || name == ".." {
            return Err(SystemError::EISDIR);
        }

        let dname = DName::from(name);
        let child = inode
            .children
            .get(&dname)
            .cloned()
            .ok_or(SystemError::ENOENT)?;

        if let Some(child) = child.as_any_ref().downcast_ref::<LockedDevFSInode>() {
            let mut child_inode = child.0.lock();
            if child_inode.metadata.file_type == FileType::Dir {
                return Err(SystemError::EISDIR);
            }

            child_inode.metadata.nlinks = child_inode
                .metadata
                .nlinks
                .checked_sub(1)
                .ok_or(SystemError::EINVAL)?;
            child_inode.metadata.ctime = PosixTimeSpec::now();
        } else if child.metadata()?.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        inode.children.remove(&dname);
        let now = PosixTimeSpec::now();
        inode.metadata.mtime = now;
        inode.metadata.ctime = now;

        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn get_entry_name(&self, ino: super::vfs::InodeId) -> Result<String, SystemError> {
        let inode: MutexGuard<DevFSInode> = self.0.lock();
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
                        if v.metadata().unwrap().inode_id.into() == ino {
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
                        "Devfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}",
                        key_len = key.len(),
                        inode_id = inode.metadata.inode_id,
                        to_find = ino
                    ),
                }
            }
        }
    }

    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
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
                .map(ToString::to_string)
                .collect(),
        );

        return Ok(keys);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
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

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写，仅支持符号链接的读取
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let meta = self.metadata()?;
        match meta.file_type {
            FileType::SymLink => {
                if buf.len() < len {
                    return Err(SystemError::EINVAL);
                }
                // 加锁
                let inode = self.0.lock();

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
            _ => {
                error!("DevFS: read_at is not supported!");
                Err(SystemError::ENOSYS)
            }
        }
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写，仅支持符号链接的写入
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let meta = self.metadata()?;
        match meta.file_type {
            FileType::SymLink => {
                if buf.len() < len {
                    return Err(SystemError::EINVAL);
                }
                let mut inode = self.0.lock();

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
            _ => {
                error!("DevFS: read_at is not supported!");
                Err(SystemError::ENOSYS)
            }
        }
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let me = self.0.lock();
        Ok(me
            .parent
            .upgrade()
            .unwrap_or(me.self_ref.upgrade().unwrap()))
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 判断需要创建的类型
        let file_type = match FileType::from(mode) {
            FileType::CharDevice => FileType::CharDevice,
            FileType::BlockDevice => FileType::BlockDevice,
            FileType::Pipe => FileType::Pipe,
            FileType::Socket => FileType::Socket,
            _ => return Err(SystemError::EINVAL),
        };

        drop(inode);
        self.create_with_data(filename, file_type, mode, dev_t.data() as usize)
    }
}

impl MountableFileSystem for DevFS {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        if raw_data.is_some_and(|raw| !raw.trim().is_empty()) {
            return Err(SystemError::EINVAL);
        }
        Ok(None)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        Ok(devfs_global_instance())
    }
}

register_mountable_fs!(DevFS, DEVTMPFSMAKER, "devtmpfs");

/// @brief 所有的设备INode都需要额外实现这个trait
pub trait DeviceINode: IndexNode {
    fn set_fs(&self, fs: Weak<DevFS>);
    fn set_parent(&self, parent: Weak<LockedDevFSInode>);
    fn set_devpts_fs(&self, _devpts: Weak<DevPtsFs>) {
        panic!("DeviceINode: set_devpts_fs is not implemented!");
    }
    fn set_devpts_parent(&self, _parent: Weak<LockedDevPtsFSInode>) {
        panic!("DeviceINode: set_devpts_parent is not implemented!");
    }
    // TODO: 增加 unregister 方法
}

/// @brief devfs device registration function
pub fn devfs_register<T: DeviceINode + 'static>(
    name: &str,
    device: Arc<T>,
) -> Result<(), SystemError> {
    return devfs_global_instance().register_device(name, device);
}

/// @brief devfs dynamic device unregistration function
pub fn devfs_unregister_dyn(name: &str, device: Arc<dyn DeviceINode>) -> Result<(), SystemError> {
    return devfs_global_instance().unregister_device_dyn(name, device);
}

/// @brief devfs dynamic device creation function; returns whether this call actually inserted a directory entry
pub fn devfs_create_node_dyn(
    name: &str,
    device: Arc<dyn DeviceINode>,
) -> Result<bool, SystemError> {
    return devfs_global_instance().register_device_dyn(name, device);
}

/// 在 /dev 下创建符号链接
#[allow(dead_code)]
pub fn devfs_add_symlink(link_name: &str, target: &str) -> Result<(), SystemError> {
    devfs_global_instance()
        .root_inode
        .add_dev_symlink(target, link_name)
}

/// @brief devfs的设备卸载函数
#[allow(dead_code)]
pub fn devfs_unregister<T: DeviceINode>(name: &str, device: Arc<T>) -> Result<(), SystemError> {
    return devfs_global_instance().unregister_device(name, device);
}

pub fn devfs_lookup_device_by_devnum(
    file_type: FileType,
    dev_t: DeviceNumber,
) -> Option<Arc<dyn IndexNode>> {
    devfs_global_instance().lookup_device_by_devnum(file_type, dev_t)
}

pub fn devfs_init() -> Result<(), SystemError> {
    info!("Initializing devtmpfs...");
    let devfs = devfs_global_instance();
    static MOUNT_INIT: Once = Once::new();
    let mut result = Ok(());
    MOUNT_INIT.call_once(|| {
        // devfs 挂载
        let root_inode = ProcessManager::current_mntns().root_inode();
        if let Err(e) = root_inode
            .mkdir("dev", InodeMode::from_bits_truncate(0o755))
            .expect("Unabled to find /dev")
            .mount(devfs.clone(), MountFlags::empty())
        {
            result = Err(e);
            return;
        }
        info!("devtmpfs mounted.");
        // 挂载 /dev/shm 为 tmpfs，符合 linux 语义
        if let Ok(dev_inode) = ProcessManager::current_mntns().root_inode().find("dev") {
            let shm_inode = dev_inode
                .find("shm")
                .or_else(|_| dev_inode.mkdir("shm", InodeMode::from_bits_truncate(0o1777)));
            if let Ok(shm_inode) = shm_inode {
                let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
                match produce_fs("tmpfs", Some("mode=1777"), "tmpfs", flags) {
                    Ok(fs) => {
                        if let Err(e) = shm_inode.mount(fs, flags) {
                            if e != SystemError::EBUSY {
                                warn!("Mount /dev/shm failed: {:?}", e);
                            }
                        }
                    }
                    Err(e) => warn!("Create tmpfs for /dev/shm failed: {:?}", e),
                }
            } else {
                warn!("Create /dev/shm failed: {:?}", shm_inode.err());
            }
        } else {
            warn!("Cannot find /dev mountpoint for /dev/shm");
        }

        result = Ok(());
    });

    return result;
}
