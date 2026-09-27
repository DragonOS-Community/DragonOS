//! Linux-compatible anonymous shmem file creation (without hugetlbfs).

use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MEMFD_CREATE},
    filesystem::{tmpfs::create_memfd_file, vfs::NAME_MAX},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};

const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;
const MFD_HUGETLB: u32 = 0x0004;
const MFD_NOEXEC_SEAL: u32 = 0x0008;
const MFD_EXEC: u32 = 0x0010;
const MFD_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB | MFD_NOEXEC_SEAL | MFD_EXEC;
const MFD_HUGE_SIZE_MASK: u32 = 0x3f << 26;
const MEMFD_PREFIX_LEN: usize = b"memfd:".len();

pub struct SysMemfdCreate;

impl Syscall for SysMemfdCreate {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = args[1] as u32;
        let allowed = MFD_FLAGS
            | if flags & MFD_HUGETLB != 0 {
                MFD_HUGE_SIZE_MASK
            } else {
                0
            };
        if flags & !allowed != 0
            || flags & (MFD_EXEC | MFD_NOEXEC_SEAL) == (MFD_EXEC | MFD_NOEXEC_SEAL)
        {
            return Err(SystemError::EINVAL);
        }

        // Linux limits the displayed name including the "memfd:" prefix to
        // NAME_MAX bytes.  The string is an arbitrary byte sequence, not UTF-8.
        let name =
            vfs_check_and_clone_cstr(args[0] as *const u8, Some(NAME_MAX - MEMFD_PREFIX_LEN + 1))
                .map_err(|err| {
                    if err == SystemError::ENAMETOOLONG {
                        SystemError::EINVAL
                    } else {
                        err
                    }
                })?
                .into_bytes();

        // Linux without CONFIG_HUGETLBFS returns ENOSYS.  Never substitute
        // ordinary 4-KiB shmem pages for a requested hugepage file.
        if flags & MFD_HUGETLB != 0 {
            return Err(SystemError::ENOSYS);
        }

        let file = create_memfd_file(
            name,
            flags & MFD_ALLOW_SEALING != 0,
            flags & MFD_NOEXEC_SEAL != 0,
        )?;
        let current = ProcessManager::current_pcb();
        let fd = current.fd_table().alloc_fd_arc(
            file,
            flags & MFD_CLOEXEC != 0,
            current.nofile_soft_limit(),
        )?;
        Ok(fd as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("name", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("flags", (args[1] as u32).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_MEMFD_CREATE, SysMemfdCreate);
