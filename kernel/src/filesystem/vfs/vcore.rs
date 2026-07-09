use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::{string::ToString, sync::Arc, vec::Vec};
use log::{error, info, warn};
use system_error::SystemError;

use crate::libs::casting::DowncastArc;
use crate::{
    define_event_trace,
    driver::base::block::{gendisk::GenDisk, manager::block_dev_manager},
    filesystem::{
        devfs::devfs_init,
        devpts::devpts_init,
        ext4::filesystem::{Ext4DaxMode, Ext4ErrorsBehavior, Ext4FileSystem, Ext4MountOptions},
        fat::fs::FATFileSystem,
        procfs::procfs_init,
        sysfs::sysfs_init,
        vfs::{
            file::{File, FileMode, FilePrivateData},
            mount::MountFlags,
            permission::PermissionMask,
            AtomicInodeId, FileSystem, FileType, InodeFlags, InodeMode, MountFS,
        },
    },
    init::cmdline::kenrel_cmdline_param_manager,
    ipc::kill::send_signal_to_pid,
    libs::mutex::MutexGuard,
    mm::truncate::truncate_inode_pages,
    process::{
        cred::CAPFlags, namespace::mnt::mnt_namespace_init, resource::RLimitID, ProcessManager,
    },
};

use crate::arch::ipc::signal::Signal;

