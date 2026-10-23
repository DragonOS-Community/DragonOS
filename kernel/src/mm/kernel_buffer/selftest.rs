//! Controlled tests for the explicit CPU buffer; absent from default kernels.
use super::*;
use crate::{
    cgroup::memory::{MemoryAccount, MemoryPolicy},
    libs::mutex::Mutex,
    mm::allocator::kernel_allocator::allocation_footprint,
    process::preempt::PreemptGuard,
};
use alloc::string::String;
use core::fmt::Write;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn empty(account: &MemoryAccount) -> bool {
    let snapshot = account.snapshot();
    snapshot.current == 0 && snapshot.kernel_bytes.iter().all(|bytes| *bytes == 0)
}

// A leaf table belongs to its creator until its final shared mapping leaves.
// Concurrent production buffers may therefore legitimately outlive this test's
// data owner. Data and metadata must be gone. The diagnostic snapshot is not
// atomic across byte/page counters, so concurrent table release cannot support
// an exact current-vs-table comparison. Whole-leaf vmalloc tests verify teardown.
fn storage_released(account: &MemoryAccount) -> bool {
    let snapshot = account.snapshot();
    let tables = snapshot.kernel_bytes[KernelByteKind::PageTables as usize];
    tables.is_multiple_of(MMArch::PAGE_SIZE as u64)
        && snapshot
            .kernel_bytes
            .iter()
            .enumerate()
            .all(|(kind, bytes)| kind == KernelByteKind::PageTables as usize || *bytes == 0)
}

fn exercise(buffer: &mut KernelBuffer, len: usize) -> bool {
    if buffer.len() != len || buffer.iter().any(|byte| *byte != 0) {
        return false;
    }
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = index.wrapping_mul(37) as u8;
    }
    buffer
        .iter()
        .enumerate()
        .all(|(index, byte)| *byte == index.wrapping_mul(37) as u8)
}

fn boundaries() -> bool {
    for len in [
        0,
        1,
        2048,
        2049,
        MMArch::PAGE_SIZE,
        MMArch::PAGE_SIZE + 1,
        2 * MMArch::PAGE_SIZE,
        2 * MMArch::PAGE_SIZE + 1,
        64 * 1024,
    ] {
        // Each boundary has its own owner: an earlier fallback may leave a
        // legitimately shared table charged until another task releases it.
        let Ok(account) = MemoryAccount::new(u64::MAX - 11, None) else {
            return false;
        };
        let Ok(prepared) = ObjectAccount::prepare(&account) else {
            return false;
        };
        let Ok(mut buffer) = KernelBuffer::allocate_in(
            len,
            prepared.object_account(),
            true,
            false,
            &mut VmAllocation::try_new_zeroed_in,
        ) else {
            return false;
        };
        let backend = match (&buffer.backend, len) {
            (Backend::Empty, 0) => true,
            (Backend::Heap { .. }, len) => len > 0 && len <= 2 * MMArch::PAGE_SIZE,
            (Backend::Virtual(_), len) => len > MMArch::PAGE_SIZE,
            _ => false,
        };
        let snapshot = account.snapshot();
        let charged = if matches!(&buffer.backend, Backend::Empty) {
            empty(&account)
        } else if matches!(&buffer.backend, Backend::Heap { .. }) {
            let layout = Layout::array::<u8>(len).unwrap();
            let expected = allocation_footprint(layout);
            let kind = if len > 2048 {
                KernelByteKind::KernelOther
            } else {
                KernelByteKind::SlabUnreclaimable
            };
            snapshot.kernel_bytes[kind as usize] == expected
                && snapshot.kernel_bytes.iter().sum::<u64>() == expected
        } else {
            snapshot.kernel_bytes[KernelByteKind::Vmalloc as usize]
                == (len.div_ceil(MMArch::PAGE_SIZE) * MMArch::PAGE_SIZE) as u64
                && snapshot.kernel_bytes[KernelByteKind::SlabUnreclaimable as usize] > 0
        };
        let contents = exercise(&mut buffer, len);
        drop(buffer);
        if !backend || !charged || !contents || !storage_released(&account) {
            return false;
        }
    }
    true
}

