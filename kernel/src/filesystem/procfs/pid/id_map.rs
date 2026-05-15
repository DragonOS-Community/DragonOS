//! /proc/[pid]/uid_map, /proc/[pid]/gid_map, /proc/[pid]/setgroups
//!
//! 实现 User Namespace 的 UID/GID 映射和 setgroups 控制文件。

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            ProcfsFilePrivateData,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{
        cred::{cap_capable, CAPFlags, Cred},
        namespace::user_namespace::{
            map_id_down, map_id_range_down, map_id_up, UidGidExtent, UidGidMap, UserNamespace,
            UID_GID_MAP_MAX_BASE_EXTENTS, UID_GID_MAP_MAX_EXTENTS, USERNS_SETGROUPS_ALLOWED,
        },
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

const PROC_ID_MAP_MAX_WRITE: usize = 4096;

/// 映射文件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapType {
    Uid,
    Gid,
}

impl MapType {
    fn parent_cap(self) -> CAPFlags {
        match self {
            MapType::Uid => CAPFlags::CAP_SETUID,
            MapType::Gid => CAPFlags::CAP_SETGID,
        }
    }

    fn is_uid(self) -> bool {
        matches!(self, MapType::Uid)
    }
}

#[derive(Clone)]
struct IdMapWriteContext {
    map_type: MapType,
    target_ns: Arc<UserNamespace>,
    opener_cred: Arc<Cred>,
    target_owner: usize,
    target_flags: u32,
    target_parent_could_setfcap: bool,
}

impl IdMapWriteContext {
    fn opener_has_cap(&self, ns: &Arc<UserNamespace>, cap: CAPFlags) -> bool {
        cap_capable(&self.opener_cred, ns, cap)
    }

    fn target_parent_ns(&self) -> Result<Arc<UserNamespace>, SystemError> {
        self.target_ns.parent_ns().ok_or(SystemError::EPERM)
    }

    fn lower_ns_for_display(&self) -> Arc<UserNamespace> {
        if Arc::ptr_eq(&self.opener_cred.user_ns, &self.target_ns) {
            self.target_ns
                .parent_ns()
                .unwrap_or_else(|| self.target_ns.clone())
        } else {
            self.opener_cred.user_ns.clone()
        }
    }
}

/// /proc/[pid]/uid_map 和 /proc/[pid]/gid_map 的 FileOps
#[derive(Debug)]
pub struct IdMapFileOps {
    target: ProcPidTarget,
    map_type: MapType,
}

impl IdMapFileOps {
    pub fn new_uid(target: ProcPidTarget) -> Self {
        Self {
            target,
            map_type: MapType::Uid,
        }
    }

    pub fn new_gid(target: ProcPidTarget) -> Self {
        Self {
            target,
            map_type: MapType::Gid,
        }
    }

    pub fn new_uid_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new_uid(target), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    pub fn new_gid_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new_gid(target), InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    fn get_user_ns(&self) -> Result<Arc<UserNamespace>, SystemError> {
        let pcb = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        Ok(pcb.cred().user_ns.clone())
    }

    fn open_cred_from_data(data: &MutexGuard<FilePrivateData>) -> Result<Arc<Cred>, SystemError> {
        let FilePrivateData::Procfs(ProcfsFilePrivateData { open_cred, .. }) = &**data else {
            return Err(SystemError::EPERM);
        };
        Ok(open_cred.clone())
    }

    fn generate_content(&self, map: &UidGidMap, ctx: &IdMapWriteContext) -> String {
        let nr = map.get_nr_extents() as usize;
        if nr == 0 {
            return String::new();
        }

        let extents = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
            &map.extent[..nr]
        } else {
            map.forward.as_deref().unwrap_or(&[])
        };

        let lower_ns = ctx.lower_ns_for_display();
        let lower_map = {
            let inner = lower_ns.inner.lock();
            match self.map_type {
                MapType::Uid => inner.uid_map.clone(),
                MapType::Gid => inner.gid_map.clone(),
            }
        };

        let mut output = String::new();
        for extent in extents {
            let visible_lower = map_id_up(&lower_map, extent.lower_first).unwrap_or(u32::MAX);
            output.push_str(&format!(
                "{:10} {:10} {:10}\n",
                extent.first, visible_lower, extent.count
            ));
        }