use super::{
    stat::LookUpFlags,
    utils::{rsplit_path, should_remove_sgid, user_path_at},
    IndexNode, InodeId, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

/// 当没有指定根文件系统时，尝试的根文件系统列表
const ROOTFS_TRY_LIST: [&str; 6] = [
    "/dev/sda1",
    "/dev/sda",
    "/dev/vda1",
    "/dev/vda",
    "/dev/pmem0",
    "/dev/sdio1",
];
kernel_cmdline_param_kv!(ROOTFS_PATH_PARAM, root, "");
kernel_cmdline_param_arg!(ROOTFS_RO_PARAM, ro, false, false);
kernel_cmdline_param_arg!(ROOTFS_RW_PARAM, rw, false, false);
kernel_cmdline_param_kv!(ROOTFS_TYPE_PARAM, rootfstype, "");
kernel_cmdline_param_kv!(ROOTFS_FLAGS_PARAM, rootflags, "");

/// @brief 原子地生成新的Inode号。
/// 请注意，所有的inode号都需要通过该函数来生成.全局的inode号，除了以下两个特殊的以外，都是唯一的
/// 特殊的两个inode号：
/// [0]: 对应'.'目录项
/// [1]: 对应'..'目录项
pub fn generate_inode_id() -> InodeId {
    static INO: AtomicInodeId = AtomicInodeId::new(InodeId::new(1));
    return INO.fetch_add(InodeId::new(1), Ordering::SeqCst);
}

/// 初始化虚拟文件系统
#[inline(never)]
pub fn vfs_init() -> Result<(), SystemError> {
    // Initialize global append lock manager before any file write path uses it.
    super::append_lock::init_append_lock_manager();
    super::posix_lock::init_posix_lock_manager();
    super::flock::init_flock_manager();

    mnt_namespace_init();

    procfs_init().expect("Failed to initialize procfs");

    devfs_init().expect("Failed to initialize devfs");

    sysfs_init().expect("Failed to initialize sysfs");

    let root_entries = ProcessManager::current_mntns()
        .root_inode()
        .list()
        .expect("VFS init failed");
    if !root_entries.is_empty() {
        info!("Successfully initialized VFS!");
    }
    return Ok(());
}

/// @brief 迁移伪文件系统的inode
/// 请注意，为了避免删掉了伪文件系统内的信息，因此没有在原root inode那里调用unlink.
fn migrate_virtual_filesystem(
    new_fs: Arc<dyn FileSystem>,
    root_mount_flags: MountFlags,
) -> Result<(), SystemError> {
    info!("VFS: Migrating filesystems...");

    let current_mntns = ProcessManager::current_mntns();
    let old_root_inode = current_mntns.root_inode();
    let old_mntfs = current_mntns.root_mntfs();
    let new_fs = MountFS::new(
        new_fs,
        None,
        None,
        old_mntfs.propagation(),
        Some(&current_mntns),
        root_mount_flags,
        old_mntfs.mount_source(),
    );

    // 获取新的根文件系统的根节点的引用
    let new_root_inode = new_fs.root_inode();
    // ==== 在这里获取要被迁移的文件系统的inode并迁移 ===
    // 因为是换根所以路径没有变化
    // 不需要重新注册挂载目录
    new_root_inode
        .mkdir("proc", InodeMode::from_bits_truncate(0o755))
        .expect("Unable to create /proc")
        .mount_from(old_root_inode.find("proc").expect("proc not mounted!"))
        .expect("Failed to migrate filesystem of proc");
    new_root_inode
        .mkdir("dev", InodeMode::from_bits_truncate(0o755))
        .expect("Unable to create /dev")
        .mount_from(old_root_inode.find("dev").expect("dev not mounted!"))
        .expect("Failed to migrate filesystem of dev");
    new_root_inode
        .mkdir("sys", InodeMode::from_bits_truncate(0o755))
        .expect("Unable to create /sys")
        .mount_from(old_root_inode.find("sys").expect("sys not mounted!"))
        .expect("Failed to migrate filesystem of sys");

    current_mntns.force_change_root_mountfs(new_fs);

    // 换根后需要同步更新“当前进程”的 fs root/pwd。
    // 我们的路径解析（绝对路径）以进程 fs root 为起点；若不更新，后续诸如 /dev/pts 的挂载、
    // 以及 init stdio 的 /dev/hvc0 查找都会仍在旧 root 上执行，导致找不到设备节点。
    let new_root_inode = current_mntns.root_inode();
    let pcb = ProcessManager::current_pcb();
    pcb.fs_struct_mut().set_root(new_root_inode.clone());
    // init 通常 cwd 为 "/"，将 pwd 同步到新根，避免落在旧根造成后续语义混乱
    pcb.fs_struct_mut().set_pwd(new_root_inode.clone());

    // WARNING: mount devpts after devfs has been mounted,
    devpts_init().expect("Failed to initialize devpts");

    info!("VFS: Migrate filesystems done!");

    return Ok(());
}

pub(crate) fn try_find_gendisk(path: &str) -> Option<Arc<GenDisk>> {
    if let Some(gd) = block_dev_manager().lookup_gendisk_by_path(path) {
        // info!("Use {} as rootfs", path);
        return Some(gd);
    }
    return None;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootFsKind {
    Ext4,
    Fat,
}

impl RootFsKind {
    fn from_cmdline_name(name: &str) -> Option<Self> {
        match name {
            "ext4" => Some(Self::Ext4),
            "fat" | "vfat" => Some(Self::Fat),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Fat => "fat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootMountMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootMountOptions {
    mount_flags: MountFlags,
    ext4_options: Ext4MountOptions,
}

impl RootMountOptions {
    fn has_ext4_specific_options(&self) -> bool {
        self.ext4_options != Ext4MountOptions::default()
    }
}

fn root_mount_mode() -> RootMountMode {
    if ROOTFS_RO_PARAM.was_supplied() && ROOTFS_RW_PARAM.was_supplied() {
        warn!("rootfs: both ro and rw are supplied; using the last pre-`--` option");
    }

    match kenrel_cmdline_param_manager()
        .last_bare_option_before_init_args(&["ro", "rw"])
        .as_deref()
    {
        Some("rw") => RootMountMode::ReadWrite,
        _ => RootMountMode::ReadOnly,
    }
}

fn rootfstype_candidates() -> Result<Vec<RootFsKind>, SystemError> {
    let Some(rootfstype) = ROOTFS_TYPE_PARAM.value_str() else {
        return Ok(Vec::new());
    };
    if rootfstype.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    for name in rootfstype
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let Some(kind) = RootFsKind::from_cmdline_name(name) else {
            error!("rootfs: unsupported rootfstype '{}'", name);
            return Err(SystemError::EINVAL);
        };
        result.push(kind);
    }

    if result.is_empty() {
        return Err(SystemError::EINVAL);
    }
    Ok(result)
}

fn parse_ext4_dax_option(opt: &str) -> Result<Ext4DaxMode, SystemError> {
    match opt {
        "dax" | "dax=always" => Ok(Ext4DaxMode::Always),
        "dax=inode" => Ok(Ext4DaxMode::Inode),
        "dax=never" => Ok(Ext4DaxMode::Never),
        _ => Err(SystemError::EINVAL),
    }
}

fn parse_ext4_errors_option(value: &str) -> Result<Ext4ErrorsBehavior, SystemError> {
    match value {
        "continue" => Ok(Ext4ErrorsBehavior::Continue),
        "remount-ro" => Ok(Ext4ErrorsBehavior::RemountRo),
        "panic" => Ok(Ext4ErrorsBehavior::Panic),
        _ => Err(SystemError::EINVAL),
    }
}

fn apply_common_rootflag(opt: &str, mount_flags: &mut MountFlags) -> bool {
    match opt {
        "defaults" => {}
        "ro" => mount_flags.insert(MountFlags::RDONLY),
        "rw" => mount_flags.remove(MountFlags::RDONLY),
        "sync" => mount_flags.insert(MountFlags::SYNCHRONOUS),
        "async" => mount_flags.remove(MountFlags::SYNCHRONOUS),
        "dirsync" => mount_flags.insert(MountFlags::DIRSYNC),
        "lazytime" => mount_flags.insert(MountFlags::LAZYTIME),
        "nolazytime" => mount_flags.remove(MountFlags::LAZYTIME),
        "mand" => mount_flags.insert(MountFlags::MANDLOCK),
        "nomand" => mount_flags.remove(MountFlags::MANDLOCK),
        "nosuid" => mount_flags.insert(MountFlags::NOSUID),
        "suid" => mount_flags.remove(MountFlags::NOSUID),
        "nodev" => mount_flags.insert(MountFlags::NODEV),
        "dev" => mount_flags.remove(MountFlags::NODEV),
        "noexec" => mount_flags.insert(MountFlags::NOEXEC),
        "exec" => mount_flags.remove(MountFlags::NOEXEC),
        "noatime" => {
            mount_flags.remove(MountFlags::RELATIME | MountFlags::STRICTATIME);
            mount_flags.insert(MountFlags::NOATIME);
        }
        "atime" | "strictatime" => {
            mount_flags.remove(MountFlags::RELATIME | MountFlags::NOATIME);
        }
        "relatime" => {
            mount_flags.remove(MountFlags::NOATIME | MountFlags::STRICTATIME);
            mount_flags.insert(MountFlags::RELATIME);
        }
        "nodiratime" => mount_flags.insert(MountFlags::NODIRATIME),
        "diratime" => mount_flags.remove(MountFlags::NODIRATIME),
        "nosymfollow" => mount_flags.insert(MountFlags::NOSYMFOLLOW),
        "symfollow" => mount_flags.remove(MountFlags::NOSYMFOLLOW),
        "iversion" => mount_flags.insert(MountFlags::I_VERSION),
        "noiversion" => mount_flags.remove(MountFlags::I_VERSION),
        _ => return false,
    }
    true
}

fn parse_rootflags(mode: RootMountMode) -> Result<RootMountOptions, SystemError> {
    let mut mount_flags = match mode {
        RootMountMode::ReadOnly => MountFlags::RDONLY | MountFlags::RELATIME,
        RootMountMode::ReadWrite => MountFlags::RELATIME,
    };
    let mut ext4_options = Ext4MountOptions::default();

    let Some(rootflags) = ROOTFS_FLAGS_PARAM.value_str() else {
        return Ok(RootMountOptions {
            mount_flags,
            ext4_options,
        });
    };
    if rootflags.is_empty() {
        return Ok(RootMountOptions {
            mount_flags,
            ext4_options,
        });
    }

    for opt in rootflags
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if apply_common_rootflag(opt, &mut mount_flags) {
            continue;
        }

        match opt {
            _ if opt == "dax" || opt.starts_with("dax=") => {
                ext4_options.dax = Some(parse_ext4_dax_option(opt)?);
            }
            _ if opt.starts_with("errors=") => {
                let value = opt.split_once('=').ok_or(SystemError::EINVAL)?.1;
                ext4_options.errors = parse_ext4_errors_option(value)?;
            }
            _ => {
                error!("rootfs: unsupported rootflags option '{}'", opt);
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
    }

    Ok(RootMountOptions {
        mount_flags,
        ext4_options,
    })
}

fn probe_rootfs_kind(gendisk: &Arc<GenDisk>) -> Option<RootFsKind> {
    match Ext4FileSystem::probe(gendisk) {
        Ok(true) => return Some(RootFsKind::Ext4),
        Ok(false) => {}
        Err(e) => {
            // 探测阶段不应阻塞启动；继续尝试其他 FS 探测/初始化。
            warn!("Rootfs probe: read ext superblock failed: {:?}", e);
        }
    }

    if FATFileSystem::probe(gendisk) {
        return Some(RootFsKind::Fat);
    }

    None
}

pub fn mount_root_fs() -> Result<(), SystemError> {
    info!("Try to mount root fs...");
    block_dev_manager().print_gendisks();
    let gendisk = if let Some(rootfs_dev_path) = ROOTFS_PATH_PARAM
        .value_str()
        .filter(|path| !path.is_empty())
    {
        try_find_gendisk(rootfs_dev_path)
            .unwrap_or_else(|| panic!("Failed to find rootfs device {}", rootfs_dev_path))
    } else {
        ROOTFS_TRY_LIST
            .iter()
            .find_map(|&path| try_find_gendisk(path))
            .ok_or(SystemError::ENODEV)?
    };

    let mode = root_mount_mode();
    let root_options = parse_rootflags(mode)?;
    let configured_kinds = rootfstype_candidates()?;
    let probed_kind = if configured_kinds.is_empty() {
        probe_rootfs_kind(&gendisk)
    } else {
        None
    };

    let init_rootfs = |kind: RootFsKind| -> Result<Arc<dyn FileSystem>, SystemError> {
        match kind {
            RootFsKind::Ext4 => Ext4FileSystem::from_gendisk_with_options(
                gendisk.clone(),
                root_options.ext4_options,
            ),
            RootFsKind::Fat => {
                if root_options.has_ext4_specific_options() {
                    error!("rootfs: ext4-specific rootflags cannot be used with FAT rootfs");
                    return Err(SystemError::EINVAL);
                }
                Ok(FATFileSystem::new(gendisk.clone())?)
            }
        }
    };

    let rootfs: Result<Arc<dyn FileSystem>, SystemError> = match probed_kind {
        Some(kind) => init_rootfs(kind),
        None => {
            let candidates = if configured_kinds.is_empty() {
                Vec::from([RootFsKind::Ext4, RootFsKind::Fat])
            } else {
                configured_kinds
            };

            let mut last_err = SystemError::EINVAL;
            let mut mounted = None;
            for kind in candidates {
                match init_rootfs(kind) {
                    Ok(fs) => {
                        mounted = Some(fs);
                        break;
                    }
                    Err(e) => {
                        warn!("rootfs: failed to initialize {}: {:?}", kind.name(), e);
                        last_err = e;
                    }
                }
            }
            mounted.ok_or(last_err)
        }
    };

    let rootfs = match rootfs {
        Ok(fs) => fs,
        Err(e) => {
            error!("Failed to initialize rootfs filesystem: {:?}", e);
            loop {
                spin_loop();
            }
        }
    };

    let fs_name = rootfs.name().to_string();
    let r = migrate_virtual_filesystem(rootfs.clone(), root_options.mount_flags);
    if r.is_err() {
        error!(
            "Failed to migrate virtual filesystem to rootfs ({}).",
            fs_name
        );
        loop {
            spin_loop();
        }
    }
    info!("Successfully migrate rootfs to {}!", fs_name);

    return Ok(());
}

#[cfg(feature = "initram")]
pub fn change_root_fs() -> Result<(), SystemError> {
    info!("Try to change root fs to initramfs...");
    let initramfs = crate::init::initram::INIT_ROOT_INODE().fs();
    let r = migrate_virtual_filesystem(initramfs, MountFlags::empty());

    if r.is_err() {
        error!("Failed to migrate virtual filesystem to initramfs!");
        loop {
            spin_loop();
        }
    }
    info!("Successfully migrate rootfs to initramfs!");

    return Ok(());
}

define_event_trace!(
    do_mkdir_at,
    TP_system(vfs),
    TP_PROTO(path:&str, mode: InodeMode),
    TP_STRUCT__entry {
        fmode: InodeMode,
        path: [u8;64],
    },
    TP_fast_assign {
        fmode: mode,
        path: {
            let mut buf = [0u8; 64];
            let path = path.as_bytes();
            let len = path.len().min(63);
            buf[..len].copy_from_slice(&path[..len]);
            buf[len] = 0; // null-terminate
            buf
        },
    },
    TP_ident(__entry),
    TP_printk({
        let path = core::str::from_utf8(&__entry.path).unwrap_or("invalid utf8");
        let mode = __entry.fmode;
        format!("mkdir at {} with mode {:?}", path, mode)
    })
);

/// @brief 创建文件/文件夹
pub fn do_mkdir_at(
    dirfd: i32,
    path: &str,
    mode: InodeMode,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    trace_do_mkdir_at(path, mode);
    if path.is_empty() {
        return Err(SystemError::ENOENT);
    }
    let (mut current_inode, path) =
        user_path_at(&ProcessManager::current_pcb(), dirfd, path.trim())?;
    // Linux 返回 EEXIST
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(SystemError::EEXIST);
    }
    let (name, parent) = rsplit_path(path);
    if name == "." || name == ".." {
        return Err(SystemError::EEXIST);
    }
    // 检查文件名长度
    if name.len() > super::NAME_MAX {
        return Err(SystemError::ENAMETOOLONG);
    }
    if let Some(parent) = parent {
        current_inode =
            current_inode.lookup_follow_symlink(parent, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    }
    let parent_md = current_inode.metadata()?;
    // 确保父节点是目录
    if parent_md.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    let pcb = ProcessManager::current_pcb();

    // Linux 语义：目录执行权限控制能否遍历（查找子项）
    // 先检查执行权限，再检查文件是否存在，最后检查写权限
    // 顺序很重要：无执行权限 → EACCES；有执行权限且已存在 → EEXIST；有执行权限无写权限 → EACCES
    crate::filesystem::vfs::permission::check_inode_permission(
        &current_inode,
        &parent_md,
        PermissionMask::MAY_EXEC,
    )?;

    // 已确认有执行权限，可以查找子项
    if current_inode.find(name).is_ok() {
        return Err(SystemError::EEXIST);
    }

    // 创建目录还需要对父目录拥有写权限
    crate::filesystem::vfs::permission::check_inode_permission(
        &current_inode,
        &parent_md,
        PermissionMask::MAY_WRITE,
    )?;

    let mut final_mode_bits = mode.bits() & InodeMode::S_IRWXUGO.bits();
    if (parent_md.mode.bits() & InodeMode::S_ISGID.bits()) != 0 {
        final_mode_bits |= InodeMode::S_ISGID.bits();
    }
    let umask = pcb.fs_struct().umask();
    let final_mode = InodeMode::from_bits_truncate(final_mode_bits) & !umask;

    // 执行创建
    return current_inode.mkdir(name, final_mode);
}

/// 解析父目录inode
///
/// 当 `parent_path` 为 `None` 时，使用当前 inode；
/// 否则查找父目录路径
///
/// # 参数
///
/// * `inode_begin` - 起始 inode
/// * `parent_path` - 父目录路径（可选）
///
/// # 返回值
///
/// 返回解析后的父目录 inode
pub(super) fn resolve_parent_inode(
    inode_begin: Arc<dyn IndexNode>,
    parent_path: Option<&str>,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    match parent_path {
        None => Ok(inode_begin),
        Some(path) => inode_begin.lookup_follow_symlink(path, VFS_MAX_FOLLOW_SYMLINK_TIMES),
    }
}

/// 检查父目录权限（写+执行权限），并尊重文件系统权限策略（如 FUSE remote 模型）。
pub(super) fn check_parent_dir_permission_inode(
    parent_inode: &Arc<dyn IndexNode>,
    parent_md: &super::Metadata,
) -> Result<(), SystemError> {
    crate::filesystem::vfs::permission::check_inode_permission(
        parent_inode,
        parent_md,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )
}

/// @brief 删除文件夹
pub fn do_remove_dir(dirfd: i32, path: &str) -> Result<u64, SystemError> {
    let path = path.trim();

    if path == "/" {
        return Err(SystemError::EBUSY);
    }
    if path.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;
    let (filename, parent_path) = rsplit_path(&remain_path);

    // 最后一项文件项为.时返回EINVAL
    if filename == "." {
        return Err(SystemError::EINVAL);
    }

    let parent_inode: Arc<dyn IndexNode> = resolve_parent_inode(inode_begin, parent_path)?;
    let parent_md = parent_inode.metadata()?;

    if parent_md.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // Linux 语义：删除目录需要对父目录拥有 W+X（写+搜索）权限
    // 注意：权限检查必须在 find 之前进行，否则当目录不存在时会返回 ENOENT 而不是 EACCES
    check_parent_dir_permission_inode(&parent_inode, &parent_md)?;

    // 在目标点为symlink时也返回ENOTDIR
    let target_inode = parent_inode.find(filename)?;

    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件夹
    parent_inode.rmdir(filename)?;

    return Ok(0);
}

/// @brief 删除文件
pub fn do_unlink_at(dirfd: i32, path: &str) -> Result<u64, SystemError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(SystemError::ENOENT);
    }
    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;
    if remain_path.ends_with('/') {
        return Err(SystemError::ENOTDIR);
    }
    // 分离父路径和文件名
    let (filename, parent_path) = rsplit_path(&remain_path);

    let parent_inode: Arc<dyn IndexNode> = resolve_parent_inode(inode_begin, parent_path)?;
    let parent_md = parent_inode.metadata()?;

    if parent_md.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // Linux 语义：删除文件需要对父目录拥有 W+X（写+搜索）权限
    // 注意：权限检查必须在 find 之前进行，否则当文件不存在时会返回 ENOENT 而不是 EACCES
    check_parent_dir_permission_inode(&parent_inode, &parent_md)?;

    // Linux 语义：unlink(2)/unlinkat(2) 删除目录项本身，不跟随最后一个符号链接。
    // 我们已解析到父目录，因此这里必须用 find() 直接取目录项对应 inode，
    // 避免触发 symlink 解析（否则可能得到 ELOOP 或删错目标）。
    let target_inode = parent_inode.find(filename)?;

    // 如果目标是目录，则返回 EISDIR
    if target_inode.metadata()?.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }

    // 对目标 inode 执行页缓存清理
    if let Some(page_cache) = target_inode.page_cache().clone() {
        truncate_inode_pages(page_cache, 0);
    }

    // 在父目录上执行 unlink 操作
    parent_inode.unlink(filename)?;

    return Ok(0);
}

pub(super) fn do_file_lookup_at(
    dfd: i32,
    path: &str,
    lookup_flags: LookUpFlags,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dfd, path)?;
    let follow_final = lookup_flags.contains(LookUpFlags::FOLLOW);
    return inode.lookup_follow_symlink2(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES, follow_final);
}

