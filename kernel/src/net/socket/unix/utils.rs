use crate::process::ProcessManager;
use core::mem::size_of;
use system_error::SystemError;

use crate::syscall::user_access::UserBufferWriter;

/// Best-effort rollback for file descriptors allocated during SCM_RIGHTS delivery.
///
/// This is used when copying control messages to userspace fails after we have
/// already allocated fds in the current process.
pub(super) fn rollback_allocated_fds(fds: &[i32]) {
    if fds.is_empty() {
        return;
    }

    let fd_table_binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table = fd_table_binding.write();
    for &fd in fds {
        let _ = fd_table.drop_fd(fd);
    }
}

// ===== Ancillary message (cmsg) support =====

/// Ancillary message header, matches Linux `struct cmsghdr`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Cmsghdr {
    pub cmsg_len: usize,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// Socket options level for `SOL_SOCKET`.
pub const SOL_SOCKET: i32 = 1;

/// SCM_RIGHTS - passes file descriptors.
pub const SCM_RIGHTS: i32 = 1;

/// SCM_CREDENTIALS - passes sender credentials.
pub const SCM_CREDENTIALS: i32 = 2;

/// MSG_CTRUNC - control data truncated.
pub const MSG_CTRUNC: i32 = 0x8;

/// Aligns a length to the alignment requirement for ancillary messages.
pub fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

/// Control message buffer for writing ancillary data.
pub struct CmsgBuffer<'a> {
    pub ptr: *mut u8,
    pub len: usize,
    pub write_off: &'a mut usize,
}

impl<'a> CmsgBuffer<'a> {
    /// Writes a control message following Linux put_cmsg semantics:
    /// - Writes if there is at least CMSG_LEN(full_len) space (no trailing padding required).
    /// - Copies at most what fits and sets MSG_CTRUNC if truncated.
    /// - Advances by min(CMSG_SPACE(full_len), remaining_space).
    pub fn put(
        &mut self,
        msg_flags: &mut i32,
        level: i32,
        cmsg_type: i32,
        full_len: usize,
        data: &[u8],
    ) -> Result<(), SystemError> {
        let hdr_len = size_of::<Cmsghdr>();
        if self.ptr.is_null() || self.len < hdr_len {
            *msg_flags |= MSG_CTRUNC;
            return Ok(());
        }

        let remaining = self.len.saturating_sub(*self.write_off);
        if remaining < hdr_len {
            *msg_flags |= MSG_CTRUNC;
            return Ok(());
        }

        let cmsg_len_full = cmsg_align(hdr_len) + full_len;
        let mut cmsg_len_to_write = cmsg_len_full;
        if remaining < cmsg_len_full {
            *msg_flags |= MSG_CTRUNC;
            cmsg_len_to_write = remaining;
        }

        let hdr = Cmsghdr {
            cmsg_len: cmsg_len_to_write,
            cmsg_level: level,
            cmsg_type,
        };

        let hdr_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts((&hdr as *const Cmsghdr) as *const u8, hdr_len) };
        {
            let ptr = unsafe { self.ptr.add(*self.write_off) };
            let mut w = UserBufferWriter::new(ptr, hdr_len, true)?;
            w.buffer_protected(0)?.write_to_user(0, hdr_bytes)?;
        }

        let data_off = *self.write_off + cmsg_align(hdr_len);
        let data_can_write = cmsg_len_to_write
            .saturating_sub(cmsg_align(hdr_len))
            .min(data.len());
        if data_can_write != 0 {
            let ptr = unsafe { self.ptr.add(data_off) };
            let mut w = UserBufferWriter::new(ptr, data_can_write, true)?;
            w.buffer_protected(0)?
                .write_to_user(0, &data[..data_can_write])?;
        }

        let cmsg_space = cmsg_align(hdr_len) + cmsg_align(full_len);
        let advance = core::cmp::min(cmsg_space, remaining);
        *self.write_off += advance;
        Ok(())
    }
}
