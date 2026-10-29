//! The ordinary `keyctl(2)` operations.  Request construction and its
//! authorization token are handled by `request`, while this module keeps
//! control operations next to the key object, permission and ring services.

use alloc::{format, vec::Vec};
use core::mem;

use system_error::SystemError;

use crate::{
    arch::MMArch,
    mm::MemoryManagementArch,
    process::{
        cred::{capable, CAPFlags, Cred},
        namespace::user_namespace::{from_kgid_munged, from_kuid_munged, make_kgid, make_kuid},
        ProcessManager,
    },
    syscall::user_buffer::UserBuffer,
    time::timekeeping::realtime_now,
};

use super::{
    object::{self, KeyFlags, KeyPayload, KeyRef, KeyStatus, KeyType},
    permission::{check_permission, validate_state, KeyPermission},
    quota::account_for,
    request, ring,
    service::{join_session_keyring, lookup_key_raw, resolve_key},
    user::{copy_cstr_bytes, copy_type_name, SensitiveBytes, DESCRIPTION_MAX},
};

const USER_PAYLOAD_MAX: usize = 32_767;
const PERMISSION_MASK: u32 = 0x3f3f_3f3f;
const CAPABILITIES: [u8; 2] = [0xe1, 0x01];

/// A rejected key may carry any valid Linux errno, including values absent
/// from `SystemError`.  Return those raw negative values unchanged from the
/// syscall rather than silently translating them to a different failure.
macro_rules! lookup_or_return_errno {
    ($id:expr, $create:expr, $partial:expr, $permission:expr) => {
        match lookup_key_raw($id, $create, $partial, $permission) {
            Ok(key) => key,
            Err(errno) => return Ok(errno as isize as usize),
        }
    };
}

#[inline]
fn key_id(arg: usize) -> i32 {
    arg as u32 as i32
}

fn copy_out(user: usize, data: &[u8]) -> Result<(), SystemError> {
    if data.is_empty() {
        return Ok(());
    }
    let mut output = UserBuffer::new_protected(user as *mut u8, data.len(), true)?;
    output.write_to_user(0, data)?;
    Ok(())
}

fn current_cred() -> alloc::sync::Arc<Cred> {
    ProcessManager::current_pcb().cred()
}

fn can_use_root_override(key: &KeyRef, flag: KeyFlags) -> bool {
    capable(CAPFlags::CAP_SYS_ADMIN) && key.state.lock().flags.contains(flag)
}

fn user_type(name: &[u8]) -> Result<KeyType, SystemError> {
    match name {
        b"user" => Ok(KeyType::User),
        b"logon" => Ok(KeyType::Logon),
        b"keyring" => Ok(KeyType::Keyring),
        _ => Err(SystemError::ENOKEY),
    }
}

fn keyctl_get_keyring_id(args: [usize; 4]) -> Result<usize, SystemError> {
    let key = lookup_or_return_errno!(
        key_id(args[0]),
        args[1] as i32 != 0,
        false,
        Some(KeyPermission::Search)
    );
    Ok(key.serial as usize)
}

fn keyctl_join_session(args: [usize; 4]) -> Result<usize, SystemError> {
    let name = if args[0] == 0 {
        None
    } else {
        let name = copy_cstr_bytes(args[0] as *const u8, DESCRIPTION_MAX)?;
        if name.first() == Some(&b'.') {
            return Err(SystemError::EPERM);
        }
        Some(name)
    };
    Ok(join_session_keyring(name)? as usize)
}

