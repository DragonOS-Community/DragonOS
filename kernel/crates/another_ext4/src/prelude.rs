#![allow(unused)]
#![feature(error_in_core)]

extern crate alloc;

pub(crate) use alloc::borrow::ToOwned;
pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::collections::BTreeMap;
pub(crate) use alloc::collections::BTreeSet;
pub(crate) use alloc::collections::LinkedList;
pub(crate) use alloc::collections::VecDeque;
pub(crate) use alloc::ffi::CString;
pub(crate) use alloc::format;
pub(crate) use alloc::string::String;
pub(crate) use alloc::string::ToString;
pub(crate) use alloc::sync::Arc;
pub(crate) use alloc::sync::Weak;
pub(crate) use alloc::vec;
pub(crate) use alloc::vec::Vec;
pub(crate) use bitflags::bitflags;
pub(crate) use core::any::Any;
pub(crate) use core::ffi::CStr;
pub(crate) use core::fmt::Debug;
pub(crate) use core::mem::{self, size_of};
pub(crate) use core::ptr;

pub(crate) use log::{debug, info, trace, warn};

pub(crate) use crate::error::*;
pub(crate) type Result<T> = core::result::Result<T, Ext4Error>;

pub(crate) type LBlockId = u32;
pub(crate) type PBlockId = u64;
pub(crate) type InodeId = u32;
pub(crate) type BlockGroupId = u32;
