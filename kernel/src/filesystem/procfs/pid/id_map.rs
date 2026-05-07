//! /proc/[pid]/uid_map, /proc/[pid]/gid_map, /proc/[pid]/setgroups
//!
//! 实现 User Namespace 的 UID/GID 映射和 setgroups 控制文件。

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::find_process_by_vpid,
            template::{Builder, FileOps, ProcFileBuilder},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::user_namespace::{
            map_id_range_down, UidGidExtent, UidGidMap, UID_GID_MAP_MAX_BASE_EXTENTS,
            UID_GID_MAP_MAX_EXTENTS, USERNS_SETGROUPS_ALLOWED,
        },
        RawPid,
    },
};
use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};
use system_error::SystemError;

/// 映射文件类型
#[derive(Debug, Clone, Copy)]
enum MapType {
    Uid,
    Gid,
}

/// /proc/[pid]/uid_map 和 /proc/[pid]/gid_map 的 FileOps
#[derive(Debug)]
pub struct IdMapFileOps {
    pid: RawPid,
    map_type: MapType,
}

impl IdMapFileOps {
    pub fn new_uid(pid: RawPid) -> Self {
        Self {
            pid,
            map_type: MapType::Uid,
        }
    }

    pub fn new_gid(pid: RawPid) -> Self {
        Self {
            pid,
            map_type: MapType::Gid,
        }
    }

    pub fn new_uid_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new_uid(pid), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    pub fn new_gid_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new_gid(pid), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    fn get_user_ns(
        &self,
    ) -> Result<Arc<crate::process::namespace::user_namespace::UserNamespace>, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;
        Ok(pcb.cred().user_ns.clone())
    }

    fn generate_content(
        &self,
        map: &UidGidMap,
        user_ns: &crate::process::namespace::user_namespace::UserNamespace,
    ) -> String {
        let nr = map.get_nr_extents() as usize;
        if nr == 0 {
            return String::new();
        }

        let mut output = String::new();
        let extents = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
            &map.extent[..nr]
        } else {
            map.forward.as_deref().unwrap_or(&[])
        };

        for e in extents {
            // lower_first 显示为对读者可见的值
            let visible_lower = if let Some(parent) = user_ns.parent_ns() {
                let parent_inner = parent.inner.lock();
                let parent_map = match self.map_type {
                    MapType::Uid => &parent_inner.uid_map,
                    MapType::Gid => &parent_inner.gid_map,
                };
                crate::process::namespace::user_namespace::map_id_up(parent_map, e.lower_first)
                    .unwrap_or(e.lower_first)
            } else {
                e.lower_first
            };
            output.push_str(&format!(
                "{:10} {:10} {:10}\n",
                e.first, visible_lower, e.count
            ));
        }
        output
    }

    fn write_map(
        &self,
        buf: &[u8],
        map: &mut UidGidMap,
        parent_map: &UidGidMap,
        cap_setid: CAPFlags,
        user_ns: &Arc<crate::process::namespace::user_namespace::UserNamespace>,
    ) -> Result<usize, SystemError> {
        // 1. 只写一次检查
        if map.is_written() {
            return Err(SystemError::EPERM);
        }

        // 2. 权限检查：需要 CAP_SYS_ADMIN 在目标 ns
        if !ns_capable(user_ns, CAPFlags::CAP_SYS_ADMIN) {
            return Err(SystemError::EPERM);
        }

        // 3. 解析输入
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let mut new_extents: Vec<UidGidExtent> = Vec::new();

        for line in input.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(SystemError::EINVAL);
            }

            let first = parts[0].parse::<u32>().map_err(|_| SystemError::EINVAL)?;
            let lower_first = parts[1].parse::<u32>().map_err(|_| SystemError::EINVAL)?;
            let count = parts[2].parse::<u32>().map_err(|_| SystemError::EINVAL)?;

            if first == u32::MAX || lower_first == u32::MAX || count == 0 {
                return Err(SystemError::EINVAL);
            }

            // 检查溢出
            if first.checked_add(count).is_none() || lower_first.checked_add(count).is_none() {
                return Err(SystemError::EINVAL);
            }

            // 检查 overlap
            for e in &new_extents {
                if (first >= e.first && first < e.first + e.count)
                    || (e.first >= first && e.first < first + count)
                {
                    return Err(SystemError::EINVAL);
                }
            }

            new_extents.push(UidGidExtent {
                first,
                lower_first,
                count,
            });
        }

        if new_extents.is_empty() {
            return Ok(buf.len());
        }

        if new_extents.len() > UID_GID_MAP_MAX_EXTENTS {
            return Err(SystemError::EINVAL);
        }

        // 4. 验证 parent map
        for e in &new_extents {
            if map_id_range_down(parent_map, e.lower_first, e.count).is_none() {
                return Err(SystemError::EPERM);
            }
        }

        // 5. 对非特权的 multi-extent 映射，需要 parent ns 的 CAP_SETUID/CAP_SETGID
        if let Some(parent_ns) = user_ns.parent_ns() {
            if !ns_capable(&parent_ns, cap_setid) {
                return Err(SystemError::EPERM);
            }
        }

        // 5. 安装映射
        let nr = new_extents.len();
        for (i, e) in new_extents.iter().enumerate() {
            if i < UID_GID_MAP_MAX_BASE_EXTENTS {
                map.extent[i] = *e;
            }
        }

        if nr > UID_GID_MAP_MAX_BASE_EXTENTS {
            // 排序并分配 forward/reverse 数组
            let mut forward = new_extents.clone();
            forward.sort_by_key(|a| a.first);
            let mut reverse = new_extents.clone();
            reverse.sort_by_key(|a| a.lower_first);
            map.forward = Some(forward);
            map.reverse = Some(reverse);
        }

        map.nr_extents.store(nr as u32, Ordering::Release);
        Ok(buf.len())
    }
}

