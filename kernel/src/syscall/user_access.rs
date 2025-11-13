//! This file contains functions for kernel-space access to user-space data

use core::{
    mem::size_of,
    num::NonZero,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{ffi::CString, vec::Vec};
use defer::defer;

use crate::{
    arch::MMArch,
    mm::{verify_area, MemoryManagementArch, VirtAddr},
    process::ProcessManager,
};

use super::SystemError;

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
    verify_area(dest, len).map_err(|_| SystemError::EFAULT)?;

    let p = dest.data() as *mut u8;
    // Clear user space data
    p.write_bytes(0, len);
    compiler_fence(Ordering::SeqCst);
    return Ok(len);
}

pub unsafe fn copy_to_user(dest: VirtAddr, src: &[u8]) -> Result<usize, SystemError> {
    verify_area(dest, src.len()).map_err(|_| SystemError::EFAULT)?;
    MMArch::disable_kernel_wp();
    defer!({
        MMArch::enable_kernel_wp();
    });

    let p = dest.data() as *mut u8;
    // 拷贝数据
    p.copy_from_nonoverlapping(src.as_ptr(), src.len());
    return Ok(src.len());
}

/// Copy data from user space to kernel space
pub unsafe fn copy_from_user(dst: &mut [u8], src: VirtAddr) -> Result<usize, SystemError> {
    verify_area(src, dst.len()).map_err(|_| SystemError::EFAULT)?;
    let src: &[u8] = core::slice::from_raw_parts(src.data() as *const u8, dst.len());
    // 拷贝数据
    dst.copy_from_slice(src);

    return Ok(dst.len());
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
        unsafe {
            copy_from_user(&mut c, VirtAddr::new(addr as usize))?;
        }
        if c[0] == 0 {
            break;
        }
        buffer.push(NonZero::new(c[0]).ok_or(SystemError::EINVAL)?);
    }

    let cstr = CString::from(buffer);

    return Ok(cstr);
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
                copy_from_user(&mut dst, VirtAddr::new(addr as usize))?;
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
        if from_user && verify_area(VirtAddr::new(addr as usize), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: unsafe { core::slice::from_raw_parts(addr as *const u8, len) },
        });
    }

    pub fn new_checked<U>(
        addr: *const U,
        len: usize,
        from_user: bool,
    ) -> Result<Self, SystemError> {
        if !check_user_access_by_page_table(VirtAddr::new(addr as usize), len, false) {
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
    /// * Returns a reference to the user space data
    ///
    pub fn read_one_from_user<T>(&self, offset: usize) -> Result<&T, SystemError> {
        return self.convert_one_with_offset(self.buffer, offset);
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
    /// * Returns a reference to the user space data
    ///
    /// # Errors
    /// * `EFAULT` - Pages are not mapped or lack required permissions
    pub fn read_one_from_user_checked<T>(&self, offset: usize) -> Result<&T, SystemError> {
        return self.convert_one_with_offset_checked(self.buffer, offset);
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
        let data = self.convert_with_offset(self.buffer, offset)?;
        dst.copy_from_slice(data);
        return Ok(dst.len());
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
        let data = self.convert_with_offset_checked(self.buffer, offset)?;
        dst.copy_from_slice(data);
        return Ok(dst.len());
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
        let data = self.convert_one_with_offset::<T>(self.buffer, offset)?;
        dst.clone_from(data);
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
        let data = self.convert_one_with_offset_checked::<T>(self.buffer, offset)?;
        dst.clone_from(data);
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
        if offset >= src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &[u8] = &src[offset..];
        if !byte_buffer.len().is_multiple_of(core::mem::size_of::<T>()) || byte_buffer.is_empty() {
            return Err(SystemError::EINVAL);
        }

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
        if size > 0
            && !check_user_access_by_page_table(
                VirtAddr::new(src.as_ptr() as usize + offset),
                size,
                false,
            )
        {
            return Err(SystemError::EFAULT);
        }
        self.convert_with_offset(src, offset)
    }

    fn convert_one_with_offset<T>(&self, src: &[u8], offset: usize) -> Result<&T, SystemError> {
        if offset + core::mem::size_of::<T>() > src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &[u8] = &src[offset..offset + core::mem::size_of::<T>()];

        let chunks = unsafe { from_raw_parts(byte_buffer.as_ptr() as *const T, 1) };
        let data = &chunks[0];
        return Ok(data);
    }

    fn convert_one_with_offset_checked<T>(
        &self,
        src: &[u8],
        offset: usize,
    ) -> Result<&T, SystemError> {
        let size = core::mem::size_of::<T>();
        if offset + size > src.len() {
            return Err(SystemError::EINVAL);
        }
        if !check_user_access_by_page_table(
            VirtAddr::new(src.as_ptr() as usize + offset),
            size,
            false,
        ) {
            return Err(SystemError::EFAULT);
        }
        self.convert_one_with_offset(src, offset)
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
        if from_user && verify_area(VirtAddr::new(addr as usize), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) },
        });
    }

    pub fn new_checked<U>(addr: *mut U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        if !check_user_access_by_page_table(VirtAddr::new(addr as usize), len, true) {
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
        dst.copy_from_slice(src);
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
        dst.copy_from_slice(src);
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
    pub fn copy_one_to_user<T: Clone>(
        &'a mut self,
        src: &T,
        offset: usize,
    ) -> Result<(), SystemError> {
        let dst = Self::convert_one_with_offset::<T>(self.buffer, offset)?;
        dst.clone_from(src);
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
    pub fn copy_one_to_user_checked<T: Clone>(
        &'a mut self,
        src: &T,
        offset: usize,
    ) -> Result<(), SystemError> {
        let dst = Self::convert_one_with_offset_checked::<T>(self.buffer, offset)?;
        dst.clone_from(src);
        return Ok(());
    }

    pub fn buffer<T>(&'a mut self, offset: usize) -> Result<&'a mut [T], SystemError> {
        Self::convert_with_offset::<T>(self.buffer, offset).map_err(|_| SystemError::EINVAL)
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
        if !byte_buffer.len().is_multiple_of(core::mem::size_of::<T>()) {
            return Err(SystemError::EINVAL);
        }

        let chunks = unsafe {
            from_raw_parts_mut(
                byte_buffer.as_mut_ptr() as *mut T,
                byte_buffer.len() / core::mem::size_of::<T>(),
            )
        };
        return Ok(chunks);
    }

    fn convert_with_offset_checked<T>(
        src: &mut [u8],
        offset: usize,
    ) -> Result<&mut [T], SystemError> {
        let size = src.len().saturating_sub(offset);
        if size > 0
            && !check_user_access_by_page_table(
                VirtAddr::new(src.as_ptr() as usize + offset),
                size,
                true,
            )
        {
            return Err(SystemError::EFAULT);
        }
        Self::convert_with_offset(src, offset)
    }

    fn convert_one_with_offset<T>(src: &mut [u8], offset: usize) -> Result<&mut T, SystemError> {
        if offset + core::mem::size_of::<T>() > src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &mut [u8] = &mut src[offset..offset + core::mem::size_of::<T>()];

        let chunks = unsafe { from_raw_parts_mut(byte_buffer.as_mut_ptr() as *mut T, 1) };
        let data = &mut chunks[0];
        return Ok(data);
    }

    fn convert_one_with_offset_checked<T>(
        src: &mut [u8],
        offset: usize,
    ) -> Result<&mut T, SystemError> {
        let size = core::mem::size_of::<T>();
        if offset + size > src.len() {
            return Err(SystemError::EINVAL);
        }
        if !check_user_access_by_page_table(
            VirtAddr::new(src.as_ptr() as usize + offset),
            size,
            true,
        ) {
            return Err(SystemError::EFAULT);
        }
        Self::convert_one_with_offset(src, offset)
    }
}

/// Check user access by page table - verifies both page mapping AND permissions
///
/// This function checks if pages are mapped in the page table AND verifies
/// the required permissions (user access, and write access if requested).
/// It returns false if pages are not mapped or lack required permissions.
///
/// # Arguments
/// * `addr` - Virtual address to check
/// * `size` - Size of the memory region to check
/// * `check_write` - Whether to check for write permission
///
/// # Returns
/// * `true` if all pages are mapped and have required permissions
/// * `false` if any page is not mapped or lacks required permissions
fn check_user_access_by_page_table(addr: VirtAddr, size: usize, check_write: bool) -> bool {
    // Check if address is valid
    if addr.is_null() {
        return false;
    }
    // Get address space and check mapping
    let vm = match ProcessManager::current_pcb().basic().user_vm() {
        Some(vm) => vm,
        None => return false,
    };

    // Calculate page-aligned address and size
    let page_mask = MMArch::PAGE_SIZE - 1;
    let aligned_addr = addr.data() & (!page_mask); // Align down to page boundary
    let offset = (addr - aligned_addr).data();
    let aligned_size = (offset + size).next_multiple_of(MMArch::PAGE_SIZE);

    // Calculate number of pages to check (rounded up)
    let pages = aligned_size / MMArch::PAGE_SIZE;

    let guard = vm.read_irqsave();
    for i in 0..pages {
        let page_addr = aligned_addr + i * MMArch::PAGE_SIZE;
        let flags = match guard.user_mapper.utable.translate(VirtAddr::new(page_addr)) {
            Some((_, flags)) => flags,
            None => return false,
        };

        if !flags.has_user() {
            // If no user access permission, return false
            return false;
        }

        if check_write && !flags.has_write() {
            // If write permission check is required but no write permission, return false
            return false;
        }
    }
    drop(guard);

    return true;
}
