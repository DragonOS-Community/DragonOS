//! Connection-private ownership transfer. No allocation or complex final Drop
//! occurs while selecting requests under the connection lock.
use super::{Arc, FusePendingState, FuseRequest, SpinLock, VecDeque};

pub(super) trait RetireLink: Sized {
    fn retire_link(&self) -> &SpinLock<Option<Arc<Self>>>;
}

impl RetireLink for FuseRequest {
    fn retire_link(&self) -> &SpinLock<Option<Arc<Self>>> {
        &self.retired_next
    }
}

impl RetireLink for FusePendingState {
    fn retire_link(&self) -> &SpinLock<Option<Arc<Self>>> {
        &self.retired_next
    }
}

pub(super) struct Retired<T: RetireLink> {
    head: Option<Arc<T>>,
}

impl<T: RetireLink> Retired<T> {
    pub(super) fn new() -> Self {
        Self { head: None }
    }

    /// The caller exclusively removes this owner from its connection container.
    /// No object may be present in two retirement batches at once.
    pub(super) fn push(&mut self, owner: Arc<T>) {
        let mut next = owner.retire_link().lock();
        debug_assert!(next.is_none());
        *next = self.head.take();
        drop(next);
        self.head = Some(owner);
    }

    pub(super) fn pop(&mut self) -> Option<Arc<T>> {
        let owner = self.head.take()?;
        self.head = owner.retire_link().lock().take();
        Some(owner)
    }
}

impl<T: RetireLink> Drop for Retired<T> {
    fn drop(&mut self) {
        // Also iterative on early returns: a long batch must not recurse through
        // Arc -> next -> Arc destructors on the small kernel stack.
        while let Some(owner) = self.pop() {
            drop(owner);
        }
    }
}

/// Stable partition in the original allocation: every retained push follows a
/// pop, so capacity and relative order are preserved without reserve/growth.
pub(super) fn remove_requests(
    queue: &mut VecDeque<Arc<FuseRequest>>,
    retired: &mut Retired<FuseRequest>,
    mut remove: impl FnMut(&FuseRequest) -> bool,
) -> usize {
    let mut count = 0;
    for _ in 0..queue.len() {
        let request = queue.pop_front().expect("initial queue length");
        if remove(&request) {
            retired.push(request);
            count += 1;
        } else {
            queue.push_back(request);
        }
    }
    count
}
