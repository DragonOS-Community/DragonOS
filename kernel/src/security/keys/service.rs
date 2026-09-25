//! Credential-facing keyring creation and ownership hooks.

use alloc::{collections::BTreeMap, format, vec::Vec};

use system_error::SystemError;

use crate::process::{
    cred::{Cred, Kgid, Kuid},
    namespace::user_namespace::{map_id_up, UserNamespace},
    ProcessManager,
};

use super::{
    object::{KeyFlags, KeyPayload, KeyRef, KeyStatus, KeyStore, KeyType},
    permission::{
        self, KeyPermission, KEY_POS_ALL, KEY_POS_SEARCH, KEY_POS_SETATTR, KEY_POS_WRITE,
        KEY_USR_ALL, KEY_USR_READ, KEY_USR_VIEW,
    },
    ring, QuotaMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialKeyringKind {
    Thread,
    Process,
    Session,
}

fn allocate_keyring(
    description: Vec<u8>,
    uid: Kuid,
    gid: Kgid,
    permissions: u32,
    quota_mode: QuotaMode,
    flags: KeyFlags,
) -> Result<KeyRef, SystemError> {
    let key = KeyStore::allocate(
        KeyType::Keyring,
        description,
        uid,
        gid,
        permissions,
        quota_mode,
        flags,
    )?;
    {
        let mut state = key.state.lock();
        state.payload = KeyPayload::Keyring(BTreeMap::new());
        state.status = KeyStatus::Positive;
        state.quota.mark_instantiated();
    }
    Ok(key.with_possession(true))
}

/// Allocate the credential-owned rings with Linux's type-specific quota
/// policy.  An anonymous session ring may exceed quota only on first install.
pub fn create_special_keyring(
    cred: &Cred,
    kind: SpecialKeyringKind,
) -> Result<KeyRef, SystemError> {
    let (description, permissions, quota_mode) = match kind {
        SpecialKeyringKind::Thread => (
            b"_tid".as_slice(),
            KEY_POS_ALL | KEY_USR_VIEW,
            QuotaMode::Overrun,
        ),
        SpecialKeyringKind::Process => (
            b"_pid".as_slice(),
            KEY_POS_ALL | KEY_USR_VIEW,
            QuotaMode::Overrun,
        ),
        SpecialKeyringKind::Session => (
            b"_ses".as_slice(),
            KEY_POS_ALL | KEY_USR_VIEW | KEY_USR_READ,
            if cred.session_keyring.is_none() {
                QuotaMode::Overrun
            } else {
                QuotaMode::Limited
            },
        ),
    };
    allocate_keyring(
        Vec::from(description),
        cred.uid,
        cred.gid,
        permissions,
        quota_mode,
        KeyFlags::empty(),
    )
}

/// The requestor ring is private to one construction and its name includes
/// the target serial, matching the helper's Linux request-key environment.
pub(crate) fn create_requestor_keyring(
    cred: &Cred,
    target_serial: i32,
) -> Result<KeyRef, SystemError> {
    allocate_keyring(
        format!("_req.{target_serial}").into_bytes(),
        cred.uid,
        cred.gid,
        KEY_POS_ALL | KEY_USR_VIEW | KEY_USR_READ,
        QuotaMode::Overrun,
        KeyFlags::empty(),
    )
}

/// Linux `key_fsuid_changed`/`key_fsgid_changed` change permission ownership
/// of a thread keyring in place.  Its original quota owner is unchanged.
pub fn set_key_owner_uid_gid(key: &KeyRef, uid: Kuid, gid: Kgid) {
    let mut state = key.state.lock();
    state.uid = uid;
    state.gid = gid;
}

/// Get the two per-UID rings from the user namespace's uncharged register
/// ring, creating and publishing them together on first use.  The namespace
/// mutex serializes this operation and is the only owner of `.user_reg`.
pub fn user_keyrings(cred: &Cred) -> Result<(KeyRef, KeyRef), SystemError> {
    let ns: &UserNamespace = &cred.user_ns;
    let local_uid = local_uid(ns, cred.uid);
    let user_name = format!("_uid.{local_uid}").into_bytes();
    let session_name = format!("_uid_ses.{local_uid}").into_bytes();
    let mut registry = ns.keyrings.lock();
    if registry.register.is_none() {
        let owner = Kuid::new(ns.inner.lock().owner);
        let register = allocate_keyring(
            b".user_reg".to_vec(),
            owner,
            Kgid::new(usize::MAX),
            KEY_POS_WRITE | KEY_POS_SEARCH | KEY_USR_VIEW | KEY_USR_READ,
            QuotaMode::Uncharged,
            KeyFlags::empty(),
        )?;
        registry.register = Some(register);
    }
    let register = registry.register.as_ref().expect("register initialized");
    let user = match ring::search(register, KeyType::Keyring, &user_name, cred, false) {
        Ok(found) => found,
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
            let created = allocate_keyring(
                user_name,
                cred.uid,
                Kgid::new(usize::MAX),
                (KEY_POS_ALL & !KEY_POS_SETATTR) | KEY_USR_ALL,
                QuotaMode::Limited,
                KeyFlags::UID_KEYRING,
            )?;
            ring::link(register, &created)?;
            created
        }
        Err(error) => return Err(error),
    };
    let session = match ring::search(register, KeyType::Keyring, &session_name, cred, false) {
        Ok(found) => found,
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
            let created = allocate_keyring(
                session_name,
                cred.uid,
                Kgid::new(usize::MAX),
                (KEY_POS_ALL & !KEY_POS_SETATTR) | KEY_USR_ALL,
                QuotaMode::Limited,
                KeyFlags::UID_KEYRING,
            )?;
            ring::link(&created, &user)?;
            ring::link(register, &created)?;
            created
        }
        Err(error) => return Err(error),
    };
    Ok((user, session))
}

