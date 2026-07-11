use alloc::vec::Vec;
use core::{
    fmt,
    ops::{Deref, Range},
};

use system_error::SystemError;

use super::virtiofs::reply::VirtioFsReplyStorage;

pub(crate) enum FuseReplyStorage {
    Bytes(Vec<u8>),
    VirtioFs(VirtioFsReplyStorage),
}

pub struct FuseReply {
    storage: FuseReplyStorage,
    range: Range<usize>,
}

impl fmt::Debug for FuseReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FuseReply")
            .field("len", &self.len())
            .field("virtiofs", &self.is_virtiofs())
            .finish()
    }
}

impl FuseReply {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            storage: FuseReplyStorage::Bytes(bytes),
            range: 0..len,
        }
    }

    pub(crate) fn from_virtiofs(storage: VirtioFsReplyStorage) -> Self {
        let len = storage.as_slice().len();
        Self {
            storage: FuseReplyStorage::VirtioFs(storage),
            range: 0..len,
        }
    }

    pub(crate) fn narrow(mut self, range: Range<usize>) -> Result<Self, SystemError> {
        if range.start > range.end || range.end > self.range.len() {
            return Err(SystemError::EINVAL);
        }
        let start = self
            .range
            .start
            .checked_add(range.start)
            .ok_or(SystemError::EOVERFLOW)?;
        let end = self
            .range
            .start
            .checked_add(range.end)
            .ok_or(SystemError::EOVERFLOW)?;
        self.range = start..end;
        Ok(self)
    }

    pub(crate) fn is_virtiofs(&self) -> bool {
        matches!(self.storage, FuseReplyStorage::VirtioFs(_))
    }

    pub(crate) fn is_device_transfer(&self) -> bool {
        matches!(&self.storage, FuseReplyStorage::VirtioFs(storage) if storage.is_device())
    }

    pub(crate) fn into_compat_bytes(self, bytes: Vec<u8>) -> Result<Self, SystemError> {
        match self.storage {
            FuseReplyStorage::VirtioFs(storage) => {
                let storage = storage.into_compat_bytes(bytes)?;
                Ok(Self::from_virtiofs(storage))
            }
            FuseReplyStorage::Bytes(_) => Ok(Self::from_bytes(bytes)),
        }
    }

    fn storage_slice(&self) -> &[u8] {
        match &self.storage {
            FuseReplyStorage::Bytes(bytes) => bytes,
            FuseReplyStorage::VirtioFs(storage) => storage.as_slice(),
        }
    }
}

impl Deref for FuseReply {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.storage_slice()[self.range.clone()]
    }
}

impl AsRef<[u8]> for FuseReply {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl PartialEq for FuseReply {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for FuseReply {}

impl PartialEq<Vec<u8>> for FuseReply {
    fn eq(&self, other: &Vec<u8>) -> bool {
        &**self == other.as_slice()
    }
}
