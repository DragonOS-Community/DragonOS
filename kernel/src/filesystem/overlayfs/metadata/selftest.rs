//! Local readers exercise the production xattr transaction without a fake FS.
use super::*;
use crate::{
    arch::MMArch,
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, MemoryAccount, MemoryPolicy,
    },
    libs::mutex::Mutex,
    mm::{allocator::kernel_allocator::allocation_footprint, MemoryManagementArch},
};
use alloc::string::String;
use core::{alloc::Layout, fmt::Write};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn owner() -> Result<(Arc<MemoryAccount>, Arc<ObjectAccount>), SystemError> {
    let account = MemoryAccount::new(u64::MAX - 30, None)?;
    let prepared = ObjectAccount::prepare(&account)?;
    Ok((account, prepared.object_account().clone()))
}

// Shared leaf tables can outlive this owner while concurrent mappings use them.
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

fn pattern_read(buf: &mut [u8], size: usize) -> Result<usize, SystemError> {
    if !buf.is_empty() {
        if buf.len() < size {
            return Err(SystemError::ERANGE);
        }
        for (index, byte) in buf[..size].iter_mut().enumerate() {
            *byte = index.wrapping_mul(37) as u8;
        }
    }
    Ok(size)
}

fn pattern_valid(buf: &[u8], size: usize) -> bool {
    buf.len() == size
        && buf
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == index.wrapping_mul(37) as u8)
}

fn read_boundaries_reuse() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let mut value = XattrBuffer::default();
    let mut allocations = 0;
    // Descending values retain capacity, including an empty value between reads.
    for size in [0, 1, 2048, 2049, 4096, 4097, 8192, 8193, 65536, 3, 0, 17] {
        let previous = value.storage.as_ref().map_or(0, |storage| storage.len());
        let before = allocations;
        let mut calls = 0;
        let result = value.read_with(
            |buf| {
                calls += 1;
                pattern_read(buf, size)
            },
            |size| {
                allocations += 1;
                if !released(&account) {
                    return Err(SystemError::EIO.into());
                }
                XattrStorage::try_new_zeroed_in(size, &owner)
            },
        );
        if result.is_err()
            || !pattern_valid(value.as_slice(), size)
            || calls != if size == 0 { 1 } else { 2 }
            || allocations - before != usize::from(size > previous)
        {
            return false;
        }
    }
    drop(value);
    released(&account)
}

fn read_errors_retries() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let allocate = |size| XattrStorage::try_new_zeroed_in(size, &owner);
    let mut value = XattrBuffer::default();
    let mut probes = 0;
    let result = value.read_with(
        |buf| {
            if buf.is_empty() {
                probes += 1;
                return Ok(probes * 4097);
            }
            if probes < XATTR_READ_RETRIES {
                return Err(SystemError::ERANGE);
            }
            pattern_read(buf, 3)
        },
        allocate,
    );
    if result.is_err() || probes != XATTR_READ_RETRIES || !pattern_valid(value.as_slice(), 3) {
        return false;
    }
    // Reused storage must still limit the read to the new probe, not capacity.
    let result = value.read_with(
        |buf| {
            if buf.is_empty() {
                Ok(1)
            } else if buf.len() == 1 {
                Ok(2)
            } else {
                Ok(0)
            }
        },
        allocate,
    );
    if !matches!(result, Err(MemoryAllocationError::System(SystemError::EIO)))
        || !value.as_slice().is_empty()
    {
        return false;
    }
    let mut calls = 0;
    let result = value.read_with(
        |buf| {
            calls += 1;
            if buf.is_empty() {
                Ok(1)
            } else {
                Err(SystemError::ERANGE)
            }
        },
        allocate,
    );
    if !matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::ERANGE))
    ) || calls != 2 * XATTR_READ_RETRIES
    {
        return false;
    }
    let mut calls = 0;
    let result = value.read_with(
        |buf| {
            calls += 1;
            if !buf.is_empty() {
                Err(SystemError::ERANGE)
            } else {
                Ok(usize::from(calls == 1))
            }
        },
        allocate,
    );
    if result.is_err() || calls != 3 || !value.as_slice().is_empty() {
        return false;
    }
    for error in [
        SystemError::ENODATA,
        SystemError::EOPNOTSUPP_OR_ENOTSUP,
        SystemError::EPERM,
    ] {
        let result = value.read_with(|_| Err(error.clone()), allocate);
        if !matches!(result, Err(MemoryAllocationError::System(actual)) if actual == error)
            || !value.as_slice().is_empty()
        {
            return false;
        }
        let result = value.read_with(
            |buf| {
                if buf.is_empty() {
                    Ok(1)
                } else {
                    Err(error.clone())
                }
            },
            allocate,
        );
        if !matches!(result, Err(MemoryAllocationError::System(actual)) if actual == error) {
            return false;
        }
    }
    let result = value.read_with(|_| Ok(XATTR_MAX_SIZE + 1), |_| Err(SystemError::EIO.into()));
    if !matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::E2BIG))
    ) {
        return false;
    }
    // Failure replacing a previously successful value cannot expose old bytes.
    let result = value.read_with(|buf| pattern_read(buf, 17), allocate);
    if result.is_err() {
        return false;
    }
    let result = value.read_with(|_| Ok(XATTR_MAX_SIZE), |_| Err(SystemError::ENOMEM.into()));
    if !matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    ) || !value.as_slice().is_empty()
    {
        return false;
    }
    drop(value);
    released(&account)
}

