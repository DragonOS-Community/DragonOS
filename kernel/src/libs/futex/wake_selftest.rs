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
            key: if index == 0 {
                foreign.clone()
            } else {
                target.clone()
            },
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
    Ok(stale_prepared && passed && denial::is_current_denied() == originally_denied)
}
