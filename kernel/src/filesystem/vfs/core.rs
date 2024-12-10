use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::sync::Arc;
use log::{error, info};
use system_error::SystemError;

use crate::{
    driver::base::block::{gendisk::GenDisk, manager::block_dev_manager},
    filesystem::{
        devfs::devfs_init,
        fat::fs::FATFileSystem,
        procfs::procfs_init,
        ramfs::RamFS,
        sysfs::sysfs_init,
        vfs::{
            mount::MountFS, syscall::ModeType, AtomicInodeId, FileSystem, FileType, MAX_PATHLEN,
        },
    },
    libs::spinlock::SpinLock,
    process::ProcessManager,
    syscall::user_access::check_and_clone_cstr,
};

use super::{
    fcntl::AtFlags,
    file::FileMode,
    mount::{init_mountlist, MOUNT_LIST},
    syscall::UmountFlag,
    utils::{rsplit_path, user_path_at},
    FilePrivateData, IndexNode, InodeId, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

/// 当没有指定根文件系统时，尝试的根文件系统列表
const ROOTFS_TRY_LIST: [&str; 4] = ["/dev/sda1", "/dev/sda", "/dev/vda1", "/dev/vda"];
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

static mut __ROOT_INODE: Option<Arc<dyn IndexNode>> = None;

/// @brief 获取全局的根节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn ROOT_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __ROOT_INODE.as_ref().unwrap().clone();
    }
}

/// 初始化虚拟文件系统
#[inline(never)]
pub fn vfs_init() -> Result<(), SystemError> {
    // 使用Ramfs作为默认的根文件系统
    let ramfs = RamFS::new();
    let mount_fs = MountFS::new(ramfs, None);
    let root_inode = mount_fs.root_inode();
    init_mountlist();
    unsafe {
        __ROOT_INODE = Some(root_inode.clone());
    }

    procfs_init().expect("Failed to initialize procfs");

    devfs_init().expect("Failed to initialize devfs");

    sysfs_init().expect("Failed to initialize sysfs");

    let root_entries = ROOT_INODE().list().expect("VFS init failed");
    if !root_entries.is_empty() {
        info!("Successfully initialized VFS!");
    }
    return Ok(());
}

/// @brief 迁移伪文件系统的inode
/// 请注意，为了避免删掉了伪文件系统内的信息，因此没有在原root inode那里调用unlink.
fn migrate_virtual_filesystem(new_fs: Arc<dyn FileSystem>) -> Result<(), SystemError> {
    info!("VFS: Migrating filesystems...");

    let new_fs = MountFS::new(new_fs, None);
    // 获取新的根文件系统的根节点的引用
    let new_root_inode = new_fs.root_inode();

    // ==== 在这里获取要被迁移的文件系统的inode并迁移 ===
    // 因为是换根所以路径没有变化
    // 不需要重新注册挂载目录
    new_root_inode
        .mkdir("proc", ModeType::from_bits_truncate(0o755))
        .expect("Unable to create /proc")
        .mount_from(ROOT_INODE().find("proc").expect("proc not mounted!"))
        .expect("Failed to migrate filesystem of proc");
    new_root_inode
        .mkdir("dev", ModeType::from_bits_truncate(0o755))
        .expect("Unable to create /dev")
        .mount_from(ROOT_INODE().find("dev").expect("dev not mounted!"))
        .expect("Failed to migrate filesystem of dev");
    new_root_inode
        .mkdir("sys", ModeType::from_bits_truncate(0o755))
        .expect("Unable to create /sys")
        .mount_from(ROOT_INODE().find("sys").expect("sys not mounted!"))
        .expect("Failed to migrate filesystem of sys");

    unsafe {
        // drop旧的Root inode
        let old_root_inode = __ROOT_INODE.take().unwrap();
        // 设置全局的新的ROOT Inode
        __ROOT_INODE = Some(new_root_inode.clone());
        drop(old_root_inode);
    }

    info!("VFS: Migrate filesystems done!");

    return Ok(());
}

fn try_find_gendisk_as_rootfs(path: &str) -> Option<Arc<GenDisk>> {
    if let Some(gd) = block_dev_manager().lookup_gendisk_by_path(path) {
        info!("Use {} as rootfs", path);
        return Some(gd);
    }
    return None;
}

pub fn mount_root_fs() -> Result<(), SystemError> {
    info!("Try to mount root fs...");
    block_dev_manager().print_gendisks();
    let gendisk = if let Some(rootfs_dev_path) = ROOTFS_PATH_PARAM.value_str() {
        try_find_gendisk_as_rootfs(rootfs_dev_path)
            .unwrap_or_else(|| panic!("Failed to find rootfs device {}", rootfs_dev_path))
    } else {
        ROOTFS_TRY_LIST
            .iter()
            .find_map(|&path| try_find_gendisk_as_rootfs(path))
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
        error!("Failed to migrate virtual filesystem to FAT32!");
        loop {
            spin_loop();
        }
    }
    info!("Successfully migrate rootfs to FAT32!");

    return Ok(());
}

