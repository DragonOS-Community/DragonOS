//! The File's public, resource-lock-free recovery boundary. Stacking providers
//! use the typed single-attempt methods instead, preserving their transaction.

use super::{DelegatedWriteResult, File, MemoryAllocationError, SystemError};
use crate::cgroup::memory::runtime::ChargeRecoveryState;

impl File {
    pub(super) fn set_completed_sync_range(
        result: &mut DelegatedWriteResult,
        start: usize,
        intent: super::WriteSyncIntent,
    ) {
        // A loop backing OFD may itself request O_SYNC even when its outer
        // block-device File does not. Retain that intent but translate the
        // range back to this File's coordinate system exactly once.
        let inherited = match result.sync_range {
            Some((_, _, false)) => super::WriteSyncIntent::Full,
            Some((_, _, true)) => super::WriteSyncIntent::Data,
            None => super::WriteSyncIntent::None,
        };
        let datasync = match intent.combine(inherited) {
            super::WriteSyncIntent::None => None,
            super::WriteSyncIntent::Data => Some(true),
            super::WriteSyncIntent::Full => Some(false),
        };
        result.sync_range = datasync.filter(|_| result.written_len > 0).map(|datasync| {
            (
                start,
                start.saturating_add(result.written_len - 1),
                datasync,
            )
        });
    }

    pub(crate) fn try_pread(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        self.pread_with_fsnotify(offset, len, buf, true)
    }

    pub(crate) fn read_at_for_transfer(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        Self::recover_io(|| self.try_read_at_for_transfer(offset, len, buf))
    }

    pub(crate) fn recover_io<T>(
        mut attempt: impl FnMut() -> Result<T, MemoryAllocationError>,
    ) -> Result<T, SystemError> {
        let mut recovery = ChargeRecoveryState::default();
        loop {
            match attempt() {
                Ok(value) => return Ok(value),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    /// Called with resource guards released, retaining the caller's OFD
    /// position mutex. Only synchronization may be retried after data commits.
    pub(crate) fn finish_write(
        &self,
        mut result: DelegatedWriteResult,
    ) -> Result<usize, SystemError> {
        let mut recovery = ChargeRecoveryState::default();
        loop {
            match core::mem::replace(&mut result.sync_result, Ok(())) {
                Ok(()) => {
                    self.complete_write_success(&mut result);
                    return Ok(result.written_len);
                }
                Err(error) => error.recover(&mut recovery)?,
            }
            let (start, end, datasync) = result
                .sync_range
                .expect("a completed synchronous File write retains its exact range");
            result.sync_result = self.try_sync_range_and_check_wb_error(start, end, datasync);
        }
    }

    /// Finish while a provider retains a resource transaction (e.g. pipe
    /// hold or copy-up). Reclaim must not run under that transaction.
    pub(crate) fn finish_write_no_recovery(
        &self,
        mut result: DelegatedWriteResult,
    ) -> Result<usize, SystemError> {
        match core::mem::replace(&mut result.sync_result, Ok(())) {
            Ok(()) => {
                self.complete_write_success(&mut result);
                Ok(result.written_len)
            }
            Err(MemoryAllocationError::System(error)) => Err(error),
            Err(MemoryAllocationError::Charge(_)) => Err(SystemError::ENOMEM),
        }
    }

    pub(crate) fn complete_write_success(&self, result: &mut DelegatedWriteResult) {
        debug_assert!(result.sync_result.is_ok());
        for (file, completion) in result.deferred.drain(..) {
            file.publish_write_completion(completion);
        }
        if let Some(completion) = result.completion.take() {
            self.publish_write_completion(completion);
        }
    }
}
