//! Owning FIFO with fallibly prepared nodes. Publication, transfer and retirement
//! do not allocate; policy and synchronization belong to the caller.
use alloc::boxed::Box;
use core::{fmt, ptr::NonNull};
use system_error::SystemError;

pub(crate) struct Node<T> {
    value: T,
    next: Option<Box<Node<T>>>,
}

impl<T: fmt::Debug> fmt::Debug for Node<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Node")
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

impl<T> Node<T> {
    pub(crate) fn try_new(value: T) -> Result<Box<Self>, SystemError> {
        Box::try_new(Self { value, next: None }).map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn into_value(self) -> T {
        debug_assert!(self.next.is_none());
        self.value
    }

    pub(crate) fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    pub(crate) fn value(&self) -> &T {
        &self.value
    }
}

pub(crate) struct Queue<T> {
    head: Option<Box<Node<T>>>,
    // Non-owning pointer into head's uniquely owned chain; null iff empty.
    tail: Option<NonNull<Node<T>>>,
    len: usize,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Queue<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never recursively format the owning next links of a deep queue.
        formatter.debug_list().entries(self.iter()).finish()
    }
}

// The raw tail never outlives the owning chain and is only mutated via &mut.
unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Sync> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(crate) fn front(&self) -> Option<&T> {
        self.head.as_ref().map(|n| &n.value)
    }

    pub(crate) fn front_mut(&mut self) -> Option<&mut T> {
        self.head.as_mut().map(|node| &mut node.value)
    }

    pub(crate) fn back(&self) -> Option<&T> {
        // SAFETY: the non-owning tail points into our live, uniquely owned chain.
        self.tail.map(|tail| unsafe { &tail.as_ref().value })
    }

    pub(crate) fn push_node(&mut self, mut node: Box<Node<T>>) {
        debug_assert!(node.next.is_none());
        let new_tail = NonNull::from(node.as_mut());
        if let Some(mut tail) = self.tail {
            // SAFETY: tail belongs to this exclusively borrowed queue. Linking
            // the new unique Box does not move the allocation tail points to.
            unsafe {
                tail.as_mut().next = Some(node);
            }
        } else {
            self.head = Some(node);
        }
        self.tail = Some(new_tail);
        self.len += 1;
    }

    /// Return an already prepared event to the head without allocating.
    pub(crate) fn push_front_node(&mut self, mut node: Box<Node<T>>) {
        debug_assert!(node.next.is_none());
        if self.head.is_none() {
            self.tail = Some(NonNull::from(node.as_mut()));
        }
        node.next = self.head.take();
        self.head = Some(node);
        self.len += 1;
    }

    pub(crate) fn pop_node(&mut self) -> Option<Box<Node<T>>> {
        let mut node = self.head.take()?;
        self.head = node.next.take();
        self.len -= 1;
        if self.head.is_none() {
            self.tail = None;
        }
        Some(node)
    }

    /// Detach matching nodes for retirement after the caller releases its lock.
    /// Both partitions retain FIFO order; no values are dropped during transfer.
    pub(crate) fn extract_if(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Self {
        let mut retained = Self::new();
        let mut extracted = Self::new();
        while let Some(node) = self.pop_node() {
            if predicate(node.value()) {
                extracted.push_node(node);
            } else {
                retained.push_node(node);
            }
        }
        *self = retained;
        extracted
    }

    pub(crate) fn iter(&self) -> Iter<'_, T> {
        Iter(self.head.as_deref())
    }

    pub(crate) fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut(self.head.as_deref_mut())
    }

    pub(crate) fn split_off(&mut self, at: usize) -> Self {
        assert!(at <= self.len);
        if at == 0 {
            return core::mem::take(self);
        }
        if at == self.len {
            return Self::new();
        }
        let mut last = self.head.as_deref_mut().unwrap();
        for _ in 1..at {
            last = last.next.as_deref_mut().unwrap();
        }
        let suffix = last.next.take();
        let tail = self.tail;
        self.tail = Some(NonNull::from(last));
        let len = self.len - at;
        self.len = at;
        Self {
            head: suffix,
            tail,
            len,
        }
    }

    pub(crate) fn append(&mut self, other: &mut Self) {
        if other.is_empty() {
            return;
        }
        if let Some(mut tail) = self.tail {
            // SAFETY: both queues are exclusively borrowed and own disjoint
            // nodes. Transfer the other head without changing node addresses.
            unsafe {
                tail.as_mut().next = other.head.take();
            }
        } else {
            self.head = other.head.take();
        }
        self.tail = other.tail.take();
        self.len += other.len;
        other.len = 0;
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        while self.pop_node().is_some() {}
    }
}

pub(crate) struct Iter<'a, T>(Option<&'a Node<T>>);
impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        let node = self.0.take()?;
        self.0 = node.next.as_deref();
        Some(&node.value)
    }
}

