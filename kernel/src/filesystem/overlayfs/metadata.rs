use super::cred::CredOverrideGuard;
use super::inode::OvlInode;
use crate::filesystem::vfs::{
    merge_metadata_masked, permission::PermissionMask, FilePrivateData, FileType, IndexNode,
    InodeFlags, InodeId, InodeMode, Metadata, SetMetadataMask, XattrFlags,
};
use crate::libs::mutex::MutexGuard;
use crate::mm::allocation::MemoryAllocationError;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::kernel_buffer::KernelBuffer as XattrStorage;
use crate::process::{cred::CAPFlags, cred::Kgid, ProcessManager};
use alloc::sync::Arc;
#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
type XattrStorage = alloc::vec::Vec<u8>;
use system_error::SystemError;

pub(super) const OVL_XATTR_ORIGIN: &str = "trusted.dragonos.overlay.origin";
const OVL_XATTR_PRIVATE_PREFIX: &str = "trusted.overlay.";
const XATTR_USER_PREFIX: &str = "user.";
const XATTR_TRUSTED_PREFIX: &str = "trusted.";
const XATTR_SECURITY_PREFIX: &str = "security.";
const XATTR_POSIX_ACL_ACCESS: &str = "system.posix_acl_access";
const XATTR_POSIX_ACL_DEFAULT: &str = "system.posix_acl_default";
const XATTR_SECURITY_CAPABILITY: &str = "security.capability";
const XATTR_MAX_SIZE: usize = 65_536;
const XATTR_READ_RETRIES: usize = 3;
const ORIGIN_MAGIC: [u8; 4] = *b"DOVL";
const ORIGIN_VERSION: u8 = 1;
const ORIGIN_ENCODED_SIZE: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OvlOrigin {
    pub(super) fsid: u32,
    pub(super) dev_id: usize,
    pub(super) inode_id: InodeId,
    pub(super) file_type: FileType,
}

fn file_type_to_u8(file_type: FileType) -> u8 {
    match file_type {
        FileType::File => 1,
        FileType::Dir => 2,
        FileType::BlockDevice => 3,
        FileType::CharDevice => 4,
        FileType::FramebufferDevice => 5,
        FileType::KvmDevice => 6,
        FileType::Pipe => 7,
        FileType::SymLink => 8,
        FileType::Socket => 9,
    }
}

fn file_type_from_u8(value: u8) -> Option<FileType> {
    Some(match value {
        1 => FileType::File,
        2 => FileType::Dir,
        3 => FileType::BlockDevice,
        4 => FileType::CharDevice,
        5 => FileType::FramebufferDevice,
        6 => FileType::KvmDevice,
        7 => FileType::Pipe,
        8 => FileType::SymLink,
        9 => FileType::Socket,
        _ => return None,
    })
}

fn encode_origin(origin: OvlOrigin) -> [u8; ORIGIN_ENCODED_SIZE] {
    let mut encoded = [0u8; ORIGIN_ENCODED_SIZE];
    encoded[..4].copy_from_slice(&ORIGIN_MAGIC);
    encoded[4] = ORIGIN_VERSION;
    encoded[5] = file_type_to_u8(origin.file_type);
    // Bytes 8..16 are reserved. Early version-1 records stored a mount-local
    // epoch there; leaving them unused keeps those records readable across
    // remounts without changing the on-disk layout.
    encoded[16..20].copy_from_slice(&origin.fsid.to_le_bytes());
    encoded[24..32].copy_from_slice(&(origin.dev_id as u64).to_le_bytes());
    encoded[32..40].copy_from_slice(&(origin.inode_id.data() as u64).to_le_bytes());
    encoded
}

fn decode_origin(value: &[u8]) -> Option<OvlOrigin> {
    if value.len() != ORIGIN_ENCODED_SIZE
        || value[..4] != ORIGIN_MAGIC
        || value[4] != ORIGIN_VERSION
        || value[6] != 0
        || value[7] != 0
        || value[20..24] != [0; 4]
    {
        return None;
    }

    let fsid = u32::from_le_bytes(value[16..20].try_into().ok()?);
    let dev_id_u64 = u64::from_le_bytes(value[24..32].try_into().ok()?);
    let inode_id_u64 = u64::from_le_bytes(value[32..40].try_into().ok()?);
    let dev_id = usize::try_from(dev_id_u64).ok()?;
    let inode_id = usize::try_from(inode_id_u64).ok()?;
    Some(OvlOrigin {
        fsid,
        dev_id,
        inode_id: InodeId::new(inode_id),
        file_type: file_type_from_u8(value[5])?,
    })
}

