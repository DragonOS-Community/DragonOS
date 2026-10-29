//! Keyring links and recursive search.
//!
//! The B-tree is indexed by the Linux `(type, description)` key.  A link is
//! an unpossessed counted reference; possession is supplied by the search
//! path.  The graph mutex covers cycle detection through insertion of a
//! keyring edge.  Non-keyring links do not contend on that mutex.

use alloc::{collections::BTreeMap, vec::Vec};
use core::{mem, ops::Bound};

use system_error::SystemError;

use crate::{libs::mutex::Mutex, process::cred::Cred, time::timekeeping::realtime_now};

use super::{
    object::{KeyPayload, KeyRef, KeyState, KeyStatus, KeyType},
    permission::{check_permission, validate_key, validate_state, KeyPermission},
};

const MAX_SEARCH_DEPTH: usize = 6;
const LINK_BYTES: usize = 4;

/// Only keyring-to-keyring insertion and move require this serialization.
/// Unlink and clear remove graph edges and cannot introduce a cycle.
static LINK_GRAPH: Mutex<()> = Mutex::new(());

fn index_of(key: &KeyRef) -> Result<(KeyType, Vec<u8>), SystemError> {
    let mut description = Vec::new();
    description
        .try_reserve_exact(key.description.len())
        .map_err(|_| SystemError::ENOMEM)?;
    description.extend_from_slice(&key.description);
    Ok((key.key_type, description))
}

fn ring_links(state: &KeyState) -> Result<&BTreeMap<(KeyType, Vec<u8>), KeyRef>, SystemError> {
    match &state.payload {
        KeyPayload::Keyring(links) => Ok(links),
        _ => Err(SystemError::ENOTDIR),
    }
}

fn ring_links_mut(
    state: &mut KeyState,
) -> Result<&mut BTreeMap<(KeyType, Vec<u8>), KeyRef>, SystemError> {
    match &mut state.payload {
        KeyPayload::Keyring(links) => Ok(links),
        _ => Err(SystemError::ENOTDIR),
    }
}

fn link_charge(description_len: usize, link_count: usize) -> Result<usize, SystemError> {
    description_len
        .checked_add(1)
        .and_then(|n| {
            link_count
                .checked_mul(LINK_BYTES)
                .and_then(|bytes| n.checked_add(bytes))
        })
        .ok_or(SystemError::EOVERFLOW)
}

/// Read one child at a time so cycle detection does not clone an entire large
/// keyring.  This helper never holds a parent and child state lock together.
fn next_child(ring: &KeyRef, after: Vec<u8>) -> Result<Option<(Vec<u8>, KeyRef)>, SystemError> {
    let state = ring.state.lock();
    if state.invalidated || state.revoked_at.is_some() {
        return Ok(None);
    }
    let links = ring_links(&state)?;
    let bound = (KeyType::Keyring, after);
    let Some(((key_type, description), child)) = links
        .range((Bound::Excluded(&bound), Bound::Unbounded))
        .next()
    else {
        return Ok(None);
    };
    if *key_type != KeyType::Keyring {
        return Ok(None);
    }
    let mut next_description = Vec::new();
    next_description
        .try_reserve_exact(description.len())
        .map_err(|_| SystemError::ENOMEM)?;
    next_description.extend_from_slice(description);
    Ok(Some((next_description, child.with_possession(false))))
}

/// The caller holds `LINK_GRAPH` until it commits the prospective edge.
fn detect_cycle(parent: &KeyRef, child: &KeyRef) -> Result<(), SystemError> {
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(MAX_SEARCH_DEPTH + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    stack.push((child.with_possession(false), 0usize, Vec::new()));

    while let Some((ring, depth, cursor)) = stack.pop() {
        if ring.serial == parent.serial {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        if let Some((next_cursor, next_ring)) = next_child(&ring, cursor)? {
            if depth >= MAX_SEARCH_DEPTH {
                return Err(SystemError::ELOOP);
            }
            stack.push((ring, depth, next_cursor));
            stack.push((next_ring, depth + 1, Vec::new()));
        }
    }
    Ok(())
}

/// Insert or atomically replace a link.  The service layer must already have
/// checked WRITE on `ring` and LINK on `key` for a user-requested operation.
pub fn link(ring: &KeyRef, key: &KeyRef) -> Result<(), SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    let index = index_of(key)?;
    let linked_ref = key.with_possession(false);
    let _graph = if key.key_type == KeyType::Keyring {
        Some(LINK_GRAPH.lock())
    } else {
        None
    };
    if key.key_type == KeyType::Keyring {
        detect_cycle(ring, key)?;
    }

    let now = realtime_now().tv_sec;
    let mut state = ring.state.lock();
    validate_state(&state, now)?;
    if state.restricted {
        return Err(SystemError::EPERM);
    }
    let old_count = ring_links(&state)?.len();
    let replacing = ring_links(&state)?.contains_key(&index);
    if !replacing {
        let new_count = old_count.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        state
            .quota
            .resize(link_charge(ring.description.len(), new_count)?)?;
    }
    let old = ring_links_mut(&mut state)?.insert(index, linked_ref);
    drop(state);
    drop(_graph);
    drop(old);
    Ok(())
}

/// Remove the link with the supplied key index.  Linux's associative lookup
/// uses the key's type and description here, rather than treating serials as
/// positions in the ring.
pub fn unlink(ring: &KeyRef, key: &KeyRef) -> Result<(), SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    let index = index_of(key)?;
    let mut state = ring.state.lock();
    let count = ring_links(&state)?.len();
    if !ring_links(&state)?.contains_key(&index) {
        return Err(SystemError::ENOENT);
    }
    // A decrease cannot hit the quota limit.  Keep accounting and mutation
    // under the same ring state lock so observers never see a torn count.
    state
        .quota
        .resize(link_charge(ring.description.len(), count - 1)?)?;
    let removed = ring_links_mut(&mut state)?.remove(&index);
    drop(state);
    drop(removed);
    Ok(())
}

