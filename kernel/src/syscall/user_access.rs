//! This file contains functions for kernel-space access to user-space data

use core::{
    cmp::min,
    mem::{size_of, MaybeUninit},
    num::NonZero,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{ffi::CString, sync::Arc, vec::Vec};

use crate::mm::{
    fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
    page::{page_manager_lock, Page},
    ucontext::AddressSpace,
    VmFaultReason,
};
use crate::{
    arch::MMArch,
    mm::{access_ok, MemoryManagementArch, VirtAddr, VirtRegion, VmFlags},
    process::ProcessManager,
};

use super::{user_buffer::UserBuffer, SystemError};

#[inline(always)]
fn checked_user_range(buffer_len: usize, offset: usize, len: usize) -> Result<(), SystemError> {
    if offset
        .checked_add(len)
        .filter(|end| *end <= buffer_len)
        .is_none()
    {
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

#[inline(always)]
unsafe fn value_as_bytes<T>(value: &T) -> &[u8] {
    core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
}

#[inline(always)]
unsafe fn value_as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    core::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), core::mem::size_of::<T>())
}

#[inline(always)]
unsafe fn maybe_uninit_as_bytes_mut<T>(value: &mut MaybeUninit<T>) -> &mut [u8] {
    core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), core::mem::size_of::<T>())
}

fn check_user_access_range(addr: VirtAddr, len: usize, write: bool) -> Result<(), SystemError> {
    if MMArch::PAGE_FAULT_ENABLED {
        return access_ok(addr, len).map_err(|_| SystemError::EFAULT);
    }

    if user_accessible_len(addr, len, write) < len {
        return Err(SystemError::EFAULT);
    }

    Ok(())
}

fn prefault_user_range(addr: VirtAddr, len: usize, write: bool) -> Result<(), SystemError> {
    prefault_user_range_mode(addr, len, write, None)
}

fn prefault_user_range_mode(
    addr: VirtAddr,
    len: usize,
    write: bool,
    mut prepared: Option<&mut PreparedUserWriteState>,
) -> Result<(), SystemError> {
    if len == 0 || (prepared.is_none() && MMArch::PAGE_FAULT_ENABLED) {
        return Ok(());
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        if prepared.is_none() {
            return Ok(());
        }
    }

    {
        let end = addr.data().checked_add(len).ok_or(SystemError::EFAULT)?;
        let start_page = VirtAddr::new(addr.data() & !MMArch::PAGE_OFFSET_MASK);
        let end_page = VirtAddr::new((end - 1) & !MMArch::PAGE_OFFSET_MASK);
        let region_len = end_page
            .data()
            .checked_sub(start_page.data())
            .and_then(|len| len.checked_add(MMArch::PAGE_SIZE))
            .ok_or(SystemError::EFAULT)?;
        let region = VirtRegion::new(start_page, region_len);

        let mm = match prepared.as_ref() {
            Some(state) => state.mm.clone(),
            None => AddressSpace::current()?,
        };
        let mut current = start_page;
        let mut retried = false;
        let mut charge_recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        'retry: loop {
            let creator = crate::cgroup::memory::object_for_task(
                &crate::process::ProcessManager::current_pcb(),
            );
            let data_account = mm.memory_account();
            let mut space_guard = mm.write_guard_no_reservation_conflict(region);
            loop {
                let vma = space_guard
                    .mappings
                    .contains(current)
                    .ok_or(SystemError::EFAULT)?;
                let vm_flags = *vma.lock().vm_flags();
                let permitted = if write {
                    vm_flags.contains(VmFlags::VM_WRITE)
                } else {
                    vm_flags.contains(VmFlags::VM_READ)
                };
                if !permitted {
                    return Err(SystemError::EFAULT);
                }

                // Explicit preparation is not a synthetic fault on an already
                // writable leaf. In particular it must not inflate PGFAULT or
                // repeat a file mkwrite operation.
                let ready = prepared.is_some() && {
                    let _pt_edit = mm.page_table_edit();
                    space_guard
                        .user_mapper
                        .utable
                        .translate(current)
                        .is_some_and(|(_, flags)| prepared_write_flags(flags))
                };
                if !ready {
                    let mut fault_flags = if write {
                        FaultFlags::FAULT_FLAG_WRITE
                    } else {
                        FaultFlags::empty()
                    };
                    if retried {
                        fault_flags |= FaultFlags::FAULT_FLAG_TRIED;
                    }
                    let fault = unsafe {
                        let message = PageFaultMessage::new(
                            vma,
                            current,
                            fault_flags,
                            &mut space_guard.user_mapper.utable,
                            mm.clone(),
                            creator.clone(),
                            data_account.clone(),
                        );
                        PageFaultHandler::handle_mm_fault(message)
                    };
                    if let Some(failure) = fault.charge_failure {
                        drop(space_guard);
                        crate::mm::fault::recover_fault_charge(
                            failure,
                            &mut charge_recovery,
                            &data_account,
                            fault_flags,
                        )?;
                        continue 'retry;
                    }
                    if fault.reason.contains(VmFaultReason::VM_FAULT_OOM) {
                        return Err(SystemError::ENOMEM);
                    }
                    if fault.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                        retried = true;
                        let wait = fault.retry_wait;
                        drop(space_guard);
                        if let Some(wait) = wait {
                            let result = match wait.wait() {
                                Err(crate::mm::allocation::MemoryAllocationError::Charge(
                                    failure,
                                )) => {
                                    crate::mm::fault::recover_fault_charge(
                                        failure,
                                        &mut charge_recovery,
                                        &data_account,
                                        fault_flags,
                                    )?;
                                    continue 'retry;
                                }
                                Err(crate::mm::allocation::MemoryAllocationError::System(
                                    error,
                                )) => Err(error),
                                Ok(()) => Ok(()),
                            };
                            if let Err(error) = result {
                                if matches!(error, SystemError::EINTR | SystemError::ERESTARTSYS) {
                                    // Explicit preparation is a cancellable
                                    // transaction before any user output.
                                    // Preserve its signal outcome; do not defer
                                    // it as a bad-pointer error to late commit.
                                    if prepared.is_some() {
                                        return Err(error);
                                    }
                                } else {
                                    crate::mm::fault::account_fault_result(
                                        &data_account,
                                        fault_flags,
                                        VmFaultReason::VM_FAULT_SIGBUS,
                                    );
                                }
                                return Err(SystemError::EFAULT);
                            }
                        }
                        continue 'retry;
                    }
                    if fault.reason.intersects(
                        VmFaultReason::VM_FAULT_SIGBUS
                            | VmFaultReason::VM_FAULT_SIGSEGV
                            | VmFaultReason::VM_FAULT_HWPOISON
                            | VmFaultReason::VM_FAULT_HWPOISON_LARGE
                            | VmFaultReason::VM_FAULT_FALLBACK,
                    ) {
                        return Err(SystemError::EFAULT);
                    }
                }

                if let Some(state) = prepared.as_deref_mut() {
                    // Hold this very MM write guard until the physical owner
                    // and cache active pin have been captured. There is no
                    // clean-reclaim gap between the fault and retention.
                    if !state.pin_leaf(&space_guard, current)? {
                        drop(space_guard);
                        // Normal DAX revocation can invalidate the provider
                        // snapshot while retaining a leaf until its final zap.
                        // Retire every candidate outside MM/PT, then let that
                        // publisher finish before retrying ordinary faults.
                        state.pages = [None, None];
                        if crate::arch::ipc::signal::Signal::oom_fatal_signal_pending(
                            &ProcessManager::current_pcb(),
                        ) {
                            return Err(SystemError::EINTR);
                        }
                        crate::sched::cond_resched();
                        current = start_page;
                        retried = false;
                        continue 'retry;
                    }
                }

                if current == end_page {
                    return Ok(());
                }
                retried = false;
                current = VirtAddr::new(
                    current
                        .data()
                        .checked_add(MMArch::PAGE_SIZE)
                        .ok_or(SystemError::EFAULT)?,
                );
            }
        }
    }
}

