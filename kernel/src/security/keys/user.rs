//! User-memory boundaries shared by the key syscalls.
//!
//! These helpers never return a Rust reference into user memory.  Every read
//! goes through the exception-table-protected user access path before a key
//! store lock is acquired.

use alloc::vec::Vec;
use core::sync::atomic::{compiler_fence, Ordering};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    mm::{MemoryManagementArch, VirtAddr},
    syscall::user_access::copy_from_user_protected,
};

pub const TYPE_NAME_MAX: usize = 32;
pub const DESCRIPTION_MAX: usize = 4096;
pub const CALLOUT_MAX: usize = MMArch::PAGE_SIZE;
pub const ADD_PAYLOAD_MAX: usize = 1024 * 1024 - 1;

/// Copy a NUL-terminated byte string, including its Linux length limit.
/// `max_len` includes the terminator; the result does not.
pub fn copy_cstr_bytes(user: *const u8, max_len: usize) -> Result<Vec<u8>, SystemError> {
    if user.is_null() {
        return Err(SystemError::EFAULT);
    }
    let mut value = Vec::new();
    value
        .try_reserve_exact(max_len.min(256))
        .map_err(|_| SystemError::ENOMEM)?;
    for offset in 0..max_len {
        let address = (user as usize)
            .checked_add(offset)
            .ok_or(SystemError::EFAULT)?;
        let mut byte = [0u8; 1];
        unsafe { copy_from_user_protected(&mut byte, VirtAddr::new(address))? };
        if byte[0] == 0 {
            return Ok(value);
        }
        value.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        value.push(byte[0]);
    }
    Err(SystemError::ENAMETOOLONG)
}

pub fn copy_type_name(user: *const u8) -> Result<Vec<u8>, SystemError> {
    match copy_cstr_bytes(user, TYPE_NAME_MAX) {
        Ok(type_name) if type_name.is_empty() => Err(SystemError::EINVAL),
        Ok(type_name) if type_name[0] == b'.' => Err(SystemError::EPERM),
        Ok(type_name) => Ok(type_name),
        Err(SystemError::ENAMETOOLONG) => Err(SystemError::EINVAL),
        Err(error) => Err(error),
    }
}

/// A temporary user-supplied key payload.  All failure paths zero the copied
/// bytes, including a fault partway through the protected copy.
pub struct SensitiveBytes(Vec<u8>);

impl SensitiveBytes {
    pub fn copy_from_user(user: *const u8, len: usize, max: usize) -> Result<Self, SystemError> {
        if len > max {
            return Err(SystemError::EINVAL);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(len, 0);
        let mut result = Self(bytes);
        if len != 0 {
            unsafe { copy_from_user_protected(&mut result.0[..], VirtAddr::new(user as usize))? };
        }
        Ok(result)
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        core::mem::take(&mut self.0)
    }
}

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}
