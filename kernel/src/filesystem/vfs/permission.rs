//! VFS 权限检查基础设施
//!
//! 本模块为 DragonOS VFS 层提供 UNIX DAC（自主访问控制）权限检查功能，
//! 遵循 Linux 内核语义。

use super::Metadata;
use crate::{
    filesystem::vfs::{mount::MountFS, FileType, InodeMode},
    libs::casting::DowncastArc,
    process::cred::{CAPFlags, Cred},
    process::namespace::user_namespace::{map_id_up, UserNamespace, INIT_USER_NAMESPACE},
    process::ProcessManager,
};
use alloc::sync::Arc;
use system_error::SystemError;

use super::{FsPermissionPolicy, IndexNode};

bitflags! {
    pub struct PermissionMask: u32 {
        /// 测试执行权限
        const MAY_EXEC = 0x1;
        /// 测试写权限
        const MAY_WRITE = 0x2;
        /// 测试读权限
        const MAY_READ = 0x4;
        /// 测试追加权限
        const MAY_APPEND = 0x8;
        /// access() 系统调用使用
        const MAY_ACCESS = 0x10;
        /// 打开文件操作
        const MAY_OPEN = 0x20;
        /// 测试 chdir 操作（用于审计/LSM）
        const MAY_CHDIR = 0x40;

        const MAY_RWX = Self::MAY_READ.bits + Self::MAY_WRITE.bits + Self::MAY_EXEC.bits;
    }
}

pub struct ChildInodeInit {
    pub uid: usize,
    pub gid: usize,
    pub mode: InodeMode,
}

/// Explicit identity for a backing-inode operation. No mount/inode lock is
/// retained here; namespace maps may be written after this snapshot is taken.
#[derive(Clone)]
pub struct InodeOpContext {
    pub cred: Option<Arc<Cred>>,
    pub idmap: Option<Arc<super::mount::idmap::MountIdmap>>,
    pub fs_userns: Arc<UserNamespace>,
}

impl InodeOpContext {
    pub fn legacy() -> Self {
        Self {
            cred: ProcessManager::initialized().then(|| ProcessManager::current_pcb().cred()),
            idmap: None,
            fs_userns: INIT_USER_NAMESPACE.clone(),
        }
    }

    pub fn is_idmapped(&self) -> bool {
        self.idmap.as_ref().is_some_and(|idmap| {
            !Arc::ptr_eq(idmap.owner(), &INIT_USER_NAMESPACE)
                && !Arc::ptr_eq(idmap.owner(), &self.fs_userns)
        })
    }

    pub fn view_metadata(&self, raw: &Metadata) -> Metadata {
        let mut view = raw.clone();
        if let Some(idmap) = &self.idmap {
            view.uid = idmap
                .uid_into_view(&self.fs_userns, raw.uid)
                .unwrap_or(u32::MAX as usize);
            view.gid = idmap
                .gid_into_view(&self.fs_userns, raw.gid)
                .unwrap_or(u32::MAX as usize);
        }
        view
    }

    fn raw_uid(&self, uid: usize) -> Option<usize> {
        match &self.idmap {
            Some(idmap) => idmap.uid_from_view(&self.fs_userns, uid),
            None => super::mount::idmap::identity_id(uid),
        }
    }

    fn raw_gid(&self, gid: usize) -> Option<usize> {
        match &self.idmap {
            Some(idmap) => idmap.gid_from_view(&self.fs_userns, gid),
            None => super::mount::idmap::identity_id(gid),
        }
    }

    /// Linux notify_change validates new IDs before unrepaired old IDs.
    pub fn validate_setattr_mapping(
        &self,
        current: &Metadata,
        requested: &Metadata,
        mask: super::SetMetadataMask,
    ) -> Result<(), SystemError> {
        if mask.is_empty() {
            return Ok(());
        }
        if mask.contains(super::SetMetadataMask::UID) {
            self.raw_uid(requested.uid).ok_or(SystemError::EOVERFLOW)?;
        }
        if mask.contains(super::SetMetadataMask::GID) {
            self.raw_gid(requested.gid).ok_or(SystemError::EOVERFLOW)?;
        }
        if !mask.contains(super::SetMetadataMask::UID) && current.uid == u32::MAX as usize {
            return Err(SystemError::EOVERFLOW);
        }
        if !mask.contains(super::SetMetadataMask::GID) && current.gid == u32::MAX as usize {
            return Err(SystemError::EOVERFLOW);
        }
        Ok(())
    }
}