fn is_unsupported(err: &SystemError) -> bool {
    matches!(
        err,
        SystemError::ENOSYS | SystemError::EOPNOTSUPP_OR_ENOTSUP
    )
}

fn is_origin_xattr_unavailable(err: &SystemError) -> bool {
    is_unsupported(err) || *err == SystemError::EPERM
}

fn is_private_xattr(name: &str) -> bool {
    name.starts_with(OVL_XATTR_PRIVATE_PREFIX) || name == OVL_XATTR_ORIGIN
}

fn caller_may_access_xattr(name: &str) -> bool {
    !name.starts_with(XATTR_TRUSTED_PREFIX)
        || ProcessManager::current_pcb()
            .cred()
            .has_capability(CAPFlags::CAP_SYS_ADMIN)
}

fn check_xattr_mutation_permission(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    let metadata = inode.metadata()?;
    if metadata
        .flags
        .intersects(InodeFlags::S_IMMUTABLE | InodeFlags::S_APPEND)
    {
        return Err(SystemError::EPERM);
    }
    if name.starts_with(XATTR_TRUSTED_PREFIX) {
        return cred
            .has_capability(CAPFlags::CAP_SYS_ADMIN)
            .then_some(())
            .ok_or(SystemError::EPERM);
    }
    if name == "security.capability" {
        return cred
            .has_capability(CAPFlags::CAP_SETFCAP)
            .then_some(())
            .ok_or(SystemError::EPERM);
    }
    if name.starts_with(XATTR_SECURITY_PREFIX) {
        // DragonOS has no LSM xattr hook yet. Do not let the overlay backing
        // credential turn arbitrary security labels into an unprivileged API.
        return cred
            .has_capability(CAPFlags::CAP_SYS_ADMIN)
            .then_some(())
            .ok_or(SystemError::EPERM);
    }

    if name.starts_with(XATTR_USER_PREFIX) {
        if !matches!(metadata.file_type, FileType::File | FileType::Dir) {
            return Err(SystemError::EPERM);
        }
        if metadata.file_type == FileType::Dir
            && metadata.mode.contains(InodeMode::S_ISVTX)
            && cred.fsuid.data() != metadata.uid
            && !cred.has_capability(CAPFlags::CAP_FOWNER)
        {
            return Err(SystemError::EPERM);
        }
        return cred.inode_permission(&metadata, PermissionMask::MAY_WRITE.bits());
    }
    if name == XATTR_POSIX_ACL_ACCESS || name == XATTR_POSIX_ACL_DEFAULT {
        return (cred.fsuid.data() == metadata.uid || cred.has_capability(CAPFlags::CAP_FOWNER))
            .then_some(())
            .ok_or(SystemError::EPERM);
    }
    cred.inode_permission(&metadata, PermissionMask::MAY_WRITE.bits())
}

fn check_xattr_read_permission(inode: &OvlInode, name: &str) -> Result<(), SystemError> {
    if !name.starts_with(XATTR_USER_PREFIX) {
        return Ok(());
    }
    let metadata = inode.metadata()?;
    if !matches!(metadata.file_type, FileType::File | FileType::Dir) {
        return Err(SystemError::ENODATA);
    }
    ProcessManager::current_pcb()
        .cred()
        .inode_permission(&metadata, PermissionMask::MAY_READ.bits())
}

fn must_copy_xattr(name: &str) -> bool {
    name == XATTR_POSIX_ACL_ACCESS
        || name == XATTR_POSIX_ACL_DEFAULT
        || name.starts_with(XATTR_SECURITY_PREFIX)
}

pub(super) fn remove_security_capability(
    inode: &Arc<dyn IndexNode>,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    inode.remove_security_privileges()
}