/// Empty a ring and refund exactly four quota bytes per link.  The removed
/// references are released only after the state lock has been dropped.
pub fn clear(ring: &KeyRef) -> Result<(), SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    let mut state = ring.state.lock();
    ring_links(&state)?;
    state
        .quota
        .resize(link_charge(ring.description.len(), 0)?)?;
    let removed = mem::take(ring_links_mut(&mut state)?);
    drop(state);
    drop(removed);
    Ok(())
}

// A one-off transaction needs both ring states, their immutable descriptions,
// the selected key, and the caller's flags in the same lock scope.
#[allow(clippy::too_many_arguments)]
fn move_locked(
    key: &KeyRef,
    from: &mut KeyState,
    to: &mut KeyState,
    now: i64,
    from_description_len: usize,
    to_description_len: usize,
    index: (KeyType, Vec<u8>),
    exclusive: bool,
) -> Result<(KeyRef, Option<KeyRef>), SystemError> {
    validate_state(from, now)?;
    validate_state(to, now)?;
    if to.restricted {
        return Err(SystemError::EPERM);
    }
    let from_len = ring_links(from)?.len();
    let to_len = ring_links(to)?.len();
    if !ring_links(from)?.contains_key(&index) {
        return Err(SystemError::ENOENT);
    }
    let replacing = ring_links(to)?.contains_key(&index);
    if replacing && exclusive {
        return Err(SystemError::EEXIST);
    }
    if !replacing {
        let new_count = to_len.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        to.quota
            .resize(link_charge(to_description_len, new_count)?)?;
    }
    // All fallible checks are complete before removing the source link.
    from.quota
        .resize(link_charge(from_description_len, from_len - 1)?)?;
    let removed = ring_links_mut(from)?
        .remove(&index)
        .expect("checked source link");
    let replaced = ring_links_mut(to)?.insert(index, key.with_possession(false));
    Ok((removed, replaced))
}

/// Move a link transactionally with both ring locks in serial order.  A
/// target quota or restriction failure leaves the source link in place.
pub fn move_key(
    key: &KeyRef,
    from: &KeyRef,
    to: &KeyRef,
    exclusive: bool,
) -> Result<(), SystemError> {
    if from.key_type != KeyType::Keyring || to.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    if from.serial == to.serial {
        return Ok(());
    }
    let index = index_of(key)?;
    let _graph = if key.key_type == KeyType::Keyring {
        Some(LINK_GRAPH.lock())
    } else {
        None
    };
    if key.key_type == KeyType::Keyring {
        detect_cycle(to, key)?;
    }
    let now = realtime_now().tv_sec;

    let removed = if from.serial < to.serial {
        let mut source = from.state.lock();
        let mut target = to.state.lock();
        move_locked(
            key,
            &mut source,
            &mut target,
            now,
            from.description.len(),
            to.description.len(),
            index,
            exclusive,
        )?
    } else {
        let mut target = to.state.lock();
        let mut source = from.state.lock();
        move_locked(
            key,
            &mut source,
            &mut target,
            now,
            from.description.len(),
            to.description.len(),
            index,
            exclusive,
        )?
    };
    drop(_graph);
    drop(removed);
    Ok(())
}

/// Install the core reject-all restriction.  The service layer resolves a
/// nonempty requested restriction type and enforces keyring SETATTR.
pub fn restrict_reject_all(ring: &KeyRef) -> Result<(), SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    let mut state = ring.state.lock();
    if state.restricted {
        return Err(SystemError::EEXIST);
    }
    state.restricted = true;
    Ok(())
}

