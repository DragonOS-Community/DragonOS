use alloc::vec::Vec;
use system_error::SystemError;

#[cfg(target_arch = "x86_64")]
use crate::arch::syscall::nr::SYS_SELECT;
use crate::{
    filesystem::{
        epoll::EPollEventType,
        poll::{do_sys_poll, poll_select_finish, poll_select_set_timeout, PollFd, PollTimeType},
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::{syscall::PosixTimeval, Instant},
};
// Maximum number of file descriptors in a set
const FD_SETSIZE: usize = 1024;
const USIZE_BITS: usize = core::mem::size_of::<usize>() * 8;
/// See https://man7.org/linux/man-pages/man2/select.2.html
pub struct SysSelect;

impl Syscall for SysSelect {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        common_sys_select(args[0], args[1], args[2], args[3], args[4])
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("nfds", format!("{}", args[0])),
            FormattedSyscallParam::new("readfds", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("writefds", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("exceptfds", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("timeout", format!("{:#x}", args[4])),
        ]
    }
}

pub fn common_sys_select(
    nfds: usize,
    readfds_addr: usize,
    writefds_addr: usize,
    exceptfds_addr: usize,
    timeout_ptr: usize,
) -> Result<usize, SystemError> {
    // log::debug!(
    //     "common_sys_select called with nfds = {}, readfds_addr = {:#x}, writefds_addr = {:#x}, exceptfds_addr = {:#x}, timeout_ptr = {:#x}",
    //     nfds, readfds_addr, writefds_addr, exceptfds_addr, timeout_ptr
    // );
    let mut end_time: Option<Instant> = None;
    if timeout_ptr != 0 {
        let tsreader = UserBufferReader::new(
            timeout_ptr as *const PosixTimeval,
            size_of::<PosixTimeval>(),
            true,
        )?;
        let ts = *tsreader.read_one_from_user::<PosixTimeval>(0)?;
        // 检查是否为负值
        if ts.tv_sec < 0 || ts.tv_usec < 0 {
            return Err(SystemError::EINVAL);
        }
        let timeout_ms = ts.tv_sec * 1000 + ts.tv_usec as i64 / 1000;
        if timeout_ms >= 0 {
            end_time = poll_select_set_timeout(timeout_ms as u64);
        }
    }
    let result = do_sys_select(
        nfds as isize,
        readfds_addr as *const FdSet,
        writefds_addr as *const FdSet,
        exceptfds_addr as *const FdSet,
        end_time,
    );
    // 更新用户空间的timeout为剩余时间
    poll_select_finish(end_time, timeout_ptr, PollTimeType::TimeVal, result)
}

pub(super) fn do_sys_select(
    nfds: isize,
    readfds_addr: *const FdSet,
    writefds_addr: *const FdSet,
    exceptfds_addr: *const FdSet,
    timeout: Option<Instant>,
) -> Result<usize, SystemError> {
    if nfds < 0 || nfds as usize > FD_SETSIZE {
        return Err(SystemError::EINVAL);
    }
    let get_fdset = |fdset_addr: *const FdSet| -> Result<Option<FdSet>, SystemError> {
        let fdset = if fdset_addr.is_null() {
            None
        } else {
            let fdset_buf = UserBufferReader::new(fdset_addr, size_of::<FdSet>(), true)?;
            let fdset = *fdset_buf.read_one_from_user::<FdSet>(0)?;
            Some(fdset)
        };
        Ok(fdset)
    };
    let mut readfds = get_fdset(readfds_addr)?;
    let mut writefds = get_fdset(writefds_addr)?;
    let mut exceptfds = get_fdset(exceptfds_addr)?;

    // log::debug!(
    //     "nfds = {}, readfds = {:?}, writefds = {:?}, exceptfds = {:?}, timeout = {:?}",
    //     nfds,
    //     readfds,
    //     writefds,
    //     exceptfds,
    //     timeout
    // );

    let num_revents = do_select(
        nfds as usize,
        readfds.as_mut(),
        writefds.as_mut(),
        exceptfds.as_mut(),
        timeout,
    )?;

    let set_fdset = |fdset_addr: *const FdSet, fdset: Option<FdSet>| -> Result<(), SystemError> {
        if let Some(fdset) = fdset {
            let mut fdset_buf =
                UserBufferWriter::new(fdset_addr as *mut FdSet, size_of::<FdSet>(), true)?;
            fdset_buf.copy_one_to_user(&fdset, 0)?;
        }
        Ok(())
    };

    set_fdset(readfds_addr, readfds)?;
    set_fdset(writefds_addr, writefds)?;
    set_fdset(exceptfds_addr, exceptfds)?;

    // log::info!("num_revents = {}", num_revents);
    Ok(num_revents)
}

fn do_select(
    nfds: usize,
    mut readfds: Option<&mut FdSet>,
    mut writefds: Option<&mut FdSet>,
    mut exceptfds: Option<&mut FdSet>,
    timeout: Option<Instant>,
) -> Result<usize, SystemError> {
    let mut poll_fds = {
        let mut poll_fds = Vec::with_capacity(nfds);
        for fd in 0..nfds {
            let events = {
                let readable = readfds.as_ref().is_some_and(|fds| fds.is_set(fd));
                let writable = writefds.as_ref().is_some_and(|fds| fds.is_set(fd));
                let except = exceptfds.as_ref().is_some_and(|fds| fds.is_set(fd));
                convert_rwe_to_events(readable, writable, except)
            };

            if events.is_empty() {
                continue;
            }

            let poll_fd = PollFd {
                fd: fd as i32,
                events: events.bits() as _,
                revents: 0,
            };
            poll_fds.push(poll_fd);
        }
        poll_fds
    };
    if let Some(fds) = readfds.as_mut() {
        fds.clear();
    }
    if let Some(fds) = writefds.as_mut() {
        fds.clear();
    }
    if let Some(fds) = exceptfds.as_mut() {
        fds.clear();
    }

    // call the underlying poll syscall
    let num_revents = do_sys_poll(&mut poll_fds, timeout)?;
    if num_revents == 0 {
        return Ok(0);
    }

    let mut total_revents = 0;
    for poll_fd in &poll_fds {
        let fd = poll_fd.fd as usize;
        let revents = poll_fd.revents;
        let revents = EPollEventType::from_bits_truncate(revents as u32);
        let (readable, writable, except) = convert_events_to_rwe(revents)?;
        if readable {
            if let Some(ref mut fds) = readfds {
                fds.set(fd)?;
                total_revents += 1;
            }
        }

        if writable {
            if let Some(ref mut fds) = writefds {
                fds.set(fd)?;
                total_revents += 1;
            }
        }
        if except {
            if let Some(ref mut fds) = exceptfds {
                fds.set(fd)?;
                total_revents += 1;
            }
        }
    }
    Ok(total_revents)
}

/// Converts `select` RWE input to `poll` I/O event input
/// according to Linux's behavior.
fn convert_rwe_to_events(readable: bool, writable: bool, except: bool) -> EPollEventType {
    let mut events = EPollEventType::empty();
    if readable {
        events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLHUP | EPollEventType::EPOLLERR;
    }
    if writable {
        events |= EPollEventType::EPOLLOUT | EPollEventType::EPOLLERR;
    }
    if except {
        events |= EPollEventType::EPOLLPRI;
    }
    events
}

/// Converts `poll` I/O event results to `select` RWE results
/// according to Linux's behavior.
fn convert_events_to_rwe(events: EPollEventType) -> Result<(bool, bool, bool), SystemError> {
    if events.contains(EPollEventType::EPOLLNVAL) {
        return Err(SystemError::EBADF);
    }

    let readable = events
        .intersects(EPollEventType::EPOLLIN | EPollEventType::EPOLLHUP | EPollEventType::EPOLLERR);
    let writable = events.intersects(EPollEventType::EPOLLOUT | EPollEventType::EPOLLERR);
    let except = events.contains(EPollEventType::EPOLLPRI);
    Ok((readable, writable, except))
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(super) struct FdSet {
    fds_bits: [usize; FD_SETSIZE / USIZE_BITS],
}

impl FdSet {
    /// Equivalent to FD_SET.
    pub fn set(&mut self, fd: usize) -> Result<(), SystemError> {
        if fd >= FD_SETSIZE {
            return Err(SystemError::EINVAL);
        }
        self.fds_bits[fd / USIZE_BITS] |= 1 << (fd % USIZE_BITS);
        Ok(())
    }

    /// Equivalent to FD_CLR.
    #[expect(unused)]
    pub fn unset(&mut self, fd: usize) -> Result<(), SystemError> {
        if fd >= FD_SETSIZE {
            return Err(SystemError::EINVAL);
        }
        self.fds_bits[fd / USIZE_BITS] &= !(1 << (fd % USIZE_BITS));
        Ok(())
    }

    /// Equivalent to FD_ISSET.
    pub fn is_set(&self, fd: usize) -> bool {
        if fd >= FD_SETSIZE {
            return false;
        }
        (self.fds_bits[fd / USIZE_BITS] & (1 << (fd % USIZE_BITS))) != 0
    }

    /// Equivalent to FD_ZERO.
    pub fn clear(&mut self) {
        for slot in self.fds_bits.iter_mut() {
            *slot = 0;
        }
    }
}
#[cfg(target_arch = "x86_64")]
syscall_table_macros::declare_syscall!(SYS_SELECT, SysSelect);