        output
    }

    fn parse_map(&self, buf: &[u8]) -> Result<Vec<UidGidExtent>, SystemError> {
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

            let extent = UidGidExtent {
                first: parts[0].parse::<u32>().map_err(|_| SystemError::EINVAL)?,
                lower_first: parts[1].parse::<u32>().map_err(|_| SystemError::EINVAL)?,
                count: parts[2].parse::<u32>().map_err(|_| SystemError::EINVAL)?,
            };

            self.validate_new_extent(&new_extents, &extent)?;
            new_extents.push(extent);
        }

        if new_extents.is_empty() || new_extents.len() > UID_GID_MAP_MAX_EXTENTS {
            return Err(SystemError::EINVAL);
        }

        Ok(new_extents)
    }

    fn validate_new_extent(
        &self,
        new_extents: &[UidGidExtent],
        extent: &UidGidExtent,
    ) -> Result<(), SystemError> {
        if extent.first == u32::MAX || extent.lower_first == u32::MAX || extent.count == 0 {
            return Err(SystemError::EINVAL);
        }

        let upper_last = extent
            .first
            .checked_add(extent.count - 1)
            .ok_or(SystemError::EINVAL)?;
        let lower_last = extent
            .lower_first
            .checked_add(extent.count - 1)
            .ok_or(SystemError::EINVAL)?;

        for prev in new_extents {
            let prev_upper_last = prev.first + prev.count - 1;
            let prev_lower_last = prev.lower_first + prev.count - 1;

            if prev.first <= upper_last && prev_upper_last >= extent.first {
                return Err(SystemError::EINVAL);
            }

            if prev.lower_first <= lower_last && prev_lower_last >= extent.lower_first {
                return Err(SystemError::EINVAL);
            }
        }

        Ok(())
    }

    fn verify_root_map(
        &self,
        ctx: &IdMapWriteContext,
        new_extents: &[UidGidExtent],
    ) -> Result<(), SystemError> {
        if !ctx.map_type.is_uid() {
            return Ok(());
        }

        if !new_extents.iter().any(|extent| extent.lower_first == 0) {
            return Ok(());
        }

        if Arc::ptr_eq(&ctx.opener_cred.user_ns, &ctx.target_ns) {
            if !ctx.target_parent_could_setfcap {
                return Err(SystemError::EPERM);
            }
        } else {
            let parent_ns = ctx.target_parent_ns()?;
            if !ctx.opener_has_cap(&parent_ns, CAPFlags::CAP_SETFCAP) {
                return Err(SystemError::EPERM);
            }
        }

        Ok(())
    }

    fn restricted_single_extent_permitted(
        &self,
        ctx: &IdMapWriteContext,
        new_extents: &[UidGidExtent],
    ) -> Result<bool, SystemError> {
        if new_extents.len() != 1 || new_extents[0].count != 1 {
            return Ok(false);
        }

        if ctx.target_owner != ctx.opener_cred.euid.data() {
            return Ok(false);
        }

        let parent_ns = ctx.target_parent_ns()?;
        let lower_id = new_extents[0].lower_first;

        match ctx.map_type {
            MapType::Uid => {
                let parent_uid = map_id_down(&parent_ns.inner.lock().uid_map, lower_id)
                    .ok_or(SystemError::EPERM)?;
                Ok(parent_uid as usize == ctx.opener_cred.euid.data())
            }
            MapType::Gid => {
                let parent_gid = map_id_down(&parent_ns.inner.lock().gid_map, lower_id)
                    .ok_or(SystemError::EPERM)?;
                Ok((ctx.target_flags & USERNS_SETGROUPS_ALLOWED) == 0
                    && parent_gid as usize == ctx.opener_cred.egid.data())
            }
        }
    }

    fn new_idmap_permitted(
        &self,
        ctx: &IdMapWriteContext,
        new_extents: &[UidGidExtent],
    ) -> Result<(), SystemError> {
        self.verify_root_map(ctx, new_extents)?;

        if self.restricted_single_extent_permitted(ctx, new_extents)? {
            return Ok(());
        }

        let parent_ns = ctx.target_parent_ns()?;
        let parent_cap = ctx.map_type.parent_cap();
        if ctx.opener_has_cap(&parent_ns, parent_cap) {
            return Ok(());
        }

        Err(SystemError::EPERM)
    }

    fn install_map(&self, map: &mut UidGidMap, mut extents: Vec<UidGidExtent>) {
        map.forward = None;
        map.reverse = None;
        map.extent = [UidGidExtent {
            first: 0,
            lower_first: 0,
            count: 0,
        }; UID_GID_MAP_MAX_BASE_EXTENTS];

        let nr = extents.len();
        if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
            extents.sort_by_key(|extent| extent.first);
            for (index, extent) in extents.iter().enumerate() {
                map.extent[index] = *extent;
            }
        } else {
            let mut forward = extents.clone();
            forward.sort_by_key(|extent| extent.first);
            let mut reverse = extents;
            reverse.sort_by_key(|extent| extent.lower_first);
            map.forward = Some(forward);
            map.reverse = Some(reverse);
        }

        map.nr_extents.store(nr as u32, Ordering::Release);
    }

    fn write_map(
        &self,
        offset: usize,
        buf: &[u8],
        map: &mut UidGidMap,
        parent_map: &UidGidMap,
        ctx: &IdMapWriteContext,
    ) -> Result<usize, SystemError> {
        if offset != 0 || buf.len() >= PROC_ID_MAP_MAX_WRITE {
            return Err(SystemError::EINVAL);
        }

        let mut new_extents = self.parse_map(buf)?;
        self.new_idmap_permitted(ctx, &new_extents)?;

        for extent in &mut new_extents {
            let mapped_lower = map_id_range_down(parent_map, extent.lower_first, extent.count)
                .ok_or(SystemError::EPERM)?;
            extent.lower_first = mapped_lower;
        }

        self.install_map(map, new_extents);
        Ok(buf.len())
    }
}

