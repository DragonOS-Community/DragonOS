// SPDX-License-Identifier: GPL-2.0-or-later
//
// Generic SysV IPC permission object and permission checks shared by shm and sem.

use alloc::sync::Arc;

use num::ToPrimitive;
use system_error::SystemError;

use crate::process::{
    cred::{ns_capable, CAPFlags, Cred, Kgid, Kuid},
    namespace::user_namespace::{map_id_down, map_id_up, UserNamespace},
    ProcessManager,
};

const DEFAULT_OVERFLOW_ID: u32 = 65534;

/// Permission bits shared by all SysV IPC objects, matching Linux `ipc_perm.mode`.
pub const PERM_MASK: u32 = 0o777;

/// Generic SysV IPC permission object, the counterpart of Linux `struct kern_ipc_perm`.
#[derive(Debug)]
pub struct IpcPerm {
    /// IPC object id (encoded raw id, see `IpcIdAllocator`)
    pub id: usize,
    /// User-specified key
    pub key: usize,
    /// Owner user id (kernel-global uid)
    pub uid: Kuid,
    /// Owner group id (kernel-global gid)
    pub gid: Kgid,
    /// Creator user id (kernel-global uid)
    pub cuid: Kuid,
    /// Creator group id (kernel-global gid)
    pub cgid: Kgid,
    /// Permission mode bits (low 9 bits)
    pub mode: u32,
    /// Sequence number distinguishing stale user ids after index reuse
    pub seq: usize,
}

/// View of an IPC object's permission fields, implemented by concrete objects
/// (shm's `KernIpcPerm`, sem's `IpcPerm`).
pub trait IpcPermView {
    fn uid(&self) -> Kuid;
    fn gid(&self) -> Kgid;
    fn cuid(&self) -> Kuid;
    fn cgid(&self) -> Kgid;
    fn mode(&self) -> u32;

    /// IPC object key in userspace form; default 0 for objects without a key.
    fn key(&self) -> usize {
        0
    }

    fn seq(&self) -> usize {
        0
    }

    fn to_posix(&self, user_ns: &Arc<UserNamespace>) -> Result<PosixIpcPerm, SystemError> {
        Ok(PosixIpcPerm {
            key: self.key() as u32 as i32,
            uid: kuid_to_user(user_ns, self.uid()),
            gid: kgid_to_user(user_ns, self.gid()),
            cuid: kuid_to_user(user_ns, self.cuid()),
            cgid: kgid_to_user(user_ns, self.cgid()),
            mode: self.mode(),
            seq: self.seq().to_i32().ok_or(SystemError::EOVERFLOW)?,
            _pad1: 0,
            _unused1: 0,
            _unused2: 0,
        })
    }
}

impl IpcPermView for IpcPerm {
    fn uid(&self) -> Kuid {
        self.uid
    }

    fn gid(&self) -> Kgid {
        self.gid
    }

    fn cuid(&self) -> Kuid {
        self.cuid
    }

    fn cgid(&self) -> Kgid {
        self.cgid
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn key(&self) -> usize {
        self.key
    }

    fn seq(&self) -> usize {
        self.seq
    }
}

impl IpcPerm {
    pub fn new_with_cred(id: usize, key: usize, cred: Arc<Cred>, mode: u32, seq: usize) -> Self {
        IpcPerm {
            id,
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode: mode & PERM_MASK,
            seq,
        }
    }