/// Internal killpriv for a stacked overlay backing. Keep copy-up and backing
/// credentials, but do not turn privilege removal into a CAP_SETFCAP request.
pub(super) fn remove_security_privileges(
    inode: &OvlInode,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    if !inode.has_upper() {
        let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
        match inode
            .current_realdata_inode()?
            .0
            .getxattr(XATTR_SECURITY_CAPABILITY, &mut [])
        {
            Err(SystemError::ENODATA) => return Ok(()),
            Err(error) if is_unsupported(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    inode.copy_up_locked()?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    upper.remove_security_privileges()
}

fn metadata_change_kills_capability(mask: SetMetadataMask) -> bool {
    mask.intersects(
        SetMetadataMask::WRITE_SIDE_EFFECT | SetMetadataMask::UID | SetMetadataMask::GID,
    )
}

/// One synchronous xattr scratch owner. Capacity may be reused between values;
/// only the bytes actually returned by the backing are ever exposed.
#[derive(Default)]
struct XattrBuffer {
    storage: Option<XattrStorage>,
    len: usize,
}

fn allocate_xattr_storage(size: usize) -> Result<XattrStorage, MemoryAllocationError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        XattrStorage::try_new_zeroed(size)
    }
    // Preserve the existing heap contract on architectures without vmalloc.
    // This compatibility branch does not provide KernelBuffer's byte accounting.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        let mut storage = XattrStorage::new();
        storage
            .try_reserve_exact(size)
            .map_err(|_| SystemError::ENOMEM)?;
        storage.resize(size, 0);
        Ok(storage)
    }
}

impl XattrBuffer {
    fn as_slice(&self) -> &[u8] {
        self.storage
            .as_ref()
            .map_or(&[], |storage| &storage[..self.len])
    }

    fn read(
        &mut self,
        read: impl FnMut(&mut [u8]) -> Result<usize, SystemError>,
    ) -> Result<(), MemoryAllocationError> {
        self.read_with(read, allocate_xattr_storage)
    }

