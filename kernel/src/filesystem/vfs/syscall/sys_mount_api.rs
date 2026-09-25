//! Linux's file-descriptor-based mount creation and attachment API.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_FSCONFIG, SYS_FSMOUNT, SYS_FSOPEN, SYS_MOVE_MOUNT, SYS_OPEN_TREE},
    },
    filesystem::vfs::{
        fcntl::AtFlags,
        file::{File, FileFlags, FilePrivateData},
        mount::{is_mountpoint_root, DetachedMountTree, MountFS, MountFSInode, MountFlags},
        mount_api::context::{
            configure_fs_context, create_mount_from_fs_context, open_fs_context, FsConfigCommand,
        },
        utils::{user_resolved_path_at, ResolvedPath},
        FileType, IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};

use super::sys_mount::may_mount;

const OPEN_TREE_CLONE: u32 = 1;
const OPEN_TREE_CLOEXEC: u32 = 0x0008_0000;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_NO_AUTOMOUNT: u32 = 0x800;
const AT_EMPTY_PATH: u32 = 0x1000;
const AT_RECURSIVE: u32 = 0x8000;

const MOVE_MOUNT_F_SYMLINKS: u32 = 0x1;
const MOVE_MOUNT_F_AUTOMOUNTS: u32 = 0x2;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;
const MOVE_MOUNT_T_SYMLINKS: u32 = 0x10;
const MOVE_MOUNT_T_AUTOMOUNTS: u32 = 0x20;
const MOVE_MOUNT_T_EMPTY_PATH: u32 = 0x40;
const MOVE_MOUNT_SET_GROUP: u32 = 0x100;
const MOVE_MOUNT_BENEATH: u32 = 0x200;
const MOVE_MOUNT_MASK: u32 = 0x377;

const FSOPEN_CLOEXEC: u32 = 1;
const FSMOUNT_CLOEXEC: u32 = 1;
const MOUNT_ATTR_RDONLY: u32 = 0x1;
const MOUNT_ATTR_NOSUID: u32 = 0x2;
const MOUNT_ATTR_NODEV: u32 = 0x4;
const MOUNT_ATTR_NOEXEC: u32 = 0x8;
const MOUNT_ATTR_NOATIME: u32 = 0x10;
const MOUNT_ATTR_STRICTATIME: u32 = 0x20;
const MOUNT_ATTR_ATIME_MASK: u32 = 0x70;
const MOUNT_ATTR_NODIRATIME: u32 = 0x80;
const MOUNT_ATTR_NOSYMFOLLOW: u32 = 0x20_0000;
const FSMOUNT_ATTR_MASK: u32 = MOUNT_ATTR_RDONLY
    | MOUNT_ATTR_NOSUID
    | MOUNT_ATTR_NODEV
    | MOUNT_ATTR_NOEXEC
    | MOUNT_ATTR_ATIME_MASK
    | MOUNT_ATTR_NODIRATIME
    | MOUNT_ATTR_NOSYMFOLLOW;

fn user_path(ptr: usize) -> Result<alloc::string::String, SystemError> {
    vfs_check_and_clone_cstr(ptr as *const u8, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)
}

fn fsconfig_string(ptr: usize) -> Result<alloc::string::String, SystemError> {
    vfs_check_and_clone_cstr(ptr as *const u8, Some(256))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)
}

/// Resolve empty paths from the supplied fd, not from cwd as the legacy
/// user_path_at helper does. The returned owner pins the selected mount until
/// the file or topology operation has acquired its own reference.
fn resolve_mount_path(
    dfd: i32,
    pathname: &str,
    allow_empty: bool,
    follow_final: bool,
) -> Result<ResolvedPath, SystemError> {
    let current = ProcessManager::current_pcb();
    if pathname.is_empty() {
        if !allow_empty {
            return Err(SystemError::ENOENT);
        }
        if dfd == AtFlags::AT_FDCWD.bits() {
            return current.fs_struct().pwd_resolved();
        }
        return current
            .fd_table()
            .get_file_by_fd(dfd)
            .ok_or(SystemError::EBADF)?
            .resolved_path();
    }
    let (start, rest) = user_resolved_path_at(&current, dfd, pathname)?;
    start.inode().lookup_follow_symlink_owned(
        &start,
        &rest,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
        follow_final,
    )
}

fn install_tree_fd(tree: Arc<DetachedMountTree>, cloexec: bool) -> Result<usize, SystemError> {
    let inode: Arc<dyn IndexNode> = tree.root_inode();
    let file = File::new_with_private_data(
        inode,
        FileFlags::O_PATH,
        FilePrivateData::DetachedMount(tree),
    )?;
    let current = ProcessManager::current_pcb();
    let fd = current
        .fd_table()
        .alloc_fd(file, cloexec, current.nofile_soft_limit())?;
    Ok(fd as usize)
}

