//! Weak controller-instance inventory for sleepable reclaim snapshots.
//!
//! Registration belongs to topology preparation, never to charge/free. Parents
//! are registered before children. Retired instances remain discoverable while
//! actual resource tokens pin them, without keeping dead generations alive.
//! Metadata allocation here is deliberately not recursively memcg-accounted.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::libs::mutex::Mutex;

use super::MemoryAccount;

static ACCOUNTS: Mutex<Vec<Weak<MemoryAccount>>> = Mutex::new(Vec::new());

/// The initial root is the uncontended bootstrap exception; other callers must
/// be in sleepable topology preparation, outside accounting/IRQ/resource locks.
pub(super) fn register(account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
    let mut accounts = ACCOUNTS.lock();
    accounts.retain(|account| account.strong_count() != 0);
    accounts.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    accounts.push(Arc::downgrade(account));
    Ok(())
}

/// Sleepable, fallible metadata snapshot. No topology locks are acquired here,
/// and callers release this lock before touching page/rmap/LRU state.
pub(super) fn snapshot() -> Result<Vec<Arc<MemoryAccount>>, SystemError> {
    let mut accounts = ACCOUNTS.lock();
    accounts.retain(|account| account.strong_count() != 0);
    let mut live = Vec::new();
    live.try_reserve(accounts.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for account in accounts.iter() {
        if let Some(account) = account.upgrade() {
            live.push(account);
        }
    }
    Ok(live)
}
