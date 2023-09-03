use core::{
    fmt::{self, Debug, Formatter},
    sync::atomic::Ordering,
};

use atomic_enum::atomic_enum;

pub struct Once {
    inner: AtomicOnceState,
}

#[atomic_enum]
#[derive(PartialEq, Eq)]
pub enum OnceState {
    Incomplete,
    Posioned,
    Complete,
}

#[allow(dead_code)]
impl Once {
    pub const fn new() -> Self {
        Self {
            inner: AtomicOnceState::new(OnceState::Incomplete),
        }
    }

    #[track_caller]
    pub fn call_once<F: FnOnce()>(&self, f: F) {
        if self.is_completed() {
            return;
        }

        // set initialized
        let r = self.inner.compare_exchange(
            OnceState::Incomplete,
            OnceState::Posioned,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        if r.is_err() {
            return;
        }
        // call function
        f();
        // set completed
        self.inner.store(OnceState::Complete, Ordering::SeqCst);
    }

    /// Performs the same function as [`call_once()`] except ignores poisoning.
    ///
    /// Unlike [`call_once()`], if this [`Once`] has been poisoned (i.e., a previous
    /// call to [`call_once()`] or [`call_once_force()`] caused a panic), calling
    /// [`call_once_force()`] will still invoke the closure `f` and will _not_
    /// result in an immediate panic. If `f` panics, the [`Once`] will remain
    /// in a poison state. If `f` does _not_ panic, the [`Once`] will no
    /// longer be in a poison state and all future calls to [`call_once()`] or
    /// [`call_once_force()`] will be no-ops.
    ///
    /// The closure `f` is yielded a [`OnceState`] structure which can be used
    /// to query the poison status of the [`Once`].
    ///
    /// [`call_once()`]: Once::call_once
    /// [`call_once_force()`]: Once::call_once_force
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Once;
    /// use std::thread;
    ///
    /// static INIT: Once = Once::new();
    ///
    /// // poison the once
    /// let handle = thread::spawn(|| {
    ///     INIT.call_once(|| panic!());
    /// });
    /// assert!(handle.join().is_err());
    ///
    /// // poisoning propagates
    /// let handle = thread::spawn(|| {
    ///     INIT.call_once(|| {});
    /// });
    /// assert!(handle.join().is_err());
    ///
    /// // call_once_force will still run and reset the poisoned state
    /// INIT.call_once_force(|state| {
    ///     assert!(state.is_poisoned());
    /// });
    ///
    /// // once any success happens, we stop propagating the poison
    /// INIT.call_once(|| {});
    /// ```
    pub fn call_once_force<F>(&self, f: F)
    where
        F: FnOnce(&OnceState),
    {
        // fast path check
        if self.is_completed() {
            return;
        }

        // set poisoned
        self.inner
            .compare_exchange(
                OnceState::Incomplete,
                OnceState::Posioned,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .ok();

        // call function
        f(&self.inner.load(Ordering::SeqCst));

        // set initialized
        self.inner.store(OnceState::Complete, Ordering::SeqCst);
    }

    /// Fast path check
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.inner.load(Ordering::SeqCst) == OnceState::Complete
    }

    /// Returns the current state of the `Once` instance.
    #[inline]
    pub fn state(&self) -> OnceState {
        self.inner.load(Ordering::SeqCst)
    }
}

impl Debug for Once {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Once").finish_non_exhaustive()
    }
}

#[allow(dead_code)]
impl OnceState {
    /// Returns `true` if the associated [`Once`] was poisoned prior to the
    /// invocation of the closure passed to [`Once::call_once_force()`].
    ///
    /// # Examples
    ///
    /// A poisoned [`Once`]:
    ///
    /// ```
    /// use std::sync::Once;
    /// use std::thread;
    ///
    /// static INIT: Once = Once::new();
    ///
    /// // poison the once
    /// let handle = thread::spawn(|| {
    ///     INIT.call_once(|| panic!());
    /// });
    /// assert!(handle.join().is_err());
    ///
    /// INIT.call_once_force(|state| {
    ///     assert!(state.is_poisoned());
    /// });
    /// ```
    ///
    /// An unpoisoned [`Once`]:
    ///
    /// ```
    /// use std::sync::Once;
    ///
    /// static INIT: Once = Once::new();
    ///
    /// INIT.call_once_force(|state| {
    ///     assert!(!state.is_poisoned());
    /// });
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        *self == OnceState::Posioned
    }
}
