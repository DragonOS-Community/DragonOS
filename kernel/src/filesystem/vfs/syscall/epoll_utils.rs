use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollEvent;
use crate::mm::VirtAddr;
use crate::syscall::user_access::UserBufferWriter;
use crate::time::PosixTimeSpec;
use system_error::SystemError;

/// System call handler for epoll_wait.
///
/// # Arguments
/// * `epfd` - File descriptor of the epoll instance
/// * `events` - User space address to store the events
/// * `max_events` - Maximum number of events to return
/// * `timeout` - Timeout in milliseconds, 0 for no wait, negative for infinite wait
///
/// # Returns
/// Returns the number of events ready or an error if the operation fails.
pub(super) fn do_epoll_wait(
    epfd: i32,
    events: VirtAddr,
    max_events: i32,
    timeout: i32,
) -> Result<usize, SystemError> {
    if max_events <= 0 || max_events as u32 > EventPoll::EP_MAX_EVENTS {
        return Err(SystemError::EINVAL);
    }

    let mut timespec = None;
    if timeout == 0 {
        timespec = Some(PosixTimeSpec::new(0, 0));
    }

    if timeout > 0 {
        let sec: i64 = timeout as i64 / 1000;
        let nsec: i64 = 1000000 * (timeout as i64 % 1000);

        timespec = Some(PosixTimeSpec::new(sec, nsec))
    }

    // 从用户传入的地址中拿到epoll_events
    let mut epds_writer = UserBufferWriter::new(
        events.as_ptr::<EPollEvent>(),
        max_events as usize * core::mem::size_of::<EPollEvent>(),
        true,
    )?;

    let epoll_events = epds_writer.buffer::<EPollEvent>(0)?;
    return EventPoll::epoll_wait(epfd, epoll_events, max_events, timespec);
}