fn prepared_write_flags(flags: crate::mm::page::EntryFlags<MMArch>) -> bool {
    MMArch::ENTRY_FLAG_PRESENT != 0
        && MMArch::ENTRY_FLAG_USER != 0
        && MMArch::ENTRY_FLAG_WRITEABLE != 0
        && flags.has_flag(MMArch::ENTRY_FLAG_PRESENT)
        && flags.has_flag(MMArch::ENTRY_FLAG_USER)
        && flags.has_write()
        // RISC-V has no lazy user A/D fault handler. Normal writable mappings
        // already carry these bits; an incomplete leaf is not ready to store.
        && (!cfg!(target_arch = "riscv64")
            || (flags.has_flag(MMArch::ENTRY_FLAG_ACCESSED)
                && flags.has_flag(MMArch::ENTRY_FLAG_DIRTY)))
}

struct PreparedUserPage {
    address: VirtAddr,
    source: PreparedUserPageSource,
}

enum PreparedUserPageSource {
    Managed {
        page: Arc<Page>,
        _cache_pin: Option<crate::filesystem::page_cache::PageCachePagePin>,
        _cache: Option<Arc<crate::filesystem::page_cache::PageCache>>,
    },
    External {
        physical: crate::mm::PhysAddr,
        vma: Arc<crate::mm::ucontext::LockedVMA>,
        _file: Arc<crate::filesystem::vfs::file::File>,
        _snapshot: Option<crate::filesystem::fuse::inode::DaxUserWriteSnapshot>,
    },
}

impl PreparedUserPageSource {
    fn matches(
        &self,
        physical: crate::mm::PhysAddr,
        vma: &Arc<crate::mm::ucontext::LockedVMA>,
    ) -> bool {
        match self {
            Self::Managed { page, .. } => page.phys_address() == physical,
            Self::External {
                physical: expected,
                vma: original,
                ..
            } => *expected == physical && Arc::ptr_eq(original, vma),
        }
    }
}

struct PreparedUserWriteState {
    mm: Arc<AddressSpace>,
    pages: [Option<PreparedUserPage>; 2],
}

impl PreparedUserWriteState {
    fn pin_leaf(
        &mut self,
        space: &crate::mm::ucontext::InnerAddressSpace,
        address: VirtAddr,
    ) -> Result<bool, SystemError> {
        let _pt_edit = self.mm.page_table_edit();
        let (physical, _flags) = space
            .user_mapper
            .utable
            .translate(address)
            .filter(|(_, flags)| prepared_write_flags(*flags))
            .ok_or(SystemError::EFAULT)?;
        let page = { page_manager_lock().get_registered(&physical) };
        if page.is_none() {
            let vma = space
                .mappings
                .contains(address)
                .ok_or(SystemError::EFAULT)?;
            let (file, page_offset) = {
                let guard = vma.lock();
                if !guard
                    .vm_flags()
                    .contains(VmFlags::VM_MIXEDMAP | VmFlags::VM_SHARED | VmFlags::VM_WRITE)
                {
                    return Err(SystemError::EFAULT);
                }
                let offset = address
                    .data()
                    .checked_sub(guard.region().start().data())
                    .map(|bytes| bytes / MMArch::PAGE_SIZE)
                    .and_then(|pages| guard.backing_page_offset()?.checked_add(pages))
                    .ok_or(SystemError::EFAULT)?;
                (guard.vm_file().ok_or(SystemError::EFAULT)?, offset)
            };
            let snapshot = crate::filesystem::fuse::virtiofs::snapshot_external_user_write(
                &file,
                page_offset,
            )?;
            let valid = snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.physical == Some(physical));
            let slot = self
                .pages
                .iter_mut()
                .find(|slot| slot.is_none())
                .expect("checked two-page preparation visits each leaf once");
            *slot = Some(PreparedUserPage {
                address,
                source: PreparedUserPageSource::External {
                    physical,
                    vma,
                    _file: file,
                    _snapshot: snapshot,
                },
            });
            return Ok(valid);
        }
        let page = page.expect("managed branch retained the actual Page");
        let file_info = {
            let guard = page.read();
            match guard.page_type() {
                crate::mm::page::PageType::File(info) => Some(info.clone()),
                _ => None,
            }
        };
        let needs_cache_pin = file_info.is_some();
        let cache = file_info
            .as_ref()
            .and_then(|info| info.page_cache.upgrade());
        let cache_pin = match (&file_info, &cache) {
            (Some(info), Some(cache)) => cache.get_ready_page_pinned(info.index),
            _ => None,
        };
        let valid = (!needs_cache_pin || cache_pin.is_some())
            && cache_pin
                .as_ref()
                .is_none_or(|pin| Arc::ptr_eq(&pin.page(), &page));
        let slot = self
            .pages
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("checked two-page preparation visits each leaf once");
        // The owner outlives this MM/PT guard, including identity failure.
        // Never release a final page/cache pin from this critical section.
        *slot = Some(PreparedUserPage {
            address,
            source: PreparedUserPageSource::Managed {
                page,
                _cache_pin: cache_pin,
                _cache: cache,
            },
        });
        Ok(valid)
    }
}

