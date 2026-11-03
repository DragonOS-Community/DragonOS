//! The production wake lookup/selection phase under real heap denial. All
//! owners and list/map backing are born before the scope; no global reset.
use super::*;
use crate::mm::oom::denial;

fn key(address: u64) -> FutexKey {
    FutexKey {
        ptr: 0,
        word: 0,
        offset: 0,
        key: InnerFutexKey::Private(PrivateKey {
            address_space: None,
            address,
        }),
    }
}

pub(super) fn run() -> Result<bool, SystemError> {
    let target = key(1);
    let foreign = key(2);
    let pairs: [(Waiter, Arc<Waker>); 6] = core::array::from_fn(|_| Waiter::new_pair());
    let nodes: [Arc<FutexObj>; 6] = core::array::from_fn(|index| {
        Arc::new(FutexObj {
            waker: pairs[index].1.clone(),
            key: SpinLock::new(Some(if index == 0 {
                foreign.clone()
            } else {
                target.clone()
            })),
            bitset: if index == 1 || index == 5 { 2 } else { 1 },
            source_priority: core::sync::atomic::AtomicI32::new(100),
            requeue_pi: None,
        })
    });
    // A previously notified entry still consumes one selected-waiter slot.
    let stale_prepared = pairs[2].1.wake();
    let mut bucket = FutexHashBucket::new();
    for node in &nodes {
        bucket.enqueue(node.clone());
    }
    let mut present = HashMap::new();
    present.insert(target.clone(), bucket);
    let capacity = present.capacity();
    let mut missing = HashMap::new();
    let originally_denied = denial::is_current_denied();
    let passed = {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        let mut absent_ok = true;
        for address in 3..4099 {
            absent_ok &= wake_existing(&mut missing, &key(address), 1, u32::MAX)?.is_none();
        }
        absent_ok &= missing.is_empty() && missing.capacity() == 0;
        let first = wake_existing(&mut present, &target, 0, 1)? == Some(1);
        let order = present.get(&target).is_some_and(|bucket| {
            bucket.chain.len() == 5
                && bucket
                    .chain
                    .iter()
                    .zip([&nodes[0], &nodes[1], &nodes[3], &nodes[4], &nodes[5]])
                    .all(|(actual, expected)| Arc::ptr_eq(actual, expected))
        });
        let mut pending = WaitQueueNodes::new();
        let literal_zero = present.get_mut(&target).unwrap().select_wake(
            target.clone(),
            Some(1),
            0,
            &mut pending,
        )? == 0
            && pending.is_empty();
        Futex::notify_selected(&mut pending);
        let second = wake_existing(&mut present, &target, 1, 1)? == Some(1);
        let rest = wake_existing(&mut present, &target, u32::MAX, u32::MAX)? == Some(3);
        let preserved = present.get(&target).is_some_and(|bucket| {
            bucket.chain.len() == 1 && Arc::ptr_eq(bucket.chain.front().unwrap(), &nodes[0])
        });
        // The foreign-key waker was never selected; all matching ones were.
        let notified = pairs[0].1.wake() && pairs[1..].iter().all(|(_, waker)| !waker.wake());
        let after = denial::probe_denied();
        before
            && after
            && absent_ok
            && first
            && order
            && literal_zero
            && second
            && rest
            && preserved
            && notified
            && present.capacity() == capacity
    };
    Ok(stale_prepared
        && passed
        && requeue()?
        && deferred_selection()?
        && requeue_selected_transfer()?
        && location_return()?
        && denial::is_current_denied() == originally_denied)
}

