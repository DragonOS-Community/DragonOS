//! Exercise the real scratch owner with local accounting and bounded backing.
use super::*;
use crate::{
    arch::MMArch,
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, MemoryAccount, MemoryPolicy,
    },
    libs::mutex::Mutex,
    mm::MemoryManagementArch,
};
use alloc::{string::String, sync::Arc};
use core::fmt::Write;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn owner() -> Result<(Arc<MemoryAccount>, Arc<ObjectAccount>), SystemError> {
    let account = MemoryAccount::new(u64::MAX - 50, None)?;
    let prepared = ObjectAccount::prepare(&account)?;
    Ok((account, prepared.object_account().clone()))
}

// A concurrent mapping may keep shared page tables charged to this owner.
fn released(account: &MemoryAccount) -> bool {
    let bytes = account.snapshot().kernel_bytes;
    bytes[KernelByteKind::PageTables as usize].is_multiple_of(MMArch::PAGE_SIZE as u64)
        && bytes
            .iter()
            .enumerate()
            .all(|(kind, bytes)| kind == KernelByteKind::PageTables as usize || *bytes == 0)
}

fn fill(bytes: &mut [u8]) {
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = index.wrapping_mul(37).wrapping_add(11) as u8;
    }
}

fn valid(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| *byte == index.wrapping_mul(37).wrapping_add(11) as u8)
}

fn lazy_reuse() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let mut scratch = ReadScratch::default();
    if scratch
        .try_prepare_with(0, |_| Err(SystemError::EIO.into()))
        .is_err()
        || scratch.storage.is_some()
        || !released(&account)
    {
        return false;
    }
    // Cover slab, buddy and virtual boundaries, including every byte of 64KiB.
    for size in [1, 2048, 2049, 4096, 4097, 8192, 8193, MAX_CHUNK] {
        let mut allocations = 0;
        if scratch
            .try_prepare_with(size, |size| {
                allocations += 1;
                ReadStorage::try_new_zeroed_in(size, &owner)
            })
            .is_err()
            || allocations != 1
        {
            return false;
        }
        let Some(storage) = scratch.storage.as_mut() else {
            return false;
        };
        if storage.len() != size || storage.iter().any(|byte| *byte != 0) {
            return false;
        }
        fill(storage);
        if !valid(storage) {
            return false;
        }
        storage.fill(0xa5);
        // A shorter reuse must clear only the active prefix, preserving capacity.
        let active = size / 2;
        if scratch
            .try_prepare_with(active, |_| Err(SystemError::EIO.into()))
            .is_err()
        {
            return false;
        }
        let storage = scratch.storage.as_ref().unwrap();
        if storage.len() != size
            || storage[..active].iter().any(|byte| *byte != 0)
            || storage[active..].iter().any(|byte| *byte != 0xa5)
        {
            return false;
        }
        if scratch
            .try_prepare_with(size, |_| Err(SystemError::EIO.into()))
            .is_err()
            || scratch
                .storage
                .as_ref()
                .unwrap()
                .iter()
                .any(|byte| *byte != 0)
        {
            return false;
        }
    }
    drop(scratch);
    released(&account)
}

fn growth_release() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let mut scratch = ReadScratch::default();
    for size in [1, 8193, MAX_CHUNK] {
        let mut allocations = 0;
        if scratch
            .try_prepare_with(size, |size| {
                allocations += 1;
                if !released(&account) {
                    return Err(SystemError::EIO.into());
                }
                ReadStorage::try_new_zeroed_in(size, &owner)
            })
            .is_err()
            || allocations != 1
        {
            return false;
        }
        let Some(storage) = scratch.storage.as_mut() else {
            return false;
        };
        fill(storage);
        if !valid(storage) || released(&account) {
            return false;
        }
    }
    drop(scratch);
    released(&account)
}

