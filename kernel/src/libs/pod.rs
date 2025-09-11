use core::mem::MaybeUninit;

/// A trait for Plain Old Data (POD) types.
/// POD types are types that can be safely treated as a sequence of bytes.
/// They can be copied, moved, and compared using their byte representation.
///
///  # Safety
pub unsafe trait Pod: Copy + Sized {
    #[allow(unused)]
    fn new_zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }

    #[allow(unused)]
    fn new_uninit() -> Self {
        #[allow(clippy::uninit_assumed_init)]
        unsafe {
            MaybeUninit::uninit().assume_init()
        }
    }

    #[allow(unused)]
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut new_self = Self::new_uninit();
        let copy_len = new_self.as_bytes().len();
        new_self.as_bytes_mut().copy_from_slice(&bytes[..copy_len]);
        new_self
    }

    #[allow(unused)]
    fn as_bytes(&self) -> &[u8] {
        let ptr = self as *const Self as *const u8;
        let len = core::mem::size_of::<Self>();
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    #[allow(unused)]
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        let ptr = self as *mut Self as *mut u8;
        let len = core::mem::size_of::<Self>();
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }
}

macro_rules! impl_pod_for {
    ($($pod_ty:ty),*) => {
        $(unsafe impl Pod for $pod_ty {})*
    };
}

// impl Pod for primitive types
impl_pod_for!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, isize, usize);