/// A delayed single-value userspace write, with at most two retained pages.
///
/// Preparation may fault/COW/recover and must occur outside resource guards.
/// `write` only validates and copies through the original user virtual address.
/// Keep this owner outside any surrounding transaction guard so its Page/cache
/// pins are also retired outside those guards, including on failure.
pub(crate) struct PreparedUserWrite<T: Copy> {
    state: PreparedUserWriteState,
    address: VirtAddr,
    value: core::marker::PhantomData<T>,
}

/// A valid writable VMA can lose its leaf or write permission through normal
/// reclaim/writeback. That is a fault to retry outside the transaction, not a
/// userspace EFAULT and not an allocator failure.
pub(crate) enum UserWriteCommitError {
    RefaultRequired,
    System(SystemError),
}

impl From<SystemError> for UserWriteCommitError {
    fn from(error: SystemError) -> Self {
        Self::System(error)
    }
}

impl<T: Copy> PreparedUserWrite<T> {
    pub(crate) fn prepare(mm: &Arc<AddressSpace>, address: VirtAddr) -> Result<Self, SystemError> {
        let len = size_of::<T>();
        access_ok(address, len).map_err(|_| SystemError::EFAULT)?;
        if len != 0 {
            let end = address.data().checked_add(len).ok_or(SystemError::EFAULT)?;
            let first = address.data() & !MMArch::PAGE_OFFSET_MASK;
            let last = (end - 1) & !MMArch::PAGE_OFFSET_MASK;
            if last
                .checked_sub(first)
                .is_none_or(|span| span >= 2 * MMArch::PAGE_SIZE)
            {
                return Err(SystemError::EINVAL);
            }
        }
        // Zero placeholder encodings cannot prove an actual writable PTE.
        // In particular the current LoongArch MM implementation is incomplete.
        if MMArch::ENTRY_FLAG_PRESENT == 0
            || MMArch::ENTRY_FLAG_WRITEABLE == 0
            || MMArch::ENTRY_FLAG_USER == 0
        {
            return Err(SystemError::EFAULT);
        }
        let mut result = Self {
            state: PreparedUserWriteState {
                mm: mm.clone(),
                pages: [None, None],
            },
            address,
            value: core::marker::PhantomData,
        };
        prefault_user_range_mode(address, len, true, Some(&mut result.state))?;
        Ok(result)
    }

    pub(crate) fn write(&self, value: &T) -> Result<(), UserWriteCommitError> {
        let current = AddressSpace::current()?;
        if !Arc::ptr_eq(&current, &self.state.mm) {
            return Err(SystemError::EFAULT.into());
        }
        let space = self.state.mm.write();
        let _pt_edit = self.state.mm.page_table_edit();
        for retained in self.state.pages.iter().flatten() {
            let vma = space
                .mappings
                .contains(retained.address)
                .ok_or(SystemError::EFAULT)?;
            if !vma.lock().vm_flags().contains(VmFlags::VM_WRITE) {
                return Err(SystemError::EFAULT.into());
            }
            let (physical, flags) = space
                .user_mapper
                .utable
                .translate(retained.address)
                .ok_or(UserWriteCommitError::RefaultRequired)?;
            if !retained.source.matches(physical, &vma) || !prepared_write_flags(flags) {
                return Err(UserWriteCommitError::RefaultRequired);
            }
        }
        let _no_fault = crate::process::preempt::PageFaultDisabledGuard::new();
        // All leaves were validated before copying even the first byte. The
        // MM/PT guards prevent concurrent unmap, write-protection and reclaim.
        // An unexpected hardware exception is fixed up, never a sleeping fault.
        let remaining = unsafe {
            MMArch::copy_with_exception_table(
                self.address.data() as *mut u8,
                (value as *const T).cast::<u8>(),
                size_of::<T>(),
            )
        };
        if remaining != 0 {
            Err(SystemError::EFAULT.into())
        } else {
            Ok(())
        }
    }
}

/// Clear data in the specified range of user space
///
/// ## Arguments
///
/// - `dest`: Destination address in user space
/// - `len`: Length of data to clear
///
/// ## Returns
///
/// Returns the length of cleared data
///
/// ## Errors
///
/// - `EFAULT`: Destination address is invalid
pub unsafe fn clear_user_protected(dest: VirtAddr, len: usize) -> Result<usize, SystemError> {
    clear_user_cow_protected(dest, len)?;
    compiler_fence(Ordering::SeqCst);
    return Ok(len);
}

/// Clear data in the specified range of user space
///
/// ## Arguments
///
/// - `dest`: Destination address in user space
/// - `len`: Length of data to clear
///
/// ## Returns
///
/// Returns the length of cleared data
///
/// ## Errors
///
/// - `EFAULT`: Destination address is invalid
pub unsafe fn clear_user(dest: VirtAddr, len: usize) -> Result<usize, SystemError> {
    access_ok(dest, len).map_err(|_| SystemError::EFAULT)?;

    let p = dest.data() as *mut u8;
    // Clear user space data
    p.write_bytes(0, len);
    compiler_fence(Ordering::SeqCst);
    return Ok(len);
}

/// 使用异常表保护清零用户空间，同时保持 CR0.WP，使只读/COW 用户页仍会触发正常缺页处理。
pub unsafe fn clear_user_cow_protected(dest: VirtAddr, len: usize) -> Result<usize, SystemError> {
    if len == 0 {
        return Ok(0);
    }

    if user_accessible_len(dest, len, true) < len {
        return Err(SystemError::EFAULT);
    }
    prefault_user_range(dest, len, true)?;

    let result = MMArch::memset_with_exception_table(dest.data() as *mut u8, 0, len);
    if result == 0 {
        Ok(len)
    } else {
        Err(SystemError::EFAULT)
    }
}

/// Check and copy a C string from user space.
///
/// Returns an error when encountering an invalid address.
///
/// ## Arguments
///
/// - `user`: Pointer to the C string in user space
/// - `max_length`: Maximum copy length
///
/// ## Returns
///
/// Returns the copied C string
///
/// ## Errors
///
/// - `EFAULT`: User space address is invalid
/// - `EINVAL`: String is not a valid C string
pub fn check_and_clone_cstr(
    user: *const u8,
    max_length: Option<usize>,
) -> Result<CString, SystemError> {
    return do_check_and_clone_cstr(user, max_length, false);
}
fn do_check_and_clone_cstr(
    user: *const u8,
    max_length: Option<usize>,
    return_name_too_long: bool,
) -> Result<CString, SystemError> {
    if user.is_null() {
        return Err(SystemError::EFAULT);
    }

    // Read from user space until null character '\0' or maximum length is reached
    let mut buffer = Vec::new();
    for i in 0.. {
        if max_length.is_some() && max_length.as_ref().unwrap() <= &i {
            break;
        }

        let addr = unsafe { user.add(i) };
        let mut c = [0u8; 1];

        // 使用受异常表保护的版本，如果用户地址无效会安全返回错误
        unsafe {
            copy_from_user_protected(&mut c, VirtAddr::new(addr as usize))?;
        }

        if c[0] == 0 {
            break;
        }
        buffer.push(NonZero::new(c[0]).ok_or(SystemError::EINVAL)?);
    }
    if return_name_too_long && buffer.len() >= max_length.unwrap_or(usize::MAX) {
        return Err(SystemError::ENAMETOOLONG);
    }

    let cstr = CString::from(buffer);

    return Ok(cstr);
}

