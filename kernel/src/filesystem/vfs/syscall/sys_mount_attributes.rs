//! Userspace ABI decoding for mount_setattr; topology transactions live in VFS.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MOUNT_SETATTR},
    filesystem::vfs::{
        file::{FilePrivateData, NamespaceFilePrivateData},
        mount::{
            attributes::{apply_mount_attributes, MountAttributeChange},
            idmap::MountIdmap,
            is_mountpoint_root, MountFSInode, MountFlags,
        },
        IndexNode, MAX_PATHLEN,
    },
    libs::casting::DowncastArc,
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::{
            propagation::PropagationType,
            user_namespace::{UserNamespace, INIT_USER_NAMESPACE},
        },
        ProcessManager,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
        user_buffer::UserBuffer,
    },
};

use super::{sys_mount::may_mount, sys_mount_api::resolve_mount_path};

const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_NO_AUTOMOUNT: u32 = 0x800;
const AT_EMPTY_PATH: u32 = 0x1000;
const AT_RECURSIVE: u32 = 0x8000;
const ATTR_ATIME: u64 = 0x70;
const ATTR_IDMAP: u64 = 0x10_0000;
const ATTR_VALID: u64 = 0x20_00ff | ATTR_IDMAP;

#[repr(C)]
#[derive(Clone, Copy)]
struct MountAttr {
    set: u64,
    clear: u64,
    propagation: u64,
    userns_fd: u64,
}

fn ordinary_flags(bits: u64) -> MountFlags {
    let mut flags = MountFlags::empty();
    for (bit, flag) in [
        (1, MountFlags::RDONLY),
        (2, MountFlags::NOSUID),
        (4, MountFlags::NODEV),
        (8, MountFlags::NOEXEC),
        (0x80, MountFlags::NODIRATIME),
        (0x20_0000, MountFlags::NOSYMFOLLOW),
    ] {
        if bits & bit != 0 {
            flags.insert(flag);
        }
    }
    flags
}

fn decode(
    attr: &MountAttr,
) -> Result<(MountAttributeChange, Option<Arc<UserNamespace>>), SystemError> {
    let propagation = match attr.propagation {
        0 => None,
        0x10_0000 => Some(PropagationType::Shared),
        0x8_0000 => Some(PropagationType::Slave),
        0x4_0000 => Some(PropagationType::Private),
        0x2_0000 => Some(PropagationType::Unbindable),
        _ => return Err(SystemError::EINVAL),
    };
    if (attr.set | attr.clear) & !ATTR_VALID != 0 {
        return Err(SystemError::EINVAL);
    }
    let mut set = ordinary_flags(attr.set);
    let mut clear = ordinary_flags(attr.clear);
    if attr.clear & ATTR_ATIME != 0 {
        if attr.clear & ATTR_ATIME != ATTR_ATIME {
            return Err(SystemError::EINVAL);
        }
        clear.insert(MountFlags::NOATIME | MountFlags::RELATIME | MountFlags::STRICTATIME);
        match attr.set & ATTR_ATIME {
            0 => set.insert(MountFlags::RELATIME),
            0x10 => set.insert(MountFlags::NOATIME),
            0x20 => {}
            _ => return Err(SystemError::EINVAL),
        }
    } else if attr.set & ATTR_ATIME != 0 {
        return Err(SystemError::EINVAL);
    }
    if attr.clear & ATTR_IDMAP != 0 {
        return Err(SystemError::EINVAL);
    }
    let namespace = if attr.set & ATTR_IDMAP != 0 {
        if attr.userns_fd > i32::MAX as u64 {
            return Err(SystemError::EINVAL);
        }
        let file = ProcessManager::current_pcb()
            .fd_table()
            .get_file_by_fd(attr.userns_fd as i32)
            .ok_or(SystemError::EBADF)?;
        let namespace = match &*file.private_data.lock() {
            FilePrivateData::Namespace(NamespaceFilePrivateData::User(ns)) => ns.clone(),
            _ => return Err(SystemError::EINVAL),
        };
        if Arc::ptr_eq(&namespace, &INIT_USER_NAMESPACE)
            || !ns_capable(&namespace, CAPFlags::CAP_SYS_ADMIN)
        {
            return Err(SystemError::EPERM);
        }
        Some(namespace)
    } else {
        None
    };
    Ok((
        MountAttributeChange {
            set,
            clear,
            propagation,
            idmap: None,
        },
        namespace,
    ))
}

/// Match Linux check_zeroed_user's aligned-word access and early termination.
/// This matters when a nonzero extension precedes an inaccessible later page.
fn check_extension(ptr: usize, size: usize) -> Result<(), SystemError> {
    if size == core::mem::size_of::<MountAttr>() {
        return Ok(());
    }
    let start = ptr
        .checked_add(core::mem::size_of::<MountAttr>())
        .ok_or(SystemError::EFAULT)?;
    let end = ptr.checked_add(size).ok_or(SystemError::EFAULT)?;
    let mut word = start & !7;
    while word < end {
        let buffer = UserBuffer::new_protected(word as *mut u8, 8, true)?;
        let bytes: [u8; 8] = buffer.read_one(0)?;
        let first = start.saturating_sub(word);
        let last = core::cmp::min(end - word, 8);
        if bytes[first..last].iter().any(|byte| *byte != 0) {
            return Err(SystemError::E2BIG);
        }
        word = word.checked_add(8).ok_or(SystemError::EFAULT)?;
    }
    Ok(())
}

pub struct SysMountSetattr;
impl Syscall for SysMountSetattr {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = args[2] as u32;
        if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_RECURSIVE) != 0 {
            return Err(SystemError::EINVAL);
        }
        let size = args[4];
        if size > 4096 {
            return Err(SystemError::E2BIG);
        }
        if size < core::mem::size_of::<MountAttr>() {
            return Err(SystemError::EINVAL);
        }
        if !may_mount() {
            return Err(SystemError::EPERM);
        }
        let buffer = UserBuffer::new_protected(args[3] as *mut u8, size, true)?;
        // Linux copy_struct_from_user checks the extension before copying the
        // known fields, including error precedence when both ranges are bad.
        check_extension(args[3], size)?;
        let attr: MountAttr = buffer.read_one(0)?;
        if attr.set == 0 && attr.clear == 0 && attr.propagation == 0 {
            return Ok(0);
        }
        let (mut request, namespace) = decode(&attr)?;
        let pathname = vfs_check_and_clone_cstr(args[1] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = resolve_mount_path(
            args[0] as i32,
            &pathname,
            flags & AT_EMPTY_PATH != 0,
            flags & AT_SYMLINK_NOFOLLOW == 0,
            flags & AT_NO_AUTOMOUNT != 0,
        )?;
        let selected = path.inode();
        if !is_mountpoint_root(&selected) {
            return Err(SystemError::EINVAL);
        }
        let inode = selected
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        request.idmap = namespace
            .map(|namespace| {
                Arc::try_new(MountIdmap::new(namespace)).map_err(|_| SystemError::ENOMEM)
            })
            .transpose()?;
        apply_mount_attributes(&inode.mount_fs(), &request, flags & AT_RECURSIVE != 0)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("path", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("attr", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("size", args[4].to_string()),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_MOUNT_SETATTR, SysMountSetattr);