impl InodeOpContext {
    pub fn validate_size_mapping(&self, current: &Metadata) -> Result<(), SystemError> {
        if current.uid == u32::MAX as usize || current.gid == u32::MAX as usize {
            return Err(SystemError::EOVERFLOW);
        }
        Ok(())
    }

    pub fn backing_metadata_request(
        &self,
        raw: &Metadata,
        requested: &Metadata,
        mask: super::SetMetadataMask,
    ) -> Result<Metadata, SystemError> {
        self.validate_setattr_mapping(&self.view_metadata(raw), requested, mask)?;
        let mut translated = requested.clone();
        if mask.contains(super::SetMetadataMask::UID) {
            translated.uid = self.raw_uid(requested.uid).ok_or(SystemError::EOVERFLOW)?;
        }
        if mask.contains(super::SetMetadataMask::GID) {
            translated.gid = self.raw_gid(requested.gid).ok_or(SystemError::EOVERFLOW)?;
        }
        let mut result = raw.clone();
        super::merge_metadata_masked(&mut result, &translated, mask);
        Ok(result)
    }
}

/// Called under the backend's parent mutation lock, before removing a child.
pub fn check_inode_delete(
    parent: &Metadata,
    victim: &Metadata,
    directory: bool,
    context: &InodeOpContext,
) -> Result<(), SystemError> {
    let victim = context.view_metadata(victim);
    let parent = context.view_metadata(parent);
    if victim.uid == u32::MAX as usize || victim.gid == u32::MAX as usize {
        return Err(SystemError::EOVERFLOW);
    }
    if parent.flags.contains(super::InodeFlags::S_IMMUTABLE) {
        return Err(SystemError::EPERM);
    }
    if let Some(cred) = &context.cred {
        cred.inode_permission(
            &parent,
            (PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC).bits(),
        )?;
        if parent.flags.contains(super::InodeFlags::S_APPEND) {
            return Err(SystemError::EPERM);
        }
        if parent.mode.contains(InodeMode::S_ISVTX)
            && cred.fsuid.data() != victim.uid
            && cred.fsuid.data() != parent.uid
            && !cred.has_capability_wrt_inode_uidgid(&victim, CAPFlags::CAP_FOWNER)
        {
            return Err(SystemError::EPERM);
        }
    } else if context.is_idmapped() {
        return Err(SystemError::EINVAL);
    }
    if victim
        .flags
        .intersects(super::InodeFlags::S_APPEND | super::InodeFlags::S_IMMUTABLE)
    {
        return Err(SystemError::EPERM);
    }
    if directory && victim.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if !directory && victim.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }
    if parent.nlinks == 0 {
        return Err(SystemError::ENOENT);
    }
    Ok(())
}