pub fn vfs_check_and_clone_cstr(
    user: *const u8,
    max_length: Option<usize>,
) -> Result<CString, SystemError> {
    return do_check_and_clone_cstr(user, max_length, true);
}

/// Check and copy a C string array from user space
///
/// Stops copying when encountering a null pointer. Returns an error when encountering an invalid address.
/// ## Arguments
///
/// - `user`: Pointer array to C strings in user space
///
/// ## Returns
///
/// Returns the copied C string array
///
/// ## Errors
///
/// - `EFAULT`: User space address is invalid
pub fn check_and_clone_cstr_array(user: *const *const u8) -> Result<Vec<CString>, SystemError> {
    if user.is_null() {
        Ok(Vec::new())
    } else {
        // debug!("check_and_clone_cstr_array: {:p}\n", user);
        let mut buffer = Vec::new();
        for i in 0.. {
            let addr = unsafe { user.add(i) };
            let str_ptr: *const u8;
            // Read the value at this address (which is also a pointer)
            unsafe {
                let dst = [0usize; 1];
                let mut dst = core::mem::transmute::<[usize; 1], [u8; size_of::<usize>()]>(dst);

                // 使用受异常表保护的版本
                copy_from_user_protected(&mut dst, VirtAddr::new(addr as usize))?;

                let dst = core::mem::transmute::<[u8; size_of::<usize>()], [usize; 1]>(dst);
                str_ptr = dst[0] as *const u8;

                // debug!("str_ptr: {:p}, addr:{addr:?}\n", str_ptr);
            }

            if str_ptr.is_null() {
                break;
            }
            // Read the string pointed to by this pointer
            let string = check_and_clone_cstr(str_ptr, None)?;
            // Put the string into the buffer
            buffer.push(string);
        }
        return Ok(buffer);
    }
}

#[derive(Debug)]
pub struct UserBufferWriter<'a> {
    buffer: &'a mut [u8],
}

#[derive(Debug)]
pub struct UserBufferReader<'a> {
    buffer: &'a [u8],
}

