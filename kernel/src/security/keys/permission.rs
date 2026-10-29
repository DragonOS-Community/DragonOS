//! Linux key permission and validity checks.
//!
//! A serial number only identifies a key.  It never implies possession: the
//! possessor bits apply only to a `KeyRef` obtained through a possessed
//! credential keyring or a search beneath one.

use system_error::SystemError;

use crate::{
    process::cred::{Cred, Kgid},
    time::timekeeping::realtime_now,
};

use super::object::{KeyRef, KeyState};

const GROUP_ALL: u32 = 0x0000_3f00;
pub const KEY_POS_ALL: u32 = 0x3f00_0000;
pub const KEY_POS_VIEW: u32 = 0x0100_0000;
pub const KEY_POS_WRITE: u32 = 0x0400_0000;
pub const KEY_POS_SEARCH: u32 = 0x0800_0000;
pub const KEY_POS_SETATTR: u32 = 0x2000_0000;
pub const KEY_USR_ALL: u32 = 0x003f_0000;
pub const KEY_USR_VIEW: u32 = 0x0001_0000;
pub const KEY_USR_READ: u32 = 0x0002_0000;
pub const KEY_USR_LINK: u32 = 0x0010_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum KeyPermission {
    View = 0x01,
    Read = 0x02,
    Write = 0x04,
    Search = 0x08,
    Link = 0x10,
    SetAttr = 0x20,
}

/// Select exactly one of the owner, group and other classes, then add the
/// possessor class.  Linux does not grant a general root override here.
pub fn check_permission(key: &KeyRef, cred: &Cred, need: KeyPermission) -> Result<(), SystemError> {
    let state = key.state.lock();
    let ordinary = if state.uid == cred.fsuid {
        state.perm >> 16
    } else if state.gid != Kgid::new(usize::MAX)
        && state.perm & GROUP_ALL != 0
        && (state.gid == cred.fsgid || cred.groups.contains(&state.gid))
    {
        state.perm >> 8
    } else {
        state.perm
    };
    let possessed = if key.is_possessed() {
        state.perm >> 24
    } else {
        0
    };
    if (ordinary | possessed) & need as u32 == need as u32 {
        Ok(())
    } else {
        Err(SystemError::EACCES)
    }
}

/// Linux `key_validate()` deliberately leaves negative instantiation errors
/// to the caller; it checks invalidation, revocation and wall-clock expiry.
pub fn validate_key(key: &KeyRef) -> Result<(), SystemError> {
    let now = realtime_now().tv_sec;
    validate_state(&key.state.lock(), now)
}

/// Reuse the same validity order while a caller already holds the state lock.
pub fn validate_state(state: &KeyState, now: i64) -> Result<(), SystemError> {
    if state.invalidated {
        return Err(SystemError::ENOKEY);
    }
    if state.revoked_at.is_some() {
        return Err(SystemError::EKEYREVOKED);
    }
    if state.expiry.is_some_and(|expiry| now >= expiry) {
        return Err(SystemError::EKEYEXPIRED);
    }
    Ok(())
}
