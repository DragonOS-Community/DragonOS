//! Local callbacks exercise the syscall staging helpers without a fake FS.
use super::*;
use crate::{
    arch::MMArch,
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, MemoryAccount, MemoryPolicy,
    },
    libs::mutex::Mutex,
    mm::MemoryManagementArch,
};
use core::fmt::Write;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn owner() -> Result<(Arc<MemoryAccount>, Arc<ObjectAccount>), SystemError> {
    let account = MemoryAccount::new(u64::MAX - 40, None)?;
    let prepared = ObjectAccount::prepare(&account)?;
    Ok((account, prepared.object_account().clone()))
}

// Concurrent mappings may retain shared page tables charged to this owner.
fn released(account: &MemoryAccount) -> bool {
    let snapshot = account.snapshot();
    snapshot.kernel_bytes[KernelByteKind::PageTables as usize]
        .is_multiple_of(MMArch::PAGE_SIZE as u64)
        && snapshot
            .kernel_bytes
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

fn input_snapshot() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    for size in [0, 1, 2048, 2049, 4096, 4097, 8192, 8193, XATTR_SIZE_MAX] {
        let mut allocations = 0;
        let mut copies = 0;
        let result = copy_xattr_input(
            size,
            |size| {
                allocations += 1;
                XattrBuffer::try_new_zeroed_in(size, &owner)
            },
            |bytes| {
                copies += 1;
                if bytes.len() != size || bytes.iter().any(|byte| *byte != 0) {
                    return Err(SystemError::EIO);
                }
                fill(bytes);
                Ok(())
            },
        );
        let Ok(value) = result else { return false };
        let good = match &value {
            None => size == 0,
            Some(bytes) => {
                let snapshot = account.snapshot();
                bytes.len() == size
                    && valid(bytes)
                    && snapshot
                        .kernel_bytes
                        .iter()
                        .enumerate()
                        .any(|(kind, bytes)| {
                            kind != KernelByteKind::PageTables as usize && *bytes != 0
                        })
            }
        };
        drop(value);
        if !good
            || allocations != usize::from(size != 0)
            || copies != allocations
            || !released(&account)
        {
            return false;
        }
    }
    let result = copy_xattr_input(
        XATTR_SIZE_MAX + 1,
        |_| Err(SystemError::EIO.into()),
        |_| Err(SystemError::EIO),
    );
    matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::E2BIG))
    ) && released(&account)
}

fn output_bounds() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    for size in [0, 1, 4096, 4097, 8192, 8193, XATTR_LIST_MAX, usize::MAX] {
        let capacity = size.min(XATTR_LIST_MAX);
        for actual in [capacity.saturating_sub(1), capacity] {
            let mut allocations = 0;
            let mut reads = 0;
            let mut copies = 0;
            let result = copy_xattr_output(
                size,
                XATTR_LIST_MAX,
                |size| {
                    allocations += 1;
                    XattrBuffer::try_new_zeroed_in(size, &owner)
                },
                |bytes| {
                    reads += 1;
                    if bytes.len() != capacity {
                        return Err(SystemError::EIO);
                    }
                    if size == 0 {
                        return Ok(XATTR_LIST_MAX + 1);
                    }
                    bytes.fill(0xa5);
                    fill(&mut bytes[..actual]);
                    Ok(actual)
                },
                |bytes| {
                    copies += 1;
                    if bytes.len() == actual && valid(bytes) {
                        Ok(())
                    } else {
                        Err(SystemError::EIO)
                    }
                },
            );
            let expected = if size == 0 {
                XATTR_LIST_MAX + 1
            } else {
                actual
            };
            if !matches!(result, Ok(value) if value == expected)
                || allocations != usize::from(size != 0)
                || reads != 1
                || copies != usize::from(actual != 0)
                || !released(&account)
            {
                return false;
            }
        }
    }
    true
}

