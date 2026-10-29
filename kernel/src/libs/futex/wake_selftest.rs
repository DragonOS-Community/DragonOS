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
            key: SpinLock::new(if index == 0 {
                foreign.clone()
            } else {
                target.clone()
            }),
            bitset: if index == 1 || index == 5 { 2 } else { 1 },
            tid: 0,
        })
    });
    // A previously notified entry must not consume the successful wake limit.
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
            absent_ok &= Futex::wake_existing(&mut missing, &key(address), 1, u32::MAX)?.is_none();
        }
        absent_ok &= missing.is_empty() && missing.capacity() == 0;
        let first = Futex::wake_existing(&mut present, &target, 0, 1)? == Some(1);
        let order = present.get(&target).is_some_and(|bucket| {
            bucket.chain.len() == 4
                && bucket
                    .chain
                    .iter()
                    .zip([&nodes[0], &nodes[1], &nodes[4], &nodes[5]])
                    .all(|(actual, expected)| Arc::ptr_eq(actual, expected))
        });
        let literal_zero = present
            .get_mut(&target)
            .unwrap()
            .wake_up(target.clone(), Some(1), 0)?
            == 0;
        let second = Futex::wake_existing(&mut present, &target, 1, 1)? == Some(1);
        let rest = Futex::wake_existing(&mut present, &target, u32::MAX, u32::MAX)? == Some(2);
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
    Ok(stale_prepared && passed && requeue()? && denial::is_current_denied() == originally_denied)
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
            key: SpinLock::new(source.clone()),
            bitset: u32::MAX,
            tid: 0,
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
        let same = Futex::requeue_existing(&mut map, &source, &source, 0, 2)?;
        let first = Futex::requeue_existing(&mut map, &source, &target, 0, 1)?;
        let first_order = map.get(&target).is_some_and(|bucket| {
            bucket.chain.len() == 1 && Arc::ptr_eq(bucket.chain.front().unwrap(), &nodes[0])
        });
        let second = Futex::requeue_existing(&mut map, &source, &target, 0, 3)?;
        let third = Futex::requeue_existing(&mut map, &target, &final_key, 0, 3)?;
        let order = map.get(&final_key).is_some_and(|bucket| {
            bucket
                .chain
                .iter()
                .zip(&nodes)
                .all(|(actual, expected)| Arc::ptr_eq(actual, expected))
                && bucket.chain.len() == 3
        });
        let changed = nodes.iter().all(|node| *node.key.lock() == final_key);
        let retired = (second.1.is_some(), third.1.is_some());
        let bucket = map.get_mut(&final_key).unwrap();
        let removed = pairs.iter().all(|(_, waker)| bucket.remove_by_waker(waker));
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
        *node.key.lock() = source.clone();
        bucket.enqueue(node.clone());
    }
    full.insert(source.clone(), bucket);
    full.insert(key(0x40), FutexHashBucket::new());
    full.insert(key(0x50), FutexHashBucket::new());
    let failed = {
        let _scope = denial::Scope::enter(true);
        matches!(
            Futex::requeue_existing(&mut full, &source, &target, 1, 1),
            Err(SystemError::ENOMEM)
        ) && full
            .get(&source)
            .is_some_and(|bucket| bucket.chain.len() == 3)
            && !full.contains_key(&target)
    };
    let intact = pairs.iter().all(|(_, waker)| waker.wake());
    Ok(result && failed && intact)
}
