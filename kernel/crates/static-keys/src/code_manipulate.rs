//! Utilities to manipulate memory protection.
//!
//! Since we need to make the code region writable and restore it during jump entry update,
//! we need to provide utility functions here.

use core::{convert::Infallible, marker::PhantomData};

/// Backend for one complete static-key update.
///
/// # Safety
///
/// Implementations must serialize opposing updates and ensure that a successful
/// transaction makes every queued instruction safe to execute on every CPU.
/// If an implementation can return an error, it must not modify any instruction
/// before `commit` has established that the whole queued update can complete.
pub unsafe trait CodePatchBackend {
    type Error;
    type Transaction: CodePatchTransaction<Error = Self::Error>;

    fn begin() -> Result<Self::Transaction, Self::Error>;
}

/// A transaction covering all jump entries belonging to one static key.
///
/// # Safety
///
/// `queue` receives instruction addresses emitted by this crate. Implementors
/// must copy the bytes if they retain them and must validate `expected` before
/// replacing it with `replacement`.
pub unsafe trait CodePatchTransaction {
    type Error;

    /// Queue one instruction replacement in this transaction.
    ///
    /// # Safety
    ///
    /// `addr` must be the instruction address associated with `expected` and
    /// `replacement`, and both byte sequences must encode complete valid
    /// instructions for that site.
    unsafe fn queue<const L: usize>(
        &mut self,
        addr: *mut core::ffi::c_void,
        expected: &[u8; L],
        replacement: &[u8; L],
    ) -> Result<(), Self::Error>;

    fn commit(self) -> Result<(), Self::Error>;
}

/// Manipulate memory protection in code region.
pub trait CodeManipulator {
    /// Write `data` as code instruction to `addr`.
    ///
    /// The `addr` is not aligned, you need to align it you self. The length is not too long, usually
    /// 5 bytes.
    ///
    /// # Safety
    ///
    /// This method will do best effort to make the code region writable, and then write the data into
    /// the code region. If the code region is still not writable, the data writing will become a UB.
    /// Never call this method when there are multi-threads running. Spawn threads after this method
    /// is called. This method may manipulate code region memory protection, and if other threads are
    /// executing codes in the same code page, it may lead to unexpected behaviors.
    unsafe fn write_code<const L: usize>(addr: *mut core::ffi::c_void, data: &[u8; L]);
}

/// Compatibility transaction for the upstream immediate-write backends.
///
/// These backends retain their original single-thread-only safety contract.
pub struct ImmediateTransaction<M>(PhantomData<M>);

unsafe impl<M: CodeManipulator> CodePatchBackend for M {
    type Error = Infallible;
    type Transaction = ImmediateTransaction<M>;

    fn begin() -> Result<Self::Transaction, Self::Error> {
        Ok(ImmediateTransaction(PhantomData))
    }
}

unsafe impl<M: CodeManipulator> CodePatchTransaction for ImmediateTransaction<M> {
    type Error = Infallible;

    unsafe fn queue<const L: usize>(
        &mut self,
        addr: *mut core::ffi::c_void,
        _expected: &[u8; L],
        replacement: &[u8; L],
    ) -> Result<(), Self::Error> {
        unsafe { M::write_code(addr, replacement) };
        Ok(())
    }

    fn commit(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Dummy code manipulator. Do nothing. Used to declare a dummy static key which is never modified
pub(crate) struct DummyCodeManipulator;

impl CodeManipulator for DummyCodeManipulator {
    unsafe fn write_code<const L: usize>(_addr: *mut core::ffi::c_void, _data: &[u8; L]) {}
}
