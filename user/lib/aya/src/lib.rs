#![allow(unused)]

#[macro_use]
extern crate log;

mod bpf;
pub mod maps;
pub mod pin;
pub mod programs;
mod sys;
pub mod util;

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use aya_obj as obj;
pub use bpf::*;
pub use obj::btf::{Btf, BtfError};
pub use object::Endianness;
pub use programs::loaded_programs;
// See https://github.com/rust-lang/rust/pull/124210; this structure exists to avoid crashing the
// process when we try to close a fake file descriptor.
#[derive(Debug)]
struct MockableFd {
    #[cfg(not(test))]
    fd: OwnedFd,
    #[cfg(test)]
    fd: Option<OwnedFd>,
}

impl MockableFd {
    #[cfg(test)]
    const fn mock_signed_fd() -> i32 {
        1337
    }

    #[cfg(test)]
    const fn mock_unsigned_fd() -> u32 {
        1337
    }

    #[cfg(not(test))]
    fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    #[cfg(test)]
    fn from_fd(fd: OwnedFd) -> Self {
        Self { fd: Some(fd) }
    }

    #[cfg(not(test))]
    fn try_clone(&self) -> std::io::Result<Self> {
        let Self { fd } = self;
        let fd = fd.try_clone()?;
        Ok(Self { fd })
    }

    #[cfg(test)]
    fn try_clone(&self) -> std::io::Result<Self> {
        let Self { fd } = self;
        let fd = fd.as_ref().map(OwnedFd::try_clone).transpose()?;
        Ok(Self { fd })
    }
}

impl AsFd for MockableFd {
    #[cfg(not(test))]
    fn as_fd(&self) -> BorrowedFd<'_> {
        let Self { fd } = self;
        fd.as_fd()
    }

    #[cfg(test)]
    fn as_fd(&self) -> BorrowedFd<'_> {
        let Self { fd } = self;
        fd.as_ref().unwrap().as_fd()
    }
}

impl Drop for MockableFd {
    #[cfg(not(test))]
    fn drop(&mut self) {
        // Intentional no-op.
    }

    #[cfg(test)]
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;

        let Self { fd } = self;
        if fd.as_ref().unwrap().as_raw_fd() >= Self::mock_signed_fd() {
            let fd: OwnedFd = fd.take().unwrap();
            std::mem::forget(fd)
        }
    }
}
