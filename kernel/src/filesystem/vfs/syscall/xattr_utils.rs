#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::kernel_buffer::KernelBuffer as XattrBuffer;
use crate::{
    filesystem::{
        fsnotify::{self, FsEvent},
        vfs::{
            file::File, syscall::AtFlags, utils::user_path_at, IndexNode, XattrFlags, MAX_PATHLEN,
        },
    },
    mm::{allocation::MemoryAllocationError, VirtAddr},
    process::ProcessManager,
    syscall::user_access::{
        check_and_clone_cstr, copy_from_user_protected, copy_to_user_protected,
        vfs_check_and_clone_cstr,
    },
};
use alloc::{string::String, sync::Arc};
#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
type XattrBuffer = alloc::vec::Vec<u8>;
use system_error::SystemError;

const XATTR_LIST_MAX: usize = 65536;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;

struct SetxattrArgs {
    name: String,
    value: Option<XattrBuffer>,
    flags: XattrFlags,
}

fn clone_xattr_name(name_ptr: *const u8) -> Result<String, SystemError> {
    let name = check_and_clone_cstr(name_ptr, Some(XATTR_NAME_MAX + 1))?;
    if name.as_bytes().is_empty() || name.as_bytes().len() > XATTR_NAME_MAX {
        return Err(SystemError::ERANGE);
    }

    name.into_string().map_err(|_| SystemError::EINVAL)
}

fn parse_setxattr_flags(flags: i32) -> Result<XattrFlags, SystemError> {
    XattrFlags::from_bits(flags).ok_or(SystemError::EINVAL)
}

fn prepare_setxattr_args(
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    flags: i32,
) -> Result<SetxattrArgs, SystemError> {
    let flags = parse_setxattr_flags(flags)?;
    let name = clone_xattr_name(name_ptr)?;

    let value = File::recover_io(|| {
        copy_xattr_input(size, allocate_xattr_buffer, |value| {
            // Copy once into owned storage; later backing retries retain this
            // snapshot rather than reading potentially changed user bytes.
            unsafe { copy_from_user_protected(value, VirtAddr::new(value_ptr as usize)) }
                .map(|_| ())
        })
    })?;

    Ok(SetxattrArgs { name, value, flags })
}

/// Extended attribute GET operations
pub(super) fn path_getxattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = crate::filesystem::vfs::file::File::recover_io(|| {
        current_node.lookup_follow_symlink(&rest_path, lookup_flags)
    })?;

    do_getxattr(inode, name_ptr, buf_ptr, size)
}

pub(super) fn fd_getxattr(
    fd: i32,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
    // 获取文件描述符对应的文件节点
    // Pin the open file, but release the fdtable's internal read guard before
    // user faults, allocation recovery, or filesystem callbacks.
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    // 调用VFS接口获取扩展属性
    do_getxattr(inode, name_ptr, buf_ptr, size)
}

/// Extended attribute LIST operations
pub(super) fn path_listxattr(
    path_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = crate::filesystem::vfs::file::File::recover_io(|| {
        current_node.lookup_follow_symlink(&rest_path, lookup_flags)
    })?;

    do_listxattr(inode, buf_ptr, size)
}

pub(super) fn fd_listxattr(fd: i32, buf_ptr: *mut u8, size: usize) -> Result<usize, SystemError> {
    // Pin the open file, but release the fdtable's internal read guard before
    // user faults, allocation recovery, or filesystem callbacks.
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    do_listxattr(inode, buf_ptr, size)
}

fn do_listxattr(
    inode: Arc<dyn IndexNode>,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
    File::recover_io(|| {
        copy_xattr_output(
            size,
            XATTR_LIST_MAX,
            allocate_xattr_buffer,
            |buf| inode.listxattr(buf),
            |bytes| {
                unsafe { copy_to_user_protected(VirtAddr::new(buf_ptr as usize), bytes) }
                    .map(|_| ())
            },
        )
    })
}

/// Extended attribute REMOVE operations
pub(super) fn path_removexattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = crate::filesystem::vfs::file::File::recover_io(|| {
        current_node.lookup_follow_symlink(&rest_path, lookup_flags)
    })?;

    do_removexattr(inode, name_ptr)
}

pub(super) fn fd_removexattr(fd: i32, name_ptr: *const u8) -> Result<usize, SystemError> {
    // Pin the open file, but release the fdtable's internal read guard before
    // user faults, allocation recovery, or filesystem callbacks.
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    do_removexattr(inode, name_ptr)
}

