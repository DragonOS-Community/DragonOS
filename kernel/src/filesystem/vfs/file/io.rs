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
        let datasync = match intent {
            super::WriteSyncIntent::None => result.sync_range.map(|(_, _, datasync)| datasync),
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

    /// Data and its notifications/f_pos have already committed. Do not invoke
    /// the primary write again when a stacked backing's sync needs admission.
    pub(crate) fn finish_write(&self, result: DelegatedWriteResult) -> Result<usize, SystemError> {
        let mut recovery = ChargeRecoveryState::default();
        let mut sync = result.sync_result;
        loop {
            match sync {
                Ok(()) => return Ok(result.written_len),
                Err(error) => error.recover(&mut recovery)?,
            }
            let (start, end, datasync) = result
                .sync_range
                .expect("a completed synchronous File write retains its exact range");
            sync = self.try_sync_range_and_check_wb_error(start, end, datasync);
        }
    }
}
