use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::sync::Arc;
use log::{error, info};
use system_error::SystemError;

use crate::{
    define_event_trace, define_trace_point,
    driver::base::block::{gendisk::GenDisk, manager::block_dev_manager},
    filesystem::{
        devfs::devfs_init,
        ext4::filesystem::Ext4FileSystem,
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
    mount::init_mountlist,
    stat::LookUpFlags,
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

    if let Some(disk) = try_find_gendisk("/dev/vdb1") {
        let ext4fs = Ext4FileSystem::from_gendisk(disk);

        let _ = ROOT_INODE()
            .mkdir("ext4", ModeType::from_bits_truncate(0o755))?
            .mount(ext4fs.unwrap())?;
    }

    if r.is_err() {
        error!("Failed to migrate virtual filesyst  em to FAT32!");
        loop {
            spin_loop();
        }
    }
    info!("Successfully migrate rootfs to FAT32!");

    return Ok(());
}

define_event_trace!(DO_MKDIR_AT,(path:&str,mode:FileMode),
    format_args!("mkdir at {} with mode {:?}",path, mode));
/// @brief 创建文件/文件夹
pub fn do_mkdir_at(
    dirfd: i32,
    path: &str,
    mode: FileMode,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    TRACE_DO_MKDIR_AT(path, mode);
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

pub(super) fn do_file_lookup_at(
    dfd: i32,
    path: &str,
    lookup_flags: LookUpFlags,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dfd, path)?;
    let follow_final = lookup_flags.contains(LookUpFlags::FOLLOW);
    return inode.lookup_follow_symlink2(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES, follow_final);
}