#[inline]
pub fn current_file_lock_owner_id() -> u64 {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();
    fd_table_guard.lock_owner_id() as u64
}

#[inline(never)]
fn vfs_truncate_inner<F>(
    inode: Arc<dyn IndexNode>,
    len: usize,
    do_resize: F,
) -> Result<(), SystemError>
where
    F: FnOnce(&Arc<dyn IndexNode>) -> Result<(), SystemError>,
{
    let md = inode.metadata()?;

    // 防御性检查：统一拒绝超出 isize::MAX 的长度，避免后续类型转换溢出
    if len > isize::MAX as usize {
        return Err(SystemError::EINVAL);
    }

    if md.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }
    if md.file_type != FileType::File {
        return Err(SystemError::EINVAL);
    }

    // S_IMMUTABLE 文件不能被截断
    if md.flags.contains(InodeFlags::S_IMMUTABLE) {
        return Err(SystemError::EPERM);
    }

    // S_APPEND 文件不能被截断（只能追加）
    if md.flags.contains(InodeFlags::S_APPEND) {
        return Err(SystemError::EPERM);
    }

    // S_SWAPFILE 文件不能被截断
    if md.flags.contains(InodeFlags::S_SWAPFILE) {
        return Err(SystemError::ETXTBSY);
    }

    // 只读挂载检查：若当前 fs 是 MountFS 且带 RDONLY 标志，拒绝写
    let fs = inode.fs();
    if let Some(mfs) = fs.clone().downcast_arc::<MountFS>() {
        let mount_flags = mfs.mount_flags();
        if mount_flags.contains(crate::filesystem::vfs::mount::MountFlags::RDONLY) {
            return Err(SystemError::EROFS);
        }
    }

    let result = do_resize(&inode);

    if result.is_ok() {
        clear_suid_sgid_after_truncate(inode.as_ref())?;
    }

    result
}