/// Exercise the production transfer phase with preallocated map capacity. Nodes
/// and key observers are born first; split/append must not allocate under denial.
fn requeue() -> Result<bool, SystemError> {
    let source = key(0x10);
    let target = key(0x20);
    let final_key = key(0x30);
    let pairs: [(Waiter, Arc<Waker>); 3] = core::array::from_fn(|_| Waiter::new_pair());
    let nodes: [Arc<FutexObj>; 3] = core::array::from_fn(|index| {
        Arc::new(FutexObj {
            waker: pairs[index].1.clone(),
            key: SpinLock::new(Some(source.clone())),
            bitset: u32::MAX,
            source_priority: core::sync::atomic::AtomicI32::new(100),
            requeue_pi: None,
        })
    });
    let mut map = HashMap::with_capacity(3);
    let mut bucket = FutexHashBucket::new();
    for node in &nodes {
        bucket.enqueue(node.clone());
    }
    map.insert(source.clone(), bucket);
    let capacity = map.capacity();
    let result = {
        let _scope = denial::Scope::enter(true);
        let same = requeue_existing(&mut map, &source, &source, 0, 2)?;
        let first = requeue_existing(&mut map, &source, &target, 0, 1)?;
        let first_order = map.get(&target).is_some_and(|bucket| {
            bucket.chain.len() == 1 && Arc::ptr_eq(bucket.chain.front().unwrap(), &nodes[0])
        });
        let second = requeue_existing(&mut map, &source, &target, 0, 3)?;
        let third = requeue_existing(&mut map, &target, &final_key, 0, 3)?;
        let order = map.get(&final_key).is_some_and(|bucket| {
            bucket
                .chain
                .iter()
                .zip(&nodes)
                .all(|(actual, expected)| Arc::ptr_eq(actual, expected))
                && bucket.chain.len() == 3
        });
        let changed = nodes
            .iter()
            .all(|node| node.key.lock().as_ref() == Some(&final_key));
        let retired = (second.1.is_some(), third.1.is_some());
        let bucket = map.get_mut(&final_key).unwrap();
        let removed = pairs.iter().zip(&nodes).all(|((_, waker), node)| {
            let removed = bucket.remove_by_waker(waker);
            if removed {
                let retired_location = node.key.lock().take();
                drop(retired_location);
            }
            removed
        });
        // A PI owner, even without either waiter chain, keeps its bucket live.
        bucket.pi_refs = 1;
        let pi_retained = FutexData::remove_empty_locked(&mut map, &final_key).is_none();
        map.get_mut(&final_key).unwrap().pi_refs = 0;
        let last = FutexData::remove_empty_locked(&mut map, &final_key);
        same.0 == 2
            && same.1.is_none()
            && first.0 == 1
            && first.1.is_none()
            && first_order
            && second.0 == 2
            && third.0 == 3
            && retired == (true, true)
            && order
            && changed
            && removed
            && pi_retained
            && last.is_some()
            && map.is_empty()
            && map.capacity() == capacity
            && denial::probe_denied()
    };

    // Capacity failure occurs before wake: no notification or node may escape.
    let mut full = HashMap::with_capacity(3);
    let mut bucket = FutexHashBucket::new();
    for node in &nodes {
        *node.key.lock() = Some(source.clone());
        bucket.enqueue(node.clone());
    }
    full.insert(source.clone(), bucket);
    full.insert(key(0x40), FutexHashBucket::new());
    full.insert(key(0x50), FutexHashBucket::new());
    let failed = {
        let _scope = denial::Scope::enter(true);
        matches!(
            requeue_existing(&mut full, &source, &target, 1, 1),
            Err(SystemError::ENOMEM)
        ) && full
            .get(&source)
            .is_some_and(|bucket| bucket.chain.len() == 3)
            && !full.contains_key(&target)
    };
    let intact = pairs.iter().all(|(_, waker)| waker.wake());
    Ok(result && failed && intact)
}

// Local maps have no registry guard, but exercise the same explicit notification
// boundary. Do not propagate a selection error before draining pending nodes.
fn wake_existing(
    map: &mut HashMap<FutexKey, FutexHashBucket>,
    key: &FutexKey,
    nr_wake: u32,
    bitset: u32,
) -> Result<Option<usize>, SystemError> {
    let mut pending = WaitQueueNodes::new();
    let result = Futex::wake_existing(map, key, nr_wake, bitset, &mut pending);
    Futex::notify_selected(&mut pending);
    result
}

