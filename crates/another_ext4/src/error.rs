// SPDX-License-Identifier: MPL-2.0
extern crate alloc;

use crate::prelude::*;

/// Ext4Error number.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    /// Operation not permitted.
    EPERM = 1,
    /// No such file or directory.
    ENOENT = 2,
    /// I/O error.
    EIO = 5,
    /// No such device or address.
    ENXIO = 6,
    /// Argument list too long.
    E2BIG = 7,
    /// Out of memory.
    ENOMEM = 12,
    /// Permission denied.
    EACCES = 13,
    /// Bad address.
    EFAULT = 14,
    /// File exists.
    EEXIST = 17,
    /// No such device.
    ENODEV = 19,
    /// Not a directory.
    ENOTDIR = 20,
    /// Is a directory.
    EISDIR = 21,
    /// Invalid argument.
    EINVAL = 22,
    /// File too large.
    EFBIG = 27,
    /// No space left on device.
    ENOSPC = 28,
    /// Read-only file system.
    EROFS = 30,
    /// Too many links.
    EMLINK = 31,
    /// Math result not representable.
    ERANGE = 34,
    /// Directory not empty.
    ENOTEMPTY = 39,
    /// No data available.
    ENODATA = 61,
    /// Not supported.
    ENOTSUP = 95,
    /// Link failed.
    ELINKFAIL = 97,
    /// Inode alloc failed.
    EALLOCFAIL = 98,
}

/// error used in this crate
pub struct Ext4Error {
    code: ErrCode,
    message: Option<String>,
}

impl Debug for Ext4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(message) = &self.message {
            write!(
                f,
                "Ext4Error {{ code: {:?}, message: {:?} }}",
                self.code, message
            )
        } else {
            write!(f, "Ext4Error {{ code: {:?} }}", self.code)
        }
    }
}

impl Ext4Error {
    pub const fn new(code: ErrCode) -> Self {
        Ext4Error {
            code,
            message: None,
        }
    }

    pub const fn with_message(code: ErrCode, message: String) -> Self {
        Ext4Error {
            code,
            message: Some(message),
        }
    }

    pub const fn code(&self) -> ErrCode {
        self.code
    }
}

#[macro_export]
macro_rules! format_error {
    ($code: expr, $message: expr) => {
        $crate::error::Ext4Error::with_message($code, format!($message))
    };
    ($code: expr, $fmt: expr,  $($args:tt)*) => {
        $crate::error::Ext4Error::with_message($code, format!($fmt, $($args)*))
    };
}

#[macro_export]
macro_rules! return_error {
    ($code: expr, $message: expr) => {
        return Err($crate::format_error!($code, $message));
    };
    ($code: expr, $fmt: expr,  $($args:tt)*) => {
        return Err($crate::format_error!($code, $fmt, $($args)*));
    }
}
