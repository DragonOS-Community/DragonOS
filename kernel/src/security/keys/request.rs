//! Linux request-key construction and authorization.
//!
//! A pending key is published in a destination ring before the helper starts.
//! This permits a second requester to join the same construction.  The short
//! construction mutex serializes the search/recheck/link transition and the
//! final status transition, but is never held across user access or exec.

use alloc::{boxed::Box, ffi::CString, format, string::ToString, sync::Arc, vec, vec::Vec};
use core::{
    fmt, mem,
    sync::atomic::{compiler_fence, Ordering},
};

use system_error::SystemError;

use crate::{
    libs::mutex::Mutex,
    mm::VirtAddr,
    process::{
        cred::{Cred, INIT_CRED},
        usermodehelper::UserModeHelper,
        ProcessManager,
    },
    syscall::user_access::copy_from_user_protected,
    time::timekeeping::realtime_now,
};

use super::{
    object::{self, KeyFlags, KeyPayload, KeyRef, KeyStatus, KeyStore, KeyType},
    permission::{self, KeyPermission, KEY_USR_VIEW},
    ring, service,
    user::{self, ADD_PAYLOAD_MAX, CALLOUT_MAX, DESCRIPTION_MAX},
    QuotaMode,
};

const NEGATIVE_TIMEOUT: i64 = 60;
const USER_PAYLOAD_MAX: usize = 32_767;
const IOV_MAX: usize = 1024;

/// A kernel-created authorization token.  The requester credential snapshot
/// deliberately contains no authorization token itself, preventing a cycle
/// when a helper recursively calls request_key().
pub struct RequestAuth {
    pub target: KeyRef,
    pub destination: Option<KeyRef>,
    pub requester: Arc<Cred>,
    pub requester_pid: i32,
    pub callout_info: Vec<u8>,
}

