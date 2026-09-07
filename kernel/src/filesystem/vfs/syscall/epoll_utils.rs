use crate::arch::ipc::signal::SigSet;
use crate::filesystem::epoll::event_poll::EventPoll;
use crate::filesystem::epoll::EPollEvent;
use crate::ipc::signal::{restore_saved_sigmask_unless, set_user_sigmask};
use crate::mm::{access_ok, VirtAddr};
use crate::syscall::user_access::{read_one_from_user_protected, write_one_to_user_protected};
use crate::time::PosixTimeSpec;
use system_error::SystemError;

/// Convert the legacy millisecond ABI at the syscall boundary.
pub(super) fn epoll_msec_deadline(timeout: i32) -> Option<PosixTimeSpec> {
    (timeout >= 0).then(|| {
        EventPoll::timeout_to_deadline(PosixTimeSpec::new(
            (timeout / 1000) as i64,
            (timeout % 1000) as i64 * 1_000_000,
        ))
    })
}

/// `deadline` is absolute CLOCK_MONOTONIC; None waits forever, zero polls.
pub(super) fn do_epoll_wait(
    epfd: i32,
    events: VirtAddr,
    max_events: i32,
    deadline: Option<PosixTimeSpec>,
) -> Result<usize, SystemError> {
    if max_events <= 0 || max_events as u32 > EventPoll::EP_MAX_EVENTS {
        return Err(SystemError::EINVAL);
    }
    // Like Linux access_ok(), only validate the address range here. Actual
    // writes can fault, including after another thread unmaps the destination.
    access_ok(
        events,
        max_events as usize * core::mem::size_of::<EPollEvent>(),
    )
    .map_err(|_| SystemError::EFAULT)?;
    EventPoll::epoll_wait(epfd, max_events, deadline, &mut |index, event| unsafe {
        let address = events.data() + index * core::mem::size_of::<EPollEvent>();
        // Linux writes the fields separately. Never expose struct padding on
        // architectures where epoll_event has natural (rather than packed) layout.
        write_one_to_user_protected(VirtAddr::new(address), &event.events())?;
        write_one_to_user_protected(
            VirtAddr::new(address + EPollEvent::DATA_OFFSET),
            &event.data(),
        )
    })
}

pub(super) fn do_epoll_pwait(
    epfd: i32,
    events: VirtAddr,
    max_events: i32,
    deadline: Option<PosixTimeSpec>,
    sigmask: VirtAddr,
    sigsetsize: usize,
) -> Result<usize, SystemError> {
    if !sigmask.is_null() {
        if sigsetsize != core::mem::size_of::<SigSet>() {
            return Err(SystemError::EINVAL);
        }
        let mut mask = SigSet::empty();
        unsafe { read_one_from_user_protected(sigmask, &mut mask)? };
        set_user_sigmask(&mut mask);
    }
    let result = do_epoll_wait(epfd, events, max_events, deadline);
    // EINTR must retain the temporary mask until signal delivery. The signal
    // frame saves the original mask, which sigreturn subsequently restores.
    restore_saved_sigmask_unless(result == Err(SystemError::EINTR));
    result
}
