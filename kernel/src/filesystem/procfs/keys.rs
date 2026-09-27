//! Linux 6.6 `/proc/keys` and `/proc/key-users`.
//!
//! Both are seq-style files: each read walks a bounded portion of the live
//! key/account tables, while the fd retains only the produced slice and its
//! resume cursor.  Visibility uses the credential pinned by opening the fd.

use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::Ordering;

use system_error::SystemError;

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_seq,
            ProcfsFilePrivateData,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::{
        cred::{Cred, Kuid},
        namespace::user_namespace::{from_kgid_munged, from_kuid_munged, map_id_up, UserNamespace},
    },
    security::keys::{
        is_possessed_by_process, live_accounts_from,
        object::{KeyPayload, KeyRef, KeyStatus},
        permission::{check_permission, KeyPermission, KEY_POS_VIEW},
        KeyStore, QUOTA_LIMITS,
    },
    time::timekeeping::realtime_now,
};

const KEY_BATCH: usize = 64;

fn mapped_uid(ns: &UserNamespace, uid: Kuid) -> Option<u32> {
    let global = u32::try_from(uid.data()).ok()?;
    map_id_up(&ns.inner.lock().uid_map, global)
}

fn opener(data: &MutexGuard<FilePrivateData>) -> Result<Arc<Cred>, SystemError> {
    let FilePrivateData::Procfs(ProcfsFilePrivateData { open_cred, .. }) = &**data else {
        return Err(SystemError::EINVAL);
    };
    Ok(open_cred.clone())
}

fn timeout_text(expiry: Option<i64>, now: i64) -> alloc::string::String {
    let Some(expiry) = expiry else {
        return String::from("perm");
    };
    if now >= expiry {
        return String::from("expd");
    }
    let seconds = expiry.saturating_sub(now) as u64;
    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 604800 {
        format!("{}d", seconds / 86400)
    } else {
        format!("{}w", seconds / 604800)
    }
}

/// Return false for keys that Linux's `proc_keys_show()` would skip.
fn append_key(key: &KeyRef, cred: &Cred, now: i64, out: &mut Vec<u8>) -> bool {
    // Most visible keys can be authorized from owner/group/other bits alone.
    // Traverse the opening credential's keyring graph only when possessor
    // VIEW could change that result.
    if check_permission(key, cred, KeyPermission::View).is_err() {
        if key.state.lock().perm & KEY_POS_VIEW == 0 || !is_possessed_by_process(key, cred) {
            return false;
        }
        let possessed = key.with_possession(true);
        if check_permission(&possessed, cred, KeyPermission::View).is_err() {
            return false;
        }
    }

    enum Detail {
        Plain,
        Data(usize),
        Ring(usize),
        Auth { pid: i32, callout_len: usize },
    }
    let (
        owner,
        group,
        quota_owner,
        perm,
        status,
        revoked,
        charged,
        constructing,
        invalidated,
        expiry,
        detail,
    ) = {
        let state = key.state.lock();
        let detail = match &state.payload {
            KeyPayload::RequestKeyAuth(auth) => Detail::Auth {
                pid: auth.requester_pid,
                callout_len: auth.callout_info.len(),
            },
            KeyPayload::Keyring(links) => Detail::Ring(links.len()),
            KeyPayload::User(data) | KeyPayload::Logon(data) => Detail::Data(data.len()),
            KeyPayload::Uninstantiated => Detail::Plain,
        };
        (
            state.uid,
            state.gid,
            state.quota.account().uid(),
            state.perm,
            state.status.clone(),
            state.revoked_at.is_some(),
            state.quota.is_charged(),
            state
                .flags
                .contains(crate::security::keys::object::KeyFlags::USER_CONSTRUCT),
            state.invalidated,
            state.expiry,
            detail,
        )
    };
    // Linux walks the serial registry by the quota account UID.  Permission
    // ownership may differ (notably for special rings after setfsuid), and
    // Linux still displays that owner through from_kuid_munged().
    if mapped_uid(&cred.user_ns, quota_owner).is_none() {
        return false;
    }
    let uid = from_kuid_munged(&cred.user_ns, owner);
    let gid = from_kgid_munged(&cred.user_ns, group);
    let state_char = if matches!(status, KeyStatus::Uninstantiated) {
        '-'
    } else {
        'I'
    };
    let negative_char = if matches!(status, KeyStatus::Negative(_)) {
        'N'
    } else {
        '-'
    };
    let flags = [
        state_char,
        if revoked { 'R' } else { '-' },
        '-', // Linux KEY_FLAG_DEAD: deleted types are not supported.
        if charged { 'Q' } else { '-' },
        if constructing { 'U' } else { '-' },
        negative_char,
        if invalidated { 'i' } else { '-' },
    ];
    let timeout = timeout_text(expiry, now);
    // The snapshot itself owns one temporary reference not present in
    // Linux's registry-locked iteration.
    let usage = key.external_ref_count().saturating_sub(1);
    let prefix = format!(
        "{:08x} {}{}{}{}{}{}{} {:5} {:>4} {:08x} {:5} {:5} {:<9.9} ",
        key.serial as u32,
        flags[0],
        flags[1],
        flags[2],
        flags[3],
        flags[4],
        flags[5],
        flags[6],
        usage,
        timeout,
        perm,
        uid,
        gid,
        core::str::from_utf8(key.key_type.name()).unwrap_or("?"),
    );
    out.extend_from_slice(prefix.as_bytes());
    match detail {
        Detail::Auth { pid, callout_len } => {
            out.extend_from_slice(b"key:");
            out.extend_from_slice(&key.description);
            if matches!(status, KeyStatus::Positive) {
                let tail = format!(" pid:{} ci:{}", pid, callout_len);
                out.extend_from_slice(tail.as_bytes());
            }
        }
        Detail::Ring(count) => {
            out.extend_from_slice(&key.description);
            if matches!(status, KeyStatus::Positive) {
                if count == 0 {
                    out.extend_from_slice(b": empty");
                } else {
                    let suffix = format!(": {}", count);
                    out.extend_from_slice(suffix.as_bytes());
                }
            }
        }
        Detail::Data(len) => {
            out.extend_from_slice(&key.description);
            if matches!(status, KeyStatus::Positive) {
                let suffix = format!(": {}", len);
                out.extend_from_slice(suffix.as_bytes());
            }
        }
        Detail::Plain => out.extend_from_slice(&key.description),
    }
    out.push(b'\n');
    true
}