impl UserBufferReader<'_> {
    /// Construct a BufferReader pointing to a user space location.
    /// Uses a separate generic for initialization to support cases like passing *const u8.
    ///
    /// # Arguments
    /// * `addr` - User space pointer
    /// * `len` - Byte length of the buffer
    /// * `from_user` - Whether to verify the address is from user space
    ///
    /// # Returns
    /// * Returns UserBufferReader instance on success, error code otherwise
    ///
    pub fn new<U>(addr: *const U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        // SAFETY: constructing a slice from a null pointer with non-zero length is UB.
        // Linux semantics: passing a null pointer for a non-empty user buffer should fail with EFAULT.
        if len != 0 && (addr as usize) == 0 {
            return Err(SystemError::EFAULT);
        }
        if from_user && access_ok(VirtAddr::new(addr as usize), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: if len == 0 {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(addr as *const u8, len) }
            },
        });
    }

    pub fn new_checked<U>(
        addr: *const U,
        len: usize,
        from_user: bool,
    ) -> Result<Self, SystemError> {
        let accessible_len = user_accessible_len(VirtAddr::new(addr as usize), len, false);
        if accessible_len < len {
            return Err(SystemError::EFAULT);
        }

        return Self::new(addr, len, from_user);
    }

    pub fn size(&self) -> usize {
        return self.buffer.len();
    }

    /// Read data from user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a slice of user space data (returns a slice of length 1 for a single struct)
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn read_from_user_checked<T>(&self, offset: usize) -> Result<&[T], SystemError> {
        return self.convert_with_offset_checked(self.buffer, offset);
    }

    /// Read data from user space (into variables)
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a slice of user space data (returns a slice of length 1 for a single struct)
    ///
    pub fn read_from_user<T>(&self, offset: usize) -> Result<&[T], SystemError> {
        return self.convert_with_offset(self.buffer, offset);
    }
    /// Read one data item with specified offset from user space (into variable)
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a copy of the user space data
    ///
    pub fn read_one_from_user<T>(&self, offset: usize) -> Result<T, SystemError> {
        let mut data = MaybeUninit::<T>::uninit();
        self.copy_one_from_user_bytes(unsafe { maybe_uninit_as_bytes_mut(&mut data) }, offset)?;
        return Ok(unsafe { data.assume_init() });
    }

    /// Read one data item from user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a copy of the user space data
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn read_one_from_user_checked<T>(&self, offset: usize) -> Result<T, SystemError> {
        let mut data = MaybeUninit::<T>::uninit();
        self.copy_one_from_user_bytes_checked(
            unsafe { maybe_uninit_as_bytes_mut(&mut data) },
            offset,
        )?;
        return Ok(unsafe { data.assume_init() });
    }

    /// Copy data from user space (to specified address)
    ///
    /// # Arguments
    /// * `dst` - Destination address pointer
    ///
    /// # Returns
    /// * Returns number of elements copied on success
    ///
    pub fn copy_from_user<T: core::marker::Copy>(
        &self,
        dst: &mut [T],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if dst.is_empty() {
            return Ok(0);
        }

        let bytes_needed = dst
            .len()
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(SystemError::EINVAL)?;

        checked_user_range(self.buffer.len(), offset, bytes_needed)?;

        let dst_bytes =
            unsafe { core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, bytes_needed) };
        self.copy_from_user_protected(dst_bytes, offset)?;
        Ok(dst.len())
    }

    /// Copy a write payload, preserving the prefix actually copied before a
    /// user fault. Existing all-or-error copy APIs keep their original contract.
    pub fn copy_from_user_prefix(
        &self,
        dst: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        checked_user_range(self.buffer.len(), offset, dst.len())?;
        if dst.is_empty() {
            return Ok(0);
        }
        let src = VirtAddr::new(self.buffer.as_ptr() as usize + offset);
        unsafe { copy_from_user_prefix_protected(dst, src) }
    }

    /// Copy data from user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `dst` - Destination address pointer
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns number of elements copied on success
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn copy_from_user_checked<T: core::marker::Copy>(
        &self,
        dst: &mut [T],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if dst.is_empty() {
            return Ok(0);
        }

        let bytes_needed = dst
            .len()
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(SystemError::EINVAL)?;

        checked_user_range(self.buffer.len(), offset, bytes_needed)?;

        let accessible_len = user_accessible_len(
            VirtAddr::new(self.buffer.as_ptr() as usize + offset),
            bytes_needed,
            false,
        );
        if accessible_len < bytes_needed {
            return Err(SystemError::EFAULT);
        }

        let dst_bytes =
            unsafe { core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, bytes_needed) };
        self.copy_from_user_protected(dst_bytes, offset)?;
        Ok(dst.len())
    }

    /// Copy one data item from user space (to specified address)
    ///
    /// # Arguments
    /// * `dst` - Destination address pointer
    ///
    /// # Returns
    /// * Ok(()) on success
    ///
    pub fn copy_one_from_user<T: core::marker::Copy>(
        &self,
        dst: &mut T,
        offset: usize,
    ) -> Result<(), SystemError> {
        self.copy_one_from_user_bytes(unsafe { value_as_bytes_mut(dst) }, offset)?;
        return Ok(());
    }

    /// Copy one data item from user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `dst` - Destination address pointer
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Ok(()) on success
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn copy_one_from_user_checked<T: core::marker::Copy>(
        &self,
        dst: &mut T,
        offset: usize,
    ) -> Result<(), SystemError> {
        self.copy_one_from_user_bytes_checked(unsafe { value_as_bytes_mut(dst) }, offset)?;
        return Ok(());
    }

    /// Convert user space data to a slice of specified type
    ///
    /// # Arguments
    ///
    /// * `offset` - Byte offset
    pub fn buffer<T>(&self, offset: usize) -> Result<&[T], SystemError> {
        self.convert_with_offset::<T>(self.buffer, offset)
            .map_err(|_| SystemError::EINVAL)
    }

    /// 返回一个受异常表保护的用户缓冲区包装
    ///
    /// 与 `buffer()` 不同，此方法返回的 `UserBuffer` 类型会在所有读写操作中
    /// 使用异常表保护的拷贝函数，确保访问无效用户地址时能安全返回错误而不是panic
    ///
    /// # 参数
    /// - `offset`: 字节偏移量
    ///
    /// # 返回值
    /// - `Ok(UserBuffer)`: 受保护的用户缓冲区包装
    /// - `Err(SystemError)`: 偏移量无效
    pub fn buffer_protected(&'_ self, offset: usize) -> Result<UserBuffer<'_>, SystemError> {
        if offset > self.buffer.len() {
            return Err(SystemError::EINVAL);
        }

        let addr = VirtAddr::new(self.buffer.as_ptr() as usize + offset);
        let len = self.buffer.len() - offset;

        Ok(unsafe { UserBuffer::new(addr, len) })
    }

    /// Convert user space data to a slice of specified type with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a slice of the specified type
    ///
    /// # Errors
    /// * `EINVAL` - Invalid offset or alignment
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn buffer_checked<T>(&self, offset: usize) -> Result<&[T], SystemError> {
        self.convert_with_offset_checked(self.buffer, offset)
            .map_err(|_| SystemError::EINVAL)
    }

    fn convert_with_offset<T>(&self, src: &[u8], offset: usize) -> Result<&[T], SystemError> {
        // offset == src.len is valid, as long as don't try to dereference it in &src[offset..]
        if offset > src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &[u8] = &src[offset..];
        if byte_buffer.is_empty() {
            // Empty buffer is valid - return empty slice
            return Ok(&[]);
        }
        if !byte_buffer.len().is_multiple_of(core::mem::size_of::<T>()) {
            return Err(SystemError::EINVAL);
        }

        debug_assert!(offset < src.len());
        let chunks = unsafe {
            from_raw_parts(
                byte_buffer.as_ptr() as *const T,
                byte_buffer.len() / core::mem::size_of::<T>(),
            )
        };
        return Ok(chunks);
    }

    fn convert_with_offset_checked<T>(
        &self,
        src: &[u8],
        offset: usize,
    ) -> Result<&[T], SystemError> {
        let size = src.len().saturating_sub(offset);
        if size > 0 {
            let accessible_len =
                user_accessible_len(VirtAddr::new(src.as_ptr() as usize + offset), size, false);
            if accessible_len < size {
                return Err(SystemError::EFAULT);
            }
        }
        self.convert_with_offset(src, offset)
    }

    #[inline(always)]
    fn copy_one_from_user_bytes(&self, dst: &mut [u8], offset: usize) -> Result<(), SystemError> {
        checked_user_range(self.buffer.len(), offset, dst.len())?;
        if dst.is_empty() {
            return Ok(());
        }

        self.copy_from_user_protected(dst, offset).map(|_| ())
    }

    #[inline(always)]
    fn copy_one_from_user_bytes_checked(
        &self,
        dst: &mut [u8],
        offset: usize,
    ) -> Result<(), SystemError> {
        checked_user_range(self.buffer.len(), offset, dst.len())?;
        if dst.is_empty() {
            return Ok(());
        }

        let accessible_len = user_accessible_len(
            VirtAddr::new(self.buffer.as_ptr() as usize + offset),
            dst.len(),
            false,
        );
        if accessible_len < dst.len() {
            return Err(SystemError::EFAULT);
        }

        self.copy_from_user_protected(dst, offset).map(|_| ())
    }
}

impl UserBufferReader<'_> {
    /// 使用异常保护的方式从用户空间拷贝数据到内核空间
    ///
    /// 此方法使用异常表机制，即使在页错误时也能安全返回错误，
    /// 而不是panic或无限循环。
    ///
    /// # Arguments
    /// * `dst` - 目标缓冲区(内核空间)
    /// * `offset` - 用户缓冲区的字节偏移
    ///
    /// # Returns
    /// * `Ok(len)` - 成功拷贝的字节数
    /// * `Err(SystemError::EFAULT)` - 访问失败
    pub fn copy_from_user_protected(
        &self,
        dst: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if offset >= self.buffer.len() {
            return Err(SystemError::EINVAL);
        }
        let src_slice = &self.buffer[offset..];
        let copy_len = core::cmp::min(dst.len(), src_slice.len());
        if copy_len == 0 {
            return Ok(0);
        }

        unsafe {
            copy_from_user_protected(
                &mut dst[..copy_len],
                VirtAddr::new(src_slice.as_ptr() as usize),
            )
        }
    }
}