fn errors_release() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    for error in [
        SystemError::ENOMEM,
        SystemError::EIO,
        SystemError::EAGAIN_OR_EWOULDBLOCK,
    ] {
        let mut scratch = ReadScratch::default();
        if scratch
            .try_prepare_with(8193, |size| ReadStorage::try_new_zeroed_in(size, &owner))
            .is_err()
        {
            return false;
        }
        let mut allocations = 0;
        let result = scratch.try_prepare_with(MAX_CHUNK, |_| {
            allocations += 1;
            if !released(&account) {
                return Err(SystemError::EINVAL.into());
            }
            Err(error.clone().into())
        });
        if !matches!(result, Err(MemoryAllocationError::System(actual)) if actual == error)
            || allocations != 1
            || scratch.storage.is_some()
            || !released(&account)
        {
            return false;
        }
        // A later valid attempt may acquire new storage after failure.
        if scratch
            .try_prepare_with(1, |size| ReadStorage::try_new_zeroed_in(size, &owner))
            .is_err()
        {
            return false;
        }
        drop(scratch);
        if !released(&account) {
            return false;
        }
    }
    let mut scratch = ReadScratch::default();
    matches!(
        scratch.try_prepare_with(MAX_CHUNK + 1, |_| Err(SystemError::EIO.into())),
        Err(MemoryAllocationError::System(SystemError::EINVAL))
    ) && scratch.storage.is_none()
        && released(&account)
}

fn charge_admission() -> bool {
    let Ok(root) = MemoryAccount::new(u64::MAX - 51, None) else {
        return false;
    };
    let Ok(account) = MemoryAccount::new(u64::MAX - 52, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    for size in [1, 8193, MAX_CHUNK] {
        let mut scratch = ReadScratch::default();
        let mut allocations = 0;
        let result = scratch.try_prepare_with(size, |size| {
            allocations += 1;
            ReadStorage::try_new_zeroed_in(size, prepared.object_account())
        });
        if !matches!(result, Err(MemoryAllocationError::Charge(_)))
            || allocations != 1
            || scratch.storage.is_some()
        {
            return false;
        }
    }
    // Rejection during growth must also release the old owner first.
    account.set_policy(MemoryPolicy::default());
    let mut scratch = ReadScratch::default();
    if scratch
        .try_prepare_with(1, |size| {
            ReadStorage::try_new_zeroed_in(size, prepared.object_account())
        })
        .is_err()
    {
        return false;
    }
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    let result = scratch.try_prepare_with(8193, |size| {
        if !released(&account) {
            return Err(SystemError::EIO.into());
        }
        ReadStorage::try_new_zeroed_in(size, prepared.object_account())
    });
    if !matches!(result, Err(MemoryAllocationError::Charge(_))) || scratch.storage.is_some() {
        return false;
    }
    // This verifies typed admission, deliberately without invoking reclaim/OOM.
    let snapshot = account.snapshot();
    snapshot.current == 0 && snapshot.kernel_bytes.iter().all(|bytes| *bytes == 0)
}

fn fragmented_staging() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let good = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|frame| {
        let mut scratch = ReadScratch::default();
        // The arena donates at most eight pages; growing 3 -> 4 uses seven.
        for pages in [3, 4] {
            let size = pages * MMArch::PAGE_SIZE;
            if scratch
                .try_prepare_with(size, |size| {
                    if !released(&account) {
                        return Err(SystemError::EIO.into());
                    }
                    ReadStorage::try_new_zeroed_in_with(size, &owner, frame)
                })
                .is_err()
            {
                return false;
            }
            let storage = scratch.storage.as_mut().unwrap();
            if storage.len() != size || storage.iter().any(|byte| *byte != 0) {
                return false;
            }
            fill(storage);
            if !valid(storage) {
                return false;
            }
        }
        drop(scratch);
        released(&account)
    });
    good && released(&account)
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let cases = [
        ("lazy_reuse", lazy_reuse()),
        ("growth_release", growth_release()),
        ("errors_release", errors_release()),
        ("charge_admission", charge_admission()),
        ("fragmented_staging", fragmented_staging()),
    ];
    let failures = cases.iter().filter(|(_, ok)| !ok).count();
    let mut report = String::new();
    let _ = writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    );
    for (name, ok) in cases {
        let _ = writeln!(report, "{name}={}", if ok { "ok" } else { "fail" });
    }
    let _ = writeln!(report, "summary_fail={failures}");
    report
}
