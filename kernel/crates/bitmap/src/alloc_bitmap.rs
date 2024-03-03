use alloc::vec::Vec;

use crate::{bitmap_core::BitMapCore, traits::BitMapOps};

#[derive(Clone)]
pub struct AllocBitmap {
    elements: usize,
    data: Vec<usize>,
    core: BitMapCore<usize>,
}

impl AllocBitmap {
    pub fn new(elements: usize) -> Self {
        let data = vec![0usize; (elements + usize::BITS as usize - 1) / (usize::BITS as usize)];
        Self {
            elements,
            data,
            core: BitMapCore::new(),
        }
    }
}

impl BitMapOps<usize> for AllocBitmap {
    #[inline]
    fn get(&self, index: usize) -> Option<bool> {
        return self.core.get(self.elements, &self.data, index);
    }

    #[inline]
    fn set(&mut self, index: usize, value: bool) -> Option<bool> {
        return self.core.set(self.elements, &mut self.data, index, value);
    }

    #[inline]
    fn len(&self) -> usize {
        self.elements
    }

    #[inline]
    fn size(&self) -> usize {
        self.data.len() * core::mem::size_of::<usize>()
    }

    #[inline]
    fn first_index(&self) -> Option<usize> {
        self.core.first_index(&self.data)
    }

    #[inline]
    fn first_false_index(&self) -> Option<usize> {
        self.core.first_false_index(self.elements, &self.data)
    }

    #[inline]
    fn last_index(&self) -> Option<usize> {
        self.core.last_index(self.elements, &self.data)
    }

    #[inline]
    fn last_false_index(&self) -> Option<usize> {
        self.core.last_false_index(self.elements, &self.data)
    }

    #[inline]
    fn next_index(&self, index: usize) -> Option<usize> {
        self.core.next_index(self.elements, &self.data, index)
    }

    #[inline]
    fn next_false_index(&self, index: usize) -> Option<usize> {
        self.core.next_false_index(self.elements, &self.data, index)
    }

    #[inline]
    fn prev_index(&self, index: usize) -> Option<usize> {
        self.core.prev_index(self.elements, &self.data, index)
    }

    #[inline]
    fn prev_false_index(&self, index: usize) -> Option<usize> {
        self.core.prev_false_index(self.elements, &self.data, index)
    }

    #[inline]
    fn invert(&mut self) {
        self.core.invert(self.elements, &mut self.data);
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.core.is_full(self.elements, &self.data)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.core.is_empty(&self.data)
    }

    #[inline]
    unsafe fn as_bytes(&self) -> &[u8] {
        core::slice::from_raw_parts(
            self.data.as_ptr() as *const u8,
            core::mem::size_of::<Self>(),
        )
    }

    fn set_all(&mut self, value: bool) {
        self.core.set_all(self.elements, &mut self.data, value);
    }
}
