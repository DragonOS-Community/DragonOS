//! Key identity and lifetime management.
//!
//! The serial index owns one strong reference to every published key.  A
//! `KeyRef` is the only external strong reference and carries a separate
//! count.  The last external reference schedules reclamation; only the GC
//! worker removes the serial entry and releases the quota reservation.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    ops::Deref,
    sync::atomic::{compiler_fence, AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use system_error::SystemError;

use crate::{
    exception::workqueue::{Work, WorkQueue},
    libs::{mutex::Mutex, rand::secure_random_bytes, wait_queue::WaitQueue},
    process::{
        cred::{Kgid, Kuid},
        namespace::user_namespace::{UserNamespace, UserNamespaceKeyrings},
    },
    time::{
        timekeeping::realtime_now,
        timer::{next_n_us_timer_jiffies, Timer, TimerFunction},
    },
};

use super::{
    quota::account_for, request::RequestAuth, ring::prune_invalid_links, KeyQuota, QuotaMode,
    QUOTA_LIMITS,
};

const FIRST_SERIAL: i32 = 3;
const GC_BATCH: usize = 128;
type RingPruneCursor = Option<(KeyType, Vec<u8>)>;
type RingPruneBatch = Option<(i32, KeyRef, RingPruneCursor)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyType {
    User,
    Logon,
    Keyring,
    /// Internal request-key authorization token; never accepted as an
    /// ordinary user-supplied type name.
    RequestKeyAuth,
}

impl KeyType {
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::User => b"user",
            Self::Logon => b"logon",
            Self::Keyring => b"keyring",
            Self::RequestKeyAuth => b".request_key_auth",
        }
    }
}