fn failure_policy() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 12, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    for len in [1, 2048, MMArch::PAGE_SIZE] {
        let mut calls = 0;
        let result = KernelBuffer::allocate_in(len, owner, true, true, &mut |_, _| {
            calls += 1;
            Err(SystemError::ENOMEM.into())
        });
        let rejected = matches!(
            &result,
            Err(MemoryAllocationError::System(SystemError::ENOMEM))
        );
        drop(result);
        if !rejected || calls != 0 || !empty(&account) {
            return false;
        }
    }
    let mut calls = 0;
    let result = KernelBuffer::allocate_in(
        MMArch::PAGE_SIZE + 1,
        owner,
        true,
        true,
        &mut |layout, selected| {
            calls += 1;
            // Failed heap admission must already be fully returned.
            if !Arc::ptr_eq(owner, selected) || !empty(&account) {
                return Err(SystemError::EIO.into());
            }
            VmAllocation::try_new_zeroed_in(layout, selected)
        },
    );
    let fallback = result
        .as_ref()
        .is_ok_and(|buffer| matches!(&buffer.backend, Backend::Virtual(_)));
    drop(result);
    if !fallback || calls != 1 || !storage_released(&account) {
        return false;
    }
    let result =
        KernelBuffer::allocate_in(MMArch::PAGE_SIZE + 1, owner, true, true, &mut |_, _| {
            Err(SystemError::ENOMEM.into())
        });
    let failed = matches!(
        &result,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    drop(result);
    if !failed || !storage_released(&account) {
        return false;
    }
    // Admission rejection uses a fresh owner: earlier virtual buffers may have
    // left a legitimately shared table charged to their original creator.
    let Ok(root) = MemoryAccount::new(u64::MAX - 15, None) else {
        return false;
    };
    // Like Linux, memory.max does not constrain the root account.
    let Ok(account) = MemoryAccount::new(u64::MAX - 16, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    for len in [1, MMArch::PAGE_SIZE + 1] {
        let mut calls = 0;
        let result = KernelBuffer::allocate_in(len, owner, true, true, &mut |_, _| {
            calls += 1;
            Err(SystemError::EIO.into())
        });
        let rejected = matches!(&result, Err(MemoryAllocationError::Charge(_)));
        drop(result);
        if !rejected || calls != 0 || !empty(&account) {
            return false;
        }
    }
    true
}

// A local capability input exercises the production selection policy without
// changing global NX state or weakening VmAllocation's mapping contract.
fn unsupported_virtual() -> bool {
    let Ok(root) = MemoryAccount::new(u64::MAX - 17, None) else {
        return false;
    };
    let Ok(account) = MemoryAccount::new(u64::MAX - 18, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    let mut calls = 0;
    let mut unexpected = |_: Layout, _: &Arc<ObjectAccount>| {
        calls += 1;
        Err(SystemError::EIO.into())
    };
    for len in [1, MMArch::PAGE_SIZE + 1, 2 * MMArch::PAGE_SIZE + 1, 65537] {
        let Ok(mut buffer) = KernelBuffer::allocate_in(len, owner, false, false, &mut unexpected)
        else {
            return false;
        };
        let snapshot = account.snapshot();
        let expected = allocation_footprint(Layout::array::<u8>(len).unwrap());
        let kind = if len > 2048 {
            KernelByteKind::KernelOther
        } else {
            KernelByteKind::SlabUnreclaimable
        };
        let good = matches!(&buffer.backend, Backend::Heap { .. })
            && snapshot.kernel_bytes[kind as usize] == expected
            && snapshot.kernel_bytes.iter().sum::<u64>() == expected
            && exercise(&mut buffer, len);
        drop(buffer);
        if !good || !empty(&account) {
            return false;
        }
        let result = KernelBuffer::allocate_in(len, owner, false, true, &mut unexpected);
        if !matches!(
            result,
            Err(MemoryAllocationError::System(SystemError::ENOMEM))
        ) || !empty(&account)
        {
            return false;
        }
    }
    // Even a valid Layout too large for buddy must fail without leaking charge.
    let result = KernelBuffer::allocate_in(1usize << 32, owner, false, false, &mut unexpected);
    if !matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    ) || !empty(&account)
    {
        return false;
    }
    account.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    let result = KernelBuffer::allocate_in(65537, owner, false, false, &mut unexpected);
    matches!(result, Err(MemoryAllocationError::Charge(_))) && empty(&account) && calls == 0
}

fn invalid_context_and_size() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 13, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    let mut calls = 0;
    let mut unexpected = |_: Layout, _: &Arc<ObjectAccount>| {
        calls += 1;
        Err(SystemError::EIO.into())
    };
    let result = {
        let _guard = PreemptGuard::new();
        KernelBuffer::allocate_in(0, owner, true, false, &mut unexpected)
    };
    let rejected = matches!(
        &result,
        Err(MemoryAllocationError::System(
            SystemError::EAGAIN_OR_EWOULDBLOCK
        ))
    );
    drop(result);
    if !rejected {
        return false;
    }
    let result = KernelBuffer::allocate_in(usize::MAX, owner, true, false, &mut unexpected);
    let invalid = matches!(
        result,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    let too_large = KernelBuffer::allocate_in(
        isize::MAX as usize,
        owner,
        true,
        false,
        &mut VmAllocation::try_new_zeroed_in,
    );
    let pool_rejected = matches!(
        &too_large,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    drop(too_large);
    invalid && pool_rejected && calls == 0 && empty(&account)
}

fn fragmented_arena() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 14, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let passed = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|data_frame| {
        let mut calls = 0;
        // More than two pages uses the real direct-virtual policy. The private
        // arena has enough single pages but cannot satisfy even order-1.
        let len = 4 * MMArch::PAGE_SIZE;
        let result = KernelBuffer::allocate_in(
            len,
            prepared.object_account(),
            true,
            false,
            &mut |layout, owner| {
                VmAllocation::try_new_zeroed_in_with(layout, owner, &mut || {
                    calls += 1;
                    data_frame()
                })
            },
        );
        let Ok(mut buffer) = result else {
            return false;
        };
        let good = matches!(&buffer.backend, Backend::Virtual(_))
            && calls == 4
            && exercise(&mut buffer, len);
        drop(buffer);
        good && storage_released(&account)
    });
    passed && storage_released(&account)
}