fn list_filter_validation() -> bool {
    let list =
        b"user.a\0trusted.overlay.\xff\0trusted.dragonos.overlay.origin\0trusted.\xfe\0user.\xff\0user.\xfe\0user.\xef\xbf\xbd\0";
    for (trusted, expected) in [
        (
            false,
            &b"user.a\0user.\xff\0user.\xfe\0user.\xef\xbf\xbd\0"[..],
        ),
        (
            true,
            &b"user.a\0trusted.\xfe\0user.\xff\0user.\xfe\0user.\xef\xbf\xbd\0"[..],
        ),
    ] {
        if copy_visible_xattr_names(list, &mut [], trusted) != Ok(expected.len()) {
            return false;
        }
        let mut output = [0xa5; 128];
        if copy_visible_xattr_names(list, &mut output[..expected.len() - 1], trusted)
            != Err(SystemError::ERANGE)
            || output.iter().any(|byte| *byte != 0xa5)
        {
            return false;
        }
        if copy_visible_xattr_names(list, &mut output, trusted) != Ok(expected.len())
            || &output[..expected.len()] != expected
            || output[expected.len()..].iter().any(|byte| *byte != 0xa5)
        {
            return false;
        }
    }
    for malformed in [&b"user.ok\0unterminated"[..], &b"user.ok\0\0"[..]] {
        let mut output = [0xa5; 64];
        if copy_visible_xattr_names(malformed, &mut output, true) != Err(SystemError::EIO)
            || output.iter().any(|byte| *byte != 0xa5)
            || copy_visible_xattr_names(malformed, &mut [], false) != Err(SystemError::EIO)
        {
            return false;
        }
    }
    let mut untouched = [0xa5; 8];
    copy_visible_xattr_names(b"trusted.overlay.hide\0", &mut untouched, true) == Ok(0)
        && copy_visible_xattr_names(&[], &mut untouched, false) == Ok(0)
        && untouched == [0xa5; 8]
}

fn charge_release() -> bool {
    for size in [1, 2048, 2049, 8193, 65536] {
        let Ok((account, owner)) = owner() else {
            return false;
        };
        let mut value = XattrBuffer::default();
        if value
            .read_with(
                |buf| pattern_read(buf, size),
                |size| XattrStorage::try_new_zeroed_in(size, &owner),
            )
            .is_err()
        {
            return false;
        }
        let snapshot = account.snapshot();
        let charged = if size <= 8192 {
            let Ok(layout) = Layout::array::<u8>(size) else {
                return false;
            };
            let kind = if size <= 2048 {
                KernelByteKind::SlabUnreclaimable
            } else {
                KernelByteKind::KernelOther
            };
            snapshot.kernel_bytes[kind as usize] == allocation_footprint(layout)
        } else {
            snapshot.kernel_bytes[KernelByteKind::Vmalloc as usize]
                == (size.div_ceil(MMArch::PAGE_SIZE) * MMArch::PAGE_SIZE) as u64
        };
        drop(value);
        if !charged || !released(&account) {
            return false;
        }
    }
    let Ok(root) = MemoryAccount::new(u64::MAX - 31, None) else {
        return false;
    };
    let Ok(account) = MemoryAccount::new(u64::MAX - 32, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    for size in [1, 8193] {
        let mut value = XattrBuffer::default();
        let mut reads = 0;
        let result = value.read_with(
            |buf| {
                if !buf.is_empty() {
                    reads += 1;
                }
                Ok(size)
            },
            |size| XattrStorage::try_new_zeroed_in(size, prepared.object_account()),
        );
        if !matches!(&result, Err(MemoryAllocationError::Charge(_)))
            || reads != 0
            || !value.as_slice().is_empty()
        {
            return false;
        }
        if result.err().map(xattr_read_error) != Some(SystemError::ENOMEM) {
            return false;
        }
    }
    let snapshot = account.snapshot();
    snapshot.current == 0 && snapshot.kernel_bytes.iter().all(|bytes| *bytes == 0)
}

fn fragmented_xattr() -> bool {
    let Ok((account, owner)) = owner() else {
        return false;
    };
    let passed = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|frame| {
        let mut value = XattrBuffer::default();
        let size = 3 * MMArch::PAGE_SIZE;
        let result = value.read_with(
            |buf| pattern_read(buf, size),
            |size| XattrStorage::try_new_zeroed_in_with(size, &owner, frame),
        );
        if result.is_err() || !pattern_valid(value.as_slice(), size) {
            return false;
        }
        drop(value);
        // Many short names put both parsing and compacted output across pages.
        const NAMES: &[u8] = b"user.visible\0trusted.overlay.hide\0trusted.secret\0";
        const REPEATS: usize = 256;
        let mut list = XattrBuffer::default();
        let result = list.read_with(
            |buf| {
                let size = NAMES.len() * REPEATS;
                if !buf.is_empty() {
                    for chunk in buf.chunks_exact_mut(NAMES.len()) {
                        chunk.copy_from_slice(NAMES);
                    }
                }
                Ok(size)
            },
            |size| XattrStorage::try_new_zeroed_in_with(size, &owner, frame),
        );
        if result.is_err() {
            return false;
        }
        let mut output = [0xa5; 8192];
        const VISIBLE: &[u8] = b"user.visible\0trusted.secret\0";
        let expected = VISIBLE.len() * REPEATS;
        let good = copy_visible_xattr_names(list.as_slice(), &mut output, true) == Ok(expected)
            && output[..expected]
                .chunks_exact(VISIBLE.len())
                .all(|chunk| chunk == VISIBLE)
            && output[expected..].iter().all(|byte| *byte == 0xa5);
        drop(list);
        good && released(&account)
    });
    passed && released(&account)
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let cases = [
        ("read_boundaries_reuse", read_boundaries_reuse()),
        ("read_errors_retries", read_errors_retries()),
        ("list_filter_validation", list_filter_validation()),
        ("charge_release", charge_release()),
        ("fragmented_xattr", fragmented_xattr()),
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