bitflags! {
    pub struct KeyFlags: u32 {
        const ROOT_CAN_CLEAR = 1 << 0;
        const ROOT_CAN_INVAL = 1 << 1;
        const UID_KEYRING = 1 << 2;
        const USER_CONSTRUCT = 1 << 3;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    Uninstantiated,
    Positive,
    /// Positive Linux errno from KEYCTL_REJECT.  The syscall accepts any
    /// non-special number below MAX_ERRNO, including values that DragonOS's
    /// `SystemError` enum does not name.
    Negative(i32),
}

/// A keyring owns its links; the link implementation prevents cycles before
/// inserting a keyring-to-keyring edge.
#[derive(Debug)]
pub enum KeyPayload {
    Uninstantiated,
    User(Vec<u8>),
    Logon(Vec<u8>),
    /// Link index by the Linux `(type, description)` identity.
    Keyring(BTreeMap<(KeyType, Vec<u8>), KeyRef>),
    RequestKeyAuth(Arc<RequestAuth>),
}

impl Drop for KeyPayload {
    fn drop(&mut self) {
        match self {
            Self::User(bytes) | Self::Logon(bytes) => {
                for byte in bytes.iter_mut() {
                    // A plain fill may be elided when the allocation is
                    // immediately freed.  Payload erasure must survive
                    // optimization on replacement, revoke, and GC.
                    unsafe { core::ptr::write_volatile(byte, 0) };
                }
                compiler_fence(Ordering::SeqCst);
            }
            Self::Uninstantiated | Self::Keyring(_) | Self::RequestKeyAuth(_) => {}
        }
    }
}

pub struct KeyState {
    pub uid: Kuid,
    pub gid: Kgid,
    pub perm: u32,
    pub status: KeyStatus,
    pub payload: KeyPayload,
    /// Real-time seconds.  Linux treats zero as no expiry.
    pub expiry: Option<i64>,
    pub revoked_at: Option<i64>,
    pub invalidated: bool,
    /// `KEYCTL_RESTRICT_KEYRING` reject-all mode for supported core types.
    pub restricted: bool,
    pub flags: KeyFlags,
    pub quota: KeyQuota,
}

pub struct Key {
    pub serial: i32,
    pub key_type: KeyType,
    /// Exact description bytes, excluding the trailing NUL charged to quota.
    /// Descriptions need not be UTF-8.
    pub description: Vec<u8>,
    pub state: Mutex<KeyState>,
    construction_wait: WaitQueue,
    external_refs: AtomicUsize,
    /// The non-owning name index is removed when this key is reclaimed.
    named_namespace: Mutex<Option<Weak<UserNamespace>>>,
}

impl Drop for Key {
    fn drop(&mut self) {
        let Some(namespace) = self
            .named_namespace
            .lock()
            .take()
            .and_then(|ns| ns.upgrade())
        else {
            return;
        };
        let mut keyrings = namespace.keyrings.lock();
        if let Some(candidates) = keyrings.named.get_mut(&self.description) {
            candidates.retain(|candidate| !core::ptr::eq(candidate.weak.as_ptr(), self));
            if candidates.is_empty() {
                keyrings.named.remove(&self.description);
            }
        }
    }
}

impl Key {
    /// Wait for a pending request key to be instantiated or rejected.  The
    /// wait queue registers each waiter before checking the status again, so
    /// a helper completing during registration cannot lose the wakeup.
    pub fn wait_until_instantiated(&self) -> Result<KeyStatus, SystemError> {
        self.construction_wait.wait_until_interruptible(|| {
            let state = self.state.lock();
            match &state.status {
                KeyStatus::Uninstantiated => None,
                status => Some(status.clone()),
            }
        })
    }

    /// Call after committing a status transition under `state`, with its
    /// mutex released.  Wakes all requesters immediately; helper reap is
    /// independent of construction completion.
    pub fn wake_construction_waiters(&self) {
        self.construction_wait.wakeup_all(None);
    }
}

/// External key reference used by credentials, links, and syscall lookups.
/// No raw `Arc<Key>` may escape this module.
pub struct KeyRef {
    key: Option<Arc<Key>>,
    possessed: bool,
}

impl KeyRef {
    #[inline]
    fn key(&self) -> &Arc<Key> {
        self.key.as_ref().expect("live KeyRef always has a key")
    }

    pub fn is_possessed(&self) -> bool {
        self.possessed
    }

    pub fn external_ref_count(&self) -> usize {
        self.key().external_refs.load(Ordering::Acquire)
    }

    /// Possession belongs to the lookup path, not to the key object or a
    /// stored keyring link.  The returned reference owns its own counted ref.
    pub fn with_possession(&self, possessed: bool) -> Self {
        let mut reference = self.clone();
        reference.possessed = possessed;
        reference
    }

    pub fn downgrade(&self) -> WeakKeyRef {
        WeakKeyRef {
            serial: self.serial,
            weak: Arc::downgrade(self.key()),
        }
    }
}

impl Clone for KeyRef {
    fn clone(&self) -> Self {
        let key = self.key();
        // Holding `self` guarantees the counter cannot reach zero during a
        // clone.  The serial lookup path has a separate nonzero check.
        key.external_refs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count > 0 && count < usize::MAX).then_some(count + 1)
            })
            .expect("key reference count overflow or resurrection");
        Self {
            key: Some(Arc::clone(key)),
            possessed: self.possessed,
        }
    }
}

impl Deref for KeyRef {
    type Target = Key;

    fn deref(&self) -> &Self::Target {
        self.key()
    }
}

impl fmt::Debug for KeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRef")
            .field("serial", &self.serial)
            .field("possessed", &self.possessed)
            .finish_non_exhaustive()
    }
}

impl Drop for KeyRef {
    fn drop(&mut self) {
        let key = self.key.take().expect("live KeyRef always has a key");
        let old = key.external_refs.fetch_sub(1, Ordering::Release);
        assert!(old > 0, "key reference count underflow");
        // Drop our Arc before publishing the GC request.  GC must not miss a
        // zero-ref key merely because this destructor has not released its
        // Arc yet.
        drop(key);
        if old == 1 {
            schedule_gc();
        }
    }
}

/// A non-owning name-index entry.  Upgrading through `KeyStore` verifies
/// that it still names the published key and cannot revive a zero-ref key.
#[derive(Clone)]
pub struct WeakKeyRef {
    serial: i32,
    weak: Weak<Key>,
}

impl fmt::Debug for WeakKeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakKeyRef")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