// Exercise the same Layout selection and initialization as the production
// constructor, with an independent account for exact release observations.
fn filled_in<T: Copy>(
    len: usize,
    initial: T,
    owner: &Arc<ObjectAccount>,
) -> Result<KernelSlice<T>, MemoryAllocationError> {
    let layout = Layout::array::<T>(len).map_err(|_| SystemError::ENOMEM)?;
    let storage = KernelBuffer::allocate_layout_in(
        layout,
        owner,
        true,
        false,
        &mut VmAllocation::try_new_zeroed_in,
    )?;
    Ok(KernelSlice::initialize(storage, len, initial))
}

fn typed_values() -> bool {
    use core::num::NonZeroUsize;
    #[derive(Clone, Copy, PartialEq)]
    #[repr(C)]
    struct Padded {
        tag: u8,
        value: u64,
    }
    let Ok(account) = MemoryAccount::new(u64::MAX - 261, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    for len in [0, 1, 256, 513, 1025, 1537] {
        let initial = NonZeroUsize::new(7).unwrap();
        let Ok(mut values) = filled_in(len, initial, owner) else {
            return false;
        };
        if values.len() != len
            || !values
                .as_ptr()
                .addr()
                .is_multiple_of(core::mem::align_of::<NonZeroUsize>())
            || values.iter().any(|value| *value != initial)
        {
            return false;
        }
        for (index, value) in values.iter_mut().enumerate() {
            *value = NonZeroUsize::new(index + 1).unwrap();
        }
        if !values
            .iter()
            .enumerate()
            .all(|(index, value)| value.get() == index + 1)
        {
            return false;
        }
        if len > 1024
            && (account.snapshot().kernel_bytes[KernelByteKind::Vmalloc as usize]
                != (len * core::mem::size_of::<NonZeroUsize>()).div_ceil(MMArch::PAGE_SIZE) as u64
                    * MMArch::PAGE_SIZE as u64)
        {
            return false;
        }
        drop(values);
        if !storage_released(&account) {
            return false;
        }
    }
    // Copy is not a promise that padding bytes are initialized. Typed access
    // must use addresses directly, never KernelBuffer's byte-slice Deref.
    let initial = Padded {
        tag: 3,
        value: 0x1234_5678_9abc_def0,
    };
    let Ok(mut padded) = filled_in(1025, initial, owner) else {
        return false;
    };
    let initialized = padded.iter().all(|value| *value == initial);
    padded[1024].value = 17;
    let changed = padded[1024].value == 17 && padded[0] == initial;
    drop(padded);
    initialized && changed && storage_released(&account)
}

fn typed_alignment_and_limits() -> bool {
    #[derive(Clone, Copy)]
    #[repr(align(8192))]
    struct AlignedZst;
    let Ok(account) = MemoryAccount::new(u64::MAX - 262, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    let Ok(zst) = filled_in(usize::MAX, AlignedZst, owner) else {
        return false;
    };
    let valid_zst = zst.len() == usize::MAX && zst.as_ptr().addr().is_multiple_of(8192);
    drop(zst);
    let Ok(empty_values) = filled_in(0, 1u64, owner) else {
        return false;
    };
    let valid_empty = empty_values.is_empty() && empty_values.as_ptr().addr().is_multiple_of(8);
    drop(empty_values);
    let overflow = matches!(
        KernelSlice::try_filled(usize::MAX, 1u64),
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    let context = {
        let _guard = PreemptGuard::new();
        matches!(
            KernelSlice::try_filled(1, 1u64),
            Err(MemoryAllocationError::System(
                SystemError::EAGAIN_OR_EWOULDBLOCK
            ))
        )
    };
    // This Layout is legal even though size < alignment; buddy's size-based
    // order would not meet it. The virtual reservation must honor alignment.
    let layout = Layout::from_size_align(1, 2 * MMArch::PAGE_SIZE).unwrap();
    let Ok(buffer) = KernelBuffer::allocate_layout_in(
        layout,
        owner,
        true,
        false,
        &mut VmAllocation::try_new_zeroed_in,
    ) else {
        return false;
    };
    let aligned = matches!(&buffer.backend, Backend::Virtual(_))
        && buffer
            .pointer()
            .as_ptr()
            .addr()
            .is_multiple_of(layout.align());
    drop(buffer);
    let unsupported = KernelBuffer::allocate_layout_in(layout, owner, false, false, &mut |_, _| {
        Err(SystemError::EIO.into())
    });
    let rejected = matches!(
        &unsupported,
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    drop(unsupported);
    valid_zst
        && valid_empty
        && overflow
        && context
        && aligned
        && rejected
        && storage_released(&account)
}

fn typed_failure_rollback() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 263, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    // Fail every data-frame boundary in a four-page typed slice. Successfully
    // donated frames must be unmapped/freed before the charge is released.
    for fail_after in 0..4 {
        let passed = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|data_frame| {
            let mut calls = 0;
            let layout = Layout::array::<usize>(1537).unwrap();
            let result = KernelBuffer::allocate_layout_in(
                layout,
                owner,
                true,
                false,
                &mut |layout, owner| {
                    VmAllocation::try_new_zeroed_in_with(layout, owner, &mut || {
                        let index = calls;
                        calls += 1;
                        if index == fail_after {
                            Err(SystemError::ENOMEM)
                        } else {
                            data_frame()
                        }
                    })
                },
            )
            .map(|storage| KernelSlice::initialize(storage, 1537, 9usize));
            let failed = matches!(
                &result,
                Err(MemoryAllocationError::System(SystemError::ENOMEM))
            );
            drop(result);
            failed && calls == fail_after + 1 && storage_released(&account)
        });
        if !passed || !storage_released(&account) {
            return false;
        }
    }
    let Ok(root) = MemoryAccount::new(u64::MAX - 264, None) else {
        return false;
    };
    let Ok(limited) = MemoryAccount::new(u64::MAX - 265, Some(root)) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&limited) else {
        return false;
    };
    limited.set_policy(MemoryPolicy {
        max: Some(0),
        ..MemoryPolicy::default()
    });
    let result = filled_in(1537, 9usize, prepared.object_account());
    let charge = matches!(&result, Err(MemoryAllocationError::Charge(_)));
    drop(result);
    charge && empty(&limited)
}

fn typed_fragmented_arena() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 266, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let passed = crate::mm::allocator::buddy::with_fragmented_buffer_arena(|data_frame| {
        let mut calls = 0;
        let layout = Layout::array::<crate::mm::VirtAddr>(1537).unwrap();
        let result = KernelBuffer::allocate_layout_in(
            layout,
            prepared.object_account(),
            true,
            false,
            &mut |layout, owner| {
                VmAllocation::try_new_zeroed_in_with(layout, owner, &mut || {
                    calls += 1;
                    data_frame()
                })
            },
        );
        let Ok(storage) = result else {
            return false;
        };
        let virtual_backing = matches!(&storage.backend, Backend::Virtual(_));
        let mut pointers = KernelSlice::initialize(storage, 1537, crate::mm::VirtAddr::new(0x1234));
        let initial = pointers.iter().all(|pointer| pointer.data() == 0x1234);
        for (index, pointer) in pointers.iter_mut().enumerate() {
            *pointer = crate::mm::VirtAddr::new(index * 32);
        }
        let contents = pointers
            .iter()
            .enumerate()
            .all(|(index, pointer)| pointer.data() == index * 32);
        drop(pointers);
        virtual_backing && calls == 4 && initial && contents && storage_released(&account)
    });
    passed && storage_released(&account)
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let cases = [
        ("paged_header_storage", crate::libs::paged_vec::selftest()),
        ("strategy_zero_data_accounting", boundaries()),
        ("failure_fallback_admission", failure_policy()),
        ("unsupported_virtual_heap", unsupported_virtual()),
        ("context_size_rejection", invalid_context_and_size()),
        ("fragmented_arena_buffer", fragmented_arena()),
        ("typed_values_accounting", typed_values()),
        ("typed_alignment_limits", typed_alignment_and_limits()),
        ("typed_failure_rollback", typed_failure_rollback()),
        ("typed_fragmented_arena", typed_fragmented_arena()),
        (
            "cpu_buffer_deferred_retirement",
            crate::mm::cpu_buffer::selftest(),
        ),
        (
            "loopback_buffer_admission",
            crate::driver::net::loopback::buffer_selftest(),
        ),
        (
            "veth_buffer_admission",
            crate::driver::net::veth::buffer_selftest(),
        ),
        (
            "local_input_resident_capacity",
            crate::driver::net::local_queue::input_residency_selftest(),
        ),
    ];
    let failures = cases.iter().filter(|(_, ok)| !ok).count();
    let mut report = String::new();
    writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    )
    .unwrap();
    for (name, ok) in cases {
        writeln!(report, "{name}={}", if ok { "ok" } else { "fail" }).unwrap();
    }
    writeln!(report, "summary_fail={failures}").unwrap();
    report
}
