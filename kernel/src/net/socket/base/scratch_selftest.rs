//! Real datagram consumer with locally controlled CPU-buffer backing refusal.
use super::*;
use crate::{
    cgroup::memory::{object::ObjectAccount, MemoryAccount},
    mm::{allocation::MemoryAllocationError, kernel_buffer::KernelBuffer},
    net::socket::unix::datagram::UnixDatagramSocket,
    syscall::user_buffer::UserBuffer,
};

pub(crate) fn run() -> bool {
    let result = (|| -> Result<bool, SystemError> {
        let account = MemoryAccount::new(u64::MAX - 207, None)?;
        let prepared = ObjectAccount::prepare(&account)?;
        let owner = prepared.object_account();
        let (sender, receiver) = UnixDatagramSocket::new_pair(true)?;
        let payload = b"scratch must not consume this datagram";
        sender.send(payload, PMSG::empty())?;
        // No copy may occur: the real backing allocation below fails first.
        let mut output = UserBuffer::new_protected(0x1000 as *mut u8, 16384, true)?;
        let mut backing_attempts = 0;
        let result = read_to_user_buffer_with(
            receiver.as_ref(),
            &mut output,
            16384,
            |size| -> Result<KernelBuffer, SystemError> {
                // Check one refused birth and consumer state here; bounded
                // global recovery policy has separate regression coverage.
                match KernelBuffer::try_new_zeroed_in_with(size, owner, &mut || {
                    backing_attempts += 1;
                    Err(SystemError::ENOMEM)
                }) {
                    Err(error @ MemoryAllocationError::Physical) => Err(error.into_errno()),
                    _ => Err(SystemError::EINVAL),
                }
            },
        );
        if !matches!(result, Err(SystemError::ENOMEM)) || backing_attempts != 1 {
            return Ok(false);
        }
        let mut bytes = [0; 128];
        let count = receiver.recv(&mut bytes, PMSG::empty())?;
        if &bytes[..count] != payload || receiver.ioctl_fionread() != 0 {
            return Ok(false);
        }
        let mut empty = UserBuffer::new_protected(core::ptr::null_mut::<u8>(), 0, true)?;
        let result = read_to_user_buffer_with::<_, KernelBuffer>(
            receiver.as_ref(),
            &mut empty,
            16384,
            |_| {
                backing_attempts += 1;
                Err(SystemError::ENOMEM)
            },
        );
        Ok(matches!(result, Ok(0))
            && backing_attempts == 1
            && account
                .snapshot()
                .kernel_bytes
                .iter()
                .all(|bytes| *bytes == 0))
    })();
    matches!(result, Ok(true))
}