fn install_path_fd(path: &ResolvedPath, cloexec: bool) -> Result<usize, SystemError> {
    let file = File::new(path.inode(), FileFlags::O_PATH)?;
    let current = ProcessManager::current_pcb();
    let fd = current
        .fd_table()
        .alloc_fd(file, cloexec, current.nofile_soft_limit())?;
    Ok(fd as usize)
}

pub struct SysOpenTree;
impl Syscall for SysOpenTree {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = args[2] as u32;
        if flags
            & !(OPEN_TREE_CLONE
                | OPEN_TREE_CLOEXEC
                | AT_EMPTY_PATH
                | AT_NO_AUTOMOUNT
                | AT_RECURSIVE
                | AT_SYMLINK_NOFOLLOW)
            != 0
            || flags & AT_RECURSIVE != 0 && flags & OPEN_TREE_CLONE == 0
        {
            return Err(SystemError::EINVAL);
        }
        let clone = flags & OPEN_TREE_CLONE != 0;
        if clone && !may_mount() {
            return Err(SystemError::EPERM);
        }
        let pathname = user_path(args[1])?;
        let path = resolve_mount_path(
            args[0] as i32,
            &pathname,
            flags & AT_EMPTY_PATH != 0,
            flags & AT_SYMLINK_NOFOLLOW == 0,
        )?;
        if !clone {
            return install_path_fd(&path, flags & OPEN_TREE_CLOEXEC != 0);
        }
        let inode = path
            .inode()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let tree = MountFS::clone_detached_tree(&inode, flags & AT_RECURSIVE != 0)?;
        install_tree_fd(tree, flags & OPEN_TREE_CLOEXEC != 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("pathname", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[2])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_OPEN_TREE, SysOpenTree);

pub struct SysFsopen;
impl Syscall for SysFsopen {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if !may_mount() {
            return Err(SystemError::EPERM);
        }
        let flags = args[1] as u32;
        if flags & !FSOPEN_CLOEXEC != 0 {
            return Err(SystemError::EINVAL);
        }
        let name = user_path(args[0])?;
        Ok(open_fs_context(&name, flags & FSOPEN_CLOEXEC != 0)? as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fs_name", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[1])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_FSOPEN, SysFsopen);

pub struct SysFsconfig;
impl Syscall for SysFsconfig {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if (args[0] as i32) < 0 {
            return Err(SystemError::EINVAL);
        }
        let command = args[1] as u32;
        let key_ptr = args[2];
        let value_ptr = args[3];
        let aux = args[4] as i32;
        let cmd = match command {
            0 if key_ptr != 0 && value_ptr == 0 && aux == 0 => {
                FsConfigCommand::SetFlag(fsconfig_string(key_ptr)?)
            }
            1 if key_ptr != 0 && value_ptr != 0 && aux == 0 => {
                FsConfigCommand::SetString(fsconfig_string(key_ptr)?, fsconfig_string(value_ptr)?)
            }
            2 if key_ptr != 0 && value_ptr != 0 && (1..=1024 * 1024).contains(&aux) => {
                FsConfigCommand::UnsupportedTyped
            }
            3 | 4
                if key_ptr != 0
                    && value_ptr != 0
                    && (aux == AtFlags::AT_FDCWD.bits() || aux >= 0) =>
            {
                FsConfigCommand::UnsupportedTyped
            }
            5 if key_ptr != 0 && value_ptr == 0 && aux >= 0 => FsConfigCommand::UnsupportedTyped,
            6 if key_ptr == 0 && value_ptr == 0 && aux == 0 => FsConfigCommand::Create,
            8 if key_ptr == 0 && value_ptr == 0 && aux == 0 => FsConfigCommand::CreateExcl,
            7 if key_ptr == 0 && value_ptr == 0 && aux == 0 => FsConfigCommand::Reconfigure,
            0..=8 => return Err(SystemError::EINVAL),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        configure_fs_context(args[0] as i32, cmd)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("cmd", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("key", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("value", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("aux", (args[4] as i32).to_string()),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_FSCONFIG, SysFsconfig);

fn fsmount_flags(attrs: u32) -> Result<MountFlags, SystemError> {
    if attrs & !FSMOUNT_ATTR_MASK != 0 {
        return Err(SystemError::EINVAL);
    }
    let mut flags = MountFlags::empty();
    for (attr, mount_flag) in [
        (MOUNT_ATTR_RDONLY, MountFlags::RDONLY),
        (MOUNT_ATTR_NOSUID, MountFlags::NOSUID),
        (MOUNT_ATTR_NODEV, MountFlags::NODEV),
        (MOUNT_ATTR_NOEXEC, MountFlags::NOEXEC),
        (MOUNT_ATTR_NODIRATIME, MountFlags::NODIRATIME),
        (MOUNT_ATTR_NOSYMFOLLOW, MountFlags::NOSYMFOLLOW),
    ] {
        if attrs & attr != 0 {
            flags.insert(mount_flag);
        }
    }
    match attrs & MOUNT_ATTR_ATIME_MASK {
        0 => flags.insert(MountFlags::RELATIME),
        MOUNT_ATTR_NOATIME => flags.insert(MountFlags::NOATIME),
        MOUNT_ATTR_STRICTATIME => {}
        _ => return Err(SystemError::EINVAL),
    }
    Ok(flags)
}

pub struct SysFsmount;
impl Syscall for SysFsmount {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if !may_mount() {
            return Err(SystemError::EPERM);
        }
        let flags = args[1] as u32;
        if flags & !FSMOUNT_CLOEXEC != 0 {
            return Err(SystemError::EINVAL);
        }
        let mount_flags = fsmount_flags(args[2] as u32)?;
        let tree =
            create_mount_from_fs_context(args[0] as i32, |fs, source, sb_flags, owner_user_ns| {
                MountFS::create_detached_tree(fs, sb_flags, mount_flags, source, owner_user_ns)
            })?;
        install_tree_fd(tree, flags & FSMOUNT_CLOEXEC != 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fsfd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("attrs", format!("{:#x}", args[2])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_FSMOUNT, SysFsmount);

pub struct SysMoveMount;
impl Syscall for SysMoveMount {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if !may_mount() {
            return Err(SystemError::EPERM);
        }
        let flags = args[4] as u32;
        if flags & !MOVE_MOUNT_MASK != 0
            || flags & (MOVE_MOUNT_SET_GROUP | MOVE_MOUNT_BENEATH)
                == (MOVE_MOUNT_SET_GROUP | MOVE_MOUNT_BENEATH)
        {
            return Err(SystemError::EINVAL);
        }
        if flags & (MOVE_MOUNT_SET_GROUP | MOVE_MOUNT_BENEATH) != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        // Automount is not a DragonOS VFS feature, so these lookup controls
        // have no additional effect. Symlink and empty-path flags do.
        let _ = flags & (MOVE_MOUNT_F_AUTOMOUNTS | MOVE_MOUNT_T_AUTOMOUNTS);
        let from_path = user_path(args[1])?;
        let to_path = user_path(args[3])?;
        let source = resolve_mount_path(
            args[0] as i32,
            &from_path,
            flags & MOVE_MOUNT_F_EMPTY_PATH != 0,
            flags & MOVE_MOUNT_F_SYMLINKS != 0,
        )?;
        let target = resolve_mount_path(
            args[2] as i32,
            &to_path,
            flags & MOVE_MOUNT_T_EMPTY_PATH != 0,
            flags & MOVE_MOUNT_T_SYMLINKS != 0,
        )?;
        let source_inode = source.inode();
        let target_inode = target.inode();
        let source_is_dir = source_inode.metadata()?.file_type == FileType::Dir;
        let target_is_dir = target_inode.metadata()?.file_type == FileType::Dir;
        if source_is_dir != target_is_dir {
            return Err(SystemError::EINVAL);
        }
        let mountpoint = target_inode
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let current_mntns = ProcessManager::current_mntns();
        if from_path.is_empty() && flags & MOVE_MOUNT_F_EMPTY_PATH != 0 {
            let file = ProcessManager::current_pcb()
                .fd_table()
                .get_file_by_fd(args[0] as i32)
                .ok_or(SystemError::EBADF)?;
            let tree = match &*file.private_data.lock() {
                FilePrivateData::DetachedMount(tree) => Some(tree.clone()),
                _ => None,
            };
            if let Some(tree) = tree {
                return current_mntns
                    .attach_detached_tree(&tree, &mountpoint)
                    .map(|_| 0);
            }
        }
        if !is_mountpoint_root(&source_inode) {
            return Err(SystemError::EINVAL);
        }
        let source_mfs = source_inode
            .fs()
            .downcast_arc::<MountFS>()
            .ok_or(SystemError::EINVAL)?;
        current_mntns.move_mount(&source_mfs, &mountpoint)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("from_dfd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("from", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("to_dfd", (args[2] as i32).to_string()),
            FormattedSyscallParam::new("to", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[4])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_MOVE_MOUNT, SysMoveMount);
