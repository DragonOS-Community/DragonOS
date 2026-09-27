//! Quota accounting for Linux key retention.
//!
//! Accounts are keyed by global KUID, not by a UID as seen in the caller's
//! user namespace.  A key owns one `KeyQuota` reservation; its description,
//! payload and keyring links all adjust that same reservation.  Published keys
//! retain this reservation through the serial registry until key GC removes
//! the final registry reference.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    libs::{mutex::Mutex, spinlock::SpinLock},
    process::cred::Kuid,
};

/// Linux 6.6 defaults from security/keys/key.c and security/keys/gc.c.
pub struct QuotaLimits {
    pub maxkeys: AtomicU32,
    pub maxbytes: AtomicU32,
    pub root_maxkeys: AtomicU32,
    pub root_maxbytes: AtomicU32,
    pub gc_delay: AtomicU32,
}

pub static QUOTA_LIMITS: QuotaLimits = QuotaLimits {
    maxkeys: AtomicU32::new(200),
    maxbytes: AtomicU32::new(20_000),
    root_maxkeys: AtomicU32::new(1_000_000),
    root_maxbytes: AtomicU32::new(25_000_000),
    gc_delay: AtomicU32::new(300),
};

impl QuotaLimits {
    fn for_uid(&self, uid: Kuid) -> (u32, u32) {
        if uid.data() == 0 {
            (
                self.root_maxkeys.load(Ordering::Relaxed),
                self.root_maxbytes.load(Ordering::Relaxed),
            )
        } else {
            (
                self.maxkeys.load(Ordering::Relaxed),
                self.maxbytes.load(Ordering::Relaxed),
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaMode {
    /// Count and byte limits apply.
    Limited,
    /// Still charged, but the creator may exceed a limit (special keyrings).
    Overrun,
    /// The key is tracked but neither its count nor bytes consume quota.
    Uncharged,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AccountCounts {
    pub nkeys: u32,
    pub nikeys: u32,
    pub qnkeys: u32,
    pub qnbytes: u32,
}

#[derive(Debug)]
pub struct QuotaAccount {
    uid: Kuid,
    counts: SpinLock<AccountCounts>,
}

impl QuotaAccount {
    pub fn uid(&self) -> Kuid {
        self.uid
    }

    pub fn snapshot(&self) -> AccountCounts {
        *self.counts.lock()
    }
}

impl Drop for QuotaAccount {
    fn drop(&mut self) {
        // Do not retain dead UID entries indefinitely when many short-lived
        // users create keys without anyone reading /proc/key-users.
        let mut accounts = ACCOUNTS.lock();
        if accounts
            .get(&self.uid)
            .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), self))
        {
            accounts.remove(&self.uid);
        }
    }
}

lazy_static::lazy_static! {
    static ref ACCOUNTS: Mutex<BTreeMap<Kuid, Weak<QuotaAccount>>> = Mutex::new(BTreeMap::new());
}

pub fn account_for(uid: Kuid) -> Result<Arc<QuotaAccount>, SystemError> {
    let mut accounts = ACCOUNTS.lock();
    if let Some(account) = accounts.get(&uid).and_then(Weak::upgrade) {
        return Ok(account);
    }
    let account = Arc::try_new(QuotaAccount {
        uid,
        counts: SpinLock::new(AccountCounts::default()),
    })
    .map_err(|_| SystemError::ENOMEM)?;
    accounts.insert(uid, Arc::downgrade(&account));
    Ok(account)
}

/// Return at most `limit` live quota users at or above a global UID.
/// Procfs uses the UID as its resume cursor, so a large table is never
/// copied or rescanned from the beginning for each seq-file slice.
pub fn live_accounts_from(uid: usize, limit: usize) -> Vec<Arc<QuotaAccount>> {
    let mut live = Vec::with_capacity(limit);
    let accounts = ACCOUNTS.lock();
    for (_, weak) in accounts.range(Kuid::new(uid)..) {
        if live.len() == limit {
            break;
        }
        if let Some(account) = weak.upgrade() {
            live.push(account);
        }
    }
    live
}

/// A reservation owned by one key.  `bytes` includes its NUL-terminated
/// description and current payload length; for keyrings the payload length is
/// four bytes per link.  No key or registry locks may be needed by Drop.
pub struct KeyQuota {
    account: Arc<QuotaAccount>,
    bytes: u32,
    mode: QuotaMode,
    instantiated: bool,
}

impl KeyQuota {
    pub fn reserve(
        account: Arc<QuotaAccount>,
        description_bytes: usize,
        mode: QuotaMode,
    ) -> Result<Self, SystemError> {
        let bytes = u32::try_from(description_bytes).map_err(|_| SystemError::EOVERFLOW)?;
        let mut counts = account.counts.lock();
        let new_nkeys = counts.nkeys.checked_add(1).ok_or(SystemError::EDQUOT)?;
        if mode != QuotaMode::Uncharged {
            let new_qnkeys = counts.qnkeys.checked_add(1).ok_or(SystemError::EDQUOT)?;
            let new_qnbytes = counts
                .qnbytes
                .checked_add(bytes)
                .ok_or(SystemError::EDQUOT)?;
            let (maxkeys, maxbytes) = QUOTA_LIMITS.for_uid(account.uid);
            if mode == QuotaMode::Limited && (new_qnkeys > maxkeys || new_qnbytes > maxbytes) {
                return Err(SystemError::EDQUOT);
            }
            counts.qnkeys = new_qnkeys;
            counts.qnbytes = new_qnbytes;
        }
        counts.nkeys = new_nkeys;
        drop(counts);
        Ok(Self {
            account,
            bytes,
            mode,
            instantiated: false,
        })
    }

    pub fn account(&self) -> &Arc<QuotaAccount> {
        &self.account
    }

    pub fn bytes(&self) -> u32 {
        self.bytes
    }

    pub fn is_charged(&self) -> bool {
        self.mode != QuotaMode::Uncharged
    }

    /// Change the total charge after all fallible payload preparation, but
    /// before exposing the new payload or link set to readers.
    pub fn resize(&mut self, new_bytes: usize) -> Result<(), SystemError> {
        let new_bytes = u32::try_from(new_bytes).map_err(|_| SystemError::EOVERFLOW)?;
        if self.mode != QuotaMode::Uncharged {
            let mut counts = self.account.counts.lock();
            if new_bytes > self.bytes {
                let delta = new_bytes - self.bytes;
                let total = counts
                    .qnbytes
                    .checked_add(delta)
                    .ok_or(SystemError::EDQUOT)?;
                // Linux key_payload_reserve checks the byte limit even for a
                // key originally allocated with QUOTA_OVERRUN.
                if total > QUOTA_LIMITS.for_uid(self.account.uid).1 {
                    return Err(SystemError::EDQUOT);
                }
                counts.qnbytes = total;
            } else {
                counts.qnbytes -= self.bytes - new_bytes;
            }
        }
        self.bytes = new_bytes;
        Ok(())
    }

    pub fn mark_instantiated(&mut self) {
        if !self.instantiated {
            self.account.counts.lock().nikeys += 1;
            self.instantiated = true;
        }
    }

    /// `KEYCTL_CHOWN` transfers the *quota owner* as well as permission UID.
    /// Both account locks are held in UID order so the capacity check and
    /// debit/credit are one transaction, even with concurrent transfers.
    pub fn transfer_to(&mut self, new_account: Arc<QuotaAccount>) -> Result<(), SystemError> {
        if Arc::ptr_eq(&self.account, &new_account) {
            return Ok(());
        }
        let old = &self.account;
        let (mut first, mut second) = if old.uid < new_account.uid {
            (old.counts.lock(), new_account.counts.lock())
        } else {
            (new_account.counts.lock(), old.counts.lock())
        };
        let (old_counts, new_counts) = if old.uid < new_account.uid {
            (&mut *first, &mut *second)
        } else {
            (&mut *second, &mut *first)
        };
        let next_nkeys = new_counts.nkeys.checked_add(1).ok_or(SystemError::EDQUOT)?;
        let next_nikeys = if self.instantiated {
            new_counts
                .nikeys
                .checked_add(1)
                .ok_or(SystemError::EDQUOT)?
        } else {
            new_counts.nikeys
        };
        let (next_qnkeys, next_qnbytes) = if self.is_charged() {
            let next_keys = new_counts
                .qnkeys
                .checked_add(1)
                .ok_or(SystemError::EDQUOT)?;
            let next_bytes = new_counts
                .qnbytes
                .checked_add(self.bytes)
                .ok_or(SystemError::EDQUOT)?;
            let (maxkeys, maxbytes) = QUOTA_LIMITS.for_uid(new_account.uid);
            if next_keys > maxkeys || next_bytes > maxbytes {
                return Err(SystemError::EDQUOT);
            }
            (next_keys, next_bytes)
        } else {
            (new_counts.qnkeys, new_counts.qnbytes)
        };
        old_counts.nkeys -= 1;
        if self.instantiated {
            old_counts.nikeys -= 1;
        }
        if self.is_charged() {
            old_counts.qnkeys -= 1;
            old_counts.qnbytes -= self.bytes;
        }
        new_counts.nkeys = next_nkeys;
        new_counts.nikeys = next_nikeys;
        new_counts.qnkeys = next_qnkeys;
        new_counts.qnbytes = next_qnbytes;
        drop(first);
        drop(second);
        self.account = new_account;
        Ok(())
    }
}

impl Drop for KeyQuota {
    fn drop(&mut self) {
        let mut counts = self.account.counts.lock();
        counts.nkeys -= 1;
        if self.instantiated {
            counts.nikeys -= 1;
        }
        if self.is_charged() {
            counts.qnkeys -= 1;
            counts.qnbytes -= self.bytes;
        }
    }
}
