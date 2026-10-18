//! Exercise actual receive storage admission and release, without socket I/O
//! replay or a test-specific production allocator.
use super::*;
use crate::{
    arch::MMArch,
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, MemoryAccount, MemoryPolicy,
    },
    mm::MemoryManagementArch,
};

fn released(account: &MemoryAccount) -> bool {
    let snapshot = account.snapshot();
    let page_table_bytes = snapshot.kernel_bytes[KernelByteKind::PageTables as usize];
    // Shared page tables can be released concurrently. The diagnostic page
    // and byte counters are not an atomic pair; whole-leaf vmalloc selftests
    // verify page teardown independently.
    page_table_bytes.is_multiple_of(MMArch::PAGE_SIZE as u64)
        && snapshot
            .kernel_bytes
            .iter()
            .enumerate()
            .all(|(kind, bytes)| {
                if kind == KernelByteKind::PageTables as usize {
                    bytes.is_multiple_of(MMArch::PAGE_SIZE as u64)
                } else {
                    *bytes == 0
                }
            })
}

pub(crate) fn run() -> bool {
    let Ok(root) = MemoryAccount::new(u64::MAX - 60, None) else {
        return false;
    };
    let Ok(account) = MemoryAccount::new(u64::MAX - 61, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    let mut buffer = RecvBuffer::default();
    if buffer
        .try_prepare_with(0, |_| Err(SystemError::EIO.into()))
        .is_err()
        || buffer.capacity() != 0
    {
        return false;
    }
    for size in [1, 8192, 8193, 128 * 1024] {
        if buffer
            .try_prepare_with(size, |size| {
                if !released(&account) {
                    return Err(SystemError::EIO.into());
                }
                ReceiveStorage::try_new_zeroed_in(size, owner)
            })
            .is_err()
            || buffer.capacity() != size
        {
            return false;
        }
        if buffer.bytes(size).iter().any(|byte| *byte != 0)
            || released(&account)
            || account.snapshot().current == 0
        {
            return false;
        }
        buffer.bytes_mut(size).fill(0xa5);
        if buffer.bytes(size).iter().any(|byte| *byte != 0xa5) {
            return false;
        }
        // A smaller record reuses storage without allocation. Its entire
        // active prefix must be overwritten before any user copy.
        let active = size / 2;
        if buffer
            .try_prepare_with(active, |_| Err(SystemError::EIO.into()))
            .is_err()
        {
            return false;
        }
        buffer.bytes_mut(active).fill(0x37);
        if buffer.bytes(active).iter().any(|byte| *byte != 0x37) || buffer.capacity() != size {
            return false;
        }
    }
    // Growth releases the old charge even when the new allocation fails.
    let failure = buffer.try_prepare_with(256 * 1024, |_| {
        if !released(&account) {
            return Err(SystemError::EINVAL.into());
        }
        Err(SystemError::ENOMEM.into())
    });
    if !matches!(
        failure,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    ) || buffer.capacity() != 0
        || !released(&account)
    {
        return false;
    }
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    for size in [1, 8193] {
        if !matches!(
            buffer.try_prepare_with(size, |size| ReceiveStorage::try_new_zeroed_in(size, owner)),
            Err(MemoryAllocationError::Charge(_))
        ) || buffer.capacity() != 0
        {
            return false;
        }
    }
    account.set_policy(MemoryPolicy::default());
    let good = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|frame| {
        let mut ok = buffer
            .try_prepare_with(3 * MMArch::PAGE_SIZE, |size| {
                ReceiveStorage::try_new_zeroed_in_with(size, owner, frame)
            })
            .is_ok();
        if ok {
            let size = 3 * MMArch::PAGE_SIZE;
            ok = buffer.bytes(size).iter().all(|byte| *byte == 0) && !released(&account);
            buffer.bytes_mut(size).fill(0x61);
            ok &= buffer.bytes(size).iter().all(|byte| *byte == 0x61);
        }
        drop(buffer);
        ok && released(&account)
    });
    good && released(&account)
}