struct Registry {
    keys: BTreeMap<i32, Arc<Key>>,
    gc_cursor: i32,
    /// The ring currently being pruned and its last visited link index.
    gc_link_serial: Option<i32>,
    gc_link_cursor: Option<(KeyType, Vec<u8>)>,
    /// Number of GC requests observed at the start of this complete scan.
    gc_pass_epoch: u64,
    /// Earliest future key cleanup deadline seen during this complete scan.
    gc_next_deadline: Option<i64>,
    /// A cleanup deadline may pass after its ring was visited in this pass.
    gc_due_seen: bool,
}

impl Registry {
    fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
            gc_cursor: FIRST_SERIAL,
            gc_link_serial: None,
            gc_link_cursor: None,
            gc_pass_epoch: 0,
            gc_next_deadline: None,
            gc_due_seen: false,
        }
    }

    /// Called with the registry lock held.  On a random collision, choose
    /// the next unoccupied positive serial, as Linux does.
    fn free_serial(&self, mut serial: i32) -> i32 {
        loop {
            if !self.keys.contains_key(&serial) {
                return serial;
            }
            serial = if serial == i32::MAX {
                FIRST_SERIAL
            } else {
                serial + 1
            };
        }
    }

    fn live_ref(key: &Arc<Key>) -> Option<KeyRef> {
        // The caller holds the registry lock, the same lock used when GC
        // checks zero and removes the entry.  Never resurrect zero to one.
        key.external_refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0 && count < usize::MAX).then_some(count + 1)
            })
            .ok()?;
        Some(KeyRef {
            key: Some(Arc::clone(key)),
            possessed: false,
        })
    }
}

lazy_static::lazy_static! {
    static ref SERIAL_REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());
    static ref KEY_GC_WQ: Arc<WorkQueue> = WorkQueue::new("key_gc");
    static ref KEY_GC_WORK: Arc<Work> = Work::new(gc_scan_batch);
    static ref KEY_GC_TIMER: Mutex<Option<(i64, Arc<Timer>)>> = Mutex::new(None);
}

#[derive(Debug)]
struct KeyGcTimer;

impl TimerFunction for KeyGcTimer {
    fn run(&mut self) -> Result<(), SystemError> {
        // Runs from timer softirq: queue the process-context worker only.
        schedule_gc();
        Ok(())
    }
}

static GC_READY: AtomicBool = AtomicBool::new(false);
static GC_REQUESTS: AtomicU64 = AtomicU64::new(0);
static LAST_DUE_RETRY_EPOCH: AtomicU64 = AtomicU64::new(u64::MAX);

/// Initialize after `workqueue_init()` and before publishing any key.
pub fn init() {
    lazy_static::initialize(&SERIAL_REGISTRY);
    lazy_static::initialize(&KEY_GC_WQ);
    lazy_static::initialize(&KEY_GC_WORK);
    lazy_static::initialize(&KEY_GC_TIMER);
    GC_READY.store(true, Ordering::Release);
}

pub fn schedule_gc() {
    if !GC_READY.load(Ordering::Acquire) {
        return;
    }
    GC_REQUESTS.fetch_add(1, Ordering::AcqRel);
    KEY_GC_WQ.enqueue(KEY_GC_WORK.clone());
}

fn cleanup_deadline(key: &KeyRef, now: i64, delay: u32) -> (Option<i64>, bool) {
    let state = key.state.lock();
    let mut future = None;
    let mut due = false;
    for deadline in state
        .expiry
        .into_iter()
        .chain(state.revoked_at)
        .map(|at| at.saturating_add(delay as i64))
    {
        if deadline <= now {
            due = true;
        } else {
            future = Some(future.map_or(deadline, |old: i64| old.min(deadline)));
        }
    }
    (future, due)
}

fn record_deadline(registry: &mut Registry, deadline: (Option<i64>, bool)) {
    registry.gc_due_seen |= deadline.1;
    if let Some(deadline) = deadline.0 {
        registry.gc_next_deadline = Some(
            registry
                .gc_next_deadline
                .map_or(deadline, |previous| previous.min(deadline)),
        );
    }
}

