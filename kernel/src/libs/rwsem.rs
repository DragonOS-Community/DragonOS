// SPDX-License-Identifier: GPL-2.0-or-later
//
// Sleepable Read-Write Semaphore (RwSem)

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{
        AtomicUsize,
        Ordering::{AcqRel, Acquire, Relaxed, Release},
    },
};

use alloc::rc::Rc;
use system_error::SystemError;

use crate::process::ProcessManager;

use super::wait_queue::{WaitQueue, Waiter};

/// A mutex that provides data access to either one writer or many readers.
///
/// # Overview
///
/// This mutex allows for multiple readers, or at most one writer to access
/// at any point in time. The writer of this mutex has exclusive access to
/// modify the underlying data, while the readers are allowed shared and
/// read-only access.
///
/// The writing and reading portions cannot be active simultaneously, when
/// one portion is in progress, the other portion will sleep. This is
/// suitable for scenarios where the mutex is expected to be held for a
/// period of time, which can avoid wasting CPU resources.
///
/// # Implementation Details
///
/// The internal representation of the mutex state is as follows:
/// - **Bit 63:** Writer mutex.
/// - **Bit 62:** Upgradeable reader mutex.
/// - **Bit 61:** Indicates if an upgradeable reader is being upgraded.
/// - **Bit 60:** Reader overflow detection (set when count reaches 2^60).
/// - **Bits 59-0:** Reader mutex count.
///
/// # Safety
///
/// Avoid using `RwSem` in an interrupt context, as it may result in sleeping
/// and never being awakened.
#[derive(Debug)]
pub struct RwSem<T: ?Sized> {
    lock: AtomicUsize,
    waiters: AtomicUsize,
    writer_waiters: AtomicUsize,
    queue: WaitQueue,
    val: UnsafeCell<T>,
}

const READER: usize = 1;
const WRITER: usize = 1 << (usize::BITS - 1);
const UPGRADEABLE_READER: usize = 1 << (usize::BITS - 2);
const BEING_UPGRADED: usize = 1 << (usize::BITS - 3);
const MAX_READER: usize = 1 << (usize::BITS - 4);

/// Read guard for [`RwSem`].
#[derive(Debug)]
pub struct RwSemReadGuard<'a, T: ?Sized + 'a> {
    inner: &'a RwSem<T>,
    // Mark as !Send
    _nosend: PhantomData<Rc<()>>,
}

/// Write guard for [`RwSem`].
#[derive(Debug)]
pub struct RwSemWriteGuard<'a, T: ?Sized + 'a> {
    inner: &'a RwSem<T>,
    // Mark as !Send
    _nosend: PhantomData<Rc<()>>,
}

/// Upgradeable read guard for [`RwSem`].
#[derive(Debug)]
pub struct RwSemUpgradeableGuard<'a, T: ?Sized + 'a> {
    inner: &'a RwSem<T>,
    // Mark as !Send
    _nosend: PhantomData<Rc<()>>,
}

struct RwSemWriterWaiter<'a, T: ?Sized + 'a> {
    inner: &'a RwSem<T>,
    active: bool,
}

// SAFETY: T must be Sync because multiple readers can access &T concurrently.
unsafe impl<T: ?Sized + Send> Send for RwSem<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwSem<T> {}

unsafe impl<T: ?Sized + Sync> Sync for RwSemWriteGuard<'_, T> {}
unsafe impl<T: ?Sized + Sync> Sync for RwSemReadGuard<'_, T> {}
unsafe impl<T: ?Sized + Sync> Sync for RwSemUpgradeableGuard<'_, T> {}

impl<T> RwSem<T> {
    /// Creates a new read-write semaphore with an initial value.
    pub const fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(val),
            lock: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            writer_waiters: AtomicUsize::new(0),
            queue: WaitQueue::default(),
        }
    }
}

impl<T: ?Sized> RwSem<T> {
    /// Acquires a read mutex and sleep until it can be acquired.
    ///
    /// The calling thread will sleep until there are no writers or upgrading
    /// upreaders present.
    #[track_caller]
    pub fn read(&self) -> RwSemReadGuard<'_, T> {
        if let Some(guard) = self.try_read() {
            return guard;
        }