fn requeue_existing(
    map: &mut HashMap<FutexKey, FutexHashBucket>,
    key1: &FutexKey,
    key2: &FutexKey,
    nr_wake: usize,
    nr_requeue: usize,
) -> Result<(usize, Option<FutexHashBucket>), SystemError> {
    let mut pending = WaitQueueNodes::new();
    let result = Futex::requeue_existing(map, key1, key2, nr_wake, nr_requeue, &mut pending);
    Futex::notify_selected(&mut pending);
    result
}

fn deferred_selection() -> Result<bool, SystemError> {
    let first = key(0x70);
    let second = key(0x80);
    let pairs: [(Waiter, Arc<Waker>); 3] = core::array::from_fn(|_| Waiter::new_pair());
    let mut map = HashMap::with_capacity(2);
    let mut bucket = FutexHashBucket::new();
    for (_, waker) in &pairs {
        bucket.enqueue(Arc::new(FutexObj {
            waker: waker.clone(),
            key: SpinLock::new(Some(first.clone())),
            bitset: u32::MAX,
            source_priority: core::sync::atomic::AtomicI32::new(100),
            requeue_pi: None,
        }));
    }
    map.insert(first.clone(), bucket);
    let mut pi = FutexHashBucket::new();
    pi.pi_refs = 1;
    map.insert(second.clone(), pi);
    pairs[2].1.close();
    let (_extra_waiter, extra_waker) = Waiter::new_pair();
    let extra = Arc::new(FutexObj {
        waker: extra_waker.clone(),
        key: SpinLock::new(Some(first.clone())),
        bitset: u32::MAX,
        source_priority: core::sync::atomic::AtomicI32::new(100),
        requeue_pi: None,
    });
    let extra_node = WaitNode::try_new(extra.clone())?;
    let mut pending = WaitQueueNodes::new();
    let _scope = denial::Scope::enter(true);
    // Second-key PI error must retain all three first-key selections, including
    // CLOSED. No notification should have occurred during selection.
    let result = Futex::select_wake_op(&mut map, &first, &second, 3, 1, true, &mut pending);
    let selected = matches!(result, Err(SystemError::EINVAL))
        && pending.len() == 3
        && map.get(&first).unwrap().chain.is_empty();
    // Simulate a timeout/signal notification between selection and dispatch.
    let raced = pairs[0].1.wake();
    Futex::notify_selected(&mut pending);
    let notified =
        !pairs[0].1.wake() && !pairs[1].1.wake() && pairs[2].1.is_closed() && pending.is_empty();
    // First-key error must not select the other key. The same production helper
    // observes keys in order rather than validating/reordering them up front.
    map.get_mut(&first).unwrap().chain.push_node(extra_node);
    let result = Futex::select_wake_op(&mut map, &second, &first, 1, 1, true, &mut pending);
    let first_error = matches!(result, Err(SystemError::EINVAL))
        && pending.is_empty()
        && map.get(&first).is_some_and(|bucket| {
            bucket.chain.len() == 1 && Arc::ptr_eq(bucket.chain.front().unwrap(), &extra)
        });
    Futex::notify_selected(&mut pending);
    // A fresh wake must still succeed: neither selection nor drain notified it.
    let untouched = extra_waker.wake();
    Ok(selected && raced && notified && first_error && untouched && denial::probe_denied())
}