impl FileOps for IdMapFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let user_ns = self.get_user_ns()?;
        let opener_cred = Self::open_cred_from_data(&data)?;
        let inner = user_ns.inner.lock();
        let ctx = IdMapWriteContext {
            map_type: self.map_type,
            target_ns: user_ns.clone(),
            opener_cred,
            target_owner: inner.owner,
            target_flags: inner.flags,
            target_parent_could_setfcap: inner.parent_could_setfcap,
        };

        let content = match self.map_type {
            MapType::Uid => self.generate_content(&inner.uid_map, &ctx),
            MapType::Gid => self.generate_content(&inner.gid_map, &ctx),
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
        offset: usize,
        _len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let opener_cred = Self::open_cred_from_data(&data)?;
        let user_ns = self.get_user_ns()?;

        if !cap_capable(&opener_cred, &user_ns, CAPFlags::CAP_SYS_ADMIN) {
            return Err(SystemError::EPERM);
        }

        let mut inner = user_ns.inner.lock();
        let ctx = IdMapWriteContext {
            map_type: self.map_type,
            target_ns: user_ns.clone(),
            opener_cred,
            target_owner: inner.owner,
            target_flags: inner.flags,
            target_parent_could_setfcap: inner.parent_could_setfcap,
        };

        match self.map_type {
            MapType::Uid => {
                if inner.uid_map.is_written() {
                    return Err(SystemError::EPERM);
                }
                let parent_map = {
                    let parent_ns = ctx.target_parent_ns()?;
                    let map = parent_ns.inner.lock().uid_map.clone();
                    map
                };
                self.write_map(offset, buf, &mut inner.uid_map, &parent_map, &ctx)
            }
            MapType::Gid => {
                if inner.gid_map.is_written() {
                    return Err(SystemError::EPERM);
                }
                let parent_map = {
                    let parent_ns = ctx.target_parent_ns()?;
                    let map = parent_ns.inner.lock().gid_map.clone();
                    map
                };
                self.write_map(offset, buf, &mut inner.gid_map, &parent_map, &ctx)
            }
        }
    }
}

/// /proc/[pid]/setgroups 的 FileOps
#[derive(Debug)]
pub struct SetgroupsFileOps {
    target: ProcPidTarget,
}

impl SetgroupsFileOps {
    pub fn new(target: ProcPidTarget) -> Self {
        Self { target }
    }

    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self::new(target), InodeMode::from_bits_truncate(0o644))
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
        let pcb = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
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
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if offset != 0 || buf.len() >= PROC_ID_MAP_MAX_WRITE {
            return Err(SystemError::EINVAL);
        }

        let pcb = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        let user_ns = pcb.cred().user_ns.clone();
        let mut inner = user_ns.inner.lock();

        let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let input = input.trim();

        match input {
            "allow" => {
                if (inner.flags & USERNS_SETGROUPS_ALLOWED) == 0 {
                    return Err(SystemError::EPERM);
                }
            }
            "deny" => {
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