fn install_credential_ring(kind: SpecialKeyringKind) -> Result<KeyRef, SystemError> {
    let current = ProcessManager::current_pcb();
    let old = current.cred();
    let mut next = (*old).clone();
    let ring = create_special_keyring(&next, kind)?;
    match kind {
        SpecialKeyringKind::Thread => next.thread_keyring = Some(ring.clone()),
        SpecialKeyringKind::Process => next.process_keyring = Some(ring.clone()),
        SpecialKeyringKind::Session => next.session_keyring = Some(ring.clone()),
    }
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(ring)
}

fn install_user_session_keyring() -> Result<KeyRef, SystemError> {
    let current = ProcessManager::current_pcb();
    let old = current.cred();
    let (_, session) = user_keyrings(&old)?;
    let mut next = (*old).clone();
    next.session_keyring = Some(session.clone());
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(session)
}

pub(crate) fn existing_user_session_keyring(cred: &Cred) -> Option<KeyRef> {
    let name = format!("_uid_ses.{}", local_uid(&cred.user_ns, cred.uid)).into_bytes();
    let registry = cred.user_ns.keyrings.lock();
    ring::search(
        registry.register.as_ref()?,
        KeyType::Keyring,
        &name,
        cred,
        false,
    )
    .ok()
}

fn local_uid(ns: &UserNamespace, uid: Kuid) -> u32 {
    u32::try_from(uid.data())
        .ok()
        .and_then(|uid| map_id_up(&ns.inner.lock().uid_map, uid))
        .unwrap_or(u32::MAX)
}

pub(crate) fn is_possessed_by_process(key: &KeyRef, cred: &Cred) -> bool {
    for root in [
        cred.thread_keyring.as_ref(),
        cred.process_keyring.as_ref(),
        cred.session_keyring.as_ref(),
    ] {
        if root.is_some_and(|root| ring::contains_key_serial(root, key, cred).unwrap_or(false)) {
            return true;
        }
    }
    if cred.session_keyring.is_none() {
        if let Some(root) = existing_user_session_keyring(cred) {
            return ring::contains_key_serial(&root, key, cred).unwrap_or(false);
        }
    }
    false
}