/// A newly assigned expiry only has to shorten the next cleanup deadline.
/// Stale early timers are harmless: the GC pass recomputes the live minimum.
/// Keeping the earlier timer also avoids a full registry scan for each
/// KEYCTL_SET_TIMEOUT in a large batch.
fn ensure_gc_timer(deadline: i64) {
    let now = realtime_now().tv_sec;
    if deadline <= now {
        schedule_gc();
        return;
    }
    let mut slot = KEY_GC_TIMER.lock();
    if slot
        .as_ref()
        .is_some_and(|(at, timer)| *at <= deadline && *at > now && !timer.timeout())
    {
        return;
    }
    let delay_us = deadline.saturating_sub(now).max(1) as u64;
    let jiffies = next_n_us_timer_jiffies(delay_us.saturating_mul(1_000_000));
    let timer = Timer::new(Box::new(KeyGcTimer), jiffies);
    if let Some((_, old)) = slot.replace((deadline, timer.clone())) {
        old.cancel();
    }
    timer.activate();
}

/// Keep one timer for the earliest future cleanup, rather than a timer per
/// key.  This runs after a completed GC pass, including expiry/revoke scans.
fn arm_gc_timer(deadline: Option<i64>, due_in_scan: bool, pass_epoch: u64) {
    let now = realtime_now().tv_sec;
    if let Some(at) = deadline.filter(|&at| at > now) {
        ensure_gc_timer(at);
    } else {
        // Do not cancel a timer installed by a concurrent SET_TIMEOUT after
        // this scan started.  A stale timer merely causes one extra pass.
        let mut slot = KEY_GC_TIMER.lock();
        if slot.as_ref().is_some_and(|(_, timer)| timer.timeout()) {
            slot.take();
        }
    }
    if (due_in_scan || deadline.is_some_and(|at| at <= now))
        && LAST_DUE_RETRY_EPOCH.swap(pass_epoch, Ordering::AcqRel) != pass_epoch
    {
        // A deadline can elapse while this full scan is in progress, after
        // its containing ring has already been visited.  Retry once per
        // request epoch, but do not spin forever on an externally held key.
        KEY_GC_WQ.enqueue(KEY_GC_WORK.clone());
    }
}

/// Set a key's cleanup deadline without rescanning all keys.  An immediate
/// deadline still asks the worker to remove expired links promptly.
pub fn schedule_expiry_gc(expiry: Option<i64>) {
    if let Some(expiry) = expiry {
        let delay = QUOTA_LIMITS.gc_delay.load(Ordering::Relaxed) as i64;
        ensure_gc_timer(expiry.saturating_add(delay));
    }
}

