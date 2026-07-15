//! /proc/sys/fs - filesystem-wide kernel parameters.

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::namespace::mnt::{mount_max, set_mount_max},
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

#[derive(Debug)]
pub struct FsDirOps;

impl FsDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for FsDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "mount-max" {
            return Err(SystemError::ENOENT);
        }
        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }
        let inode = MountMaxFileOps::new_inode(dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("mount-max".to_string())
            .or_insert_with(|| MountMaxFileOps::new_inode(dir.self_ref_weak().clone()));
    }
}

#[derive(Debug)]
struct MountMaxFileOps;

impl MountMaxFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountMaxFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Numeric proc sysctls return EOF after the first read, regardless of
        // where that non-zero offset falls in their textual representation.
        if offset != 0 {
            return Ok(0);
        }
        let content = format!("{}\n", mount_max());
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Linux strict numeric sysctls consume non-zero-offset writes without
        // changing the value.
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_mount_max(buf)?;
        if !(1..=i32::MAX as i64).contains(&value) {
            return Err(SystemError::EINVAL);
        }
        set_mount_max(value as u32)?;
        Ok(consumed)
    }
}

/// Parse one Linux numeric-sysctl token. Linux consumes leading whitespace,
/// one base-0 signed integer and the whitespace immediately following it. A
/// later token is left for a short write rather than rejecting the first one.
fn parse_mount_max(buf: &[u8]) -> Result<(i64, usize), SystemError> {
    let mut cursor = 0;
    while cursor < buf.len() && buf[cursor].is_ascii_whitespace() {
        cursor += 1;
    }

    let token_start = cursor;
    let negative = if buf.get(cursor) == Some(&b'-') {
        cursor += 1;
        true
    } else {
        false
    };
    let digits_start = cursor;
    let (radix, prefix_len) = if buf.get(cursor) == Some(&b'0')
        && matches!(buf.get(cursor + 1), Some(b'x' | b'X'))
        && buf
            .get(cursor + 2)
            .is_some_and(|byte| byte.is_ascii_hexdigit())
    {
        (16u32, 2usize)
    } else if buf.get(cursor) == Some(&b'0') {
        (8u32, 0usize)
    } else {
        (10u32, 0usize)
    };
    cursor += prefix_len;
    let number_start = cursor;
    let mut magnitude = 0u64;
    while let Some(byte) = buf.get(cursor) {
        let Some(digit) = (*byte as char).to_digit(radix) else {
            break;
        };
        magnitude = magnitude
            .checked_mul(radix as u64)
            .and_then(|value| value.checked_add(digit as u64))
            .ok_or(SystemError::EINVAL)?;
        cursor += 1;
    }
    if cursor == number_start || (prefix_len == 0 && cursor == digits_start) {
        return Err(SystemError::EINVAL);
    }
    // Linux proc_get_long() uses TMPBUFLEN=22 and rejects a parsed token
    // whose sign/prefix/digits reach TMPBUFLEN-1 bytes.
    if cursor - token_start >= 21 {
        return Err(SystemError::EINVAL);
    }
    if let Some(byte) = buf.get(cursor) {
        if !matches!(byte, b' ' | b'\t' | b'\n') {
            return Err(SystemError::EINVAL);
        }
    }

    let magnitude = i64::try_from(magnitude).map_err(|_| SystemError::EINVAL)?;
    let value = if negative { -magnitude } else { magnitude };
    while cursor < buf.len() && buf[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    Ok((value, cursor))
}