fn render_keys_slice(
    cred: &Cred,
    cursor: Option<usize>,
    budget: usize,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, SystemError> {
    let now = realtime_now().tv_sec;
    let mut next = cursor.unwrap_or(0);
    loop {
        if next > i32::MAX as usize {
            return Ok(None);
        }
        let keys = KeyStore::snapshot_from(next as i32, KEY_BATCH);
        if keys.is_empty() {
            return Ok(None);
        }
        let len = keys.len();
        for key in &keys {
            next = key.serial as usize + 1;
            append_key(key, cred, now, out);
            if !out.is_empty() && out.len() >= budget {
                return Ok((next <= i32::MAX as usize).then_some(next));
            }
        }
        if len < KEY_BATCH {
            return Ok(None);
        }
    }
}

#[derive(Debug)]
pub struct KeysFileOps;

impl KeysFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for KeysFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let cred = opener(&data)?;
        proc_read_seq(offset, len, buf, &mut data, |cursor, budget, out| {
            render_keys_slice(&cred, cursor, budget, out)
        })
    }
}

fn render_users_slice(
    ns: &UserNamespace,
    cursor: Option<usize>,
    budget: usize,
    out: &mut Vec<u8>,
) -> Option<usize> {
    let mut next = cursor.unwrap_or(0);
    loop {
        let accounts = live_accounts_from(next, KEY_BATCH);
        if accounts.is_empty() {
            return None;
        }
        let len = accounts.len();
        for account in &accounts {
            let after = account.uid().data().checked_add(1)?;
            next = after;
            let Some(uid) = mapped_uid(ns, account.uid()) else {
                continue;
            };
            let counts = account.snapshot();
            let limits = if account.uid().data() == 0 {
                (&QUOTA_LIMITS.root_maxkeys, &QUOTA_LIMITS.root_maxbytes)
            } else {
                (&QUOTA_LIMITS.maxkeys, &QUOTA_LIMITS.maxbytes)
            };
            let usage = Arc::strong_count(account).saturating_sub(1);
            let line = format!(
                "{:5}: {:5} {}/{} {}/{} {}/{}\n",
                uid,
                usage,
                counts.nkeys,
                counts.nikeys,
                counts.qnkeys,
                limits.0.load(Ordering::Relaxed),
                counts.qnbytes,
                limits.1.load(Ordering::Relaxed),
            );
            out.extend_from_slice(line.as_bytes());
            if out.len() >= budget {
                return Some(next);
            }
        }
        if len < KEY_BATCH {
            return None;
        }
    }
}

#[derive(Debug)]
pub struct KeyUsersFileOps;

impl KeyUsersFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for KeyUsersFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let cred = opener(&data)?;
        proc_read_seq(offset, len, buf, &mut data, |cursor, budget, out| {
            Ok(render_users_slice(&cred.user_ns, cursor, budget, out))
        })
    }
}