impl fmt::Debug for RequestAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestAuth")
            .field("target", &self.target.serial)
            .field(
                "destination",
                &self.destination.as_ref().map(|key| key.serial),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for RequestAuth {
    fn drop(&mut self) {
        for byte in &mut self.callout_info {
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

static CONSTRUCTION: Mutex<()> = Mutex::new(());

fn kind(name: &[u8]) -> Result<KeyType, SystemError> {
    match name {
        b"user" => Ok(KeyType::User),
        b"logon" => Ok(KeyType::Logon),
        b"keyring" => Ok(KeyType::Keyring),
        _ => Err(SystemError::ENOKEY),
    }
}

fn search_roots(cred: &Cred, kind: KeyType, name: &[u8]) -> Result<Option<KeyRef>, SystemError> {
    let mut skipped = None;
    let mut negative = None;
    for root in [
        cred.thread_keyring.as_ref(),
        cred.process_keyring.as_ref(),
        cred.session_keyring.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        match ring::search_partial(root, kind, name, cred, true) {
            Ok(key) => {
                if matches!(&key.state.lock().status, KeyStatus::Negative(_)) {
                    negative.get_or_insert(key);
                } else {
                    return Ok(Some(key));
                }
            }
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK | SystemError::EKEYEXPIRED) => {}
            Err(error) => skipped = Some(error),
        }
    }
    if cred.session_keyring.is_none() {
        let (_, root) = service::user_keyrings(cred)?;
        match ring::search_partial(&root, kind, name, cred, true) {
            Ok(key) => {
                if matches!(&key.state.lock().status, KeyStatus::Negative(_)) {
                    negative.get_or_insert(key);
                } else {
                    return Ok(Some(key));
                }
            }
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK | SystemError::EKEYEXPIRED) => {}
            Err(error) => skipped = Some(error),
        }
    }
    // A helper's token also grants search of the original requester's rings.
    if kind != KeyType::RequestKeyAuth {
        if let Some(auth_key) = cred.request_key_auth.as_ref() {
            if let Ok(auth) = active_auth(auth_key) {
                if !core::ptr::eq(auth.requester.as_ref(), cred) {
                    if let Some(key) = search_roots(&auth.requester, kind, name)? {
                        if matches!(&key.state.lock().status, KeyStatus::Negative(_)) {
                            negative.get_or_insert(key);
                        } else {
                            return Ok(Some(key));
                        }
                    }
                }
            }
        }
    }
    if let Some(key) = negative {
        return Ok(Some(key));
    }
    match skipped {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

fn default_destination(cred: &Cred) -> Result<KeyRef, SystemError> {
    let requestor = cred.request_key_auth.as_ref().and_then(|key| {
        active_auth(key)
            .ok()
            .and_then(|auth| auth.destination.clone())
    });
    if matches!(cred.jit_keyring, 0 | 7) {
        if let Some(ring) = requestor {
            // Linux deliberately bypasses WRITE only for the requestor's
            // destination saved in a valid authorization token.
            return Ok(ring);
        }
    }
    let selected = match cred.jit_keyring {
        4 => service::user_keyrings(cred)?.0,
        5 => service::user_keyrings(cred)?.1,
        0 | 1 | 2 | 3 | 7 => {
            if cred.jit_keyring <= 1 || cred.jit_keyring == 7 {
                if let Some(ring) = cred.thread_keyring.as_ref() {
                    permission::check_permission(ring, cred, KeyPermission::Write)?;
                    return Ok(ring.clone());
                }
            }
            if cred.jit_keyring <= 2 || cred.jit_keyring == 7 {
                if let Some(ring) = cred.process_keyring.as_ref() {
                    permission::check_permission(ring, cred, KeyPermission::Write)?;
                    return Ok(ring.clone());
                }
            }
            if let Some(ring) = cred.session_keyring.as_ref() {
                permission::check_permission(ring, cred, KeyPermission::Write)?;
                return Ok(ring.clone());
            }
            service::user_keyrings(cred)?.1
        }
        _ => return Err(SystemError::EINVAL),
    };
    permission::check_permission(&selected, cred, KeyPermission::Write)?;
    Ok(selected)
}

/// Resolve KEY_SPEC_REQUESTOR_KEYRING from the active authorization token.
pub(crate) fn requestor_destination(cred: &Cred) -> Result<KeyRef, SystemError> {
    let key = cred.request_key_auth.as_ref().ok_or(SystemError::ENOKEY)?;
    active_auth(key)?
        .destination
        .clone()
        .ok_or(SystemError::ENOKEY)
}

fn destination(id: i32, cred: &Cred) -> Result<KeyRef, SystemError> {
    if id == 0 {
        return default_destination(cred);
    }
    if id == -8 {
        let auth = cred.request_key_auth.as_ref().ok_or(SystemError::ENOKEY)?;
        return active_auth(auth)?
            .destination
            .clone()
            .ok_or(SystemError::ENOKEY);
    }
    let key = service::lookup_key(id, true, true, Some(KeyPermission::Write))?;
    if key.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    Ok(key)
}

fn active_auth(key: &KeyRef) -> Result<Arc<RequestAuth>, SystemError> {
    if key.key_type != KeyType::RequestKeyAuth {
        return Err(SystemError::EPERM);
    }
    let state = key.state.lock();
    permission::validate_state(&state, realtime_now().tv_sec)?;
    match &state.payload {
        KeyPayload::RequestKeyAuth(auth) if state.status == KeyStatus::Positive => Ok(auth.clone()),
        _ => Err(SystemError::EKEYREVOKED),
    }
}

fn key_outcome(key: &KeyRef) -> Result<usize, SystemError> {
    match key.wait_until_instantiated()? {
        KeyStatus::Positive => {
            permission::validate_key(key)?;
            Ok(key.serial as usize)
        }
        KeyStatus::Negative(errno) => Ok((-(errno as isize)) as usize),
        KeyStatus::Uninstantiated => Err(SystemError::EIO),
    }
}

pub fn request_key_from_user(args: [usize; 4]) -> Result<usize, SystemError> {
    let type_name = user::copy_type_name(args[0] as *const u8)?;
    let description = user::copy_cstr_bytes(args[1] as *const u8, DESCRIPTION_MAX)?;
    if description.is_empty() {
        return Err(SystemError::EINVAL);
    }
    let callout = if args[2] == 0 {
        None
    } else {
        Some(user::copy_cstr_bytes(args[2] as *const u8, CALLOUT_MAX)?)
    };
    let key_type = kind(&type_name)?;
    if key_type == KeyType::Logon
        && (description.first() == Some(&b':') || !description.contains(&b':'))
    {
        return Err(SystemError::EINVAL);
    }
    let dest_id = args[3] as i32;
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    let explicit = if dest_id == 0 {
        None
    } else {
        Some(destination(dest_id, &cred)?)
    };

    if let Some(found) = search_roots(&cred, key_type, &description)? {
        let negative_errno = match &found.state.lock().status {
            KeyStatus::Negative(errno) => Some(*errno),
            _ => None,
        };
        if let Some(errno) = negative_errno {
            return Ok((-(errno as isize)) as usize);
        }
        if let Some(ring) = explicit.as_ref() {
            permission::check_permission(&found, &cred, KeyPermission::Link)?;
            ring::link(ring, &found)?;
        }
        return key_outcome(&found);
    }
    let callout = callout.ok_or(SystemError::ENOKEY)?;
    if key_type == KeyType::Keyring {
        return Err(SystemError::EPERM);
    }
    let dest = if let Some(ring) = explicit {
        ring
    } else {
        default_destination(&cred)?
    };
    let _construction = CONSTRUCTION.lock();
    if let Some(found) = search_roots(&cred, key_type, &description)? {
        // A concurrent constructor may have published the key after our
        // first search.  Linux links that key into this request's destination
        // even when the destination differs from the first request's ring.
        ring::link(&dest, &found)?;
        drop(_construction);
        return key_outcome(&found);
    }
    // Linux construct_alloc_key(): POS VIEW/SEARCH/LINK/SETATTR, POS WRITE
    // when the type supports update, POS READ only for readable types.
    let perm = 0x0100_0000
        | 0x0400_0000
        | 0x0800_0000
        | 0x1000_0000
        | 0x2000_0000
        | 0x0001_0000
        | if key_type == KeyType::User {
            0x0200_0000
        } else {
            0
        };
    let pending = KeyStore::allocate(
        key_type,
        description,
        cred.fsuid,
        cred.fsgid,
        perm,
        QuotaMode::Limited,
        KeyFlags::USER_CONSTRUCT,
    )?;
    ring::link(&dest, &pending)?;
    drop(_construction);

    if let Err(error) = start_upcall(&pending, &dest, &cred, callout) {
        let _ = finish_pending(
            &pending,
            None,
            None,
            Some(SystemError::ENOKEY as i32),
            NEGATIVE_TIMEOUT,
            None,
        );
        return Err(error);
    }
    key_outcome(&pending)
}

fn start_upcall(
    target: &KeyRef,
    destination: &KeyRef,
    cred: &Arc<Cred>,
    callout: Vec<u8>,
) -> Result<(), SystemError> {
    let mut requester = (**cred).clone();
    let mut requester_pid = ProcessManager::current_pcb().raw_pid().data() as i32;
    if let Some(auth_key) = requester.request_key_auth.take() {
        let parent = active_auth(&auth_key)?;
        requester_pid = parent.requester_pid;
        requester = (*parent.requester).clone();
    }
    requester.request_key_auth = None;
    let requester = Cred::try_new_arc(requester)?;
    let auth_data = Arc::try_new(RequestAuth {
        target: target.clone(),
        destination: Some(destination.clone()),
        requester,
        requester_pid,
        callout_info: callout,
    })
    .map_err(|_| SystemError::ENOMEM)?;
    let auth_key = KeyStore::allocate(
        KeyType::RequestKeyAuth,
        format!("{:x}", target.serial).into_bytes(),
        cred.fsuid,
        cred.fsgid,
        0x1b00_0000 | KEY_USR_VIEW,
        QuotaMode::Uncharged,
        KeyFlags::empty(),
    )?;
    {
        let mut state = auth_key.state.lock();
        state.payload = KeyPayload::RequestKeyAuth(auth_data);
        state.status = KeyStatus::Positive;
        state.quota.mark_instantiated();
    }
    let requestor_ring = service::create_requestor_keyring(cred, target.serial)?;
    ring::link(&requestor_ring, &auth_key)?;

    let mut helper_cred = (**INIT_CRED).clone();
    helper_cred.session_keyring = Some(requestor_ring);
    helper_cred.thread_keyring = None;
    helper_cred.process_keyring = None;
    helper_cred.request_key_auth = None;
    let helper_cred = Cred::try_new_arc(helper_cred)?;
    let session_serial = match cred.session_keyring.as_ref() {
        Some(session) => session.serial,
        None => service::user_keyrings(cred)?.1.serial,
    };
    let args = [
        "/sbin/request-key".to_string(),
        "create".to_string(),
        target.serial.to_string(),
        cred.fsuid.data().to_string(),
        cred.fsgid.data().to_string(),
        cred.thread_keyring
            .as_ref()
            .map_or(0, |key| key.serial)
            .to_string(),
        cred.process_keyring
            .as_ref()
            .map_or(0, |key| key.serial)
            .to_string(),
        session_serial.to_string(),
    ];
    let argv = args
        .into_iter()
        .map(|s| CString::new(s).map_err(|_| SystemError::EINVAL))
        .collect::<Result<Vec<_>, _>>()?;
    let envp = vec![
        CString::new("HOME=/").map_err(|_| SystemError::EINVAL)?,
        CString::new("PATH=/sbin:/bin:/usr/sbin:/usr/bin").map_err(|_| SystemError::EINVAL)?,
    ];
    let target_on_exit = target.clone();
    let auth_on_exit = auth_key.clone();
    let callback = Box::new(move |_status: &Result<i32, SystemError>| {
        let _ = finish_pending(
            &target_on_exit,
            None,
            None,
            Some(SystemError::ENOKEY as i32),
            NEGATIVE_TIMEOUT,
            None,
        );
        revoke_auth(&auth_on_exit);
    });
    UserModeHelper::start_with_context(
        "/sbin/request-key".to_string(),
        argv,
        envp,
        Some(helper_cred),
        Some(callback),
    )?;
    Ok(())
}

/// Completes at most one construction.  A user helper's explicit result wins
/// over its later exit/reap callback.  An optional link is installed before
/// publishing the final status so awakened waiters observe a cached key.
fn finish_pending(
    target: &KeyRef,
    auth: Option<&KeyRef>,
    mut payload: Option<PayloadBuffer>,
    negative_errno: Option<i32>,
    negative_timeout: i64,
    destination: Option<&KeyRef>,
) -> Result<(), SystemError> {
    let _guard = CONSTRUCTION.lock();
    // Keep the authorization state locked through the commit.  A concurrent
    // revoke must not invalidate the token after `current_auth()` but before
    // this operation changes the target key.
    let auth_state = auth.map(|key| key.state.lock());
    if let Some(state) = auth_state.as_ref() {
        permission::validate_state(state, realtime_now().tv_sec)?;
        match &state.payload {
            KeyPayload::RequestKeyAuth(data)
                if state.status == KeyStatus::Positive && data.target.serial == target.serial => {}
            _ => return Err(SystemError::EKEYREVOKED),
        }
    }
    let mut state = target.state.lock();
    if state.status != KeyStatus::Uninstantiated {
        return Err(SystemError::EBUSY);
    }
    // Linux completes construction even if a timeout was set on the pending
    // key and has since elapsed.  The waiter checks validity after wakeup;
    // rejecting completion here would leave it asleep forever.
    if let Some(bytes) = payload.as_ref() {
        state
            .quota
            .resize(target.description.len() + 1 + bytes.0.len())?;
    }
    if let Some(dest) = destination {
        if let Err(error) = ring::link(dest, target) {
            if payload.is_some() {
                state.quota.resize(target.description.len() + 1)?;
            }
            return Err(error);
        }
    }
    if let Some(errno) = negative_errno {
        state.status = KeyStatus::Negative(errno);
        state.expiry = Some(realtime_now().tv_sec.saturating_add(negative_timeout));
    } else {
        let bytes = payload
            .take()
            .expect("positive request needs a payload")
            .into_vec();
        state.payload = match target.key_type {
            KeyType::User => KeyPayload::User(bytes),
            KeyType::Logon => KeyPayload::Logon(bytes),
            _ => return Err(SystemError::EINVAL),
        };
        state.status = KeyStatus::Positive;
    }
    state.flags.remove(KeyFlags::USER_CONSTRUCT);
    state.quota.mark_instantiated();
    drop(state);
    drop(auth_state);
    drop(_guard);
    target.wake_construction_waiters();
    if negative_errno.is_some() {
        object::schedule_gc();
    }
    if let Some(auth_key) = auth {
        revoke_auth(auth_key);
    }
    Ok(())
}

fn revoke_auth(auth_key: &KeyRef) {
    let old = {
        let mut state = auth_key.state.lock();
        state.revoked_at = Some(realtime_now().tv_sec);
        mem::replace(&mut state.payload, KeyPayload::Uninstantiated)
    };
    drop(old);
    object::schedule_gc();
}

/// Temporary payload copied with exception-table protection.  Every failed
/// validation, quota reservation, and link operation erases these bytes.
struct PayloadBuffer(Vec<u8>);

impl PayloadBuffer {
    fn from_user(address: usize, length: usize) -> Result<Self, SystemError> {
        if length > ADD_PAYLOAD_MAX {
            return Err(SystemError::EINVAL);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(length, 0);
        let mut value = Self(bytes);
        if address != 0 && length != 0 {
            unsafe { copy_from_user_protected(&mut value.0, VirtAddr::new(address))? };
        }
        Ok(value)
    }

    fn from_iov(address: usize, count: usize) -> Result<Self, SystemError> {
        if address == 0 {
            return Ok(Self(Vec::new()));
        }
        if count > IOV_MAX {
            return Err(SystemError::EINVAL);
        }
        let mut pieces = Vec::new();
        pieces
            .try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut total = 0usize;
        for index in 0..count {
            let offset = index
                .checked_mul(core::mem::size_of::<UserIovec>())
                .ok_or(SystemError::EFAULT)?;
            let user_address = address.checked_add(offset).ok_or(SystemError::EFAULT)?;
            let mut raw = [0u8; core::mem::size_of::<UserIovec>()];
            unsafe { copy_from_user_protected(&mut raw, VirtAddr::new(user_address))? };
            let pointer = usize::from_ne_bytes(
                raw[..core::mem::size_of::<usize>()]
                    .try_into()
                    .map_err(|_| SystemError::EFAULT)?,
            );
            let length = usize::from_ne_bytes(
                raw[core::mem::size_of::<usize>()..]
                    .try_into()
                    .map_err(|_| SystemError::EFAULT)?,
            );
            total = total.checked_add(length).ok_or(SystemError::EINVAL)?;
            if total > ADD_PAYLOAD_MAX {
                return Err(SystemError::EINVAL);
            }
            pieces.push(UserIovec { pointer, length });
        }
        let mut value = Self::from_user(0, total)?;
        let mut copied = 0usize;
        for piece in pieces {
            if piece.length != 0 {
                unsafe {
                    copy_from_user_protected(
                        &mut value.0[copied..copied + piece.length],
                        VirtAddr::new(piece.pointer),
                    )?
                };
                copied += piece.length;
            }
        }
        Ok(value)
    }

    fn into_vec(mut self) -> Vec<u8> {
        mem::take(&mut self.0)
    }
}

impl Drop for PayloadBuffer {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

#[repr(C)]
struct UserIovec {
    pointer: usize,
    length: usize,
}

fn current_auth(target_serial: i32) -> Result<(KeyRef, Arc<RequestAuth>), SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    let auth_key = cred.request_key_auth.as_ref().ok_or(SystemError::EPERM)?;
    let auth = active_auth(auth_key)?;
    if auth.target.serial != target_serial {
        return Err(SystemError::EPERM);
    }
    Ok((auth_key.clone(), auth))
}

/// Linux permits the active request-key authority to set a timeout on its
/// still-under-construction target even without ordinary SETATTR permission.
pub(crate) fn has_authority_for(target_serial: i32) -> bool {
    current_auth(target_serial).is_ok()
}

fn completion_destination(ring_id: i32, auth: &RequestAuth) -> Result<Option<KeyRef>, SystemError> {
    match ring_id {
        0 => Ok(None),
        value if value > 0 => {
            let ring = service::lookup_key(value, true, true, Some(KeyPermission::Write))?;
            if ring.key_type != KeyType::Keyring {
                return Err(SystemError::ENOTDIR);
            }
            Ok(Some(ring))
        }
        -7 => Err(SystemError::EINVAL),
        -8..=-1 => Ok(auth.destination.clone()),
        _ => Err(SystemError::ENOKEY),
    }
}

fn clear_authority() -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    let original = current.cred();
    if original.request_key_auth.is_none() {
        return Ok(());
    }
    let mut next = (*original).clone();
    next.request_key_auth = None;
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(())
}

fn assume_authority(target_serial: i32) -> Result<usize, SystemError> {
    if target_serial < 0 {
        return Err(SystemError::EINVAL);
    }
    if target_serial == 0 {
        clear_authority()?;
        return Ok(0);
    }
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    let description = format!("{:x}", target_serial);
    let auth_key = search_roots(&cred, KeyType::RequestKeyAuth, description.as_bytes())?
        .ok_or(SystemError::ENOKEY)?;
    let auth = active_auth(&auth_key)?;
    if auth.target.serial != target_serial {
        return Err(SystemError::ENOKEY);
    }
    let mut next = (*cred).clone();
    next.request_key_auth = Some(auth_key);
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(target_serial as usize)
}

fn set_default_keyring(setting: i32) -> Result<usize, SystemError> {
    let current = ProcessManager::current_pcb();
    let old = current.cred().jit_keyring;
    if setting == -1 {
        return Ok(old as usize);
    }
    match setting {
        0 | 3 | 4 | 5 | 7 => {}
        1 => {
            service::resolve_key(-1, true)?;
        }
        2 => {
            service::resolve_key(-2, true)?;
        }
        _ => return Err(SystemError::EINVAL),
    }
    let mut next = (*current.cred()).clone();
    next.jit_keyring = setting;
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(old as usize)
}

enum PayloadSource {
    Contiguous { address: usize, length: usize },
    Vectored { address: usize, count: usize },
}

fn instantiate(
    target_serial: i32,
    source: PayloadSource,
    ring_id: i32,
) -> Result<usize, SystemError> {
    let (auth_key, auth) = current_auth(target_serial)?;
    let payload = match source {
        PayloadSource::Contiguous { address: 0, .. } => PayloadBuffer(Vec::new()),
        PayloadSource::Contiguous { address, length } => PayloadBuffer::from_user(address, length)?,
        PayloadSource::Vectored { address, count } => PayloadBuffer::from_iov(address, count)?,
    };
    if payload.0.is_empty() || payload.0.len() > USER_PAYLOAD_MAX {
        return Err(SystemError::EINVAL);
    }
    let destination = completion_destination(ring_id, &auth)?;
    finish_pending(
        &auth.target,
        Some(&auth_key),
        Some(payload),
        None,
        0,
        destination.as_ref(),
    )?;
    clear_authority()?;
    Ok(0)
}

fn reject(
    target_serial: i32,
    timeout: u32,
    errno: i32,
    ring_id: i32,
) -> Result<usize, SystemError> {
    if errno <= 0 || errno >= 4095 || matches!(errno, 512 | 513 | 514 | 516) {
        return Err(SystemError::EINVAL);
    }
    let (auth_key, auth) = current_auth(target_serial)?;
    let destination = completion_destination(ring_id, &auth)?;
    finish_pending(
        &auth.target,
        Some(&auth_key),
        None,
        Some(errno),
        timeout as i64,
        destination.as_ref(),
    )?;
    clear_authority()?;
    Ok(0)
}

/// Returns `None` only when another keyctl module owns this option.
pub fn dispatch_request_control(
    option: u32,
    args: [usize; 4],
) -> Result<Option<usize>, SystemError> {
    let result = match option {
        12 => instantiate(
            args[0] as i32,
            PayloadSource::Contiguous {
                address: args[1],
                length: args[2],
            },
            args[3] as i32,
        ),
        13 => reject(
            args[0] as i32,
            args[1] as u32,
            SystemError::ENOKEY as i32,
            args[2] as i32,
        ),
        14 => set_default_keyring(args[0] as i32),
        16 => assume_authority(args[0] as i32),
        19 => reject(
            args[0] as i32,
            args[1] as u32,
            args[2] as i32,
            args[3] as i32,
        ),
        20 => instantiate(
            args[0] as i32,
            PayloadSource::Vectored {
                address: args[1],
                count: args[2],
            },
            args[3] as i32,
        ),
        _ => return Ok(None),
    }?;
    Ok(Some(result))
}