/// Nonzero wake budget plus transfer: both halves use the production helper.
fn requeue_selected_transfer() -> Result<bool, SystemError> {
    let source = key(0x90);
    let target = key(0xa0);
    let pairs: [(Waiter, Arc<Waker>); 3] = core::array::from_fn(|_| Waiter::new_pair());
    let nodes: [Arc<FutexObj>; 3] = core::array::from_fn(|i| {
        Arc::new(FutexObj {
            waker: pairs[i].1.clone(),
            key: SpinLock::new(Some(source.clone())),
            bitset: u32::MAX,
            source_priority: core::sync::atomic::AtomicI32::new(100),
            requeue_pi: None,
        })
    });
    let mut map = HashMap::with_capacity(2);
    let mut bucket = FutexHashBucket::new();
    for node in &nodes {
        bucket.enqueue(node.clone());
    }
    map.insert(source.clone(), bucket);
    let mut pending = WaitQueueNodes::new();
    let result = {
        let _scope = denial::Scope::enter(true);
        let selected = Futex::requeue_existing(&mut map, &source, &target, 1, 2, &mut pending);
        let shape = selected
            .as_ref()
            .is_ok_and(|(count, retired)| *count == 3 && retired.is_some())
            && !map.contains_key(&source)
            && pending.len() == 1
            && Arc::ptr_eq(pending.front().unwrap(), &nodes[0])
            && map.get(&target).is_some_and(|b| {
                b.chain.len() == 2
                    && b.chain
                        .iter()
                        .zip(&nodes[1..])
                        .all(|(a, b)| Arc::ptr_eq(a, b))
            })
            && nodes[0].key.lock().is_none()
            && nodes[1..]
                .iter()
                .all(|n| n.key.lock().as_ref() == Some(&target));
        // Logical competing notification: selection itself has not notified.
        // This does not claim to execute a sleeping WAIT/timeout race.
        let before_dispatch = pairs[0].1.wake();
        Futex::notify_selected(&mut pending);
        let after_dispatch = !pairs[0].1.wake() && pending.is_empty();
        let transferred_untouched = pairs[1].1.wake() && pairs[2].1.wake();
        drop(selected);
        shape
            && before_dispatch
            && after_dispatch
            && transferred_untouched
            && denial::probe_denied()
    };
    Ok(result)
}

/// Deterministic interleavings of the production cancellation helper; not a
/// replacement for real sleeping WAIT/WAKE/timer race validation.
fn location_return() -> Result<bool, SystemError> {
    let source = key(0xb0);
    let target = key(0xc0);
    let pairs: [(Waiter, Arc<Waker>); 2] = core::array::from_fn(|_| Waiter::new_pair());
    let nodes: [Arc<FutexObj>; 2] = core::array::from_fn(|i| {
        Arc::new(FutexObj {
            waker: pairs[i].1.clone(),
            key: SpinLock::new(Some(source.clone())),
            bitset: u32::MAX,
            source_priority: core::sync::atomic::AtomicI32::new(100),
            requeue_pi: None,
        })
    });
    let mut map = HashMap::with_capacity(2);
    let mut bucket = FutexHashBucket::new();
    for node in &nodes {
        bucket.enqueue(node.clone());
    }
    map.insert(source.clone(), bucket);
    let mut pending = WaitQueueNodes::new();
    let _deny = denial::Scope::enter(true);
    // Some sampled before requeue: cancellation must use the new location.
    let observed_some = nodes[0].key.lock().is_some();
    let moved = Futex::requeue_existing(&mut map, &source, &target, 0, 1, &mut pending)?;
    let cancelled = Futex::unqueue_waiter(&mut map, &nodes[0])?;
    let self_cancel = observed_some
        && moved.0 == 1
        && cancelled.0
        && nodes[0].key.lock().is_none()
        && !map.contains_key(&target);
    drop(cancelled);
    drop(moved);
    // Some sampled before WAKE: marker becomes None before notification.
    let observed_some = nodes[1].key.lock().is_some();
    let selected = Futex::wake_existing(&mut map, &source, 1, u32::MAX, &mut pending)?;
    let before_notify =
        observed_some && selected == Some(1) && pending.len() == 1 && nodes[1].key.lock().is_none();
    let rechecked = Futex::unqueue_waiter(&mut map, &nodes[1])?;
    let wake_won = !rechecked.0;
    drop(rechecked);
    // Competing notification can precede pending dispatch; selected count stays.
    let raced = pairs[1].1.wake();
    Futex::notify_selected(&mut pending);
    Ok(self_cancel
        && before_notify
        && wake_won
        && raced
        && !pairs[1].1.wake()
        && pairs[0].1.wake()
        && pending.is_empty()
        && denial::probe_denied())
}
