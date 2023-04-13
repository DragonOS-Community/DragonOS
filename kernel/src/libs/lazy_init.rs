// Copyright (C) DragonOS Community  longjin

// This program is free software; you can redistribute it and/or
// modify it under the terms of the GNU General Public License
// as published by the Free Software Foundation; either version 2
// of the License, or (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
// Or you can visit https://www.gnu.org/licenses/gpl-2.0.html
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::fmt::Debug;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use super::spinlock::SpinLock;

/// A wrapper around a value that is initialized lazily.
pub struct Lazy<T> {
    /// The lock that is used to ensure that only one thread calls the init function at the same time.
    init_lock: SpinLock<()>,
    /// The value that is initialized lazily.
    value: UnsafeCell<MaybeUninit<T>>,
    /// Whether the value has been initialized.
    initialized: AtomicBool,
}

impl<T> Lazy<T> {
    /// Creates a new `Lazy` value that will be initialized with the
    /// result of the closure `init`.
    pub const fn new() -> Lazy<T> {
        Lazy {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            init_lock: SpinLock::new(()),
        }
    }

    /// Returns true if the value has been initialized.
    #[inline(always)]
    pub fn initialized(&self) -> bool {
        let initialized = self.initialized.load(Ordering::Acquire);
        if initialized {
            return true;
        }
        return false;
    }

    /// Ensures that this lazy value is initialized. If the value has not
    /// yet been initialized, this will raise a panic.
    #[inline(always)]
    pub fn ensure(&self) {
        if self.initialized() {
            return;
        }
        panic!("Lazy value was not initialized");
    }

    pub fn init(&self, value: T) {
        assert!(!self.initialized());

        // We need this lock to ensure that only one thread calls the init function at the same time.
        let _init_guard = self.init_lock.lock();
        // Check again, in case another thread initialized it while we were waiting for the lock.
        let initialized = self.initialized();
        if initialized {
            return;
        }
        unsafe {
            (*self.value.get()).as_mut_ptr().write(value);
        }
        self.initialized.store(true, Ordering::Release);
    }
    /// Forces the evaluation of this lazy value and returns a reference to
    /// the result. This is equivalent to the `Deref` impl, but is explicit.
    /// This will initialize the value if it has not yet been initialized.
    pub fn get(&self) -> &T {
        self.ensure();
        return unsafe { self.get_unchecked() };
    }

    /// Returns a reference to the value if it has been initialized.
    /// Otherwise, returns `None`.
    pub fn try_get(&self) -> Option<&T> {
        if self.initialized() {
            return Some(unsafe { self.get_unchecked() });
        }
        return None;
    }

    /// Forces the evaluation of this lazy value and returns a mutable
    /// reference to the result. This is equivalent to the `DerefMut` impl,
    /// but is explicit. This will initialize the value if it has not yet
    /// been initialized.
    pub fn get_mut(&mut self) -> &mut T {
        self.ensure();
        return unsafe { self.get_mut_unchecked() };
    }

    #[inline(always)]
    pub unsafe fn get_unchecked(&self) -> &T {
        return &*(*self.value.get()).as_ptr();
    }

    #[inline(always)]
    pub unsafe fn get_mut_unchecked(&mut self) -> &mut T {
        return &mut *(*self.value.get()).as_mut_ptr();
    }
}

impl<T> Deref for Lazy<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        return self.get();
    }
}

impl<T> DerefMut for Lazy<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        return self.get_mut();
    }
}

impl<T: Debug> Debug for Lazy<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if let Some(value) = self.try_get() {
            return write!(f, "Lazy({:?})", value);
        } else {
            return write!(f, "Lazy(uninitialized)");
        }
    }
}

impl<T> Drop for Lazy<T> {
    fn drop(&mut self) {
        if self.initialized() {
            unsafe {
                (*self.value.get()).as_mut_ptr().drop_in_place();
            }
        }
    }
}

unsafe impl<T: Send + Sync> Sync for Lazy<T> {}
unsafe impl<T: Send> Send for Lazy<T> {}
