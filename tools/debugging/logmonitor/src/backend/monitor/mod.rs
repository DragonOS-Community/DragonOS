use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

pub mod logset;
pub mod mm;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectWrapper<T> {
    object: Box<T>,
}

impl<T: Debug + Sized> ObjectWrapper<T> {
    pub fn new(buf: &[u8]) -> Option<Self> {
        if buf.len() != std::mem::size_of::<T>() {
            println!(
                "ObjectWrapper::new(): buf.len() '{}'  != std::mem::size_of::<T>(): '{}'",
                buf.len(),
                std::mem::size_of::<T>()
            );
            return None;
        }
        let x = unsafe { std::ptr::read(buf.as_ptr() as *const T) };

        let object = Box::new(x);

        // let object = ManuallyDrop::new(x);
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