/// @brief 创建文件/文件夹
pub fn do_mkdir_at(
    dirfd: i32,
    path: &str,
    mode: FileMode,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    // debug!("Call do mkdir at");
    let (mut current_inode, path) =
        user_path_at(&ProcessManager::current_pcb(), dirfd, path.trim())?;
    let (name, parent) = rsplit_path(&path);
    if let Some(parent) = parent {
        current_inode =
            current_inode.lookup_follow_symlink(parent, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    }
    // debug!("mkdir at {:?}", current_inode.metadata()?.inode_id);
    return current_inode.mkdir(name, ModeType::from_bits_truncate(mode.bits()));
}

/// @brief 删除文件夹
pub fn do_remove_dir(dirfd: i32, path: &str) -> Result<u64, SystemError> {
    let path = path.trim();

    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;
    let (filename, parent_path) = rsplit_path(&remain_path);

    // 最后一项文件项为.时返回EINVAL
    if filename == "." {
        return Err(SystemError::EINVAL);
    }

    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = inode_begin
        .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

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

    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;
    let inode: Result<Arc<dyn IndexNode>, SystemError> =
        inode_begin.lookup_follow_symlink(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES);

    if inode.is_err() {
        let errno = inode.clone().unwrap_err();
        // 文件不存在，且需要创建
        if errno == SystemError::ENOENT {
            return Err(SystemError::ENOENT);
        }
    }
    // 禁止在目录上unlink
    if inode.unwrap().metadata()?.file_type == FileType::Dir {
        return Err(SystemError::EPERM);
    }

    let (filename, parent_path) = rsplit_path(&remain_path);
    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = inode_begin
        .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件
    parent_inode.unlink(filename)?;

    return Ok(0);
}

pub fn do_symlinkat(from: *const u8, newdfd: i32, to: *const u8) -> Result<usize, SystemError> {
    let oldname = check_and_clone_cstr(from, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let newname = check_and_clone_cstr(to, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let from = oldname.as_str().trim();
    let to = newname.as_str().trim();

    // TODO: 添加权限检查，确保进程拥有目标路径的权限

    let pcb = ProcessManager::current_pcb();
    let (old_begin_inode, old_remain_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), from)?;
    // info!("old_begin_inode={:?}", old_begin_inode.metadata());
    let _ =
        old_begin_inode.lookup_follow_symlink(&old_remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // 得到新创建节点的父节点
    let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newdfd, to)?;
    let (new_name, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent = new_begin_inode
        .lookup_follow_symlink(new_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    // info!("new_parent={:?}", new_parent.metadata());

    if new_parent.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    let new_inode = new_parent.create_with_data(
        new_name,
        FileType::SymLink,
        ModeType::from_bits_truncate(0o777),
        0,
    )?;

    let buf = old_remain_path.as_bytes();
    let len = buf.len();
    new_inode.write_at(0, len, buf, SpinLock::new(FilePrivateData::Unused).lock())?;
    return Ok(0);
}

/// # do_mount - 挂载文件系统
///
/// 将给定的文件系统挂载到指定的挂载点。
///
/// 此函数会检查是否已经挂载了相同的文件系统，如果已经挂载，则返回错误。
/// 它还会处理符号链接，并确保挂载点是有效的。
///
/// ## 参数
///
/// - `fs`: Arc<dyn FileSystem>，要挂载的文件系统。
/// - `mount_point`: &str，挂载点路径。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 挂载成功后返回挂载的文件系统。
/// - `Err(SystemError)`: 挂载失败时返回错误。
pub fn do_mount(fs: Arc<dyn FileSystem>, mount_point: &str) -> Result<Arc<MountFS>, SystemError> {
    let (current_node, rest_path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        mount_point,
    )?;
    let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    if let Some((_, rest, _fs)) = MOUNT_LIST().get_mount_point(mount_point) {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    // 移至IndexNode.mount()来记录
    return inode.mount(fs);
}

/// # do_mount_mkdir - 在指定挂载点创建目录并挂载文件系统
///
/// 在指定的挂载点创建一个目录，并将其挂载到文件系统中。如果挂载点已经存在，并且不是空的，
/// 则会返回错误。成功时，会返回一个新的挂载文件系统的引用。
///
/// ## 参数
///
/// - `fs`: FileSystem - 文件系统的引用，用于创建和挂载目录。
/// - `mount_point`: &str - 挂载点路径，用于创建和挂载目录。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 成功挂载文件系统后，返回挂载文件系统的共享引用。
/// - `Err(SystemError)`: 挂载失败时，返回系统错误。
pub fn do_mount_mkdir(
    fs: Arc<dyn FileSystem>,
    mount_point: &str,
) -> Result<Arc<MountFS>, SystemError> {
    let inode = do_mkdir_at(
        AtFlags::AT_FDCWD.bits(),
        mount_point,
        FileMode::from_bits_truncate(0o755),
    )?;
    if let Some((_, rest, _fs)) = MOUNT_LIST().get_mount_point(mount_point) {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    return inode.mount(fs);
}

/// # do_umount2 - 执行卸载文件系统的函数
///
/// 这个函数用于卸载指定的文件系统。
///
/// ## 参数
///
/// - dirfd: i32 - 目录文件描述符，用于指定要卸载的文件系统的根目录。
/// - target: &str - 要卸载的文件系统的目标路径。
/// - _flag: UmountFlag - 卸载标志，目前未使用。
///
/// ## 返回值
///
/// - Ok(Arc<MountFS>): 成功时返回文件系统的 Arc 引用。
/// - Err(SystemError): 出错时返回系统错误。
///
/// ## 错误处理
///
/// 如果指定的路径没有对应的文件系统，或者在尝试卸载时发生错误，将返回错误。
pub fn do_umount2(
    dirfd: i32,
    target: &str,
    _flag: UmountFlag,
) -> Result<Arc<MountFS>, SystemError> {
    let (work, rest) = user_path_at(&ProcessManager::current_pcb(), dirfd, target)?;
    let path = work.absolute_path()? + &rest;
    let do_umount = || -> Result<Arc<MountFS>, SystemError> {
        if let Some(fs) = MOUNT_LIST().remove(path) {
            // Todo: 占用检测
            fs.umount()?;
            return Ok(fs);
        }
        return Err(SystemError::EINVAL);
    };
    return do_umount();
}