/// Find one direct link by the associative index used by add_key().  Unlike
/// request_key search, this does not recurse into child keyrings.
pub fn direct_link(
    ring: &KeyRef,
    key_type: KeyType,
    description: &[u8],
) -> Result<Option<KeyRef>, SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    let mut name = Vec::new();
    name.try_reserve_exact(description.len())
        .map_err(|_| SystemError::ENOMEM)?;
    name.extend_from_slice(description);
    let state = ring.state.lock();
    let links = ring_links(&state)?;
    Ok(links
        .get(&(key_type, name))
        .map(|key| key.with_possession(ring.is_possessed())))
}

/// One bounded pass over a ring's links for the background key collector.
/// `cursor` is the last index examined and survives between workqueue batches.
/// Invalidation is reaped immediately; revocation and ordinary expiry retain
/// their links until Linux's `gc_delay` has elapsed.  The ring and linked key
/// state locks are never held together, including during the final removal.
pub fn prune_invalid_links(
    ring: &KeyRef,
    now: i64,
    gc_delay: u32,
    cursor: &mut Option<(KeyType, Vec<u8>)>,
    max_scan: usize,
) -> Result<(usize, bool), SystemError> {
    if ring.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    if max_scan == 0 {
        return Err(SystemError::EINVAL);
    }
    let mut removed_count = 0;
    for _ in 0..max_scan {
        let next = {
            let state = ring.state.lock();
            if state.invalidated || state.revoked_at.is_some() {
                *cursor = None;
                return Ok((removed_count, true));
            }
            let links = ring_links(&state)?;
            let entry = if let Some(after) = cursor.as_ref() {
                links
                    .range((Bound::Excluded(after), Bound::Unbounded))
                    .next()
            } else {
                links.iter().next()
            };
            match entry {
                Some(((kind, description), child)) => {
                    let mut name = Vec::new();
                    name.try_reserve_exact(description.len())
                        .map_err(|_| SystemError::ENOMEM)?;
                    name.extend_from_slice(description);
                    Some(((*kind, name), child.with_possession(false)))
                }
                None => None,
            }
        };
        let Some((index, child)) = next else {
            *cursor = None;
            return Ok((removed_count, true));
        };
        let dead = {
            let state = child.state.lock();
            dead_for_gc(&state, now, gc_delay)
        };
        if dead {
            let mut state = ring.state.lock();
            let linked = ring_links(&state)?
                .get(&index)
                .is_some_and(|current| current.serial == child.serial);
            let removed = if linked {
                let count = ring_links(&state)?.len();
                state
                    .quota
                    .resize(link_charge(ring.description.len(), count - 1)?)?;
                ring_links_mut(&mut state)?.remove(&index)
            } else {
                None
            };
            drop(state);
            if removed.is_some() {
                removed_count += 1;
            }
            drop(removed);
        }
        *cursor = Some(index);
    }
    Ok((removed_count, false))
}

fn dead_for_gc(state: &KeyState, now: i64, gc_delay: u32) -> bool {
    state.invalidated
        || state
            .revoked_at
            .is_some_and(|time| time.saturating_add(gc_delay as i64) <= now)
        || state
            .expiry
            .is_some_and(|time| time.saturating_add(gc_delay as i64) <= now)
}

/// Search a ring tree with Linux's maximum nesting depth.  A matching serial
/// remains subject to SEARCH permission, validity and negative-key status.
pub fn search(
    root: &KeyRef,
    key_type: KeyType,
    description: &[u8],
    cred: &Cred,
    recurse: bool,
) -> Result<KeyRef, SystemError> {
    search_inner(root, key_type, description, cred, recurse, false)
}

/// Locate a matching key without interpreting its negative result.  Request
/// syscalls must preserve the original KEYCTL_REJECT errno, including numbers
/// not represented in DragonOS's `SystemError` enum.
pub fn search_partial(
    root: &KeyRef,
    key_type: KeyType,
    description: &[u8],
    cred: &Cred,
    recurse: bool,
) -> Result<KeyRef, SystemError> {
    search_inner(root, key_type, description, cred, recurse, true)
}