/// Resolve Linux keyring shortcuts and positive serials.  Possession is
/// derived from the caller's credential graph, never from knowing a serial.
/// Status and required permission are checked by the operation after this
/// lookup; some keyctl commands deliberately accept partial/revoked keys.
pub fn resolve_key(id: i32, create: bool) -> Result<KeyRef, SystemError> {
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    match id {
        -1 => cred.thread_keyring.clone().map(Ok).unwrap_or_else(|| {
            if create {
                install_credential_ring(SpecialKeyringKind::Thread)
            } else {
                Err(SystemError::ENOKEY)
            }
        }),
        -2 => cred.process_keyring.clone().map(Ok).unwrap_or_else(|| {
            if create {
                install_credential_ring(SpecialKeyringKind::Process)
            } else {
                Err(SystemError::ENOKEY)
            }
        }),
        -3 => {
            if let Some(ring) = cred.session_keyring.as_ref() {
                if !create || !ring.state.lock().flags.contains(KeyFlags::UID_KEYRING) {
                    return Ok(ring.clone());
                }
            }
            if create {
                install_credential_ring(SpecialKeyringKind::Session)
            } else {
                install_user_session_keyring()
            }
        }
        -4 => Ok(user_keyrings(&cred)?.0),
        -5 => Ok(user_keyrings(&cred)?.1),
        -6 => Err(SystemError::EINVAL),
        -7 => cred.request_key_auth.clone().ok_or(SystemError::ENOKEY),
        -8 => super::request::requestor_destination(&cred),
        serial if serial > 0 => {
            let key = KeyStore::lookup(serial).ok_or(SystemError::ENOKEY)?;
            if is_possessed_by_process(&key, &cred) {
                Ok(key.with_possession(true))
            } else {
                Ok(key)
            }
        }
        _ => Err(SystemError::EINVAL),
    }
}

pub fn lookup_key(
    id: i32,
    create: bool,
    partial: bool,
    permission: Option<KeyPermission>,
) -> Result<KeyRef, SystemError> {
    lookup_key_raw(id, create, partial, permission)
        .map_err(|errno| SystemError::from_posix_errno(errno).unwrap_or(SystemError::EKEYREJECTED))
}

/// Like lookup_key, but retains the exact errno carried by a negative key.
/// SystemError intentionally does not model every Linux errno accepted by
/// KEYCTL_REJECT, so keyctl callers that wait for construction use this API.
pub fn lookup_key_raw(
    id: i32,
    create: bool,
    partial: bool,
    permission: Option<KeyPermission>,
) -> Result<KeyRef, i32> {
    let key = resolve_key(id, create).map_err(|error| error.to_posix_errno())?;
    if !partial {
        match key
            .wait_until_instantiated()
            .map_err(|error| error.to_posix_errno())?
        {
            KeyStatus::Positive => {}
            KeyStatus::Negative(errno) => return Err(-errno),
            KeyStatus::Uninstantiated => return Err(SystemError::EIO.to_posix_errno()),
        }
    }
    permission::validate_key(&key).map_err(|error| error.to_posix_errno())?;
    if let Some(needed) = permission {
        permission::check_permission(&key, &ProcessManager::current_pcb().cred(), needed)
            .map_err(|error| error.to_posix_errno())?;
    }
    Ok(key)
}

/// Join an anonymous or user-namespace-scoped named session keyring.
/// A named lookup never takes ownership through the name index alone; it
/// upgrades a live weak entry and checks SEARCH permission before reuse.
pub fn join_session_keyring(name: Option<Vec<u8>>) -> Result<i32, SystemError> {
    let current = ProcessManager::current_pcb();
    let old = current.cred();
    let named_request = name.is_some();
    let ring = if let Some(name) = name {
        if name.is_empty() {
            return Err(SystemError::EINVAL);
        }
        let mut named = old.user_ns.keyrings.lock();
        let existing = named.named.get(&name).and_then(KeyStore::lookup_weak);
        let existing = existing.filter(|key| {
            let visible = {
                let state = key.state.lock();
                !state.invalidated
                    && state.revoked_at.is_none()
                    && u32::try_from(state.quota.account().uid().data())
                        .ok()
                        .and_then(|uid| map_id_up(&old.user_ns.inner.lock().uid_map, uid))
                        .is_some()
            };
            visible && permission::check_permission(key, &old, KeyPermission::Search).is_ok()
        });
        match existing {
            Some(key) => key,
            None => {
                let key = allocate_keyring(
                    name.clone(),
                    old.uid,
                    old.gid,
                    KEY_POS_ALL | KEY_USR_VIEW | KEY_USR_READ | super::permission::KEY_USR_LINK,
                    QuotaMode::Limited,
                    KeyFlags::empty(),
                )?;
                named.named.insert(name, key.downgrade());
                key
            }
        }
    } else {
        create_special_keyring(&old, SpecialKeyringKind::Session)?
    };
    if named_request
        && old
            .session_keyring
            .as_ref()
            .is_some_and(|current| current.serial == ring.serial)
    {
        return Ok(0);
    }
    // A session ring installed in credentials is possessed even when the
    // named lookup came from the weak namespace index (which returns an
    // unpossessed serial reference).
    let ring = ring.with_possession(true);
    let mut next = (*old).clone();
    next.session_keyring = Some(ring.clone());
    current.commit_cred(Cred::try_new_arc(next)?)?;
    Ok(ring.serial)
}