fn do_removexattr(inode: Arc<dyn IndexNode>, name_ptr: *const u8) -> Result<usize, SystemError> {
    let name = clone_xattr_name(name_ptr)?;

    let result = crate::filesystem::vfs::file::File::recover_io(|| inode.removexattr(&name))?;
    fsnotify::fsnotify_inode(FsEvent::ATTRIB, &inode);
    Ok(result)
}

fn do_getxattr(
    inode: Arc<dyn IndexNode>,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
    let name = clone_xattr_name(name_ptr)?;

    File::recover_io(|| {
        copy_xattr_output(
            size,
            XATTR_SIZE_MAX,
            allocate_xattr_buffer,
            |buf| inode.getxattr(&name, buf),
            |bytes| {
                unsafe { copy_to_user_protected(VirtAddr::new(buf_ptr as usize), bytes) }
                    .map(|_| ())
            },
        )
    })
}

/// Extended attribute SET operations
pub(super) fn path_setxattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    lookup_flags: usize,
    flags: i32,
) -> Result<usize, SystemError> {
    let xattr = prepare_setxattr_args(name_ptr, value_ptr, size, flags)?;

    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = crate::filesystem::vfs::file::File::recover_io(|| {
        current_node.lookup_follow_symlink(&rest_path, lookup_flags)
    })?;

    do_setxattr(inode, xattr)
}

pub(super) fn fd_setxattr(
    fd: i32,
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    flags: i32,
) -> Result<usize, SystemError> {
    // Pin the open file, but release the fdtable's internal read guard before
    // user faults, allocation recovery, or filesystem callbacks.
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    let xattr = prepare_setxattr_args(name_ptr, value_ptr, size, flags)?;
    do_setxattr(inode, xattr)
}

fn do_setxattr(inode: Arc<dyn IndexNode>, xattr: SetxattrArgs) -> Result<usize, SystemError> {
    let result = crate::filesystem::vfs::file::File::recover_io(|| {
        inode.setxattr(
            &xattr.name,
            xattr.value.as_deref().unwrap_or(&[]),
            xattr.flags,
        )
    })?;
    fsnotify::fsnotify_inode(FsEvent::ATTRIB, &inode);
    Ok(result)
}

fn allocate_xattr_buffer(size: usize) -> Result<XattrBuffer, MemoryAllocationError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        XattrBuffer::try_new_zeroed(size)
    }
    // Architectures without vmalloc retain their existing heap contract. This
    // compatibility branch does not provide KernelBuffer's byte accounting.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        let mut buffer = XattrBuffer::new();
        buffer
            .try_reserve_exact(size)
            .map_err(|_| SystemError::ENOMEM)?;
        buffer.resize(size, 0);
        Ok(buffer)
    }
}

/// A successful input copy is an owned snapshot, independent of VFS retries.
fn copy_xattr_input(
    size: usize,
    allocate: impl FnOnce(usize) -> Result<XattrBuffer, MemoryAllocationError>,
    copy: impl FnOnce(&mut [u8]) -> Result<(), SystemError>,
) -> Result<Option<XattrBuffer>, MemoryAllocationError> {
    if size > XATTR_SIZE_MAX {
        return Err(SystemError::E2BIG.into());
    }
    if size == 0 {
        return Ok(None);
    }
    let mut value = allocate(size)?;
    copy(&mut value)?;
    Ok(Some(value))
}

/// Only scratch admission is typed: it happens before any backing read or
/// copyout. Backing/copy errors are terminal, so the outer recovery cannot
/// repeat either operation. Local closures allow controlled tests of this
/// same transaction without changing global allocator or user access state.
fn copy_xattr_output(
    size: usize,
    limit: usize,
    allocate: impl FnOnce(usize) -> Result<XattrBuffer, MemoryAllocationError>,
    read: impl FnOnce(&mut [u8]) -> Result<usize, SystemError>,
    copy: impl FnOnce(&[u8]) -> Result<(), SystemError>,
) -> Result<usize, MemoryAllocationError> {
    if size == 0 {
        return Ok(read(&mut [])?);
    }
    let capacity = size.min(limit);
    let mut buffer = allocate(capacity)?;
    let actual = match read(&mut buffer) {
        Err(SystemError::ERANGE) if capacity == limit => return Err(SystemError::E2BIG.into()),
        result => result?,
    };
    if actual > capacity {
        return Err(if capacity == limit {
            SystemError::E2BIG
        } else {
            SystemError::ERANGE
        }
        .into());
    }
    if actual != 0 {
        copy(&buffer[..actual])?;
    }
    Ok(actual)
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
mod selftest;
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) use selftest::report as selftest_report;