fn errors_release() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    for size in [1, 8193, XATTR_SIZE_MAX] {
        let result = copy_xattr_input(
            size,
            |size| XattrBuffer::try_new_zeroed_in(size, &owner),
            |_| Err(SystemError::EFAULT),
        );
        if !matches!(
            result,
            Err(MemoryAllocationError::System(SystemError::EFAULT))
        ) || !released(&account)
        {
            return false;
        }
        for (read_error, expected) in [
            (
                SystemError::ERANGE,
                if size == XATTR_SIZE_MAX {
                    SystemError::E2BIG
                } else {
                    SystemError::ERANGE
                },
            ),
            (SystemError::ENODATA, SystemError::ENODATA),
            (SystemError::EPERM, SystemError::EPERM),
        ] {
            let result = copy_xattr_output(
                size,
                XATTR_SIZE_MAX,
                |size| XattrBuffer::try_new_zeroed_in(size, &owner),
                |_| Err(read_error.clone()),
                |_| Err(SystemError::EIO),
            );
            if !matches!(result, Err(MemoryAllocationError::System(error)) if error == expected)
                || !released(&account)
            {
                return false;
            }
        }
        let result = copy_xattr_output(
            size,
            XATTR_SIZE_MAX,
            |size| XattrBuffer::try_new_zeroed_in(size, &owner),
            |_| Ok(size + 1),
            |_| Err(SystemError::EIO),
        );
        let expected = if size == XATTR_SIZE_MAX {
            SystemError::E2BIG
        } else {
            SystemError::ERANGE
        };
        if !matches!(result, Err(MemoryAllocationError::System(error)) if error == expected)
            || !released(&account)
        {
            return false;
        }
        let result = copy_xattr_output(
            size,
            XATTR_SIZE_MAX,
            |size| XattrBuffer::try_new_zeroed_in(size, &owner),
            |bytes| {
                fill(bytes);
                Ok(bytes.len())
            },
            |_| Err(SystemError::EFAULT),
        );
        if !matches!(
            result,
            Err(MemoryAllocationError::System(SystemError::EFAULT))
        ) || !released(&account)
        {
            return false;
        }
    }
    true
}

fn charge_admission() -> bool {
    let Ok(root) = MemoryAccount::new(u64::MAX - 41, None) else {
        return false;
    };
    let Ok(account) = MemoryAccount::new(u64::MAX - 42, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    for size in [1, 8193, XATTR_SIZE_MAX] {
        let result = copy_xattr_input(
            size,
            |size| XattrBuffer::try_new_zeroed_in(size, prepared.object_account()),
            |_| Err(SystemError::EIO),
        );
        if !matches!(result, Err(MemoryAllocationError::Charge(_))) {
            return false;
        }
        let result = copy_xattr_output(
            size,
            XATTR_SIZE_MAX,
            |size| XattrBuffer::try_new_zeroed_in(size, prepared.object_account()),
            |_| Err(SystemError::EIO),
            |_| Err(SystemError::EIO),
        );
        if !matches!(result, Err(MemoryAllocationError::Charge(_))) {
            return false;
        }
    }
    let snapshot = account.snapshot();
    snapshot.current == 0 && snapshot.kernel_bytes.iter().all(|bytes| *bytes == 0)
}

fn fragmented_staging() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let good = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|frame| {
        let size = 3 * MMArch::PAGE_SIZE;
        let result = copy_xattr_input(
            size,
            |size| XattrBuffer::try_new_zeroed_in_with(size, &owner, frame),
            |bytes| {
                fill(bytes);
                Ok(())
            },
        );
        let Ok(Some(value)) = result else {
            return false;
        };
        let good = value.len() == size && valid(&value);
        drop(value);
        if !good || !released(&account) {
            return false;
        }
        // This arena donates at most eight pages, even after a mapping drops.
        let size = 4 * MMArch::PAGE_SIZE;
        let result = copy_xattr_output(
            size,
            XATTR_SIZE_MAX,
            |size| XattrBuffer::try_new_zeroed_in_with(size, &owner, frame),
            |bytes| {
                fill(bytes);
                Ok(bytes.len())
            },
            |bytes| {
                if bytes.len() == size && valid(bytes) {
                    Ok(())
                } else {
                    Err(SystemError::EIO)
                }
            },
        );
        matches!(result, Ok(actual) if actual == size) && released(&account)
    });
    good && released(&account)
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let cases = [
        ("input_snapshot", input_snapshot()),
        ("output_bounds", output_bounds()),
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