fn clear_suid_sgid_after_truncate(inode: &dyn IndexNode) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    if cred.has_capability(CAPFlags::CAP_FSETID) {
        return Ok(());
    }

    let mut md = inode.metadata()?;
    if md.file_type == FileType::File && md.mode.intersects(InodeMode::S_ISUID | InodeMode::S_ISGID)
    {
        md.mode.remove(InodeMode::S_ISUID);

        if should_remove_sgid(md.mode, md.gid, &cred) {
            md.mode.remove(InodeMode::S_ISGID);
        }

        inode.set_metadata(&md)?;
    }

    Ok(())
}

/// 统一的 VFS 截断封装：对 inode 进行基本检查并调用 resize
/// - 目录返回 EISDIR
/// - 非普通文件返回 EINVAL
/// - 只读挂载返回 EROFS
#[inline(never)]
pub fn vfs_truncate(inode: Arc<dyn IndexNode>, len: usize) -> Result<(), SystemError> {
    let lock_owner = current_file_lock_owner_id();
    vfs_truncate_inner(inode, len, |inode| {
        inode.resize_with_lock_owner(len, lock_owner)
    })
}

/// 基于已打开文件执行 VFS 截断，保留公共检查，并把 fd 私有数据传给文件系统。
#[inline(never)]
pub fn vfs_truncate_file<'a>(
    inode: Arc<dyn IndexNode>,
    len: usize,
    lock_owner: u64,
    data: impl FnOnce() -> MutexGuard<'a, FilePrivateData>,
) -> Result<(), SystemError> {
    vfs_truncate_inner(inode, len, |inode| {
        inode.resize_file(len, lock_owner, data())
    })
}

