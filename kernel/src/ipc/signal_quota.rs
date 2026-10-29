//! RLIMIT_SIGPENDING accounts, distinct from allocation/memcg ownership.
//!
//! A namespace's index is non-owning. Live queue entries and POSIX timer
//! reservations pin their account and its immutable namespace-creator chain.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::process::namespace::user_namespace::UserNamespace;

const MAX_COUNT: u64 = i64::MAX as u64;

#[derive(Debug)]
pub struct SignalAccount {
    namespace: Arc<UserNamespace>,
    uid: usize,
    pending: AtomicU64,
}

impl SignalAccount {
    /// Prepare outside pending locks. No account-index lock may surround the
    /// destruction of an upgraded Arc: its last drop re-enters this index.
    pub fn get(namespace: &Arc<UserNamespace>, uid: usize) -> Result<Arc<Self>, SystemError> {
        let existing = {
            let accounts = namespace.signal_accounts.lock_irqsave();
            accounts.get(&uid).and_then(|entry| entry.upgrade())
        };
        if let Some(account) = existing {
            return Ok(account);
        }

        let candidate = Arc::try_new(Self {
            namespace: namespace.clone(),
            uid,
            pending: AtomicU64::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut retired = None;
        let selected = {
            let mut accounts = namespace.signal_accounts.lock_irqsave();
            match accounts.get(&uid).and_then(|entry| entry.upgrade()) {
                Some(account) => Ok(account),
                None => match accounts.try_reserve(1) {
                    Ok(()) => {
                        retired = accounts.insert(uid, Arc::downgrade(&candidate));
                        Ok(candidate.clone())
                    }
                    Err(_) => Err(SystemError::ENOMEM),
                },
            }
        };
        // Both a losing candidate and a retired weak reference are destroyed
        // only after the index lock is released.
        drop(retired);
        drop(candidate);
        selected
    }

    /// /proc observation must not instantiate a previously absent account.
    pub fn usage(namespace: &Arc<UserNamespace>, uid: usize) -> u64 {
        let account = {
            let accounts = namespace.signal_accounts.lock_irqsave();
            accounts.get(&uid).and_then(|entry| entry.upgrade())
        };
        account.map_or(0, |account| account.pending.load(Ordering::Acquire))
    }

    fn parent(&self) -> Option<&Self> {
        self.namespace.signal_parent.as_deref()
    }

    fn increment(&self, ceiling: u64) -> Result<(), SystemError> {
        let ceiling = ceiling.min(MAX_COUNT);
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < ceiling).then_some(count + 1)
            })
            .map(|_| ())
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn decrement(&self) {
        let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "signal quota underflow");
    }
}

impl Drop for SignalAccount {
    fn drop(&mut self) {
        debug_assert_eq!(self.pending.load(Ordering::Relaxed), 0);
        let retired = {
            let mut accounts = self.namespace.signal_accounts.lock_irqsave();
            if accounts
                .get(&self.uid)
                .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), self))
            {
                accounts.remove(&self.uid)
            } else {
                None
            }
        };
        drop(retired);
        // namespace/creator references are dropped after releasing the index.
    }
}

/// Exactly one charge, owned by one ordinary queue entry or shared through an
/// Arc by a POSIX timer and its outstanding notification. Not Clone.
#[derive(Debug)]
pub struct SignalCharge {
    account: Arc<SignalAccount>,
}

impl SignalCharge {
    pub fn acquire(
        account: &Arc<SignalAccount>,
        limit: u64,
        override_limit: bool,
    ) -> Result<Self, SystemError> {
        let mut current = account.as_ref();
        let mut ceiling = if override_limit { MAX_COUNT } else { limit };
        loop {
            if let Err(error) = current.increment(ceiling) {
                // Roll back exactly the already charged prefix. No allocation
                // or index lookup is needed, including at interrupt time.
                let mut rollback = account.as_ref();
                while !core::ptr::eq(rollback, current) {
                    rollback.decrement();
                    rollback = rollback.parent().expect("fixed signal account chain");
                }
                return Err(error);
            }
            ceiling = if override_limit {
                MAX_COUNT
            } else {
                current.namespace.signal_limit
            };
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(Self {
            account: account.clone(),
        })
    }
}

impl Drop for SignalCharge {
    fn drop(&mut self) {
        let mut current = Some(self.account.as_ref());
        while let Some(account) = current {
            account.decrement();
            current = account.parent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{
        cred::{Kuid, INIT_CRED},
        namespace::user_namespace::INIT_USER_NAMESPACE,
    };

    #[test]
    fn shared_uid_limit_override_and_last_owner_cleanup() {
        let namespace = &*INIT_USER_NAMESPACE;
        let uid = 2_000_000_001;
        let account = SignalAccount::get(namespace, uid).unwrap();
        let same = SignalAccount::get(namespace, uid).unwrap();
        assert!(Arc::ptr_eq(&account, &same));
        assert_eq!(SignalAccount::usage(namespace, uid), 0);
        let first = SignalCharge::acquire(&account, 1, false).unwrap();
        assert!(matches!(
            SignalCharge::acquire(&same, 1, false),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        let override_charge = SignalCharge::acquire(&account, 0, true).unwrap();
        assert_eq!(SignalAccount::usage(namespace, uid), 2);
        drop(first);
        drop(override_charge);
        assert_eq!(SignalAccount::usage(namespace, uid), 0);
        let weak = Arc::downgrade(&account);
        drop(account);
        drop(same);
        assert!(weak.upgrade().is_none());
        assert!(!namespace.signal_accounts.lock_irqsave().contains_key(&uid));
        // Observation is not a new owner or a new registry entry.
        assert_eq!(SignalAccount::usage(namespace, uid), 0);
        assert!(!namespace.signal_accounts.lock_irqsave().contains_key(&uid));
    }

    #[test]
    fn unpublished_candidate_drop_does_not_remove_live_account() {
        let namespace = &*INIT_USER_NAMESPACE;
        let uid = 2_000_000_002;
        let account = SignalAccount::get(namespace, uid).unwrap();
        let loser = Arc::new(SignalAccount {
            namespace: namespace.clone(),
            uid,
            pending: AtomicU64::new(0),
        });
        drop(loser);
        let same = SignalAccount::get(namespace, uid).unwrap();
        assert!(Arc::ptr_eq(&account, &same));
    }

    #[test]
    fn ancestor_overflow_rolls_back_local_charge_even_for_override() {
        let mut cred = (**INIT_CRED).clone();
        cred.euid = Kuid::new(2_000_000_003);
        let namespace = UserNamespace::create_user_ns(&cred).unwrap();
        let account = SignalAccount::get(&namespace, 0).unwrap();
        let parent = namespace.signal_parent.as_ref().unwrap();
        assert_eq!(parent.pending.load(Ordering::Relaxed), 0);
        parent.pending.store(MAX_COUNT, Ordering::Relaxed);
        let result = SignalCharge::acquire(&account, u64::MAX, true);
        parent.pending.store(0, Ordering::Relaxed);
        assert!(matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)));
        assert_eq!(account.pending.load(Ordering::Relaxed), 0);
    }
}
