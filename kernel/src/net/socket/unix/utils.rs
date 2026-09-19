use crate::process::ProcessManager;
use alloc::sync::Arc;
use core::mem::size_of;
use system_error::SystemError;

use crate::filesystem::vfs::file::File;
use crate::syscall::user_access::UserBufferWriter;
use crate::syscall::user_buffer::UserBuffer;

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
    /// Linux scm_detach_fds: deliver a successful prefix, never roll back a
    /// published fd. Ancillary failures do not discard the received payload.
    pub(super) fn put_rights(&mut self, msg_flags: &mut i32, rights: &[Arc<File>], cloexec: bool) {
        let hdr_len = size_of::<Cmsghdr>();
        let remaining = self.len.saturating_sub(*self.write_off);
        let fit = if self.ptr.is_null() {
            0
        } else {
            (remaining.saturating_sub(hdr_len) / size_of::<i32>()).min(rights.len())
        };
        let current = ProcessManager::current_pcb();
        let table = current.fd_table();
        let mut delivered = 0;
        for file in rights.iter().take(fit) {
            let Ok(reservation) = table.reserve::<1>(current.nofile_soft_limit(), 0, cloexec)
            else {
                break;
            };
            let offset = hdr_len + delivered * size_of::<i32>();
            if self
                .write_rights_field(offset, &reservation.fd(0).to_ne_bytes())
                .is_err()
            {
                // Dropping this reservation only releases its unpublished slot.
                break;
            }
            reservation.commit_arc(file.clone());
            delivered += 1;
        }

        if delivered != 0 {
            let data_len = delivered * size_of::<i32>();
            // Header faults preserve the delivered fds and the old offset;
            // only an incomplete fd prefix implies MSG_CTRUNC.
            if self.write_rights_header(data_len).is_ok() {
                *self.write_off += (hdr_len + cmsg_align(data_len)).min(remaining);
            }
        }
        if delivered < rights.len() {
            *msg_flags |= MSG_CTRUNC;
        }
    }

    /// scm_detach_fds writes level, type, then length, stopping at the first
    /// fault. This differs from put(), which writes the header before data.
    fn write_rights_header(&self, data_len: usize) -> Result<(), SystemError> {
        let cmsg_len = size_of::<Cmsghdr>() + data_len;
        self.write_rights_field(
            core::mem::offset_of!(Cmsghdr, cmsg_level),
            &SOL_SOCKET.to_ne_bytes(),
        )?;
        self.write_rights_field(
            core::mem::offset_of!(Cmsghdr, cmsg_type),
            &SCM_RIGHTS.to_ne_bytes(),
        )?;
        self.write_rights_field(
            core::mem::offset_of!(Cmsghdr, cmsg_len),
            &cmsg_len.to_ne_bytes(),
        )
    }

    /// Write just the current field, so an inaccessible later page does not
    /// prevent delivery of the writable prefix. Do not form a user Rust slice.
    fn write_rights_field(&self, offset: usize, bytes: &[u8]) -> Result<(), SystemError> {
        let addr = (self.ptr as usize)
            .checked_add(*self.write_off)
            .and_then(|addr| addr.checked_add(offset))
            .ok_or(SystemError::EFAULT)?;
        UserBuffer::new_protected(addr as *mut u8, bytes.len(), true)?
            .write_to_user(0, bytes)
            .map(|_| ())
    }

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