pub fn check_file_size_limit(new_size: usize) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    let fsize_limit = current_pcb.get_rlimit(RLimitID::Fsize);
    if fsize_limit.rlim_cur != u64::MAX && new_size as u64 > fsize_limit.rlim_cur {
        let _ = send_signal_to_pid(current_pcb.raw_pid(), Signal::SIGXFSZ);
        return Err(SystemError::EFBIG);
    }
    Ok(())
}

/// Generic resize-backed `fallocate(mode=0)` for real local regular files.
///
/// This helper provides DragonOS' compatibility guarantee that mode=0 makes the
/// file size at least `offset + len`. It is intentionally opt-in: pseudo
/// filesystems and protocol filesystems must keep returning EOPNOTSUPP or
/// provide their own fallocate implementation.
pub fn resize_based_fallocate(
    inode: &dyn IndexNode,
    mode: i32,
    offset: usize,
    len: usize,
    lock_owner: u64,
) -> Result<(), SystemError> {
    if mode != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let new_size = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
    if new_size > isize::MAX as usize {
        return Err(SystemError::EFBIG);
    }

    let md = inode.metadata()?;
    if md.file_type != FileType::File {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let current_size = md.size.max(0) as usize;
    if new_size <= current_size {
        return Ok(());
    }

    check_file_size_limit(new_size)?;

    let result = inode.resize_with_lock_owner(new_size, lock_owner);
    if result.is_ok() && md.size != new_size as i64 {
        clear_suid_sgid_after_truncate(inode)?;
    }

    result
}

/// 基于已打开文件执行 VFS fallocate 公共检查，再分派给具体文件系统。
#[inline(never)]
pub fn vfs_fallocate_file(
    file: Arc<File>,
    mode: i32,
    offset: usize,
    len: usize,
) -> Result<(), SystemError> {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
    const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
    const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
    const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
    const FALLOC_FL_UNSHARE_RANGE: u32 = 0x40;
    const FALLOC_FL_SUPPORTED_MASK: u32 = FALLOC_FL_KEEP_SIZE
        | FALLOC_FL_PUNCH_HOLE
        | FALLOC_FL_COLLAPSE_RANGE
        | FALLOC_FL_ZERO_RANGE
        | FALLOC_FL_INSERT_RANGE
        | FALLOC_FL_UNSHARE_RANGE;

    if len == 0 || offset > isize::MAX as usize || len > isize::MAX as usize {
        return Err(SystemError::EINVAL);
    }

    let mode_bits = mode as u32;
    if mode < 0 || (mode_bits & !FALLOC_FL_SUPPORTED_MASK) != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if (mode_bits & (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE))
        == (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE)
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if (mode_bits & FALLOC_FL_PUNCH_HOLE) != 0 && (mode_bits & FALLOC_FL_KEEP_SIZE) == 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if (mode_bits & FALLOC_FL_COLLAPSE_RANGE) != 0 && (mode_bits & !FALLOC_FL_COLLAPSE_RANGE) != 0 {
        return Err(SystemError::EINVAL);
    }
    if (mode_bits & FALLOC_FL_INSERT_RANGE) != 0 && (mode_bits & !FALLOC_FL_INSERT_RANGE) != 0 {
        return Err(SystemError::EINVAL);
    }
    if (mode_bits & FALLOC_FL_UNSHARE_RANGE) != 0
        && (mode_bits & !(FALLOC_FL_UNSHARE_RANGE | FALLOC_FL_KEEP_SIZE)) != 0
    {
        return Err(SystemError::EINVAL);
    }

    let mode_flags = file.mode();
    if mode_flags.contains(FileMode::FMODE_PATH) {
        return Err(SystemError::EBADF);
    }
    if !mode_flags.contains(FileMode::FMODE_WRITE) || !mode_flags.can_write() {
        return Err(SystemError::EBADF);
    }

    let inode = file.inode();
    let md = inode.metadata()?;
    if md.flags.contains(InodeFlags::S_APPEND) && (mode_bits & !FALLOC_FL_KEEP_SIZE) != 0 {
        return Err(SystemError::EPERM);
    }
    if md.flags.contains(InodeFlags::S_IMMUTABLE) {
        return Err(SystemError::EPERM);
    }
    if md.flags.contains(InodeFlags::S_SWAPFILE) {
        return Err(SystemError::ETXTBSY);
    }

    let fs = inode.fs();
    if let Some(mfs) = fs.clone().downcast_arc::<MountFS>() {
        if mfs
            .mount_flags()
            .contains(crate::filesystem::vfs::mount::MountFlags::RDONLY)
        {
            return Err(SystemError::EROFS);
        }
    }

    match md.file_type {
        FileType::File | FileType::BlockDevice => {}
        FileType::Dir => return Err(SystemError::EISDIR),
        FileType::Pipe => return Err(SystemError::ESPIPE),
        _ => return Err(SystemError::ENODEV),
    }

    let new_size = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
    if new_size > isize::MAX as usize {
        return Err(SystemError::EFBIG);
    }

    inode.fallocate_file(
        mode,
        offset,
        len,
        current_file_lock_owner_id(),
        file.private_data.lock(),
    )
}