fn gc_scan_batch() {
    let mut retired: [Option<Arc<Key>>; GC_BATCH] = core::array::from_fn(|_| None);
    let mut deadline_refs: [Option<KeyRef>; GC_BATCH] = core::array::from_fn(|_| None);
    let mut retired_count = 0;
    let mut deadline_ref_count = 0;
    let mut needs_next_batch = true;
    let mut finished_pass = false;
    let mut timer_update: Option<(Option<i64>, bool, u64)> = None;
    let mut ring_to_prune: RingPruneBatch = None;
    let mut stale_link_cursor = None;
    {
        let mut registry = SERIAL_REGISTRY.lock();
        if registry.gc_cursor == FIRST_SERIAL && registry.gc_link_serial.is_none() {
            registry.gc_pass_epoch = GC_REQUESTS.load(Ordering::Acquire);
            registry.gc_next_deadline = None;
            registry.gc_due_seen = false;
        }

        if let Some(serial) = registry.gc_link_serial {
            if let Some(key) = registry.keys.get(&serial).and_then(Registry::live_ref) {
                ring_to_prune = Some((serial, key, registry.gc_link_cursor.take()));
            } else {
                // A ring can lose its last external reference between two
                // batches.  Its final drop requests another GC pass.
                stale_link_cursor = registry.gc_link_cursor.take();
                registry.gc_link_serial = None;
                registry.gc_cursor = serial.saturating_add(1);
            }
        }

        if registry.gc_link_serial.is_none() {
            let mut candidates = [0i32; GC_BATCH];
            let mut next_cursor = FIRST_SERIAL;
            let mut more = false;
            let mut selected_ring = None;
            for (scanned, (&serial, key)) in registry.keys.range(registry.gc_cursor..).enumerate() {
                if scanned == GC_BATCH {
                    more = true;
                    break;
                }
                next_cursor = serial.saturating_add(1);
                if key.external_refs.load(Ordering::Acquire) == 0 && Arc::strong_count(key) == 1 {
                    candidates[retired_count] = serial;
                    retired_count += 1;
                } else if key.key_type == KeyType::Keyring {
                    if let Some(reference) = Registry::live_ref(key) {
                        selected_ring = Some((serial, reference));
                        break;
                    }
                } else if let Some(reference) = Registry::live_ref(key) {
                    deadline_refs[deadline_ref_count] = Some(reference);
                    deadline_ref_count += 1;
                }
            }
            for (index, serial) in candidates.into_iter().take(retired_count).enumerate() {
                retired[index] = registry.keys.remove(&serial);
            }

            if let Some((serial, reference)) = selected_ring {
                registry.gc_link_serial = Some(serial);
                registry.gc_cursor = serial;
                ring_to_prune = Some((serial, reference, None));
            } else if more {
                registry.gc_cursor = next_cursor;
            } else {
                needs_next_batch = finish_gc_pass(&mut registry);
                finished_pass = true;
            }
        }
    }

    // Key payload destruction and quota refund may sleep.  Neither happens
    // under the serial registry lock.
    drop(retired);
    drop(stale_link_cursor);

    let now = realtime_now().tv_sec;
    let delay = QUOTA_LIMITS.gc_delay.load(Ordering::Relaxed);
    let mut batch_deadline = (None, false);
    for key in deadline_refs.iter().take(deadline_ref_count).flatten() {
        let (future, due) = cleanup_deadline(key, now, delay);
        batch_deadline.0 = match (batch_deadline.0, future) {
            (Some(old), Some(new)) => Some(old.min(new)),
            (None, candidate) | (candidate, None) => candidate,
        };
        batch_deadline.1 |= due;
    }
    drop(deadline_refs);

    if batch_deadline.0.is_some() || batch_deadline.1 || finished_pass {
        let mut registry = SERIAL_REGISTRY.lock();
        record_deadline(&mut registry, batch_deadline);
        if finished_pass {
            timer_update = Some((
                registry.gc_next_deadline,
                registry.gc_due_seen,
                registry.gc_pass_epoch,
            ));
        }
    }

    if let Some((serial, ring, mut cursor)) = ring_to_prune {
        let ring_deadline = cleanup_deadline(&ring, now, delay);
        let done = match prune_invalid_links(&ring, now, delay, &mut cursor, GC_BATCH) {
            Ok((_, done)) => done,
            Err(error) => {
                // A failed allocation while copying an index cannot leave
                // the worker spinning forever under memory pressure.  The
                // next GC request or expiry wake will retry this ring.
                log::warn!("key GC link scan deferred: {:?}", error);
                true
            }
        };
        drop(ring);
        let mut registry = SERIAL_REGISTRY.lock();
        record_deadline(&mut registry, ring_deadline);
        if done {
            registry.gc_link_serial = None;
            registry.gc_link_cursor = None;
            if let Some(next) = serial.checked_add(1) {
                registry.gc_cursor = next;
            } else {
                needs_next_batch = finish_gc_pass(&mut registry);
                timer_update = Some((
                    registry.gc_next_deadline,
                    registry.gc_due_seen,
                    registry.gc_pass_epoch,
                ));
            }
        } else {
            registry.gc_link_cursor = cursor.take();
        }
        drop(registry);
        drop(cursor);
    }
    if let Some((deadline, due_in_scan, pass_epoch)) = timer_update {
        arm_gc_timer(deadline, due_in_scan, pass_epoch);
    }
    if needs_next_batch {
        KEY_GC_WQ.enqueue(KEY_GC_WORK.clone());
    }
}

fn finish_gc_pass(registry: &mut Registry) -> bool {
    registry.gc_cursor = FIRST_SERIAL;
    registry.gc_link_serial = None;
    // A final reference may have disappeared in an already-scanned range.
    // It increments the epoch even if the Work was already pending.
    registry.gc_pass_epoch != GC_REQUESTS.load(Ordering::Acquire)
}

