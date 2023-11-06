use std::{
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

pub mod mm;

#[derive(Debug)]
pub struct ObjectWrapper<T> {
    object: ManuallyDrop<T>,
}

impl<T> ObjectWrapper<T> {
    pub fn new(buf: &[u8]) -> Option<Self> {
        if buf.len() != std::mem::size_of::<T>() {
            return None;
        }
        let buf = buf.as_ptr() as *const T;

        let object = unsafe { ManuallyDrop::new(std::ptr::read(buf)) };
        Some(Self { object })
    }
}

impl<T> DerefMut for ObjectWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.object
    }
}

impl<T> Deref for ObjectWrapper<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.object
    }
}