impl FileOps for IdMapFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;
        let user_ns = pcb.cred().user_ns.clone();
        let inner = user_ns.inner.lock();

        let content = match self.map_type {
            MapType::Uid => self.generate_content(&inner.uid_map, &user_ns),
            MapType::Gid => self.generate_content(&inner.gid_map, &user_ns),
        };

        let content_bytes = content.as_bytes();
        if offset >= content_bytes.len() {
            return Ok(0);
        }
        let end = (offset + len).min(content_bytes.len());
        let to_copy = end - offset;
        buf[..to_copy].copy_from_slice(&content_bytes[offset..end]);
        Ok(to_copy)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;
        let user_ns = pcb.cred().user_ns.clone();
        let mut inner = user_ns.inner.lock();

        match self.map_type {
            MapType::Uid => {
                let parent_map = if let Some(ref parent) = user_ns.parent {
                    if let Some(parent_ns) = parent.upgrade() {
                        parent_ns.inner.lock().uid_map.clone()
                    } else {
                        return Err(SystemError::EPERM);
                    }
                } else {
                    return Err(SystemError::EPERM);
                };
                self.write_map(
                    buf,
                    &mut inner.uid_map,
                    &parent_map,
                    CAPFlags::CAP_SETUID,
                    &user_ns,
                )
            }
            MapType::Gid => {
                let parent_map = if let Some(ref parent) = user_ns.parent {
                    if let Some(parent_ns) = parent.upgrade() {
                        parent_ns.inner.lock().gid_map.clone()
                    } else {
                        return Err(SystemError::EPERM);
                    }
                } else {
                    return Err(SystemError::EPERM);
                };
                self.write_map(
                    buf,
                    &mut inner.gid_map,
                    &parent_map,
                    CAPFlags::CAP_SETGID,
                    &user_ns,
                )
            }
        }
    }
}

/// /proc/[pid]/setgroups 的 FileOps
#[derive(Debug)]
pub struct SetgroupsFileOps {
    pid: RawPid,
}

impl SetgroupsFileOps {
    pub fn new(pid: RawPid) -> Self {
        Self { pid }
    }

    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(pid), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for SetgroupsFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;
        let user_ns = pcb.cred().user_ns.clone();
        let inner = user_ns.inner.lock();

        let content = if (inner.flags & USERNS_SETGROUPS_ALLOWED) != 0 {
            "allow\n"
        } else {
            "deny\n"
        };

        let content_bytes = content.as_bytes();
        if offset >= content_bytes.len() {
            return Ok(0);
        }
        let end = (offset + len).min(content_bytes.len());
        let to_copy = end - offset;
        buf[..to_copy].copy_from_slice(&content_bytes[offset..end]);
        Ok(to_copy)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;
        let user_ns = pcb.cred().user_ns.clone();
        let mut inner = user_ns.inner.lock();

        // 只能写入 "allow" 或 "deny"
        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let input = input.trim();

        match input {
            "allow" => {
                // 只能允许已经允许的情况（不能重新启用）
                if (inner.flags & USERNS_SETGROUPS_ALLOWED) == 0 {
                    return Err(SystemError::EPERM);
                }
            }
            "deny" => {
                // 只能在 gid_map 未写入时拒绝
                if inner.gid_map.is_written() {
                    return Err(SystemError::EPERM);
                }
                inner.flags &= !USERNS_SETGROUPS_ALLOWED;
            }
            _ => return Err(SystemError::EINVAL),
        }

        Ok(buf.len())
    }
}

// UidGidMap 需要实现 Clone 以支持 parent_map 的复制
impl Clone for UidGidMap {
    fn clone(&self) -> Self {
        Self {
            nr_extents: AtomicU32::new(self.get_nr_extents()),
            extent: self.extent,
            forward: self.forward.clone(),
            reverse: self.reverse.clone(),
        }
    }
}
