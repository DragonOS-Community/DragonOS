use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::{string::ToString, sync::Arc};
use log::{error, info, warn};
use system_error::SystemError;

use crate::libs::casting::DowncastArc;
use crate::{
    define_event_trace,
    driver::base::block::{gendisk::GenDisk, manager::block_dev_manager},
    filesystem::{
        devfs::devfs_init,
        devpts::devpts_init,
        ext4::filesystem::Ext4FileSystem,
        fat::fs::FATFileSystem,
        procfs::procfs_init,
        sysfs::sysfs_init,
        vfs::{
            permission::PermissionMask, AtomicInodeId, FileSystem, FileType, InodeFlags, InodeMode,
            MountFS,
        },
    },
    mm::truncate::truncate_inode_pages,
    process::{namespace::mnt::mnt_namespace_init, ProcessManager},
};

use super::{
    stat::LookUpFlags,
    utils::{rsplit_path, user_path_at},
    IndexNode, InodeId, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

/// 当没有指定根文件系统时，尝试的根文件系统列表
const ROOTFS_TRY_LIST: [&str; 5] = [
    "/dev/sda1",
    "/dev/sda",
    "/dev/vda1",
    "/dev/vda",
    "/dev/sdio1",
];
kernel_cmdline_param_kv!(ROOTFS_PATH_PARAM, root, "");

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
fn migrate_virtual_filesystem(new_fs: Arc<dyn FileSystem>) -> Result<(), SystemError> {
    info!("VFS: Migrating filesystems...");

    let current_mntns = ProcessManager::current_mntns();
    let old_root_inode = current_mntns.root_inode();
    let old_mntfs = current_mntns.root_mntfs().clone();
    let new_fs = MountFS::new(
        new_fs,
        None,
        old_mntfs.propagation(),
        Some(&current_mntns),
        old_mntfs.mount_flags(),
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

    unsafe {
        current_mntns.force_change_root_mountfs(new_fs);
    }

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
    let gendisk = if let Some(rootfs_dev_path) = ROOTFS_PATH_PARAM.value_str() {
        try_find_gendisk(rootfs_dev_path)
            .unwrap_or_else(|| panic!("Failed to find rootfs device {}", rootfs_dev_path))
    } else {
        ROOTFS_TRY_LIST
            .iter()
            .find_map(|&path| try_find_gendisk(path))
            .ok_or(SystemError::ENODEV)?
    };

    let kind = probe_rootfs_kind(&gendisk);

    let rootfs: Result<Arc<dyn FileSystem>, SystemError> = match kind {
        Some(RootFsKind::Ext4) => Ext4FileSystem::from_gendisk(gendisk.clone()),
        Some(RootFsKind::Fat) => Ok(FATFileSystem::new(gendisk.clone())?),
        None => {
            // 兜底：按常见顺序尝试初始化（ext4 -> fat），便于未来扩展 probe 或处理特殊镜像。
            Ext4FileSystem::from_gendisk(gendisk.clone()).or_else(|_| {
                let fat: Arc<FATFileSystem> = FATFileSystem::new(gendisk.clone())?;
                Ok(fat)
            })
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
    let r = migrate_virtual_filesystem(rootfs.clone());
    if r.is_err() {
        error!(
            "Failed to migrate virtual filesystem to rootfs ({}).",
            fs_name
        );
        loop {
            spin_loop();
        }
    }
    let fatfs: Arc<FATFileSystem> = fatfs.unwrap();
    let r = migrate_virtual_filesystem(fatfs);

    if r.is_err() {
        error!("Failed to migrate virtual filesystem to FAT32!");
        loop {
            spin_loop();
        }
    }
    info!("Successfully migrate rootfs to FAT32!");

    return Ok(());
}

#[cfg(feature = "initram")]
pub fn change_root_fs() -> Result<(), SystemError> {
    info!("Try to change root fs to initramfs...");
    let initramfs = crate::init::initram::INIT_ROOT_INODE().fs();
    let r = migrate_virtual_filesystem(initramfs);

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
    let cred = pcb.cred();
    cred.inode_permission(&parent_md, PermissionMask::MAY_EXEC.bits())?;
    if current_inode.find(name).is_ok() {
        return Err(SystemError::EEXIST);
    }
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

/// 检查父目录权限（写+执行权限）
///
/// Linux 语义：删除/创建文件/目录需要对父目录拥有 W+X（写+执行）权限
/// 注意：权限检查必须在 find 之前进行，否则当文件不存在时会返回 ENOENT 而不是 EACCES
pub(super) fn check_parent_dir_permission(parent_md: &super::Metadata) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    cred.inode_permission(
        parent_md,
        (PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC).bits(),
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
    check_parent_dir_permission(&parent_md)?;

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
    check_parent_dir_permission(&parent_md)?;

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

/// 统一的 VFS 截断封装：对 inode 进行基本检查并调用 resize
/// - 目录返回 EISDIR
/// - 非普通文件返回 EINVAL
/// - 只读挂载返回 EROFS
#[inline(never)]
pub fn vfs_truncate(inode: Arc<dyn IndexNode>, len: usize) -> Result<(), SystemError> {
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

    let result = inode.resize(len);

    if result.is_ok() {
        let vm_opt: Option<Arc<crate::mm::ucontext::AddressSpace>> =
            ProcessManager::current_pcb().basic().user_vm();
        if let Some(vm) = vm_opt {
            let _ = vm.write().zap_file_mappings(md.inode_id);
        }
    }

    result
}