    pub fn copy_from_posix(
        &mut self,
        uid: u32,
        gid: u32,
        mode: u32,
        user_ns: &Arc<UserNamespace>,
    ) -> Result<(), SystemError> {
        let uid = make_kuid(user_ns, uid)?;
        let gid = make_kgid(user_ns, gid)?;

        self.uid = uid;
        self.gid = gid;
        self.mode = mode & PERM_MASK;
        Ok(())
    }
}

/// Generic version of the shm/sem permission check, matching Linux `ipcperms()`.
pub fn ipc_permission<P: IpcPermView>(
    perm: &P,
    requested: u32,
    target_user_ns: &Arc<UserNamespace>,
) -> Result<(), SystemError> {
    let requested = ((requested >> 6) | (requested >> 3) | requested) & 0o7;
    if requested == 0 {
        return Ok(());
    }

    let cred = ProcessManager::current_pcb().cred();
    let mut granted = perm.mode();
    if cred.euid == perm.cuid() || cred.euid == perm.uid() {
        granted >>= 6;
    } else if cred_in_group(&cred, perm.cgid()) || cred_in_group(&cred, perm.gid()) {
        granted >>= 3;
    }

    if (requested & !(granted & 0o7)) != 0 && !ns_capable(target_user_ns, CAPFlags::CAP_IPC_OWNER) {
        return Err(SystemError::EACCES);
    }

    Ok(())
}

/// Permission check for control operations (IPC_SET/IPC_RMID), matching Linux
/// `ipcctl_pre_down`'s caller check.
pub fn check_control_permission<P: IpcPermView>(
    perm: &P,
    target_user_ns: &Arc<UserNamespace>,
) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    if cred.euid == perm.cuid()
        || cred.euid == perm.uid()
        || ns_capable(target_user_ns, CAPFlags::CAP_SYS_ADMIN)
    {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

/// Permission check for SHM_LOCK/SHM_UNLOCK, matching Linux `security_shm_shmctl`.
pub fn check_lock_permission<P: IpcPermView>(
    perm: &P,
    target_user_ns: &Arc<UserNamespace>,
) -> Result<(), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    if cred.euid == perm.cuid()
        || cred.euid == perm.uid()
        || ns_capable(target_user_ns, CAPFlags::CAP_IPC_LOCK)
    {
        Ok(())
    } else {
        Err(SystemError::EPERM)
    }
}

fn cred_in_group(cred: &Cred, gid: Kgid) -> bool {
    cred.fsgid == gid
        || cred.groups.contains(&gid)
        || cred
            .group_info
            .as_ref()
            .map(|group_info| group_info.gids.contains(&gid))
            .unwrap_or(false)
}

pub fn make_kuid(user_ns: &Arc<UserNamespace>, uid: u32) -> Result<Kuid, SystemError> {
    let inner = user_ns.inner.lock();
    map_id_down(&inner.uid_map, uid)
        .map(|uid| Kuid::new(uid as usize))
        .ok_or(SystemError::EINVAL)
}

pub fn make_kgid(user_ns: &Arc<UserNamespace>, gid: u32) -> Result<Kgid, SystemError> {
    let inner = user_ns.inner.lock();
    map_id_down(&inner.gid_map, gid)
        .map(|gid| Kgid::new(gid as usize))
        .ok_or(SystemError::EINVAL)
}

pub fn kuid_to_user(user_ns: &Arc<UserNamespace>, kuid: Kuid) -> u32 {
    let Ok(uid) = u32::try_from(kuid.data()) else {
        return DEFAULT_OVERFLOW_ID;
    };
    let inner = user_ns.inner.lock();
    map_id_up(&inner.uid_map, uid).unwrap_or(DEFAULT_OVERFLOW_ID)
}

pub fn kgid_to_user(user_ns: &Arc<UserNamespace>, kgid: Kgid) -> u32 {
    let Ok(gid) = u32::try_from(kgid.data()) else {
        return DEFAULT_OVERFLOW_ID;
    };
    let inner = user_ns.inner.lock();
    map_id_up(&inner.gid_map, gid).unwrap_or(DEFAULT_OVERFLOW_ID)
}

/// IPC permission object in the userspace ABI, matching Linux `struct ipc_perm` (48 bytes on x86_64).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixIpcPerm {
    /// IPC object key
    key: i32,
    /// Current user id
    uid: u32,
    /// Current user group id
    gid: u32,
    /// Creator user id
    cuid: u32,
    /// Creator group id
    cgid: u32,
    /// Permission mode
    mode: u32,
    /// Sequence number
    seq: i32,
    _pad1: i32,
    _unused1: usize,
    _unused2: usize,
}

impl PosixIpcPerm {
    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }
}