fn search_inner(
    root: &KeyRef,
    key_type: KeyType,
    description: &[u8],
    cred: &Cred,
    recurse: bool,
    accept_negative: bool,
) -> Result<KeyRef, SystemError> {
    if root.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    check_permission(root, cred, KeyPermission::Search)?;
    let mut requested_description = Vec::new();
    requested_description
        .try_reserve_exact(description.len())
        .map_err(|_| SystemError::ENOMEM)?;
    requested_description.extend_from_slice(description);
    let index = (key_type, requested_description);
    let possessed = root.is_possessed();
    let mut skipped = SystemError::EAGAIN_OR_EWOULDBLOCK;
    let mut negative_candidate = None;
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(MAX_SEARCH_DEPTH + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    stack.push((root.clone(), 0usize, Vec::new(), true));

    while let Some((ring, depth, cursor, first_visit)) = stack.pop() {
        if first_visit {
            if ring.key_type == key_type && ring.description == description {
                match candidate(&ring, cred, possessed, accept_negative) {
                    Ok(()) => {
                        let found = ring.with_possession(possessed);
                        if accept_negative && is_negative(&found) {
                            if negative_candidate.is_none() {
                                negative_candidate = Some(found);
                            }
                        } else {
                            return Ok(found);
                        }
                    }
                    Err(error) => skipped = error,
                }
            }
            let direct = {
                let state = ring.state.lock();
                if state.invalidated || state.revoked_at.is_some() {
                    None
                } else {
                    ring_links(&state)?
                        .get(&index)
                        .map(|key| key.with_possession(possessed))
                }
            };
            if let Some(key) = direct {
                match candidate(&key, cred, possessed, accept_negative) {
                    Ok(()) => {
                        if accept_negative && is_negative(&key) {
                            if negative_candidate.is_none() {
                                negative_candidate = Some(key);
                            }
                        } else {
                            return Ok(key);
                        }
                    }
                    Err(error) => skipped = error,
                }
            }
        }
        if !recurse || depth >= MAX_SEARCH_DEPTH {
            continue;
        }
        if let Some((next_cursor, next_ring)) = next_child(&ring, cursor)? {
            let next_ring = next_ring.with_possession(possessed);
            if check_permission(&next_ring, cred, KeyPermission::Search).is_err() {
                stack.push((ring, depth, next_cursor, false));
                continue;
            }
            stack.push((ring, depth, next_cursor, false));
            stack.push((next_ring, depth + 1, Vec::new(), true));
        }
    }
    negative_candidate.ok_or(skipped)
}

fn is_negative(key: &KeyRef) -> bool {
    matches!(&key.state.lock().status, KeyStatus::Negative(_))
}

/// Determine whether a key found by serial is possessed through a credential
/// ring.  This is deliberately separate from `search`: Linux's
/// `lookup_user_key()` uses NO_STATE_CHECK here, so negative, revoked and
/// under-construction keys can still carry the possession bit.  Every path
/// still requires SEARCH on the root, intermediate rings and target key.
pub fn contains_key_serial(
    root: &KeyRef,
    target: &KeyRef,
    cred: &Cred,
) -> Result<bool, SystemError> {
    if root.key_type != KeyType::Keyring {
        return Err(SystemError::ENOTDIR);
    }
    check_permission(root, cred, KeyPermission::Search)?;
    let index = index_of(target)?;
    let possessed = root.is_possessed();
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(MAX_SEARCH_DEPTH + 1)
        .map_err(|_| SystemError::ENOMEM)?;
    stack.push((root.clone(), 0usize, Vec::new(), true));

    while let Some((ring, depth, cursor, first_visit)) = stack.pop() {
        if first_visit {
            if ring.serial == target.serial {
                let match_ref = ring.with_possession(possessed);
                if check_permission(&match_ref, cred, KeyPermission::Search).is_ok() {
                    return Ok(true);
                }
            }
            let direct = {
                let state = ring.state.lock();
                if state.invalidated || state.revoked_at.is_some() {
                    None
                } else {
                    ring_links(&state)?.get(&index).and_then(|key| {
                        (key.serial == target.serial).then(|| key.with_possession(possessed))
                    })
                }
            };
            if let Some(key) = direct {
                if check_permission(&key, cred, KeyPermission::Search).is_ok() {
                    return Ok(true);
                }
            }
        }
        if depth >= MAX_SEARCH_DEPTH {
            continue;
        }
        if let Some((next_cursor, next_ring)) = next_child(&ring, cursor)? {
            let next_ring = next_ring.with_possession(possessed);
            stack.push((ring, depth, next_cursor, false));
            if check_permission(&next_ring, cred, KeyPermission::Search).is_ok() {
                stack.push((next_ring, depth + 1, Vec::new(), true));
            }
        }
    }
    Ok(false)
}

fn candidate(
    key: &KeyRef,
    cred: &Cred,
    possessed: bool,
    accept_negative: bool,
) -> Result<(), SystemError> {
    let key = key.with_possession(possessed);
    validate_key(&key)?;
    check_permission(&key, cred, KeyPermission::Search)?;
    let result = match &key.state.lock().status {
        KeyStatus::Negative(_) if accept_negative => Ok(()),
        KeyStatus::Negative(error) => {
            Err(SystemError::from_posix_errno(-*error).unwrap_or(SystemError::EKEYREJECTED))
        }
        KeyStatus::Uninstantiated | KeyStatus::Positive => Ok(()),
    };
    result
}