pub struct KeyStore;

impl KeyStore {
    pub fn allocate(
        key_type: KeyType,
        description: Vec<u8>,
        uid: Kuid,
        gid: Kgid,
        perm: u32,
        quota_mode: QuotaMode,
        flags: KeyFlags,
    ) -> Result<KeyRef, SystemError> {
        if !GC_READY.load(Ordering::Acquire) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        if description.is_empty() || description.contains(&0) {
            return Err(SystemError::EINVAL);
        }
        let description_bytes = description
            .len()
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let quota = KeyQuota::reserve(account_for(uid)?, description_bytes, quota_mode)?;
        let mut serial_bytes = [0u8; 4];
        let random_serial = loop {
            secure_random_bytes(&mut serial_bytes)?;
            let candidate = (u32::from_ne_bytes(serial_bytes) & i32::MAX as u32) as i32;
            if candidate >= FIRST_SERIAL {
                break candidate;
            }
        };
        // Allocate the key and its quota-bearing state before taking the
        // serial registry lock.  No allocator or failure-path destructor may
        // run under that lock.
        let mut key = Arc::try_new(Key {
            serial: random_serial,
            key_type,
            description,
            state: Mutex::new(KeyState {
                uid,
                gid,
                perm,
                status: KeyStatus::Uninstantiated,
                payload: KeyPayload::Uninstantiated,
                expiry: None,
                revoked_at: None,
                invalidated: false,
                restricted: false,
                flags,
                quota,
            }),
            construction_wait: WaitQueue::default(),
            external_refs: AtomicUsize::new(1),
            named_namespace: Mutex::new(None),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut registry = SERIAL_REGISTRY.lock();
        let serial = registry.free_serial(random_serial);
        Arc::get_mut(&mut key)
            .expect("unpublished key has no other references")
            .serial = serial;
        registry.keys.insert(serial, Arc::clone(&key));
        Ok(KeyRef {
            key: Some(key),
            possessed: false,
        })
    }

    pub fn lookup(serial: i32) -> Option<KeyRef> {
        let registry = SERIAL_REGISTRY.lock();
        registry.keys.get(&serial).and_then(Registry::live_ref)
    }

    pub fn lookup_weak(weak: &WeakKeyRef) -> Option<KeyRef> {
        let registry = SERIAL_REGISTRY.lock();
        let key = registry.keys.get(&weak.serial)?;
        if weak.weak.as_ptr() != Arc::as_ptr(key) {
            return None;
        }
        Registry::live_ref(key)
    }

    /// Publish an ordinary named keyring only after its owning link or
    /// credential reference has been installed.  The namespace index never
    /// extends the key's lifetime; Key::drop removes the entry after GC.
    pub fn publish_named_keyring(key: &KeyRef, namespace: &Arc<UserNamespace>) {
        let mut names = namespace.keyrings.lock();
        Self::publish_named_keyring_locked(key, namespace, &mut names);
    }

    pub(crate) fn publish_named_keyring_locked(
        key: &KeyRef,
        namespace: &Arc<UserNamespace>,
        names: &mut UserNamespaceKeyrings,
    ) {
        debug_assert_eq!(key.key_type, KeyType::Keyring);
        debug_assert_ne!(key.description[0], b'.');
        names
            .named
            .entry(key.description.clone())
            .or_default()
            .push(key.downgrade());
        *key.named_namespace.lock() = Some(Arc::downgrade(namespace));
    }

    /// Bounded serial-ordered snapshot for procfs iteration.  Reserve before
    /// taking the registry lock so collecting references cannot allocate
    /// while the serial index is locked.
    pub fn snapshot_from(serial: i32, limit: usize) -> Vec<KeyRef> {
        let mut snapshot = Vec::with_capacity(limit);
        let registry = SERIAL_REGISTRY.lock();
        for (_, key) in registry.keys.range(serial..) {
            if snapshot.len() == limit {
                break;
            }
            if let Some(reference) = Registry::live_ref(key) {
                snapshot.push(reference);
            }
        }
        snapshot
    }
}
