use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RECVMMSG;
use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::file::FileFlags;
use crate::libs::wait_queue::{TimeoutWaker, Waiter};
use crate::net::posix::MsgHdr;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::time::timer::{next_n_us_timer_jiffies, Timer};
use crate::time::{Duration, Instant, PosixTimeSpec};
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Linux `struct mmsghdr`.
///
/// ```c
/// struct mmsghdr {
///   struct msghdr msg_hdr;
///   unsigned int  msg_len;
/// };
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MMsgHdr {
    pub msg_hdr: MsgHdr,
    pub msg_len: u32,
    /// Padding to keep the same layout as Linux `struct mmsghdr` on 64-bit.
    #[cfg(target_pointer_width = "64")]
    pub _pad0: u32,
}

/// System call handler for the `recvmmsg` syscall
pub struct SysRecvmmsgHandle;

impl Syscall for SysRecvmmsgHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0];
        let msgvec = args[1] as *mut MMsgHdr;
        let vlen = args[2] as usize;
        let flags = args[3] as u32;
        let timeout = args[4] as *mut PosixTimeSpec;

        if msgvec.is_null() {
            return Err(SystemError::EFAULT);
        }
        if vlen == 0 {
            return Err(SystemError::EINVAL);
        }
        // Linux caps vlen to UIO_MAXIOV. Keep it conservative.
        if vlen > 1024 {
            return Err(SystemError::EINVAL);
        }

        let (timeout_dur, start_time) = if timeout.is_null() {
            (None, None)
        } else {
            let reader = UserBufferReader::new(
                timeout,
                core::mem::size_of::<PosixTimeSpec>(),
                frame.is_from_user(),
            )?;
            let ts = reader.buffer_protected(0)?.read_one::<PosixTimeSpec>(0)?;
            if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= crate::time::NSEC_PER_SEC as i64 {
                return Err(SystemError::EINVAL);
            }
            let us = (ts.tv_sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(((ts.tv_nsec as u64) + 999) / 1000);
            (Some(Duration::from_micros(us)), Some(Instant::now()))
        };

        let file_nonblock = {
            let binding = ProcessManager::current_pcb().fd_table();
            let guard = binding.read();
            let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
            file.flags().contains(FileFlags::O_NONBLOCK)
        };

        let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
        let sock = socket_inode.as_socket().unwrap();

        // Wait-for-one semantics: after receiving the first message, don't block for subsequent.
        let wait_for_one =
            !timeout.is_null() || (flags & (socket::PMSG::WAITFORONE.bits() as u32)) != 0;

        let total_len = vlen
            .checked_mul(core::mem::size_of::<MMsgHdr>())
            .ok_or(SystemError::EINVAL)?;
        let mut msg_writer = UserBufferWriter::new(msgvec, total_len, frame.is_from_user())?;
        let msgs = msg_writer.buffer::<MMsgHdr>(0)?;

        let mut received: usize = 0;

        for i in 0..vlen {
            // For i>0, force nonblocking if we're in WAITFORONE/timeout mode.
            let mut this_flags = flags;
            if received > 0 && wait_for_one {
                this_flags |= socket::PMSG::DONTWAIT.bits() as u32;
            }

            // First message: if blocking and no data, optionally wait up to timeout.
            if received == 0
                && !file_nonblock
                && (this_flags & (socket::PMSG::DONTWAIT.bits() as u32)) == 0
                && !timeout.is_null()
                && !sock.check_io_event().contains(EPollEventType::EPOLLIN)
            {
                // Wait until readable or timeout.
                wait_readable_with_timeout(sock, timeout_dur)?;
            }

            match crate::net::syscall::sys_recvmsg::do_recvmsg(fd, &mut msgs[i].msg_hdr, this_flags)
            {
                Ok(n) => {
                    msgs[i].msg_len = n as u32;
                    received += 1;
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    if received > 0 {
                        break;
                    }

                    // No message received.
                    if timeout.is_null() {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                Err(e) => {
                    if received > 0 {
                        break;
                    }
                    return Err(e);
                }
            }
        }

        // Update timeout with remaining time (best-effort, microsecond precision).
        if let (Some(dur), Some(start)) = (timeout_dur, start_time) {
            let elapsed = Instant::now() - start;
            let remain_us = dur.total_micros().saturating_sub(elapsed.total_micros());
            let sec = (remain_us / 1_000_000) as i64;
            let nsec = ((remain_us % 1_000_000) * 1000) as i64;
            let new_ts = PosixTimeSpec {
                tv_sec: sec,
                tv_nsec: nsec,
            };

            let mut writer = UserBufferWriter::new(
                timeout,
                core::mem::size_of::<PosixTimeSpec>(),
                frame.is_from_user(),
            )?;
            writer
                .buffer_protected(0)?
                .write_one::<PosixTimeSpec>(0, &new_ts)?;
        }

        Ok(received)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args.get(0).copied().unwrap_or(0)).to_string()),
            FormattedSyscallParam::new(
                "msgvec",
                format!("{:#x}", args.get(1).copied().unwrap_or(0)),
            ),
            FormattedSyscallParam::new("vlen", (args.get(2).copied().unwrap_or(0)).to_string()),
            FormattedSyscallParam::new(
                "flags",
                format!("{:#x}", args.get(3).copied().unwrap_or(0) as u32),
            ),
            FormattedSyscallParam::new(
                "timeout",
                format!("{:#x}", args.get(4).copied().unwrap_or(0)),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_RECVMMSG, SysRecvmmsgHandle);

fn wait_readable_with_timeout(
    sock: &dyn crate::net::socket::Socket,
    timeout: Option<Duration>,
) -> Result<(), SystemError> {
    let deadline = timeout.map(|t| Instant::now() + t);

    loop {
        if sock.check_io_event().contains(EPollEventType::EPOLLIN) {
            return Ok(());
        }

        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        let remain = deadline.map(|d| d.duration_since(Instant::now()).unwrap_or(Duration::ZERO));

        let (waiter, waker) = Waiter::new_pair();
        sock.wait_queue().register_waker(waker.clone())?;

        if sock.check_io_event().contains(EPollEventType::EPOLLIN) {
            sock.wait_queue().remove_waker(&waker);
            return Ok(());
        }

        if crate::arch::ipc::signal::Signal::signal_pending_state(
            true,
            false,
            &ProcessManager::current_pcb(),
        ) {
            sock.wait_queue().remove_waker(&waker);
            return Err(SystemError::ERESTARTSYS);
        }

        let timer = if let Some(remain) = remain {
            if remain == Duration::ZERO {
                sock.wait_queue().remove_waker(&waker);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let sleep_us = remain.total_micros();
            let t: Arc<Timer> = Timer::new(
                TimeoutWaker::new(waker.clone()),
                next_n_us_timer_jiffies(sleep_us),
            );
            t.activate();
            Some(t)
        } else {
            None
        };

        let wait_res = waiter.wait(true);
        let was_timeout = timer.as_ref().map(|t| t.timeout()).unwrap_or(false);
        if !was_timeout {
            if let Some(t) = timer {
                t.cancel();
            }
        }

        sock.wait_queue().remove_waker(&waker);

        if let Err(SystemError::ERESTARTSYS) = wait_res {
            return Err(SystemError::ERESTARTSYS);
        }
        wait_res?;
    }
}