        self.wait_read(false).unwrap()
    }

    /// Acquires a write mutex and sleep until it can be acquired.
    ///
    /// The calling thread will sleep until there are no writers, upreaders,
    /// or readers present.
    #[track_caller]
    pub fn write(&self) -> RwSemWriteGuard<'_, T> {
        if self.waiters.load(Acquire) == 0 || ProcessManager::current_pcb().preempt_count() != 0 {
            if let Some(guard) = self.try_write() {
                return guard;
            }
        }

        self.wait_write(false).unwrap()
    }

    /// Acquires a upread mutex and sleep until it can be acquired.
    ///
    /// The calling thread will sleep until there are no writers or upreaders present.
    #[track_caller]
    pub fn upread(&self) -> RwSemUpgradeableGuard<'_, T> {
        if let Some(guard) = self.try_upread() {
            return guard;
        }

        self.wait_upread(false).unwrap()
    }

    /// Blocking read acquire (interruptible).
    pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError> {
        if let Some(guard) = self.try_read() {
            return Ok(guard);
        }

        self.wait_read(true)
    }

    /// Blocking write acquire (interruptible).
    pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError> {
        if self.waiters.load(Acquire) == 0 || ProcessManager::current_pcb().preempt_count() != 0 {
            if let Some(guard) = self.try_write() {
                return Ok(guard);
            }
        }

        self.wait_write(true)
    }

    /// Attempts to acquire a read lock.
    ///
    /// This function will never sleep and will return immediately.
    pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>> {
        if self.writer_waiters.load(Acquire) != 0 {
            return None;
        }

        let lock = self.lock.fetch_add(READER, Acquire);
        if lock & (WRITER | BEING_UPGRADED | MAX_READER) == 0
            && self.writer_waiters.load(Acquire) == 0
        {
            Some(RwSemReadGuard {
                inner: self,
                _nosend: PhantomData,
            })
        } else {
            if self.lock.fetch_sub(READER, Release) == READER {
                self.queue.wake_one();
            }
            None
        }
    }

    /// Attempts to acquire a write lock.
    ///
    /// This function will never sleep and will return immediately.
    pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>> {
        if self
            .lock
            .compare_exchange(0, WRITER, Acquire, Relaxed)
            .is_ok()
        {
            Some(RwSemWriteGuard {
                inner: self,
                _nosend: PhantomData,
            })
        } else {
            None
        }
    }

    /// Attempts to acquire a upread mutex.
    ///
    /// This function will never sleep and will return immediately.
    pub fn try_upread(&self) -> Option<RwSemUpgradeableGuard<'_, T>> {
        if self.writer_waiters.load(Acquire) != 0 {
            return None;
        }

        let lock = self.lock.fetch_or(UPGRADEABLE_READER, Acquire) & (WRITER | UPGRADEABLE_READER);
        if lock == 0 && self.writer_waiters.load(Acquire) == 0 {
            return Some(RwSemUpgradeableGuard {
                inner: self,
                _nosend: PhantomData,
            });
        } else if (lock == 0 || lock == WRITER)
            && self.lock.fetch_sub(UPGRADEABLE_READER, Release) == UPGRADEABLE_READER
        {
            self.queue.wake_all();
        }
        None
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// This method is zero-cost: By holding a mutable reference to the lock, the compiler has
    /// already statically guaranteed that access to the data is exclusive.
    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }

    fn wait_read(&self, interruptible: bool) -> Result<RwSemReadGuard<'_, T>, SystemError> {
        self.waiters.fetch_add(1, AcqRel);
        let (waiter, waker) = Waiter::new_pair();

        loop {
            if let Err(e) = self.queue.register_waker(waker.clone()) {
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }

            if let Some(guard) = self.try_read() {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                return Ok(guard);
            }

            if let Err(e) = waiter.wait(interruptible) {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }
        }
    }

    fn wait_write(&self, interruptible: bool) -> Result<RwSemWriteGuard<'_, T>, SystemError> {
        let writer_waiter = self.begin_writer_wait();
        self.waiters.fetch_add(1, AcqRel);
        let (waiter, waker) = Waiter::new_pair();

        loop {
            if let Err(e) = self.queue.register_waker(waker.clone()) {
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }

            if let Some(guard) = self.try_write() {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                writer_waiter.finish();
                return Ok(guard);
            }

            if let Err(e) = waiter.wait(interruptible) {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }
        }
    }

    fn begin_writer_wait(&self) -> RwSemWriterWaiter<'_, T> {
        self.writer_waiters.fetch_add(1, AcqRel);
        RwSemWriterWaiter {
            inner: self,
            active: true,
        }
    }

    fn end_writer_wait(&self) {
        if self.writer_waiters.fetch_sub(1, Release) == 1 {
            self.queue.wake_all();
        }
    }

    fn wait_upread(
        &self,
        interruptible: bool,
    ) -> Result<RwSemUpgradeableGuard<'_, T>, SystemError> {
        self.waiters.fetch_add(1, AcqRel);
        let (waiter, waker) = Waiter::new_pair();

        loop {
            if let Err(e) = self.queue.register_waker(waker.clone()) {
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }

            if let Some(guard) = self.try_upread() {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                return Ok(guard);
            }

            if let Err(e) = waiter.wait(interruptible) {
                self.queue.remove_waker(&waker);
                self.waiters.fetch_sub(1, Release);
                return Err(e);
            }
        }
    }
}

