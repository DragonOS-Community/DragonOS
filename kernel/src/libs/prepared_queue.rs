//! Owning FIFO with fallibly prepared nodes. Publication, transfer and retirement
//! do not allocate; policy and synchronization belong to the caller.
use alloc::boxed::Box;
use core::ptr::NonNull;
use system_error::SystemError;

pub(crate) struct Node<T> {
    value: T,
    next: Option<Box<Node<T>>>,
}

impl<T> Node<T> {
    pub(crate) fn try_new(value: T) -> Result<Box<Self>, SystemError> {
        Box::try_new(Self { value, next: None }).map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn into_value(self) -> T {
        debug_assert!(self.next.is_none());
        self.value
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

// The raw tail never outlives the owning chain and is only mutated via &mut.
unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Sync> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn split_off(&mut self, at: usize) -> Self {
        assert!(at <= self.len);
        if at == 0 {
            return core::mem::replace(self, Self::new());
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
