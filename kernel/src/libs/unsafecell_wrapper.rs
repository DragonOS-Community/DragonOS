#![allow(dead_code)]
use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
};

#[derive(Debug)]
pub struct UnsafeCellWrapper<T>(pub UnsafeCell<T>);

unsafe impl<T> Sync for UnsafeCellWrapper<T> {}
unsafe impl<T> Send for UnsafeCellWrapper<T> {}

impl<T> Deref for UnsafeCellWrapper<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl<T> DerefMut for UnsafeCellWrapper<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

#[allow(clippy::mut_from_ref)]
impl<T> UnsafeCellWrapper<T> {
    pub fn force_get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}