pub(crate) struct IterMut<'a, T>(Option<&'a mut Node<T>>);
impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.0.take()?;
        self.0 = node.next.as_deref_mut();
        Some(&mut node.value)
    }
}
impl<'a, T> IntoIterator for &'a Queue<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn mutable_iteration_and_transfer_keep_nodes_and_tail() {
        // Default must not require the element to implement Default.
        struct Value(usize);
        let mut queue = Queue::<Value>::default();
        assert!(queue.front_mut().is_none());
        assert!(queue.back().is_none());
        let node = Node::try_new(Value(1)).unwrap();
        let address = node.as_ref() as *const Node<Value>;
        queue.push_node(node);
        queue.push_node(Node::try_new(Value(2)).unwrap());
        queue.front_mut().unwrap().0 = 3;
        for value in queue.iter_mut() {
            value.0 += 1;
        }
        assert_eq!(queue.front().unwrap().0, 4);
        assert_eq!(queue.back().unwrap().0, 3);
        let mut suffix = queue.split_off(1);
        suffix.append(&mut queue);
        assert_eq!(suffix.back().unwrap().0, 4);
        let first = suffix.pop_node().unwrap();
        assert_eq!(first.value().0, 3);
        let original = suffix.pop_node().unwrap();
        assert_eq!(original.as_ref() as *const Node<Value>, address);
        assert!(suffix.back().is_none());
    }

    #[test]
    fn blocked_node_returns_ahead_of_concurrent_arrivals() {
        let mut queue = Queue::new();
        let blocked = Node::try_new(1usize).unwrap();
        let address = blocked.as_ref() as *const Node<usize>;
        queue.push_node(blocked);
        let blocked = queue.pop_node().unwrap();
        queue.push_node(Node::try_new(2).unwrap());
        queue.push_node(Node::try_new(3).unwrap());
        queue.push_front_node(blocked);
        let first = queue.pop_node().unwrap();
        assert_eq!(first.as_ref() as *const Node<usize>, address);
        assert_eq!(*first.value(), 1);
        assert_eq!(queue.pop_node().unwrap().into_value(), 2);
        assert_eq!(queue.pop_node().unwrap().into_value(), 3);
        assert!(queue.is_empty());
        queue.push_front_node(first);
        assert_eq!(queue.back(), Some(&1));
        queue.push_node(Node::try_new(4).unwrap());
        assert_eq!(queue.pop_node().unwrap().into_value(), 1);
        assert_eq!(queue.pop_node().unwrap().into_value(), 4);
        assert!(queue.back().is_none());
    }

    #[test]
    fn deep_debug_and_drop_visit_each_value_once() {
        #[derive(Debug)]
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = Queue::new();
        for _ in 0..4096 {
            queue.push_node(Node::try_new(Counted(drops.clone())).unwrap());
        }
        let formatted = format!("{queue:?}");
        assert_eq!(formatted.matches("Counted").count(), 4096);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(queue);
        assert_eq!(drops.load(Ordering::Relaxed), 4096);
    }

    #[test]
    fn prepared_nodes_preserve_fifo_across_split_and_append() {
        let mut source = Queue::new();
        for value in 0..4096 {
            source.push_node(Node::try_new(value).unwrap());
        }
        let mut suffix = source.split_off(17);
        assert_eq!(source.len(), 17);
        assert_eq!(suffix.front(), Some(&17));
        source.append(&mut suffix);
        assert!(suffix.is_empty());
        assert!(source.iter().copied().eq(0..4096));
        let mut all = source.split_off(0);
        assert!(source.is_empty());
        for expected in 0..4096 {
            assert_eq!(*all.pop_node().unwrap().value(), expected);
        }
        source.append(&mut all);
        source.push_node(Node::try_new(9).unwrap());
        assert_eq!(source.front(), Some(&9));
    }

    #[test]
    fn extraction_preserves_order_and_tail_for_reuse() {
        let mut queue = Queue::new();
        for value in 0..100 {
            queue.push_node(Node::try_new(value).unwrap());
        }
        let mut even = queue.extract_if(|value| value % 2 == 0);
        assert!(queue.iter().copied().eq((0..100).filter(|v| v % 2 != 0)));
        assert!(even.iter().copied().eq((0..100).filter(|v| v % 2 == 0)));
        let mut all = queue.extract_if(|_| true);
        assert!(queue.is_empty());
        queue.push_node(Node::try_new(101).unwrap());
        assert_eq!(queue.pop_node().unwrap().into_value(), 101);
        all.append(&mut even);
        assert_eq!(all.len(), 100);
        assert!(even.is_empty());
    }

    #[test]
    fn long_queue_drop_is_iterative() {
        let mut queue = Queue::new();
        for value in 0..32768 {
            queue.push_node(Node::try_new(value).unwrap());
        }
        drop(queue);
    }
}
