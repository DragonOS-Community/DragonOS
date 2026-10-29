//! Per-filesystem write admission after an unrecoverable metadata error.
//!
//! The device remains usable by other filesystems. No raw disk handle escapes
//! this owner: writeback, FAT allocation/free and namespace I/O share the gate.
use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    driver::base::{block::gendisk::GenDisk, device::device_number::DeviceNumber},
    libs::rwsem::RwSem,
};

#[derive(Debug)]
pub(super) struct FatStorage {
    disk: Arc<GenDisk>,
    failed: RwSem<bool>,
}

impl FatStorage {
    pub(super) fn new(disk: Arc<GenDisk>) -> Self {
        Self {
            disk,
            failed: RwSem::new(false),
        }
    }

    pub(super) fn check_writable(&self) -> Result<(), SystemError> {
        if *self.failed.read() {
            Err(SystemError::EROFS)
        } else {
            Ok(())
        }
    }

    /// Called only after the failing I/O and explicit undo have returned.
    /// Wait for earlier admitted writes, then permanently close admission.
    pub(super) fn fail(&self) {
        *self.failed.write() = true;
    }

    pub(super) fn read_at(&self, buffer: &mut [u8], lba: usize) -> Result<usize, SystemError> {
        self.disk.read_at(buffer, lba)
    }

    pub(super) fn read_at_bytes(
        &self,
        buffer: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        self.disk.read_at_bytes(buffer, offset)
    }

    pub(super) fn write_at(&self, buffer: &[u8], lba: usize) -> Result<usize, SystemError> {
        let failed = self.failed.read();
        if *failed {
            return Err(SystemError::EROFS);
        }
        self.disk.write_at(buffer, lba)
    }

    pub(super) fn write_at_bytes(
        &self,
        buffer: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let failed = self.failed.read();
        if *failed {
            return Err(SystemError::EROFS);
        }
        self.disk.write_at_bytes(buffer, offset)
    }

    pub(super) fn sync(&self) -> Result<(), SystemError> {
        let failed = self.failed.read();
        if *failed {
            return Err(SystemError::EROFS);
        }
        self.disk.sync()
    }

    pub(super) fn block_size_log2(&self) -> u8 {
        self.disk.block_size_log2()
    }
    pub(super) fn device_num(&self) -> DeviceNumber {
        self.disk.device_num()
    }
    pub(super) fn supports_reliable_flush(&self) -> bool {
        self.disk
            .block_device()
            .is_ok_and(|device| device.supports_reliable_flush())
    }
}
