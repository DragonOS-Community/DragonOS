use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_READV;
use crate::arch::MMArch;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::mm::MemoryManagementArch;
use crate::mm::VirtAddr;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::{copy_to_user_protected, user_accessible_len};
use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_read::do_read;

/// System call handler for `readv` operation
///
/// The `readv` system call reads data into multiple buffers from a file descriptor.
/// It is equivalent to multiple `read` calls but is more efficient.
pub struct SysReadVHandle;

impl Syscall for SysReadVHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let count = Self::count(args);

        // IoVecs 会进行用户态检验(包含 len==0 的 iov_base 校验)。
        let iovecs = unsafe { IoVecs::from_user(iov, count, true) }?;

        // TODO: Here work around, not suppose to read entire buf once
        use crate::process::ProcessManager;
        if let Ok(_socket_inode) = ProcessManager::current_pcb().get_socket_inode(fd) {
            // Socket: read entire message then scatter to iovecs
            let mut buf = iovecs.new_buf(true);
            let nread = do_read(fd, &mut buf)?;
            iovecs.scatter(&buf[..nread])?;
            return Ok(nread);
        }

        // Linux: limit per readv() to MAX_RW_COUNT = INT_MAX & ~(PAGE_SIZE-1)
        let max_rw_count = (i32::MAX as usize) & !(MMArch::PAGE_SIZE - 1);

        let mut total_read: usize = 0;

        // Keep kernel-side buffer modest to avoid huge allocations.
        // Also used as the granularity for accessibility checks to avoid
        // traversing huge address ranges at once.
        const CHUNK: usize = 64 * 1024;

        for one in iovecs.iovs().iter() {
            // Check if we've reached MAX_RW_COUNT limit
            if total_read >= max_rw_count {
                break;
            }

            let remain = max_rw_count - total_read;
            let want = core::cmp::min(one.iov_len, remain);
            if want == 0 {
                continue;
            }

            let mut copied_this_iov = 0usize;
            while copied_this_iov < want {
                // Calculate how much to process in this iteration
                let remain_iov = want - copied_this_iov;
                let chunk_len = core::cmp::min(CHUNK, remain_iov);

                let current_base = one.iov_base as usize + copied_this_iov;

                // Check accessibility for this chunk only (not the entire iovec)
                // This avoids traversing huge address ranges at once
                let accessible = user_accessible_len(VirtAddr::new(current_base), chunk_len, true);
                if accessible == 0 {
                    if total_read == 0 && copied_this_iov == 0 {
                        return Err(SystemError::EFAULT);
                    }
                    // Hit unmapped region, return what we've read so far
                    return Ok(total_read);
                }

                // Read into kernel buffer
                let to_read = core::cmp::min(accessible, chunk_len);
                let mut kbuf = alloc::vec![0u8; to_read];
                let n = do_read(fd, &mut kbuf[..])?;
                if n == 0 {
                    // EOF
                    return Ok(total_read);
                }

                // Copy to user space
                let dst = VirtAddr::new(current_base);
                let write_res = unsafe { copy_to_user_protected(dst, &kbuf[..n]) };
                match write_res {
                    Ok(_) => {
                        copied_this_iov += n;
                        total_read = total_read.saturating_add(n);

                        // Check MAX_RW_COUNT limit after each chunk
                        if total_read >= max_rw_count {
                            return Ok(total_read);
                        }
                    }
                    Err(SystemError::EFAULT) => {
                        // Linux: return partial count if any bytes were copied.
                        if total_read == 0 {
                            return Err(SystemError::EFAULT);
                        }
                        return Ok(total_read);
                    }
                    Err(e) => return Err(e),
                }

                // Stop on short read (EOF or error in underlying file)
                if n < to_read {
                    return Ok(total_read);
                }
            }
        }

        Ok(total_read)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("count", Self::count(args).to_string()),
        ]
    }
}

impl SysReadVHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    fn count(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_READV, SysReadVHandle);