#[allow(dead_code)]
impl<'a> UserBufferWriter<'a> {
    /// Construct a BufferWriter pointing to a user space location
    ///
    /// # Arguments
    /// * `addr` - User space pointer
    /// * `len` - Byte length of the buffer
    ///
    /// # Returns
    /// * Returns UserBufferWriter instance on success, error code otherwise
    ///
    pub fn new<U>(addr: *mut U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        // SAFETY: constructing a slice from a null pointer with non-zero length is UB.
        if len != 0 && (addr as usize) == 0 {
            return Err(SystemError::EFAULT);
        }
        if from_user && access_ok(VirtAddr::new(addr as usize), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: if len == 0 {
                &mut []
            } else {
                unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) }
            },
        });
    }

    pub fn new_checked<U>(addr: *mut U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        let accessible_len = user_accessible_len(VirtAddr::new(addr as usize), len, true);
        if accessible_len < len {
            return Err(SystemError::EFAULT);
        }

        return Self::new(addr, len, from_user);
    }

    pub fn size(&self) -> usize {
        return self.buffer.len();
    }

    /// Write data from specified address to user space
    ///
    /// # Arguments
    /// * `src` - Source data address
    /// * `offset` - Byte offset in UserBuffer
    ///
    /// # Returns
    /// * Returns number of elements written
    ///
    pub fn copy_to_user<T: core::marker::Copy>(
        &'a mut self,
        src: &[T],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let dst = Self::convert_with_offset(self.buffer, offset)?;
        if dst.len() < src.len() {
            return Err(SystemError::EINVAL);
        }
        dst[..src.len()].copy_from_slice(src);
        return Ok(src.len());
    }

    /// Write data from specified address to user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `src` - Source data address
    /// * `offset` - Byte offset in UserBuffer
    ///
    /// # Returns
    /// * Returns number of elements written
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn copy_to_user_checked<T: core::marker::Copy>(
        &'a mut self,
        src: &[T],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let dst = Self::convert_with_offset_checked(self.buffer, offset)?;
        if dst.len() < src.len() {
            return Err(SystemError::EINVAL);
        }
        dst[..src.len()].copy_from_slice(src);
        return Ok(src.len());
    }

    /// Write one data item from specified address to user space
    ///
    /// # Arguments
    /// * `src` - Source data address
    /// * `offset` - Byte offset in UserBuffer
    ///
    /// # Returns
    /// * Ok(()) on success
    ///
    pub fn copy_one_to_user<T>(&mut self, src: &T, offset: usize) -> Result<(), SystemError> {
        self.copy_one_to_user_bytes(unsafe { value_as_bytes(src) }, offset)?;
        return Ok(());
    }

    /// Write one data item from specified address to user space with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `src` - Source data address
    /// * `offset` - Byte offset in UserBuffer
    ///
    /// # Returns
    /// * Ok(()) on success
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn copy_one_to_user_checked<T>(
        &mut self,
        src: &T,
        offset: usize,
    ) -> Result<(), SystemError> {
        self.copy_one_to_user_bytes_checked(unsafe { value_as_bytes(src) }, offset)?;
        return Ok(());
    }

    pub fn buffer<T>(&'a mut self, offset: usize) -> Result<&'a mut [T], SystemError> {
        Self::convert_with_offset::<T>(self.buffer, offset).map_err(|_| SystemError::EINVAL)
    }

    /// 返回一个受异常表保护的用户缓冲区包装
    ///
    /// 与 `buffer()` 不同，此方法返回的 `UserBuffer` 类型会在所有读写操作中
    /// 使用异常表保护的拷贝函数，确保访问无效用户地址时能安全返回错误而不是panic
    ///
    /// # 参数
    /// - `offset`: 字节偏移量
    ///
    /// # 返回值
    /// - `Ok(UserBuffer)`: 受保护的用户缓冲区包装
    /// - `Err(SystemError)`: 偏移量无效
    ///
    /// # 示例
    /// ```rust
    /// let mut writer = UserBufferWriter::new(user_ptr, len, true)?;
    /// let mut buffer = writer.buffer_protected(0)?;
    /// // 这个写入是安全的，即使地址无效也会返回 EFAULT 而不是 panic
    /// buffer.write_to_user(0, kernel_data)?;
    /// ```
    pub fn buffer_protected(&'a mut self, offset: usize) -> Result<UserBuffer<'a>, SystemError> {
        if offset > self.buffer.len() {
            return Err(SystemError::EINVAL);
        }

        let addr = VirtAddr::new(self.buffer.as_ptr() as usize + offset);
        let len = self.buffer.len() - offset;

        Ok(unsafe { UserBuffer::new(addr, len) })
    }

    /// Convert to a mutable slice of specified type with page mapping and permission verification
    ///
    /// This function verifies that the pages are mapped AND have the required permissions,
    /// not just performing permission checks.
    ///
    /// # Arguments
    /// * `offset` - Byte offset
    ///
    /// # Returns
    /// * Returns a mutable slice of the specified type
    ///
    /// # Errors
    /// * `EINVAL` - Invalid offset or alignment
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn buffer_checked<T>(&'a mut self, offset: usize) -> Result<&'a mut [T], SystemError> {
        Self::convert_with_offset_checked::<T>(self.buffer, offset).map_err(|_| SystemError::EINVAL)
    }

    fn convert_with_offset<T>(src: &mut [u8], offset: usize) -> Result<&mut [T], SystemError> {
        if offset > src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &mut [u8] = &mut src[offset..];

        let len = byte_buffer.len() / core::mem::size_of::<T>();
        if len == 0 {
            // Empty buffer is valid - return empty slice
            return Ok(&mut []);
        }

        if !byte_buffer.len().is_multiple_of(core::mem::size_of::<T>()) {
            return Err(SystemError::EINVAL);
        }

        let chunks = unsafe { from_raw_parts_mut(byte_buffer.as_mut_ptr() as *mut T, len) };
        return Ok(chunks);
    }

    fn convert_with_offset_checked<T>(
        src: &mut [u8],
        offset: usize,
    ) -> Result<&mut [T], SystemError> {
        let size = src.len().saturating_sub(offset);
        if size > 0 {
            let accessible_len =
                user_accessible_len(VirtAddr::new(src.as_ptr() as usize + offset), size, true);
            if accessible_len < size {
                return Err(SystemError::EFAULT);
            }
        }
        Self::convert_with_offset(src, offset)
    }

    #[inline(always)]
    fn copy_one_to_user_bytes(&mut self, src: &[u8], offset: usize) -> Result<(), SystemError> {
        checked_user_range(self.buffer.len(), offset, src.len())?;
        if src.is_empty() {
            return Ok(());
        }

        self.copy_to_user_protected(src, offset).map(|_| ())
    }

    #[inline(always)]
    fn copy_one_to_user_bytes_checked(
        &mut self,
        src: &[u8],
        offset: usize,
    ) -> Result<(), SystemError> {
        checked_user_range(self.buffer.len(), offset, src.len())?;
        if src.is_empty() {
            return Ok(());
        }

        let accessible_len = user_accessible_len(
            VirtAddr::new(self.buffer.as_ptr() as usize + offset),
            src.len(),
            true,
        );
        if accessible_len < src.len() {
            return Err(SystemError::EFAULT);
        }

        self.copy_to_user_protected(src, offset).map(|_| ())
    }
}

impl<'a> UserBufferWriter<'a> {
    /// Copy from kernel space to user space with exception-table protection.
    ///
    /// Uses the exception table so page faults return an error safely instead of
    /// panicking or looping indefinitely.
    ///
    /// # Arguments
    /// * `src` - source buffer (kernel space)
    /// * `offset` - byte offset into the user buffer
    ///
    /// # Returns
    /// * `Ok(len)` - number of bytes copied
    /// * `Err(SystemError::EFAULT)` - access failed
    #[allow(dead_code)]
    pub fn copy_to_user_protected(
        &mut self,
        src: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if offset >= self.buffer.len() {
            return Err(SystemError::EINVAL);
        }
        let dst_slice = &mut self.buffer[offset..];
        let copy_len = core::cmp::min(src.len(), dst_slice.len());
        if copy_len == 0 {
            return Ok(0);
        }

        unsafe {
            copy_to_user_protected(
                VirtAddr::new(dst_slice.as_mut_ptr() as usize),
                &src[..copy_len],
            )
        }
    }
}

/// 带异常保护的用户空间数据拷贝
///
/// 与`copy_from_user`不同,此函数使用异常表机制,
/// 即使在页错误时也能安全返回错误
///
/// ## 参数
/// - `dst`: 目标缓冲区(内核空间)
/// - `src`: 源地址(用户空间)
///
/// ## 返回值
/// - `Ok(len)`: 成功拷贝的字节数
/// - `Err(SystemError::EFAULT)`: 访问失败
pub unsafe fn copy_from_user_protected(
    dst: &mut [u8],
    src: VirtAddr,
) -> Result<usize, SystemError> {
    let len = dst.len();
    if len == 0 {
        return Ok(0);
    }

    let dst_ptr = dst.as_mut_ptr();
    let src_ptr = src.data() as *const u8;

    check_user_access_range(src, len, false)?;
    prefault_user_range(src, len, false)?;

    // 执行实际拷贝,使用异常表保护
    let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, len);

    match result {
        0 => Ok(len),
        _ => Err(SystemError::EFAULT),
    }
}

/// A write-stage-only variant of protected copying. Never infer successfully
/// copied bytes from a VMA snapshot on architectures with faulting user access.
pub(crate) unsafe fn copy_from_user_prefix_protected(
    dst: &mut [u8],
    src: VirtAddr,
) -> Result<usize, SystemError> {
    access_ok(src, dst.len()).map_err(|_| SystemError::EFAULT)?;
    let len = if MMArch::PAGE_FAULT_ENABLED {
        dst.len()
    } else {
        // Non-faulting architectures still need mapping/prefault admission,
        // but only at the actual copy stage, after file privilege removal.
        user_accessible_len(src, dst.len(), false)
    };
    if len == 0 {
        return Err(SystemError::EFAULT);
    }
    prefault_user_range(src, len, false)?;
    let remaining =
        MMArch::copy_with_exception_table(dst.as_mut_ptr(), src.data() as *const u8, len);
    let copied = len.saturating_sub(remaining);
    if copied == 0 {
        Err(SystemError::EFAULT)
    } else {
        Ok(copied)
    }
}

/// Copy to user space with exception-table protection.
///
/// Writes through the exception table while keeping CR0.WP enabled, so read-only/COW
/// user pages still take the normal page-fault path. Writes to writable VMAs may fault;
/// the fault handler performs lazy allocation or COW. Semantics match Linux
/// `copy_to_user()` / `put_user()`.
///
/// ## Arguments
/// - `dest`: destination address (user space)
/// - `src`: source buffer (kernel space)
///
/// ## Returns
/// - `Ok(len)`: number of bytes written
/// - `Err(SystemError::EFAULT)`: access failed
pub unsafe fn copy_to_user_protected(dest: VirtAddr, src: &[u8]) -> Result<usize, SystemError> {
    let len = src.len();
    if len == 0 {
        return Ok(0);
    }

    let dst_ptr = dest.data() as *mut u8;
    let src_ptr = src.as_ptr();

    check_user_access_range(dest, len, true)?;
    prefault_user_range(dest, len, true)?;

    let result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, len);

    match result {
        0 => Ok(len),
        _ => Err(SystemError::EFAULT),
    }
}

