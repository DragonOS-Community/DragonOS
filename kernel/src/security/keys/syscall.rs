//! Linux CONFIG_KEYS syscall boundary.  User pointers are copied before any
//! key-store or ring lock is taken; the key objects own only kernel buffers.

use alloc::{collections::BTreeMap, string::ToString, vec::Vec};
use core::mem;

use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_ADD_KEY, SYS_KEYCTL, SYS_REQUEST_KEY},
    },
    libs::mutex::Mutex,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
    time::timekeeping::realtime_now,
};

use super::{
    control,
    object::{KeyFlags, KeyPayload, KeyStatus, KeyStore, KeyType},
    permission::{self, KeyPermission},
    request, ring, service,
    user::{copy_cstr_bytes, copy_type_name, SensitiveBytes, ADD_PAYLOAD_MAX, DESCRIPTION_MAX},
    QuotaMode,
};

const USER_PAYLOAD_MAX: usize = 32_767;
const DEFAULT_PERMISSION: u32 = 0x3901_0000;
const DEFAULT_READ: u32 = 0x0200_0000;
const DEFAULT_WRITE: u32 = 0x0400_0000;

// Serialize the direct-index search and insertion for concurrent add_key()
// calls.  This lock is not held during user-copy or helper execution.
static ADD_KEY: Mutex<()> = Mutex::new(());

fn key_type(name: &[u8]) -> Result<KeyType, SystemError> {
    match name {
        b"user" => Ok(KeyType::User),
        b"logon" => Ok(KeyType::Logon),
        b"keyring" => Ok(KeyType::Keyring),
        _ => Err(SystemError::ENODEV),
    }
}

fn add_key(args: &[usize]) -> Result<usize, SystemError> {
    let payload_len = args[3];
    if payload_len > ADD_PAYLOAD_MAX {
        return Err(SystemError::EINVAL);
    }
    let type_name = copy_type_name(args[0] as *const u8)?;
    let key_type = key_type(&type_name)?;
    let description = if args[1] == 0 {
        Vec::new()
    } else {
        copy_cstr_bytes(args[1] as *const u8, DESCRIPTION_MAX)?
    };
    if description.is_empty() {
        return Err(SystemError::EINVAL);
    }
    if key_type == KeyType::Keyring && description[0] == b'.' {
        return Err(SystemError::EPERM);
    }
    if key_type == KeyType::Logon
        && description
            .iter()
            .position(|byte| *byte == b':')
            .is_none_or(|index| index == 0)
    {
        return Err(SystemError::EINVAL);
    }
    if key_type == KeyType::Keyring && payload_len != 0 {
        return Err(SystemError::EINVAL);
    }
    if key_type != KeyType::Keyring && (payload_len == 0 || payload_len > USER_PAYLOAD_MAX) {
        return Err(SystemError::EINVAL);
    }
    let payload =
        SensitiveBytes::copy_from_user(args[2] as *const u8, payload_len, ADD_PAYLOAD_MAX)?;
    let destination = service::lookup_key(args[4] as i32, true, false, Some(KeyPermission::Write))?;
    if destination.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }

    let _add = ADD_KEY.lock();
    if key_type != KeyType::Keyring {
        if let Some(existing) = ring::direct_link(&destination, key_type, &description)? {
            let mut state = existing.state.lock();
            if state.status == KeyStatus::Positive
                && permission::validate_state(&state, realtime_now().tv_sec).is_ok()
            {
                drop(state);
                permission::check_permission(
                    &existing,
                    &ProcessManager::current_pcb().cred(),
                    KeyPermission::Write,
                )?;
                state = existing.state.lock();
                permission::validate_state(&state, realtime_now().tv_sec)?;
                state
                    .quota
                    .resize(existing.description.len() + 1 + payload_len)?;
                let new_payload = match key_type {
                    KeyType::User => KeyPayload::User(payload.into_vec()),
                    KeyType::Logon => KeyPayload::Logon(payload.into_vec()),
                    _ => unreachable!(),
                };
                let old = mem::replace(&mut state.payload, new_payload);
                state.expiry = None;
                drop(state);
                drop(old);
                return Ok(existing.serial as usize);
            }
        }
    }

    let cred = ProcessManager::current_pcb().cred();
    let mut permissions = DEFAULT_PERMISSION;
    if key_type != KeyType::Logon {
        permissions |= DEFAULT_READ;
    }
    permissions |= DEFAULT_WRITE;
    let key = KeyStore::allocate(
        key_type,
        description,
        cred.fsuid,
        cred.fsgid,
        permissions,
        QuotaMode::Limited,
        KeyFlags::empty(),
    )?;
    {
        let mut state = key.state.lock();
        if key_type != KeyType::Keyring {
            state
                .quota
                .resize(key.description.len() + 1 + payload_len)?;
        }
        state.payload = match key_type {
            KeyType::User => KeyPayload::User(payload.into_vec()),
            KeyType::Logon => KeyPayload::Logon(payload.into_vec()),
            KeyType::Keyring => KeyPayload::Keyring(BTreeMap::new()),
            KeyType::RequestKeyAuth => unreachable!(),
        };
        state.status = KeyStatus::Positive;
        state.quota.mark_instantiated();
    }
    ring::link(&destination, &key)?;
    Ok(key.serial as usize)
}

pub struct SysAddKey;

impl Syscall for SysAddKey {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        add_key(args)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "ringid",
            (args[4] as i32).to_string(),
        )]
    }
}

pub struct SysRequestKey;

impl Syscall for SysRequestKey {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        request::request_key_from_user([args[0], args[1], args[2], args[3]])
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "ringid",
            (args[3] as i32).to_string(),
        )]
    }
}

pub struct SysKeyctl;

impl Syscall for SysKeyctl {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let option = args[0] as u32;
        let options = [args[1], args[2], args[3], args[4]];
        if let Some(result) = control::dispatch_control(option, options)? {
            return Ok(result);
        }
        if let Some(result) = request::dispatch_request_control(option, options)? {
            return Ok(result);
        }
        if option == 18 {
            return service::session_to_parent();
        }
        // Core CONFIG_KEYS is enabled; optional key types and facilities are
        // absent exactly as on a Linux kernel built without their Kconfig.
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("option", args[0].to_string())]
    }
}

syscall_table_macros::declare_syscall!(SYS_ADD_KEY, SysAddKey);
syscall_table_macros::declare_syscall!(SYS_REQUEST_KEY, SysRequestKey);
syscall_table_macros::declare_syscall!(SYS_KEYCTL, SysKeyctl);