    // The reader is shared by list/get; the allocator parameter also permits
    // local, controlled accounting/fragmentation tests without global hooks.
    fn read_with(
        &mut self,
        mut read: impl FnMut(&mut [u8]) -> Result<usize, SystemError>,
        mut allocate: impl FnMut(usize) -> Result<XattrStorage, MemoryAllocationError>,
    ) -> Result<(), MemoryAllocationError> {
        self.len = 0;
        for _ in 0..XATTR_READ_RETRIES {
            let size = read(&mut [])?;
            if size > XATTR_MAX_SIZE {
                return Err(SystemError::E2BIG.into());
            }
            if size == 0 {
                return Ok(());
            }
            if self
                .storage
                .as_ref()
                .is_none_or(|storage| storage.len() < size)
            {
                // No published data needs preserving. Drop the old capacity
                // before admitting its replacement, avoiding double charging.
                self.storage = None;
                self.storage = Some(allocate(size)?);
            }
            let storage = self.storage.as_mut().unwrap();
            // A reused capacity can exceed this probe. It must not mask an
            // invalid actual length or alter the backing's ERANGE semantics.
            match read(&mut storage[..size]) {
                Ok(actual) if actual <= size => {
                    self.len = actual;
                    return Ok(());
                }
                Ok(_) => return Err(SystemError::EIO.into()),
                Err(SystemError::ERANGE) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(SystemError::ERANGE.into())
    }
}

/// Read-only IndexNode interfaces cannot carry admission recovery. Preserve
/// backing errno, and report rejected scratch admission without reclaim/OOM
/// under the caller's resource locks or overridden backing credentials.
pub(super) fn xattr_read_error(error: MemoryAllocationError) -> SystemError {
    match error {
        MemoryAllocationError::System(error) => error,
        MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
    }
}

fn for_each_xattr_name<E: From<SystemError>>(
    list: &[u8],
    mut visit: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let mut offset = 0usize;
    while offset < list.len() {
        let relative_end = list[offset..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(SystemError::EIO)?;
        if relative_end == 0 {
            return Err(SystemError::EIO.into());
        }
        let end = offset.checked_add(relative_end).ok_or(SystemError::E2BIG)?;
        let name = core::str::from_utf8(&list[offset..end]).map_err(|_| SystemError::EIO)?;
        visit(name)?;
        offset = end.checked_add(1).ok_or(SystemError::E2BIG)?;
    }
    Ok(())
}

pub(super) fn copy_xattrs(
    lower: &Arc<dyn IndexNode>,
    upper: &Arc<dyn IndexNode>,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let mut list = XattrBuffer::default();
    match list.read(|buf| lower.listxattr(buf)) {
        Ok(()) => {}
        Err(MemoryAllocationError::System(error)) if is_unsupported(&error) => return Ok(()),
        Err(error) => return Err(error),
    }
    let mut value = XattrBuffer::default();
    for_each_xattr_name::<MemoryAllocationError>(list.as_slice(), |name| {
        if is_private_xattr(name) {
            return Ok(());
        }
        value.read(|buf| lower.getxattr(name, buf))?;
        match upper.setxattr(name, value.as_slice(), XattrFlags::empty()) {
            Ok(_) => Ok(()),
            Err(crate::mm::allocation::MemoryAllocationError::System(err))
                if is_unsupported(&err) && !must_copy_xattr(name) =>
            {
                Ok(())
            }
            Err(err) => Err(err),
        }
    })
}

pub(super) fn prepare_origin(
    inode: &OvlInode,
    lower: &Arc<dyn IndexNode>,
    upper: &Arc<dyn IndexNode>,
    lower_metadata: &Metadata,
) -> Result<Option<OvlOrigin>, crate::mm::allocation::MemoryAllocationError> {
    if lower_metadata.file_type != FileType::Dir && lower_metadata.nlinks > 1 {
        return Ok(None);
    }

    let fs = inode.overlay_fs()?;
    let fsid = fs.backing_fsid(lower)?;
    let origin = OvlOrigin {
        fsid,
        dev_id: lower_metadata.dev_id,
        inode_id: lower_metadata.inode_id,
        file_type: lower_metadata.file_type,
    };
    match upper.setxattr(
        OVL_XATTR_ORIGIN,
        &encode_origin(origin),
        XattrFlags::empty(),
    ) {
        Ok(_) => Ok(Some(origin)),
        // Some backings (notably the current ext4 symlink path) cannot store
        // xattrs. Fall back consistently to the upper identity rather than
        // claiming provenance that a later lookup cannot recover.
        Err(crate::mm::allocation::MemoryAllocationError::System(err))
            if is_origin_xattr_unavailable(&err) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

pub(super) fn load_origin(
    inode: &OvlInode,
    upper: &Arc<dyn IndexNode>,
) -> Result<Option<OvlOrigin>, MemoryAllocationError> {
    let fs = inode.overlay_fs()?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    let mut value = XattrBuffer::default();
    match value.read(|buf| upper.getxattr(OVL_XATTR_ORIGIN, buf)) {
        Ok(()) => {}
        Err(MemoryAllocationError::System(SystemError::ENODATA)) => return Ok(None),
        // Keep this symmetric with prepare_origin(): if the backing
        // filesystem cannot store the private origin xattr, a later inode
        // instantiation must use the upper identity instead of exposing the
        // backing xattr error through lookup/stat.
        Err(MemoryAllocationError::System(err)) if is_origin_xattr_unavailable(&err) => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    }
    let Some(origin) = decode_origin(value.as_slice()) else {
        return Ok(None);
    };
    if origin.file_type != inode.file_type
        || !fs.backing_fsid_matches_device(origin.fsid, origin.dev_id)?
    {
        return Ok(None);
    }

    if !inode.lower_inodes.is_empty() {
        let mut matched = false;
        for lower in &inode.lower_inodes {
            if fs.backing_fsid(lower)? != origin.fsid {
                continue;
            }
            let metadata = lower.metadata()?;
            if metadata.dev_id == origin.dev_id
                && metadata.inode_id == origin.inode_id
                && metadata.file_type == origin.file_type
            {
                matched = true;
                break;
            }
        }
        if !matched {
            return Ok(None);
        }
    }

    Ok(Some(origin))
}

pub(super) fn metadata(inode: &OvlInode) -> Result<Metadata, SystemError> {
    let fs = inode.overlay_fs()?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    let (real_inode, _) = inode.current_realdata_inode()?;
    let mut metadata = real_inode.metadata()?;
    let origin = inode.origin();

    if fs.samefs {
        metadata.dev_id = 0;
        if let Some(origin) = origin {
            metadata.inode_id = origin.inode_id;
        }
    } else if inode.file_type == FileType::Dir {
        metadata.dev_id = 0;
        metadata.inode_id = inode.overlay_inode_id.ok_or(SystemError::EIO)?;
    } else if let Some(origin) = origin {
        metadata.dev_id = origin.dev_id;
        metadata.inode_id = origin.inode_id;
    }

    let real_dir_count = inode.lower_inodes.len() + usize::from(inode.upper_inode.lock().is_some());
    if inode.file_type == FileType::Dir && real_dir_count > 1 {
        metadata.nlinks = 1;
    }
    Ok(metadata)
}

pub(super) fn resize_with_lock_owner(
    inode: &OvlInode,
    len: usize,
    lock_owner: u64,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    inode.copy_up_locked_for_truncate(len)?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    remove_security_capability(&upper)?;
    upper.resize_with_lock_owner(len, lock_owner)
}

pub(super) fn set_metadata_masked(
    inode: &OvlInode,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    if mask.is_empty() {
        return Ok(());
    }
    let fs = inode.overlay_fs()?;
    let kill_capability = metadata_change_kills_capability(mask);
    let _privilege_guard = inode.content_privilege_lock.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    inode.copy_up_locked()?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    if kill_capability {
        remove_security_capability(&upper)?;
    }
    let upper_metadata = prepare_upper_metadata(&upper, requested, mask)?;
    upper.set_metadata_masked(&upper_metadata, mask)
}

fn prepare_upper_metadata(
    upper: &Arc<dyn IndexNode>,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<Metadata, crate::mm::allocation::MemoryAllocationError> {
    let mut upper_metadata = upper.metadata()?;
    merge_metadata_masked(&mut upper_metadata, requested, mask);
    Ok(upper_metadata)
}

pub(super) fn resize_with_metadata(
    inode: &OvlInode,
    len: usize,
    lock_owner: u64,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    inode.copy_up_locked_for_truncate(len)?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    remove_security_capability(&upper)?;
    let mut upper_metadata = prepare_upper_metadata(&upper, requested, mask)?;
    upper_metadata.size = len as i64;
    upper.resize_with_metadata(len, lock_owner, &upper_metadata, mask)
}

fn check_metadata_mutation_permission(
    inode: &OvlInode,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<(), SystemError> {
    if mask.contains(SetMetadataMask::WRITE_SIDE_EFFECT) {
        return Ok(());
    }

    let current = inode.metadata()?;
    if current.flags.contains(InodeFlags::S_IMMUTABLE) {
        return Err(SystemError::EPERM);
    }
    let cred = ProcessManager::current_pcb().cred();

    if mask.intersects(SetMetadataMask::UID | SetMetadataMask::GID) {
        if cred.uid.data() == 0 {
            return Ok(());
        }
        if cred.uid.data() != current.uid
            || (mask.contains(SetMetadataMask::UID) && requested.uid != current.uid)
        {
            return Err(SystemError::EPERM);
        }
        if mask.contains(SetMetadataMask::GID)
            && requested.gid != cred.gid.data()
            && !cred.groups.contains(&Kgid::from(requested.gid))
        {
            return Err(SystemError::EPERM);
        }
        return Ok(());
    }

    if mask.contains(SetMetadataMask::MODE) {
        return (cred.fsuid.data() == current.uid || cred.has_capability(CAPFlags::CAP_FOWNER))
            .then_some(())
            .ok_or(SystemError::EPERM);
    }

    if mask.intersects(SetMetadataMask::ATIME | SetMetadataMask::MTIME) {
        if cred.fsuid.data() == current.uid || cred.has_capability(CAPFlags::CAP_FOWNER) {
            return Ok(());
        }
        if mask.contains(SetMetadataMask::TIMES_BY_WRITE) {
            return cred.inode_permission(&current, PermissionMask::MAY_WRITE.bits());
        }
        return Err(SystemError::EPERM);
    }
    Ok(())
}

pub(super) fn resize_file(
    inode: &OvlInode,
    len: usize,
    lock_owner: u64,
    data: MutexGuard<FilePrivateData>,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    drop(data);
    resize_with_lock_owner(inode, len, lock_owner)
}

pub(super) fn resize_file_with_metadata(
    inode: &OvlInode,
    len: usize,
    lock_owner: u64,
    data: MutexGuard<FilePrivateData>,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    inode.copy_up_locked_for_truncate(len)?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    check_metadata_mutation_permission(inode, requested, mask)?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    remove_security_capability(&upper)?;
    let mut upper_metadata = prepare_upper_metadata(&upper, requested, mask)?;
    upper_metadata.size = len as i64;
    super::file::resize_file_with_metadata(inode, data, len, lock_owner, &upper_metadata, mask)
}

pub(super) fn getxattr(inode: &OvlInode, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
    if is_private_xattr(name) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if !caller_may_access_xattr(name) {
        return Err(SystemError::EPERM);
    }
    check_xattr_read_permission(inode, name)?;
    let fs = inode.overlay_fs()?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    inode.current_realdata_inode()?.0.getxattr(name, buf)
}

pub(super) fn setxattr(
    inode: &OvlInode,
    name: &str,
    value: &[u8],
    flags: XattrFlags,
) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
    if is_private_xattr(name) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
    }
    if !caller_may_access_xattr(name) {
        return Err(SystemError::EPERM.into());
    }
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    check_xattr_mutation_permission(inode, name)?;
    inode.copy_up_locked()?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    check_xattr_mutation_permission(inode, name)?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    upper.setxattr(name, value, flags)
}

pub(super) fn listxattr(inode: &OvlInode, buf: &mut [u8]) -> Result<usize, SystemError> {
    let may_list_trusted = ProcessManager::current_pcb()
        .cred()
        .has_capability(CAPFlags::CAP_SYS_ADMIN);
    let fs = inode.overlay_fs()?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    let real = inode.current_realdata_inode()?.0;
    let mut list = XattrBuffer::default();
    list.read(|buf| real.listxattr(buf))
        .map_err(xattr_read_error)?;
    copy_visible_xattr_names(list.as_slice(), buf, may_list_trusted)
}

fn copy_visible_xattr_names(
    list: &[u8],
    buf: &mut [u8],
    may_list_trusted: bool,
) -> Result<usize, SystemError> {
    let visible = |name: &str| {
        !is_private_xattr(name) && (may_list_trusted || !name.starts_with(XATTR_TRUSTED_PREFIX))
    };
    // Fully validate even a size query or a too-small output before touching
    // the caller's buffer. The same immutable list/credential snapshot is used
    // in both passes, so no second full-size filtering allocation is needed.
    let mut size = 0;
    for_each_xattr_name::<SystemError>(list, |name| {
        if visible(name) {
            size += name.len() + 1;
        }
        Ok(())
    })?;
    if buf.is_empty() {
        return Ok(size);
    }
    if buf.len() < size {
        return Err(SystemError::ERANGE);
    }
    let mut offset = 0;
    for_each_xattr_name::<SystemError>(list, |name| {
        if visible(name) {
            let end = offset + name.len();
            buf[offset..end].copy_from_slice(name.as_bytes());
            buf[end] = 0;
            offset = end + 1;
        }
        Ok(())
    })?;
    Ok(size)
}

pub(super) fn removexattr(
    inode: &OvlInode,
    name: &str,
) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
    if is_private_xattr(name) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
    }
    if !caller_may_access_xattr(name) {
        return Err(SystemError::EPERM.into());
    }
    let fs = inode.overlay_fs()?;
    let _privilege_guard = inode.content_privilege_lock.lock();
    check_xattr_mutation_permission(inode, name)?;
    let copied_up_for_remove = !inode.has_upper();
    if copied_up_for_remove {
        let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
        inode.current_realdata_inode()?.0.getxattr(name, &mut [])?;
    }

    inode.copy_up_locked()?;
    let upper = inode.upper_inode.lock().clone().ok_or(SystemError::EIO)?;
    let _content_guard = fs.content_lock(&upper)?.lock();
    check_xattr_mutation_permission(inode, name)?;
    let _cred_guard = CredOverrideGuard::new(fs.backing_cred.clone())?;
    match upper.removexattr(name) {
        Ok(size) => Ok(size),
        Err(crate::mm::allocation::MemoryAllocationError::System(err))
            if copied_up_for_remove && (err == SystemError::ENODATA || is_unsupported(&err)) =>
        {
            // Optional lower xattrs may be intentionally discarded when the
            // upper backing does not support their namespace. The attribute
            // existed in the merged view before copy-up, so report a
            // completed removal only after confirming that it is no longer
            // observable through the published upper inode.
            match upper.getxattr(name, &mut []) {
                Err(SystemError::ENODATA) => Ok(0),
                Err(probe_err) if is_unsupported(&probe_err) => Ok(0),
                Ok(_) => Err(err.into()),
                Err(probe_err) => Err(probe_err.into()),
            }
        }
        Err(err) => Err(err),
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
mod selftest;
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) use selftest::report as selftest_report;