fn keyctl_update(args: [usize; 4]) -> Result<usize, SystemError> {
    let len = args[2];
    if len > MMArch::PAGE_SIZE {
        return Err(SystemError::EINVAL);
    }
    let input = SensitiveBytes::copy_from_user(args[1] as *const u8, len, MMArch::PAGE_SIZE)?;
    let key = lookup_or_return_errno!(key_id(args[0]), false, false, Some(KeyPermission::Write));
    if key.key_type != KeyType::User && key.key_type != KeyType::Logon {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if len == 0 || len > USER_PAYLOAD_MAX {
        return Err(SystemError::EINVAL);
    }

    let mut state = key.state.lock();
    validate_state(&state, realtime_now().tv_sec)?;
    // A pending request may only be instantiated by its authorization token.
    if state.status == KeyStatus::Uninstantiated {
        return Err(SystemError::EACCES);
    }
    state.quota.resize(key.description.len() + 1 + len)?;
    let payload = match key.key_type {
        KeyType::User => KeyPayload::User(input.into_vec()),
        KeyType::Logon => KeyPayload::Logon(input.into_vec()),
        _ => unreachable!(),
    };
    let replaced = mem::replace(&mut state.payload, payload);
    state.expiry = None;
    state.status = KeyStatus::Positive;
    drop(state);
    drop(replaced);
    Ok(0)
}

fn keyctl_revoke(args: [usize; 4]) -> Result<usize, SystemError> {
    let key = match lookup_key_raw(key_id(args[0]), false, false, Some(KeyPermission::Write)) {
        Ok(key) => key,
        Err(error) if error == SystemError::EACCES.to_posix_errno() => {
            lookup_or_return_errno!(key_id(args[0]), false, false, Some(KeyPermission::SetAttr))
        }
        Err(error) => return Ok(error as isize as usize),
    };
    let mut state = key.state.lock();
    if state.revoked_at.is_none() {
        state.revoked_at = Some(realtime_now().tv_sec);
        state.quota.resize(key.description.len() + 1)?;
        let replaced = mem::replace(
            &mut state.payload,
            match key.key_type {
                KeyType::Keyring => KeyPayload::Keyring(Default::default()),
                _ => KeyPayload::Uninstantiated,
            },
        );
        drop(state);
        drop(replaced);
        object::schedule_gc();
    }
    Ok(0)
}

fn keyctl_invalidate(args: [usize; 4]) -> Result<usize, SystemError> {
    let id = key_id(args[0]);
    let key = match lookup_key_raw(id, false, false, Some(KeyPermission::Search)) {
        Ok(key) => key,
        Err(error)
            if error == SystemError::EACCES.to_posix_errno()
                && capable(CAPFlags::CAP_SYS_ADMIN) =>
        {
            let key = lookup_or_return_errno!(id, false, false, None);
            if !can_use_root_override(&key, KeyFlags::ROOT_CAN_INVAL) {
                return Err(SystemError::EACCES);
            }
            key
        }
        Err(error) => return Ok(error as isize as usize),
    };
    key.state.lock().invalidated = true;
    object::schedule_gc();
    Ok(0)
}

fn keyctl_describe(args: [usize; 4]) -> Result<usize, SystemError> {
    let key = lookup_or_return_errno!(key_id(args[0]), false, true, Some(KeyPermission::View));
    let cred = current_cred();
    let (uid, gid, perm) = {
        let state = key.state.lock();
        (state.uid, state.gid, state.perm)
    };
    let prefix = format!(
        "{};{};{};{:08x};",
        core::str::from_utf8(key.key_type.name()).expect("fixed type name"),
        from_kuid_munged(&cred.user_ns, uid),
        from_kgid_munged(&cred.user_ns, gid),
        perm
    );
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(prefix.len() + key.description.len() + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(&key.description);
    bytes.push(0);
    if args[1] != 0 && args[2] >= bytes.len() {
        copy_out(args[1], &bytes)?;
    }
    Ok(bytes.len())
}

fn keyctl_clear(args: [usize; 4]) -> Result<usize, SystemError> {
    let id = key_id(args[0]);
    let key = match lookup_key_raw(id, true, false, Some(KeyPermission::Write)) {
        Ok(key) => key,
        Err(error)
            if error == SystemError::EACCES.to_posix_errno()
                && capable(CAPFlags::CAP_SYS_ADMIN) =>
        {
            let key = lookup_or_return_errno!(id, false, false, None);
            if !can_use_root_override(&key, KeyFlags::ROOT_CAN_CLEAR) {
                return Err(SystemError::EACCES);
            }
            key
        }
        Err(error) => return Ok(error as isize as usize),
    };
    ring::clear(&key)?;
    Ok(0)
}

fn keyctl_link(args: [usize; 4]) -> Result<usize, SystemError> {
    let ring = lookup_or_return_errno!(key_id(args[1]), true, false, Some(KeyPermission::Write));
    let key = lookup_or_return_errno!(key_id(args[0]), true, false, Some(KeyPermission::Link));
    ring::link(&ring, &key)?;
    Ok(0)
}

fn keyctl_unlink(args: [usize; 4]) -> Result<usize, SystemError> {
    let ring = lookup_or_return_errno!(key_id(args[1]), false, false, Some(KeyPermission::Write));
    let key = resolve_key(key_id(args[0]), false)?;
    ring::unlink(&ring, &key)?;
    Ok(0)
}

fn keyctl_search(args: [usize; 4]) -> Result<usize, SystemError> {
    let type_name = copy_type_name(args[1] as *const u8)?;
    let description = copy_cstr_bytes(args[2] as *const u8, DESCRIPTION_MAX)?;
    let ring = lookup_or_return_errno!(key_id(args[0]), false, false, Some(KeyPermission::Search));
    let destination = if key_id(args[3]) != 0 {
        Some(lookup_or_return_errno!(
            key_id(args[3]),
            true,
            false,
            Some(KeyPermission::Write)
        ))
    } else {
        None
    };
    let key_type = user_type(&type_name)?;
    let key = match ring::search(&ring, key_type, &description, &current_cred(), true) {
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => return Err(SystemError::ENOKEY),
        result => result?,
    };
    if let Some(destination) = destination {
        check_permission(&key, &current_cred(), KeyPermission::Link)?;
        ring::link(&destination, &key)?;
    }
    Ok(key.serial as usize)
}

struct ReadSnapshot(Vec<u8>);

impl Drop for ReadSnapshot {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

fn keyctl_read(args: [usize; 4]) -> Result<usize, SystemError> {
    let key = resolve_key(key_id(args[0]), false).map_err(|_| SystemError::ENOKEY)?;
    match key.wait_until_instantiated()? {
        KeyStatus::Negative(errno) => return Ok((-(errno as isize)) as usize),
        KeyStatus::Uninstantiated => return Err(SystemError::EIO),
        KeyStatus::Positive => {}
    }
    let cred = current_cred();
    if check_permission(&key, &cred, KeyPermission::Read).is_err()
        && (!key.is_possessed() || check_permission(&key, &cred, KeyPermission::Search).is_err())
    {
        return Err(SystemError::EACCES);
    }
    let (snapshot, required) = {
        let state = key.state.lock();
        validate_state(&state, realtime_now().tv_sec)?;
        match &state.payload {
            KeyPayload::User(bytes) => {
                let copy_len = if args[1] != 0 && args[2] >= bytes.len() {
                    bytes.len()
                } else {
                    0
                };
                let mut copy = Vec::new();
                copy.try_reserve_exact(copy_len)
                    .map_err(|_| SystemError::ENOMEM)?;
                copy.extend_from_slice(&bytes[..copy_len]);
                (ReadSnapshot(copy), bytes.len())
            }
            KeyPayload::Keyring(links) => {
                if args[2] & 3 != 0 {
                    return Err(SystemError::EINVAL);
                }
                let required = links.len().checked_mul(4).ok_or(SystemError::EOVERFLOW)?;
                let copy_len = if args[1] != 0 && args[2] >= required {
                    required
                } else {
                    0
                };
                let mut copy = Vec::new();
                copy.try_reserve_exact(copy_len)
                    .map_err(|_| SystemError::ENOMEM)?;
                for child in links.values().take(copy_len / 4) {
                    copy.extend_from_slice(&child.serial.to_ne_bytes());
                }
                (ReadSnapshot(copy), required)
            }
            KeyPayload::RequestKeyAuth(auth) => {
                let copy_len = if args[1] != 0 && args[2] >= auth.callout_info.len() {
                    auth.callout_info.len()
                } else {
                    0
                };
                let mut copy = Vec::new();
                copy.try_reserve_exact(copy_len)
                    .map_err(|_| SystemError::ENOMEM)?;
                copy.extend_from_slice(&auth.callout_info[..copy_len]);
                (ReadSnapshot(copy), auth.callout_info.len())
            }
            KeyPayload::Logon(_) | KeyPayload::Uninstantiated => {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
        }
    };
    if !snapshot.0.is_empty() {
        copy_out(args[1], &snapshot.0)?;
    }
    Ok(required)
}

fn keyctl_chown(args: [usize; 4]) -> Result<usize, SystemError> {
    let uid = args[1] as u32;
    let gid = args[2] as u32;
    let cred = current_cred();
    let new_uid = if uid == u32::MAX {
        None
    } else {
        Some(make_kuid(&cred.user_ns, uid)?)
    };
    let new_gid = if gid == u32::MAX {
        None
    } else {
        Some(make_kgid(&cred.user_ns, gid)?)
    };
    if new_uid.is_none() && new_gid.is_none() {
        return Ok(0);
    }
    let key = lookup_or_return_errno!(key_id(args[0]), true, true, Some(KeyPermission::SetAttr));
    let account = new_uid.map(account_for).transpose()?;
    let mut state = key.state.lock();
    let requires_admin = new_uid.is_some_and(|uid| uid != state.uid)
        || new_gid.is_some_and(|gid| {
            gid != state.gid && gid != cred.fsgid && !cred.groups.contains(&gid)
        });
    if requires_admin && !capable(CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EACCES);
    }
    if let (Some(uid), Some(account)) = (new_uid, account) {
        if uid != state.uid {
            state.quota.transfer_to(account)?;
            state.uid = uid;
        }
    }
    if let Some(gid) = new_gid {
        state.gid = gid;
    }
    Ok(0)
}

fn keyctl_setperm(args: [usize; 4]) -> Result<usize, SystemError> {
    let perm = args[1] as u32;
    if perm & !PERMISSION_MASK != 0 {
        return Err(SystemError::EINVAL);
    }
    let key = lookup_or_return_errno!(key_id(args[0]), true, true, Some(KeyPermission::SetAttr));
    let cred = current_cred();
    let mut state = key.state.lock();
    if state.uid != cred.fsuid && !capable(CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EACCES);
    }
    state.perm = perm;
    Ok(0)
}

fn keyctl_set_timeout(args: [usize; 4]) -> Result<usize, SystemError> {
    let id = key_id(args[0]);
    let key = match lookup_key_raw(id, true, true, Some(KeyPermission::SetAttr)) {
        Ok(key) => key,
        Err(error)
            if error == SystemError::EACCES.to_posix_errno() && request::has_authority_for(id) =>
        {
            lookup_or_return_errno!(id, false, true, None)
        }
        Err(error) => return Ok(error as isize as usize),
    };
    let seconds = args[1] as u32;
    let expiry = if seconds == 0 {
        None
    } else {
        Some(realtime_now().tv_sec.saturating_add(seconds as i64))
    };
    key.state.lock().expiry = expiry;
    object::schedule_expiry_gc(expiry);
    Ok(0)
}

fn keyctl_get_security(args: [usize; 4]) -> Result<usize, SystemError> {
    let _key = lookup_or_return_errno!(key_id(args[0]), false, true, Some(KeyPermission::View));
    if args[1] != 0 && args[2] > 0 {
        copy_out(args[1], &[0])?;
    }
    Ok(1)
}

fn keyctl_restrict(args: [usize; 4]) -> Result<usize, SystemError> {
    let ring = lookup_or_return_errno!(key_id(args[0]), false, false, Some(KeyPermission::SetAttr));
    if args[1] == 0 && args[2] == 0 {
        ring::restrict_reject_all(&ring)?;
        return Ok(0);
    }
    if args[1] == 0 || args[2] == 0 {
        return Err(SystemError::EINVAL);
    }
    let type_name = copy_type_name(args[1] as *const u8)?;
    let _restriction = copy_cstr_bytes(args[2] as *const u8, MMArch::PAGE_SIZE)?;
    let _type = user_type(&type_name)?;
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    Err(SystemError::ENOENT)
}

fn keyctl_move(args: [usize; 4]) -> Result<usize, SystemError> {
    let flags = args[3] as u32;
    if flags & !1 != 0 {
        return Err(SystemError::EINVAL);
    }
    let key = lookup_or_return_errno!(key_id(args[0]), true, false, Some(KeyPermission::Link));
    let source = lookup_or_return_errno!(key_id(args[1]), false, false, Some(KeyPermission::Write));
    let target = lookup_or_return_errno!(key_id(args[2]), true, false, Some(KeyPermission::Write));
    ring::move_key(&key, &source, &target, flags & 1 != 0)?;
    Ok(0)
}

fn keyctl_capabilities(args: [usize; 4]) -> Result<usize, SystemError> {
    let length = args[1];
    if length == 0 {
        return Ok(CAPABILITIES.len());
    }
    let mut output = UserBuffer::new_protected(args[0] as *mut u8, length, true)?;
    output.write_to_user(0, &CAPABILITIES[..length.min(CAPABILITIES.len())])?;
    // Linux clears the entire caller-provided remainder.  A fixed block
    // avoids an allocation controlled by a potentially huge buflen.
    let zeros = [0u8; 256];
    let mut offset = CAPABILITIES.len().min(length);
    while offset < length {
        let count = (length - offset).min(zeros.len());
        output.write_to_user(offset, &zeros[..count])?;
        offset += count;
    }
    Ok(CAPABILITIES.len())
}

/// `None` means that the command belongs to request-key authorization, the
/// parent-session handoff, or an optional feature handled by the dispatcher.
pub fn dispatch_control(option: u32, args: [usize; 4]) -> Result<Option<usize>, SystemError> {
    let result = match option {
        0 => keyctl_get_keyring_id(args),
        1 => keyctl_join_session(args),
        2 => keyctl_update(args),
        3 => keyctl_revoke(args),
        4 => keyctl_chown(args),
        5 => keyctl_setperm(args),
        6 => keyctl_describe(args),
        7 => keyctl_clear(args),
        8 => keyctl_link(args),
        9 => keyctl_unlink(args),
        10 => keyctl_search(args),
        11 => keyctl_read(args),
        15 => keyctl_set_timeout(args),
        17 => keyctl_get_security(args),
        21 => keyctl_invalidate(args),
        29 => keyctl_restrict(args),
        30 => keyctl_move(args),
        31 => keyctl_capabilities(args),
        _ => return Ok(None),
    };
    result.map(Some)
}