/// Queue this task's session ring for installation by its real creating
/// parent at the next return-to-user boundary.  The child never changes the
/// parent's credentials directly; the parent clones its then-current Cred.
pub fn session_to_parent() -> Result<usize, SystemError> {
    let ring = lookup_key(-3, false, false, Some(KeyPermission::Link))?;
    let current = ProcessManager::current_pcb();
    // DragonOS keeps the Linux thread-level real-parent relationship here;
    // real_parent_pcb is currently leader-normalized for other consumers.
    let parent = current.fork_parent_pcb().ok_or(SystemError::EPERM)?;
    if parent.is_global_init() || parent.basic().user_vm().is_none() || parent.is_exited() {
        return Err(SystemError::EPERM);
    }

    let my_cred = current.cred();
    let parent_cred = parent.cred();
    let uid = my_cred.euid;
    let gid = my_cred.egid;
    if ring.state.lock().uid != uid
        || parent_cred
            .session_keyring
            .as_ref()
            .is_some_and(|existing| existing.state.lock().uid != uid)
    {
        return Err(SystemError::EPERM);
    }
    let parent_threads = parent.threads_read_irqsave();
    if !parent_threads
        .group_leader()
        .is_some_and(|leader| alloc::sync::Arc::ptr_eq(&leader, &parent))
        || parent_threads.group_tasks().iter().any(|member| {
            member
                .upgrade()
                .is_some_and(|task| task.is_live_thread_group_member())
        })
    {
        return Err(SystemError::EPERM);
    }
    // The owner checks above take key mutexes, so do them before disabling
    // IRQs for the thread-list read lock.  If credentials changed meanwhile,
    // require the child to retry with a fresh authorization snapshot.
    if !alloc::sync::Arc::ptr_eq(&parent_cred, &parent.cred()) {
        return Err(SystemError::EPERM);
    }
    if alloc::sync::Arc::ptr_eq(&my_cred, &parent_cred)
        || parent_cred
            .session_keyring
            .as_ref()
            .is_some_and(|existing| existing.serial == ring.serial)
    {
        return Ok(0);
    }
    if parent_cred.uid != uid
        || parent_cred.euid != uid
        || parent_cred.suid != uid
        || parent_cred.gid != gid
        || parent_cred.egid != gid
        || parent_cred.sgid != gid
    {
        return Err(SystemError::EPERM);
    }
    // The leader's read lock also serializes publishing new CLONE_THREAD
    // members.  A replacement of pending work is O(1) under the spin lock.
    let previous = parent.queue_session_keyring(ring);
    drop(parent_threads);
    drop(previous);
    Ok(0)
}

/// Runs in the parent task, outside the child's context.  Only the session
/// reference is transferred; UID/caps/other keyrings remain current.
pub fn apply_pending_session_keyring() {
    let current = ProcessManager::current_pcb();
    let Some(ring) = current.take_pending_session_keyring() else {
        return;
    };
    if current.is_exited() {
        return;
    }
    let mut next = (*current.cred()).clone();
    next.session_keyring = Some(ring.clone());
    match Cred::try_new_arc(next).and_then(|new| current.commit_cred(new)) {
        Ok(()) => {}
        Err(error) => {
            current.restore_pending_session_keyring_if_empty(ring);
            log::warn!("session-keyring parent handoff deferred: {:?}", error);
        }
    }
}