impl<T: ?Sized> Deref for RwSemReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.inner.val.get() }
    }
}

impl<T: ?Sized> Drop for RwSemReadGuard<'_, T> {
    fn drop(&mut self) {
        // When there are no readers, wake up a waiting writer.
        if self.inner.lock.fetch_sub(READER, Release) == READER {
            self.inner.queue.wake_one();
        }
    }
}

impl<T: ?Sized> Deref for RwSemWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.inner.val.get() }
    }
}

impl<T: ?Sized> DerefMut for RwSemWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inner.val.get() }
    }
}

impl<'a, T> RwSemWriteGuard<'a, T> {
    /// Atomically downgrades a write guard to an upgradeable reader guard.
    ///
    /// This method always succeeds because the lock is exclusively held by the writer.
    pub fn downgrade(mut self) -> RwSemUpgradeableGuard<'a, T> {
        loop {
            self = match self.try_downgrade() {
                Ok(guard) => return guard,
                Err(e) => e,
            };
        }
    }

    /// This is not exposed as a public method to prevent intermediate lock states from affecting the
    /// downgrade process.
    fn try_downgrade(self) -> Result<RwSemUpgradeableGuard<'a, T>, Self> {
        let inner = self.inner;
        let res = self
            .inner
            .lock
            .compare_exchange(WRITER, UPGRADEABLE_READER, AcqRel, Relaxed);
        if res.is_ok() {
            core::mem::forget(self);
            // A writer->upread transition makes readers runnable again.
            inner.queue.wake_all();
            Ok(RwSemUpgradeableGuard {
                inner,
                _nosend: PhantomData,
            })
        } else {
            Err(self)
        }
    }
}

impl<T: ?Sized> Drop for RwSemWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.inner.lock.fetch_and(!WRITER, Release);

        // When the current writer releases, wake up all the sleeping threads.
        // All awakened threads may include readers and writers.
        // Thanks to the `wait_until` method, either all readers
        // continue to execute or one writer continues to execute.
        self.inner.queue.wake_all();
    }
}

impl<T: ?Sized> Deref for RwSemUpgradeableGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.inner.val.get() }
    }
}

impl<'a, T> RwSemUpgradeableGuard<'a, T> {
    /// Upgrades this upread guard to a write guard atomically.
    ///
    /// After calling this method, subsequent readers will be blocked
    /// while previous readers remain unaffected.
    ///
    /// The calling thread will not sleep, but spin to wait for the existing
    /// reader to be released.
    pub fn upgrade(mut self) -> RwSemWriteGuard<'a, T> {
        self.inner.lock.fetch_or(BEING_UPGRADED, Acquire);
        loop {
            self = match self.try_upgrade() {
                Ok(guard) => return guard,
                Err(e) => e,
            };
        }
    }

    /// Attempts to upgrade this upread guard to a write guard atomically.
    ///
    /// This function will return immediately.
    pub fn try_upgrade(self) -> Result<RwSemWriteGuard<'a, T>, Self> {
        let res = self.inner.lock.compare_exchange(
            UPGRADEABLE_READER | BEING_UPGRADED,
            WRITER | UPGRADEABLE_READER,
            AcqRel,
            Relaxed,
        );
        if res.is_ok() {
            let inner = self.inner;
            // Drop the upgradeable guard to clear the UPGRADEABLE_READER bit,
            // matching the asterinas semantics and avoiding a phantom upreader.
            core::mem::drop(self);
            Ok(RwSemWriteGuard {
                inner,
                _nosend: PhantomData,
            })
        } else {
            Err(self)
        }
    }
}

impl<T: ?Sized> Drop for RwSemUpgradeableGuard<'_, T> {
    fn drop(&mut self) {
        let res = self.inner.lock.fetch_sub(UPGRADEABLE_READER, Release);
        if res == UPGRADEABLE_READER {
            self.inner.queue.wake_all();
        }
    }
}

impl<T: ?Sized> RwSemWriterWaiter<'_, T> {
    fn finish(mut self) {
        if self.active {
            self.inner.end_writer_wait();
            self.active = false;
        }
    }
}

impl<T: ?Sized> Drop for RwSemWriterWaiter<'_, T> {
    fn drop(&mut self) {
        if self.active {
            self.inner.end_writer_wait();
        }
    }
}
