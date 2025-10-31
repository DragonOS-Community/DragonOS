use crate::arch::syscall::nr::SYS_SENDFILE;
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::vec::Vec;
use system_error::SystemError;

/// See <https://man7.org/linux/man-pages/man2/sendfile64.2.html>
pub struct SysSendfileHandle;

impl Syscall for SysSendfileHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let offset_ptr = args[2] as *const isize;
        let out_fd = args[0] as i32;
        let in_fd = args[1] as i32;
        let count = args[3] as isize;

        let offset = if offset_ptr.is_null() {
            None
        } else {
            let offset = *UserBufferReader::new(offset_ptr, size_of::<isize>(), true)?
                .read_one_from_user::<isize>(0)?;
            if offset < 0 {
                return Err(SystemError::EINVAL);
            }
            Some(offset)
        };

        log::trace!(
            "out_fd = {}, in_fd = {}, offset = {:x?}, count = 0x{:x}",
            out_fd,
            in_fd,
            offset,
            count
        );

        let count = if count < 0 {
            return Err(SystemError::EINVAL);
        } else {
            count as usize
        };

        let (out_file, in_file) = {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.write();

            let out_file = fd_table_guard
                .get_file_by_fd(out_fd)
                .ok_or(SystemError::EBADF)?;
            let in_file = fd_table_guard
                .get_file_by_fd(in_fd)
                .ok_or(SystemError::EBADF)?;
            (out_file, in_file)
        };

        let mut buffer = vec![0u8; 4096].into_boxed_slice();
        let mut total_len = 0;
        let mut offset = offset.map(|offset| offset as usize);

        while total_len < count {
            // The offset decides how to read from `in_file`.
            // If offset is `Some(_)`, the data will be read from the given offset,
            // and after reading, the file offset of `in_file` will remain unchanged.
            // If offset is `None`, the data will be read from the file offset,
            // and the file offset of `in_file` is adjusted
            // to reflect the number of bytes read from `in_file`.
            let max_readlen = buffer.len().min(count - total_len);
            // Read from `in_file`
            let read_res = if let Some(offset) = offset.as_mut() {
                let res = in_file.do_read(*offset, max_readlen, &mut buffer[..max_readlen], false);
                if let Ok(len) = res.as_ref() {
                    *offset += *len;
                }
                res
            } else {
                in_file.read(max_readlen, &mut buffer[..max_readlen])
            };

            let read_len = match read_res {
                Ok(len) => len,
                Err(e) => {
                    if total_len > 0 {
                        log::warn!("error occurs when trying to read file: {:?}", e);
                        break;
                    }
                    return Err(e);
                }
            };

            if read_len == 0 {
                break;
            }

            // Note: `sendfile` allows sending partial data,
            // so short reads and short writes are all acceptable
            let write_res = out_file.write(read_len, &buffer[..read_len]);

            match write_res {
                Ok(len) => {
                    total_len += len;
                    if len < 4096 {
                        break;
                    }
                }
                Err(e) => {
                    if total_len > 0 {
                        log::warn!("error occurs when trying to write file: {:?}", e);
                        break;
                    }
                    return Err(e);
                }
            }
        }

        Ok(total_len)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            crate::syscall::table::FormattedSyscallParam::new("out_fd", format!("{:#x}", args[0])),
            crate::syscall::table::FormattedSyscallParam::new("in_fd", format!("{:#x}", args[1])),
            crate::syscall::table::FormattedSyscallParam::new("offset", format!("{:#x}", args[2])),
            crate::syscall::table::FormattedSyscallParam::new("count", format!("{:#x}", args[3])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SENDFILE, SysSendfileHandle);