/// Copy an output prefix, reporting bytes actually written before a fault.
/// Unlike the whole-copy API, this is for byte-oriented reads whose position
/// is committed after the copy. Record and socket owners keep their existing
/// whole-copy transaction. A failed copy may still have changed user memory.
///
/// # Safety
/// `dest` must designate userspace; accesses are exception-table protected.
pub unsafe fn copy_to_user_prefix_protected(
    dest: VirtAddr,
    src: &[u8],
) -> Result<usize, SystemError> {
    if src.is_empty() {
        return Ok(0);
    }
    access_ok(dest, src.len()).map_err(|_| SystemError::EFAULT)?;
    let mut copied = 0;
    while copied < src.len() {
        let address = dest + copied;
        let want = if MMArch::PAGE_FAULT_ENABLED {
            src.len() - copied
        } else {
            // Prepare only the page about to be copied. A later bad page must
            // not discard bytes already copied on non-faulting architectures.
            (MMArch::PAGE_SIZE - (address.data() & MMArch::PAGE_OFFSET_MASK))
                .min(src.len() - copied)
        };
        if check_user_access_range(address, want, true)
            .and_then(|()| prefault_user_range(address, want, true))
            .is_err()
        {
            break;
        }
        let remaining = MMArch::copy_with_exception_table(
            address.data() as *mut u8,
            src.as_ptr().add(copied),
            want,
        );
        copied += want.saturating_sub(remaining);
        if remaining != 0 {
            break;
        }
    }
    if copied == 0 {
        Err(SystemError::EFAULT)
    } else {
        Ok(copied)
    }
}