pub fn check_inode_link_source(
    raw: &Metadata,
    context: &InodeOpContext,
) -> Result<(), SystemError> {
    let view = context.view_metadata(raw);
    if view.uid == u32::MAX as usize
        || view.gid == u32::MAX as usize
        || view
            .flags
            .intersects(super::InodeFlags::S_IMMUTABLE | super::InodeFlags::S_APPEND)
    {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

/// Called under the backend's parent mutation lock, before publishing a child.
pub fn check_parent_create(
    parent: &Metadata,
    context: &InodeOpContext,
    allow_unlinked_parent: bool,
) -> Result<(), SystemError> {
    if parent.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if !allow_unlinked_parent && parent.nlinks == 0 {
        return Err(SystemError::ENOENT);
    }
    let Some(cred) = &context.cred else {
        return if context.is_idmapped() {
            Err(SystemError::EINVAL)
        } else {
            Ok(())
        };
    };
    // Linux may_create checks both caller IDs even when SGID will inherit GID.
    context
        .raw_uid(cred.fsuid.data())
        .ok_or(SystemError::EOVERFLOW)?;
    context
        .raw_gid(cred.fsgid.data())
        .ok_or(SystemError::EOVERFLOW)?;
    if parent.flags.contains(super::InodeFlags::S_IMMUTABLE) {
        return Err(SystemError::EPERM);
    }
    cred.inode_permission(
        &context.view_metadata(parent),
        (PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC).bits(),
    )
}

#[cfg(test)]
mod idmap_tests {
    use super::*;
    use crate::process::{
        cred::{Kgid, Kuid, INIT_CRED},
        namespace::user_namespace::{UidGidExtent, UidGidMap},
    };
    use core::sync::atomic::Ordering;

    fn context() -> InodeOpContext {
        let owner = UserNamespace::create_user_ns(&INIT_CRED).unwrap();
        let extent = |lower_first| {
            let mut map = UidGidMap::default();
            map.extent[0] = UidGidExtent {
                first: 0,
                lower_first,
                count: 100,
            };
            map.nr_extents.store(1, Ordering::Release);
            map
        };
        {
            let mut inner = owner.inner.lock();
            inner.uid_map = extent(1000);
            inner.gid_map = extent(2000);
        }
        let mut cred = (**INIT_CRED).clone();
        cred.fsuid = Kuid::new(1003);
        cred.fsgid = Kgid::new(2004);
        cred.groups.clear();
        cred.cap_effective = CAPFlags::CAP_EMPTY_SET;
        InodeOpContext {
            cred: Some(Arc::new(cred)),
            idmap: Some(Arc::new(super::super::mount::idmap::MountIdmap::new(owner))),
            fs_userns: INIT_USER_NAMESPACE.clone(),
        }
    }

    fn parent() -> Metadata {
        Metadata {
            file_type: FileType::Dir,
            mode: InodeMode::S_IRWXU,
            uid: 3,
            gid: 7,
            nlinks: 2,
            ..Metadata::default()
        }
    }

    #[test]
    fn setattr_mapping_repairs_only_explicitly_selected_owner_fields() {
        let context = context();
        let mut raw = parent();
        raw.uid = 999;
        raw.gid = 999;
        let view = context.view_metadata(&raw);
        let mut requested = view.clone();
        requested.uid = 1003;
        requested.gid = 2004;
        assert_eq!(
            context.validate_setattr_mapping(&view, &requested, super::super::SetMetadataMask::UID),
            Err(SystemError::EOVERFLOW)
        );
        assert!(context
            .validate_setattr_mapping(
                &view,
                &requested,
                super::super::SetMetadataMask::UID | super::super::SetMetadataMask::GID
            )
            .is_ok());
        assert_eq!(
            context.validate_setattr_mapping(
                &view,
                &requested,
                super::super::SetMetadataMask::MODE
            ),
            Err(SystemError::EOVERFLOW)
        );
        assert!(context
            .validate_setattr_mapping(&view, &requested, super::super::SetMetadataMask::empty())
            .is_ok());
    }

    #[test]
    fn masked_translation_does_not_store_unselected_view_owners() {
        let context = context();
        let raw = parent();
        let mut requested = context.view_metadata(&raw);
        requested.uid = 9000;
        requested.gid = 9001;
        requested.mode = InodeMode::S_IRUSR;
        let backing = context
            .backing_metadata_request(&raw, &requested, super::super::SetMetadataMask::MODE)
            .unwrap();
        assert_eq!((backing.uid, backing.gid), (raw.uid, raw.gid));
        assert_eq!(backing.mode, requested.mode);
    }

    #[test]
    fn resize_returns_mode_changes_derived_from_current_not_prepared_snapshot() {
        let context = context();
        let mut current = parent();
        current.file_type = FileType::File;
        current.mode = InodeMode::S_IRUSR | InodeMode::S_ISUID;
        let mut stale = context.view_metadata(&current);
        stale.mode = InodeMode::S_IWUSR;
        let intent =
            super::super::SetMetadataMask::WRITE_SIDE_EFFECT | super::super::SetMetadataMask::CTIME;
        let (backing, actual) = super::super::vcore::prepare_backing_resize_metadata(
            &context, &current, &stale, intent, 32,
        )
        .unwrap();
        assert!(actual.contains(super::super::SetMetadataMask::MODE));
        assert_eq!(backing.mode, InodeMode::S_IRUSR);
        assert_eq!((backing.uid, backing.gid), (current.uid, current.gid));
        current.mode = InodeMode::S_IRUSR;
        let (_, actual) = super::super::vcore::prepare_backing_resize_metadata(
            &context,
            &current,
            &stale,
            intent | super::super::SetMetadataMask::MODE,
            32,
        )
        .unwrap();
        assert!(!actual.contains(super::super::SetMetadataMask::MODE));
    }

    #[test]
    fn sticky_delete_uses_view_owner_and_rejects_holes_before_parent_flags() {
        let context = context();
        let mut parent = parent();
        parent.uid = 8;
        parent.mode = InodeMode::S_IRWXUGO | InodeMode::S_ISVTX;
        let mut victim = Metadata {
            file_type: FileType::File,
            uid: 3,
            gid: 4,
            ..Metadata::default()
        };
        assert!(check_inode_delete(&parent, &victim, false, &context).is_ok());
        victim.uid = 9;
        assert_eq!(
            check_inode_delete(&parent, &victim, false, &context),
            Err(SystemError::EPERM)
        );
        parent.flags.insert(super::super::InodeFlags::S_IMMUTABLE);
        victim.uid = 999;
        assert_eq!(
            check_inode_delete(&parent, &victim, false, &context),
            Err(SystemError::EOVERFLOW)
        );
        victim.uid = 3;
        assert_eq!(
            check_inode_delete(&parent, &victim, false, &context),
            Err(SystemError::EPERM)
        );
    }

    #[test]
    fn mapped_parent_dac_and_child_ownership_use_different_id_spaces() {
        let context = context();
        let parent = parent();
        assert!(context
            .cred
            .as_ref()
            .unwrap()
            .inode_permission(
                &parent,
                (PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC).bits()
            )
            .is_err());
        assert!(check_parent_create(&parent, &context, false).is_ok());
        let child =
            child_inode_init_with_context(&parent, FileType::File, InodeMode::S_IRUSR, &context)
                .unwrap();
        assert_eq!((child.uid, child.gid), (3, 4));
    }

    #[test]
    fn sgid_inherits_raw_group_but_validates_callers_group_mapping() {
        let mut context = context();
        let mut parent = parent();
        parent.mode.insert(InodeMode::S_ISGID);
        let child =
            child_inode_init_with_context(&parent, FileType::Dir, InodeMode::S_IRWXU, &context)
                .unwrap();
        assert_eq!(child.gid, 7);
        assert!(child.mode.contains(InodeMode::S_ISGID));
        let executable = child_inode_init_with_context(
            &parent,
            FileType::File,
            InodeMode::S_ISGID | InodeMode::S_IXGRP,
            &context,
        )
        .unwrap();
        assert!(!executable.mode.contains(InodeMode::S_ISGID));
        let mut cred = (**context.cred.as_ref().unwrap()).clone();
        cred.fsgid = Kgid::new(9000);
        context.cred = Some(Arc::new(cred));
        assert_eq!(
            check_parent_create(&parent, &context, false),
            Err(SystemError::EOVERFLOW)
        );
    }

    #[test]
    fn unlinked_parent_is_only_allowed_for_tmpfile_and_unmapped_view_cannot_write() {
        let context = context();
        let mut parent = parent();
        parent.nlinks = 0;
        assert_eq!(
            check_parent_create(&parent, &context, false),
            Err(SystemError::ENOENT)
        );
        assert!(check_parent_create(&parent, &context, true).is_ok());
        parent.nlinks = 2;
        parent.uid = 500;
        assert_eq!(
            check_parent_create(&parent, &context, false),
            Err(SystemError::EACCES)
        );
    }
}

pub fn child_inode_init_with_context(
    parent: &Metadata,
    file_type: FileType,
    mut mode: InodeMode,
    context: &InodeOpContext,
) -> Result<ChildInodeInit, SystemError> {
    let Some(cred) = &context.cred else {
        if context.is_idmapped() {
            return Err(SystemError::EINVAL);
        }
        let gid = if parent.mode.contains(InodeMode::S_ISGID) {
            if file_type == FileType::Dir {
                mode.insert(InodeMode::S_ISGID);
            }
            parent.gid
        } else {
            0
        };
        return Ok(ChildInodeInit { uid: 0, gid, mode });
    };
    let uid = context
        .raw_uid(cred.fsuid.data())
        .ok_or(SystemError::EOVERFLOW)?;
    let caller_gid = context
        .raw_gid(cred.fsgid.data())
        .ok_or(SystemError::EOVERFLOW)?;
    let parent_sgid = parent.mode.contains(InodeMode::S_ISGID);
    let gid = if parent_sgid {
        if file_type == FileType::Dir {
            mode.insert(InodeMode::S_ISGID);
        }
        parent.gid
    } else {
        caller_gid
    };
    let view = context.view_metadata(parent);
    let in_group =
        cred.fsgid.data() == view.gid || cred.groups.iter().any(|group| group.data() == view.gid);
    // Linux mode_strip_sgid uses the parent directory's mapped IDs.
    if file_type != FileType::Dir
        && parent_sgid
        && mode.contains(InodeMode::S_ISGID | InodeMode::S_IXGRP)
        && !in_group
        && !cred.has_capability_wrt_inode_uidgid(&view, CAPFlags::CAP_FSETID)
    {
        mode.remove(InodeMode::S_ISGID);
    }
    Ok(ChildInodeInit { uid, gid, mode })
}

/// Compute owner and directory-SGID inheritance before publishing a child.
pub fn child_inode_init(parent: &Metadata, file_type: FileType, mode: InodeMode) -> ChildInodeInit {
    child_inode_init_with_context(parent, file_type, mode, &InodeOpContext::legacy())
        .expect("identity credentials must contain valid filesystem IDs")
}

/// VFS permission check wrapper that respects per-filesystem policy.
///
/// This is the single entry point that should be used by VFS/pathwalk/syscalls
/// when deciding whether to apply local Unix DAC checks.
///
/// Linux FUSE remote permission model:
/// - Without `default_permissions`, the kernel bypasses most DAC checks and
///   lets the userspace daemon decide.
/// - Execute permission is still checked locally for regular files.
pub fn check_inode_permission(
    inode: &Arc<dyn IndexNode>,
    metadata: &Metadata,
    mask: PermissionMask,
) -> Result<(), SystemError> {
    // Match Linux sb_permission(): a read-only mount rejects write access to
    // filesystem objects before DAC/capability overrides are considered.
    if mask.contains(PermissionMask::MAY_WRITE)
        && matches!(
            metadata.file_type,
            FileType::File | FileType::Dir | FileType::SymLink
        )
        && inode
            .try_fs()
            .and_then(|fs| fs.downcast_arc::<MountFS>())
            .is_some_and(|mount| mount.is_readonly())
    {
        return Err(SystemError::EROFS);
    }
    if mask.contains(PermissionMask::MAY_WRITE)
        && metadata.flags.contains(super::InodeFlags::S_IMMUTABLE)
    {
        return Err(SystemError::EPERM);
    }
    // Linux proc_sys_permission does not let root or CAP_DAC_OVERRIDE bypass
    // a read-only sysctl entry. Check before generic DAC capability overrides.
    if mask.contains(PermissionMask::MAY_WRITE)
        && metadata
            .flags
            .contains(super::InodeFlags::S_SYSCTL_READONLY)
    {
        return Err(SystemError::EACCES);
    }

    let cred = ProcessManager::current_pcb().cred();
    if metadata.flags.contains(super::InodeFlags::S_PROC_SYSCTL) {
        // Linux proc_sys_permission uses global euid/egid and does not grant
        // CAP_DAC_OVERRIDE a bypass (including caps in a child userns).
        let mode = metadata.mode.bits();
        let allowed = if cred.euid.data() == 0 {
            (mode >> 6) & 7
        } else if cred.egid.data() == 0 || cred.groups.iter().any(|gid| gid.data() == 0) {
            (mode >> 3) & 7
        } else {
            mode & 7
        };
        if mask.bits() & PermissionMask::MAY_RWX.bits() & !allowed != 0 {
            return Err(SystemError::EACCES);
        }
        return check_device_inode_permission(metadata, mask);
    }
    match inode.try_fs().map(|fs| fs.permission_policy()) {
        None | Some(FsPermissionPolicy::Dac) => inode.check_dac_permission(metadata, mask),
        Some(FsPermissionPolicy::Remote) => {
            if mask.contains(PermissionMask::MAY_EXEC)
                && metadata.file_type == FileType::File
                && (metadata.mode.bits() & InodeMode::S_IXUGO.bits()) == 0
            {
                return Err(SystemError::EACCES);
            }
            Ok(())
        }
    }?;
    check_device_inode_permission(metadata, mask)
}

/// Device BPF is an additional VFS permission layer, independent of DAC and
/// the filesystem's remote-permission policy. This also runs for F_OK, where
/// Linux passes an access type with no READ/WRITE bits.
pub(crate) fn check_device_inode_permission(
    metadata: &Metadata,
    mask: PermissionMask,
) -> Result<(), SystemError> {
    if !matches!(
        metadata.file_type,
        FileType::CharDevice | FileType::BlockDevice
    ) || metadata.raw_dev.data() == 0
    {
        return Ok(());
    }
    let mut access = 0u16;
    if mask.contains(PermissionMask::MAY_READ) {
        access |= 2;
    }
    if mask.contains(PermissionMask::MAY_WRITE) {
        access |= 4;
    }
    crate::bpf::check_device_permission(metadata.file_type, metadata.raw_dev, access)
}

/// Check whether the current task may suppress access-time updates for an inode.
///
/// Linux restricts enabling `O_NOATIME` to the inode owner or a task with
/// `CAP_FOWNER`.  This check is separate from read permission: being allowed to
/// read another user's file does not imply permission to hide that access from
/// its timestamp metadata.
pub fn check_noatime_permission(metadata: &Metadata) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    if cred.is_owner_or_capable(metadata) {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

impl Cred {
    /// 检查具有给定凭证的进程是否有权限访问 inode。
    ///
    /// 这是 DragonOS VFS 的核心权限检查函数，等价于 Linux 的
    /// `inode_permission()` + `generic_permission()`。
    ///
    /// ## 算法流程
    ///
    /// 1. 检查所有者权限（mode >> 6）
    /// 2. 如果在组内，检查组权限（mode >> 3）
    /// 3. 检查其他用户权限（mode & 7）
    /// 4. 如果被拒绝，尝试 capability 覆盖（CAP_DAC_OVERRIDE / CAP_DAC_READ_SEARCH）
    ///
    /// ## 参数
    ///
    /// - `metadata`: Inode 元数据（包含 mode、uid、gid）
    /// - `cred`: 进程凭证（fsuid、fsgid、groups、capabilities）
    /// - `mask`: 权限掩码（MAY_READ | MAY_WRITE | MAY_EXEC）
    ///
    /// ## 返回值
    ///
    /// - `Ok(())`: 权限允许
    /// - `Err(SystemError::EACCES)`: 权限拒绝
    ///
    /// ## 示例
    ///
    /// ```rust
    /// let metadata = inode.metadata()?;
    /// let cred = ProcessManager::current_pcb().cred();
    /// inode_permission(&metadata, &cred, MAY_EXEC)?;
    /// ```
    pub fn inode_permission(&self, metadata: &Metadata, mask: u32) -> Result<(), SystemError> {
        if mask & PermissionMask::MAY_WRITE.bits() != 0
            && (metadata.uid == u32::MAX as usize || metadata.gid == u32::MAX as usize)
        {
            return Err(SystemError::EACCES);
        }
        // 从 mode 中提取权限位
        let file_mode = metadata.mode.bits();

        // 确定要检查哪组权限位
        let perm = if self.is_owner(metadata) {
            // 所有者权限（第 6-8 位）
            (file_mode & InodeMode::S_IRWXU.bits()) >> 6
        } else if self.in_group(metadata) {
            // 组权限（第 3-5 位）
            (file_mode & InodeMode::S_IRWXG.bits()) >> 3
        } else {
            // 其他用户权限（第 0-2 位）
            file_mode & InodeMode::S_IRWXO.bits()
        };

        // PermissionMask 的低 3 位已经是 Unix 权限位格式 (rwx)
        let need = mask & PermissionMask::MAY_RWX.bits();

        // 检查权限位是否满足请求
        if (need & !perm) == 0 {
            return Ok(()); // 通过普通检查，权限允许
        }

        // 尝试 capability 覆盖（类似 Linux 的 capable_wrt_inode_uidgid）
        if self.try_capability_override(metadata, mask) {
            return Ok(());
        }

        // 所有检查都失败
        Err(SystemError::EACCES)
    }

    /// 检查当前进程是否以 inode 所有者的身份运行
    #[inline]
    pub fn is_owner(&self, metadata: &Metadata) -> bool {
        self.fsuid.data() == metadata.uid
    }

    /// Linux capable_wrt_inode_uidgid(): a capability in a user namespace
    /// cannot authorize changes to an inode whose owner IDs are unmapped.
    pub fn has_capability_wrt_inode_uidgid(&self, metadata: &Metadata, cap: CAPFlags) -> bool {
        if !self.has_capability(cap) {
            return false;
        }
        let (Ok(uid), Ok(gid)) = (u32::try_from(metadata.uid), u32::try_from(metadata.gid)) else {
            return false;
        };
        let inner = self.user_ns.inner.lock();
        map_id_up(&inner.uid_map, uid).is_some() && map_id_up(&inner.gid_map, gid).is_some()
    }

    /// Linux inode_owner_or_capable(): CAP_FOWNER only needs the inode UID
    /// mapped; an exact fsuid owner match needs no capability.
    pub fn is_owner_or_capable(&self, metadata: &Metadata) -> bool {
        if self.is_owner(metadata) {
            return true;
        }
        if !self.has_capability(CAPFlags::CAP_FOWNER) {
            return false;
        }
        let Ok(uid) = u32::try_from(metadata.uid) else {
            return false;
        };
        let inner = self.user_ns.inner.lock();
        map_id_up(&inner.uid_map, uid).is_some()
    }

    /// 检查进程是否在 inode 的所属组中
    fn in_group(&self, metadata: &Metadata) -> bool {
        // 检查主组
        if self.fsgid.data() == metadata.gid {
            return true;
        }
        // 检查附加组
        self.groups.iter().any(|gid| gid.data() == metadata.gid)
    }

    /// 尝试使用 capabilities 覆盖权限拒绝
    ///
    /// 实现 Linux 的 capable_wrt_inode_uidgid() 逻辑：
    /// - CAP_DAC_OVERRIDE: 绕过所有 DAC 检查
    /// - CAP_DAC_READ_SEARCH: 绕过读/搜索检查
    #[inline(never)]
    fn try_capability_override(&self, metadata: &Metadata, mask: u32) -> bool {
        // CAP_DAC_OVERRIDE: 绕过所有文件读、写和执行权限检查
        if self.has_capability_wrt_inode_uidgid(metadata, CAPFlags::CAP_DAC_OVERRIDE) {
            // Linux: CAP_DAC_OVERRIDE does not bypass execute checks for regular files
            // when no execute bit is set.
            if mask & PermissionMask::MAY_EXEC.bits() != 0
                && metadata.file_type != super::FileType::Dir
                && (metadata.mode.bits() & InodeMode::S_IXUGO.bits()) == 0
            {
                return false;
            }
            return true;
        }

        // CAP_DAC_READ_SEARCH: 绕过读和搜索（目录上的执行）检查
        if self.has_capability_wrt_inode_uidgid(metadata, CAPFlags::CAP_DAC_READ_SEARCH) {
            // 目录：只要不请求写权限，就允许 (即允许 Read 和 Exec/Search)
            if metadata.file_type == FileType::Dir {
                if (mask & PermissionMask::MAY_WRITE.bits()) == 0 {
                    return true;
                }
            } else {
                // 文件：仅允许只读权限
                let check_mask = mask
                    & (PermissionMask::MAY_READ.bits()
                        | PermissionMask::MAY_EXEC.bits()
                        | PermissionMask::MAY_WRITE.bits());
                if check_mask == PermissionMask::MAY_READ.bits() {
                    return true;
                }
            }
        }

        false
    }

    /// 检查 chdir 操作的权限
    ///
    /// ## 权限要求
    ///
    /// 目录的 chdir 操作需要执行(搜索)权限。
    ///
    /// ## MAY_CHDIR 标志说明
    ///
    /// `MAY_CHDIR` 标志主要用于语义标注和审计(audit)/LSM钩子,
    /// **不影响实际的 DAC (Discretionary Access Control) 权限检查**。
    /// 实际权限检查只依赖 `MAY_EXEC` 位。
    ///
    /// 这一设计与 Linux 内核保持一致,参见:
    /// - `fs/open.c::may_open()`
    /// - `security/security.c::security_path_chdir()`
    #[inline(never)]
    pub fn check_chdir_permission(&self, metadata: &Metadata) -> Result<(), SystemError> {
        // 验证是否为目录
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 检查执行权限(目录的搜索权限)
        // MAY_CHDIR 用于语义标注,不影响实际权限检查
        self.inode_permission(
            metadata,
            PermissionMask::MAY_EXEC.bits() | PermissionMask::MAY_CHDIR.bits(),
        )
    }
}
