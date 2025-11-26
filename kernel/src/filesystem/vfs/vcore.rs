use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::sync::Arc;
use log::{error, info};
use system_error::SystemError;

use crate::libs::casting::DowncastArc;
use crate::{
    define_event_trace,
    driver::base::block::{gendisk::GenDisk, manager::block_dev_manager},
    filesystem::{
        devfs::devfs_init,
        devpts::devpts_init,
        fat::fs::FATFileSystem,
        procfs::procfs_init,
        sysfs::sysfs_init,
        vfs::{syscall::InodeMode, AtomicInodeId, FileSystem, FileType, MountFS},
    },
    mm::truncate::truncate_inode_pages,
    process::{namespace::mnt::mnt_namespace_init, ProcessManager},
};

use super::{
    file::FileFlags,
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

    let fatfs: Result<Arc<FATFileSystem>, SystemError> = FATFileSystem::new(gendisk);
    if fatfs.is_err() {
        error!(
            "Failed to initialize fatfs, code={:?}",
            fatfs.as_ref().err()
        );
        loop {
            spin_loop();
        }
    }
    let fatfs: Arc<FATFileSystem> = fatfs.unwrap();
    let r = migrate_virtual_filesystem(fatfs);

    if r.is_err() {
        error!("Failed to migrate virtual filesyst  em to FAT32!");
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
    TP_PROTO(path:&str, mode: FileFlags),
    TP_STRUCT__entry {
        fmode: FileFlags,
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
    mode: FileFlags,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    trace_do_mkdir_at(path, mode);
    let (mut current_inode, path) =
        user_path_at(&ProcessManager::current_pcb(), dirfd, path.trim())?;
    let (name, parent) = rsplit_path(&path);
    if let Some(parent) = parent {
        current_inode =
            current_inode.lookup_follow_symlink(parent, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    }
    return current_inode.mkdir(name, InodeMode::from_bits_truncate(mode.bits()));
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

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
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
    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 查找目标 inode，但 *不* 跟随最后的符号链接
    let target_inode = parent_inode.lookup_follow_symlink(filename, 0)?;

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

    if md.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }
    if md.file_type != FileType::File {
        return Err(SystemError::EINVAL);
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

    result
}
