use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SENDMMSG;
use crate::net::posix::MsgHdr;
use crate::net::socket;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_recvmmsg::MMsgHdr;

/// System call handler for the `sendmmsg` syscall.
///
/// Sends multiple messages on a socket in a single system call,
/// reducing user/kernel context switch overhead.
pub struct SysSendmmsgHandle;

/// Upper limit on the number of messages per call (same as Linux `UIO_MAXIOV`).
const UIO_MAXIOV: usize = 1024;

impl Syscall for SysSendmmsgHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0];
        let msgvec = args[1] as *mut MMsgHdr;
        let vlen = args[2];
        let flags = args[3] as u32;

        if msgvec.is_null() {
            return Err(SystemError::EFAULT);
        }

        // Linux truncates vlen to UIO_MAXIOV rather than returning an error.
        let vlen = vlen.min(UIO_MAXIOV);
        if vlen == 0 {
            return Ok(0);
        }

        let total_len = vlen
            .checked_mul(core::mem::size_of::<MMsgHdr>())
            .ok_or(SystemError::EINVAL)?;

        // Validate that the entire msgvec is readable and writable.
        let _ = UserBufferReader::new(msgvec as *const u8, total_len, frame.is_from_user())?;
        let _ = UserBufferWriter::new(msgvec as *mut u8, total_len, frame.is_from_user())?;

        let mut sent: usize = 0;

        for i in 0..vlen {
            let base = unsafe { (msgvec as *mut u8).add(i * core::mem::size_of::<MMsgHdr>()) };
            let msg_hdr_ptr = base as *const MsgHdr;

            // Read the MsgHdr from user space using exception-table-protected copy.
            let reader = UserBufferReader::new(
                msg_hdr_ptr,
                core::mem::size_of::<MsgHdr>(),
                frame.is_from_user(),
            )?;
            let msg_hdr = reader.buffer_protected(0)?.read_one::<MsgHdr>(0)?;

            // Apply MSG_BATCH to all messages except the last one, hinting the
            // protocol stack that more data follows and it may defer transmission.
            let this_flags = if i < vlen - 1 {
                flags | socket::PMSG::BATCH.bits()
            } else {
                flags
            };

            match super::sys_sendmsg::do_sendmsg(fd, &msg_hdr, this_flags) {
                Ok(n) => {
                    // Write the number of bytes sent into msgvec[i].msg_len.
                    let msg_len_off = core::mem::offset_of!(MMsgHdr, msg_len);
                    let mut writer = match UserBufferWriter::new(
                        unsafe { base.add(msg_len_off) },
                        core::mem::size_of::<u32>(),
                        frame.is_from_user(),
                    ) {
                        Ok(writer) => writer,
                        Err(e) => {
                            if sent > 0 {
                                break;
                            }
                            return Err(e);
                        }
                    };
                    let mut protected = match writer.buffer_protected(0) {
                        Ok(protected) => protected,
                        Err(e) => {
                            if sent > 0 {
                                break;
                            }
                            return Err(e);
                        }
                    };
                    if let Err(e) = protected.write_one::<u32>(0, &(n as u32)) {
                        if sent > 0 {
                            break;
                        }
                        return Err(e);
                    }
                    sent += 1;
                }
                Err(e) => {
                    if sent > 0 {
                        break;
                    }
                    return Err(e);
                }
            }
        }

        Ok(sent)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args.first().copied().unwrap_or(0)).to_string()),
            FormattedSyscallParam::new(
                "msgvec",
                format!("{:#x}", args.get(1).copied().unwrap_or(0)),
            ),
            FormattedSyscallParam::new("vlen", (args.get(2).copied().unwrap_or(0)).to_string()),
            FormattedSyscallParam::new(
                "flags",
                format!("{:#x}", args.get(3).copied().unwrap_or(0) as u32),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SENDMMSG, SysSendmmsgHandle);