/// Copy an output prefix without resolving user faults or acquiring MM locks.
///
/// This may be used while a source page is locked. An inaccessible destination
/// stops the copy and leaves any successfully written prefix committed; callers
/// must release their source locks before retrying the remainder with the normal
/// faulting copy API.
///
/// # Safety
/// `dest` must designate userspace; accesses are exception-table protected.
pub(crate) unsafe fn copy_to_user_prefix_nofault(
    dest: VirtAddr,
    src: &[u8],
) -> Result<usize, SystemError> {
    if src.is_empty() {
        return Ok(0);
    }
    access_ok(dest, src.len()).map_err(|_| SystemError::EFAULT)?;
    let _no_fault = crate::process::preempt::PageFaultDisabledGuard::new();
    let remaining =
        MMArch::copy_with_exception_table(dest.data() as *mut u8, src.as_ptr(), src.len());
    let copied = src.len().saturating_sub(remaining);
    if copied == 0 {
        Err(SystemError::EFAULT)
    } else {
        Ok(copied)
    }
}

/// Write a single `Copy` value to user space with exception-table protection.
pub unsafe fn write_one_to_user_protected<T: Copy>(
    dest: VirtAddr,
    value: &T,
) -> Result<(), SystemError> {
    let src =
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>());
    copy_to_user_protected(dest, src).map(|_| ())
}

/// Read a single `Copy` value from user space with exception-table protection.
pub unsafe fn read_one_from_user_protected<T: Copy>(
    src: VirtAddr,
    dst: &mut T,
) -> Result<(), SystemError> {
    let bytes =
        core::slice::from_raw_parts_mut((dst as *mut T).cast::<u8>(), core::mem::size_of::<T>());
    copy_from_user_protected(bytes, src).map(|_| ())
}

/// Compute the contiguous accessible length starting at `addr`.
///
/// Returns the number of bytes that can be accessed before hitting an unmapped
/// page or a page that lacks the requested permissions.
pub fn user_accessible_len(addr: VirtAddr, size: usize, check_write: bool) -> usize {
    // log::error!(
    //     "user_accessible_len(addr: {:?}, size:{:?}, check_write:{:?}",
    //     addr,
    //     size,
    //     check_write
    // );
    if size == 0 || addr.is_null() {
        return 0;
    }

    // 获取当前进程的 vm （可访问的地址空间）
    let vm = match ProcessManager::current_pcb().basic().user_vm() {
        Some(vm) => vm,
        None => return 0,
    };

    let mut checked = 0usize;
    let mut current = addr;

    while checked < size {
        let current_page = VirtAddr::new(current.data() & !MMArch::PAGE_OFFSET_MASK);
        let vma_read_guard =
            vm.read_guard_no_reservation_conflict(VirtRegion::new(current_page, MMArch::PAGE_SIZE));
        let mappings = &vma_read_guard.mappings;
        // 判断当前地址是否落在一个有效 VMA 中
        let Some(vma) = mappings.contains(current) else {
            break;
        };

        // 获取地址所在 VMA 的起始地址 和结束地址，访问权限标志，后备的文件和当前VMA第一页映射到文件的哪一页
        let (region_start, region_end, vm_flags, vma_size, file, backing_page_offset) = {
            let guard = vma.lock();
            let region_start = guard.region().start().data();
            let region_end = guard.region().end().data();
            let vm_flags = *guard.vm_flags();
            let vma_size = region_end.saturating_sub(region_start);
            let file = guard.vm_file();
            let backing_page_offset = guard.backing_page_offset();

            drop(guard);
            (
                region_start,
                region_end,
                vm_flags,
                vma_size,
                file,
                backing_page_offset,
            )
        };

        // 根据 vm_flags 判断是否具备访问权限
        let has_permission = if check_write {
            vm_flags.contains(VmFlags::VM_WRITE)
        } else {
            vm_flags.contains(VmFlags::VM_READ)
        };
        if !has_permission {
            break;
        }

        let file_backed_len = file.and_then(|file| {
            let file_offset_pages = backing_page_offset.unwrap_or(0);
            let file_offset_bytes = file_offset_pages.saturating_mul(MMArch::PAGE_SIZE);
            let file_size = match file.metadata() {
                Ok(md) if md.size > 0 => {
                    let capped = core::cmp::min(md.size as u128, usize::MAX as u128);
                    capped as usize
                }
                Ok(_) => 0,
                Err(_) => return None,
            };

            // File faults admit the entire final partial page. In particular,
            // ELF padzero and syscall buffers may access bytes past i_size in
            // that page; only the next whole page must be rejected.
            let mapped_file_end = file_size
                .div_ceil(MMArch::PAGE_SIZE)
                .saturating_mul(MMArch::PAGE_SIZE);
            let backed = mapped_file_end.saturating_sub(file_offset_bytes);
            Some(core::cmp::min(backed, vma_size))
        });

        // 计算当前 VMA 内从 current 地址开始的可用长度
        let current_addr = current.data();
        let mut available = region_end.saturating_sub(current_addr);

        if let Some(backed_len) = file_backed_len {
            let offset_in_vma = current_addr.saturating_sub(region_start);
            let backed_available = backed_len.saturating_sub(offset_in_vma);
            // Clamp to the range actually backed by the file to avoid walking into holes.
            available = min(available, backed_available);
        }
        if available == 0 {
            break;
        }

        // 这里的 `step` 要区分两种情况
        // - 第一种情况：`available`（当前 VMA 剩余长度）已经覆盖了 `size - checked`，说明
        //   本次检查的剩余数据全部落在这个 VMA 内，`step` 直接等于 `size - checked`。
        // - 第二种情况：`available` 比 `size - checked` 小，意味着我们会在这个 VMA 的末尾停下，
        //   需要等下一次循环再确认后续地址是否仍有 VMA 覆盖。
        // - 例如 (addr = 0x1, size = 10)，若某个 VMA 只覆盖 [0x0, 0x5)，则第一轮只能推进 4 个字节，
        //   后续是否继续完全取决于下一个 VMA 是否与 0x5 处相接且具有相同访问权限。
        //   若下一轮 VMA 覆盖 [0x5, 0xf)，虽然这块 VMA 可访问空间 available == 10 ,但是我们需要检查的部分就只剩 10 - 4 = 6 bytes。
        //   所以 `step` 选择为  size - checked
        let step = min(available, size - checked);
        checked += step;

        let Some(next) = current_addr.checked_add(step) else {
            break;
        };
        current = VirtAddr::new(next);
    }

    checked
}
