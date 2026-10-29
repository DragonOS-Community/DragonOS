//! Loop's typed data plane. The active pin protects the retained mapping;
//! io_gate freezes actual I/O and serializes partial-sector RMW with all writers.
use alloc::sync::Arc;
use system_error::SystemError;

use super::{constants::LoopFlags, loop_device::LoopDevice};
use crate::{
    driver::base::block::block_device::{BlockIter, LBA_SIZE},
    filesystem::vfs::{
        file::File, read_buffer::ReadScratch, DelegatedWriteResult, WriteSyncIntent,
    },
    mm::allocation::MemoryAllocationError,
};

struct IoGuard<'a>(&'a LoopDevice);

impl<'a> IoGuard<'a> {
    fn new(device: &'a LoopDevice) -> Result<Option<Self>, SystemError> {
        Ok(device.io_start()?.then(|| Self(device)))
    }
}

impl Drop for IoGuard<'_> {
    fn drop(&mut self) {
        self.0.io_end();
    }
}

struct Snapshot {
    file: Arc<File>,
    base: usize,
    capacity: usize,
    force_direct: bool,
}

impl Snapshot {
    fn position(&self, offset: usize) -> Result<usize, SystemError> {
        self.base.checked_add(offset).ok_or(SystemError::EOVERFLOW)
    }
}

impl LoopDevice {
    // Called only after io_gate admission. A prepared active pin may wait
    // outside that gate while a mode change publishes a newer generation.
    fn io_snapshot(&self, write: bool) -> Result<Snapshot, SystemError> {
        let inner = self.inner();
        if write && inner.is_read_only() {
            return Err(SystemError::EROFS);
        }
        Ok(Snapshot {
            file: inner.backing_file.clone().ok_or(SystemError::ENODEV)?,
            base: inner.offset,
            capacity: inner.file_size,
            force_direct: inner.flags.contains(LoopFlags::DIRECT_IO),
        })
    }

    fn read_piece(
        snapshot: &Snapshot,
        offset: usize,
        out: &mut [u8],
    ) -> Result<(), MemoryAllocationError> {
        let position = snapshot.position(offset)?;
        let mut filled = 0;
        loop {
            let count = Self::recover_backing_io(|| {
                snapshot.file.try_pread_backing(
                    position.checked_add(filled).ok_or(SystemError::EOVERFLOW)?,
                    out.len() - filled,
                    &mut out[filled..],
                    snapshot.force_direct,
                )
            })
            .map_err(Self::backing_error)?;
            if count > out.len() - filled {
                return Err(SystemError::EIO.into());
            }
            filled += count;
            if filled == out.len() {
                break;
            }
            if !snapshot.force_direct {
                // lo_read_simple clears the submitted bio after any short
                // read, including the prefix supplied by the backing file.
                out.fill(0);
                break;
            }
            // lo_complete_rq resubmits only the uncompleted direct range.
            if count == 0 {
                return Err(SystemError::EIO.into());
            }
        }
        Ok(())
    }

    pub(crate) fn try_read_bytes_operation(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::EINVAL.into());
        }
        if len == 0 {
            return Ok(0);
        }
        let Some(_pin) = IoGuard::new(self)? else {
            return Ok(0);
        };
        // The active pin prevents mapping/capacity publication while scratch
        // is prepared. Resolve EOF before admitting memory, then take the
        // effective mode snapshot only after joining the I/O gate.
        if offset >= self.inner().file_size {
            return Ok(0);
        }
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let mut scratch = ReadScratch::default();
        if (offset | len) & (LBA_SIZE - 1) != 0 {
            scratch.prepare(LBA_SIZE)?;
        }
        let _gate = self.io_gate.read();
        let snapshot = self.io_snapshot(false)?;
        if offset >= snapshot.capacity {
            return Ok(0);
        }
        let len = len.min(snapshot.capacity - offset);
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let mut done = 0;
        for range in BlockIter::new_multiblock(offset, end, 9) {
            let count = range.len();
            let result = if range.is_multi() {
                Self::read_piece(
                    &snapshot,
                    range.origin_begin(),
                    &mut buf[done..done + count],
                )
            } else {
                let sector = scratch.prepare(LBA_SIZE)?;
                Self::read_piece(&snapshot, range.lba_start * LBA_SIZE, sector).map(|()| {
                    buf[done..done + count].copy_from_slice(&sector[range.begin..range.end]);
                })
            };
            match result {
                Ok(()) => done += count,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    fn write_piece(
        snapshot: &Snapshot,
        offset: usize,
        bytes: &[u8],
        intent: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        let position = snapshot.position(offset)?;
        let mut result = Self::recover_backing_io(|| {
            snapshot.file.pwrite_backing_with_sync_intent(
                position,
                bytes.len(),
                bytes,
                intent,
                snapshot.force_direct,
            )
        })
        .map_err(Self::backing_error)?;
        result.sync_result = Self::normalize_backing_sync_result(result.sync_result);
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::default();
        while let Err(error) = core::mem::replace(&mut result.sync_result, Ok(())) {
            // Payload is committed: recover only its retained exact sync range.
            if let Err(error) = error.recover(&mut recovery) {
                result.sync_result = Err(error.into());
                break;
            }
            let Some((start, end, datasync)) = result.sync_range else {
                result.sync_result = Err(SystemError::EIO.into());
                break;
            };
            result.sync_result = Self::normalize_backing_sync_result(
                snapshot
                    .file
                    .try_sync_range_and_check_wb_error(start, end, datasync),
            );
        }
        if result.sync_result.is_ok() {
            snapshot.file.complete_write_success(&mut result);
        }
        Ok(result)
    }

    pub(crate) fn try_write_bytes_operation(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        sync: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::EINVAL.into());
        }
        if len == 0 {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        let Some(_pin) = IoGuard::new(self)? else {
            return Err(SystemError::ENOSPC.into());
        };
        {
            let inner = self.inner();
            if inner.is_read_only() {
                return Err(SystemError::EROFS.into());
            }
            if offset >= inner.file_size {
                return Err(SystemError::ENOSPC.into());
            }
        }
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let partial = (offset | len) & (LBA_SIZE - 1) != 0;
        let mut scratch = ReadScratch::default();
        if partial {
            scratch.prepare(LBA_SIZE)?;
        }
        // Full writes must participate too: an edge writer's old sector must
        // never overwrite a concurrent whole-sector update.
        let _shared = (!partial).then(|| self.io_gate.read());
        let _exclusive = partial.then(|| self.io_gate.write());
        let snapshot = self.io_snapshot(true)?;
        if offset >= snapshot.capacity {
            return Err(SystemError::ENOSPC.into());
        }
        let len = len.min(snapshot.capacity - offset);
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let mut done = 0;
        for range in BlockIter::new_multiblock(offset, end, 9) {
            let count = range.len();
            let full = range.is_multi();
            let submitted = if full {
                Self::write_piece(
                    &snapshot,
                    range.origin_begin(),
                    &buf[done..done + count],
                    sync,
                )
            } else {
                let sector = scratch.prepare(LBA_SIZE)?;
                Self::read_piece(&snapshot, range.lba_start * LBA_SIZE, sector).and_then(|()| {
                    sector[range.begin..range.end].copy_from_slice(&buf[done..done + count]);
                    Self::write_piece(&snapshot, range.lba_start * LBA_SIZE, sector, sync)
                })
            };
            let mut result = match submitted {
                Ok(result) => result,
                // This is a data error, not a recoverable sync failure. No
                // invented sync_range may replay an unsubmitted remainder.
                Err(error) if done != 0 => return Ok(DelegatedWriteResult::new(done, Err(error))),
                Err(error) => return Err(error),
            };
            let expected = if full { count } else { LBA_SIZE };
            if result.written_len > expected {
                return if done == 0 {
                    Err(SystemError::EIO.into())
                } else {
                    Ok(DelegatedWriteResult::new(
                        done,
                        Err(SystemError::EIO.into()),
                    ))
                };
            }
            let visible = if full {
                result.written_len
            } else {
                result
                    .written_len
                    .min(range.end)
                    .saturating_sub(range.begin)
            };
            done += visible;
            if result.written_len != expected {
                // A short sector submission cannot be retried as a new RMW.
                // Preserve exactly the intersection already written to users.
                result.sync_result = Err(SystemError::EIO.into());
                result.sync_range = None;
            }
            result.written_len = done;
            if result.sync_result.is_err() || visible != count {
                return if done == 0 {
                    Err(result
                        .sync_result
                        .err()
                        .unwrap_or_else(|| SystemError::EIO.into()))
                } else {
                    Ok(result)
                };
            }
        }
        Ok(DelegatedWriteResult::new(done, Ok(())))
    }

    pub(crate) fn try_sync(&self) -> Result<(), MemoryAllocationError> {
        let Some(_pin) = IoGuard::new(self)? else {
            return Ok(());
        };
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _gate = self.io_gate.read();
        let snapshot = self.io_snapshot(false)?;
        Self::recover_backing_io(|| {
            Self::normalize_backing_sync_result(snapshot.file.try_sync_range_and_check_wb_error(
                0,
                usize::MAX,
                false,
            ))
        })
    }

    fn recover_backing_io<T>(
        mut attempt: impl FnMut() -> Result<T, MemoryAllocationError>,
    ) -> Result<T, MemoryAllocationError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::default();
        loop {
            match attempt() {
                Ok(value) => return Ok(value),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    fn normalize_backing_sync_result(
        result: Result<(), MemoryAllocationError>,
    ) -> Result<(), MemoryAllocationError> {
        // Linux keeps -EINVAL in lo_req_flush, but errno_to_blk_status maps
        // it to BLK_STS_IOERR. No backing flush error is a successful flush.
        match result {
            Ok(()) => Ok(()),
            Err(error @ MemoryAllocationError::Charge(_)) => Err(error),
            Err(MemoryAllocationError::System(_)) => Err(SystemError::EIO.into()),
        }
    }

    // Linux errno_to_blk_status()/blk_status_to_errno(): preserve recognized
    // block errors; other backend errors (notably direct EINVAL) become EIO.
    fn backing_error(error: MemoryAllocationError) -> MemoryAllocationError {
        match error {
            MemoryAllocationError::System(error) => match error {
                SystemError::EOPNOTSUPP_OR_ENOTSUP
                | SystemError::ETIMEDOUT
                | SystemError::ENOSPC
                | SystemError::ENOLINK
                | SystemError::EREMOTEIO
                | SystemError::EBADE
                | SystemError::ENODATA
                | SystemError::EILSEQ
                | SystemError::ENOMEM
                | SystemError::EBUSY
                | SystemError::EAGAIN_OR_EWOULDBLOCK
                | SystemError::ENODEV
                | SystemError::EREMCHG
                | SystemError::ETOOMANYREFS
                | SystemError::EOVERFLOW
                | SystemError::ETIME => error.into(),
                _ => SystemError::EIO.into(),
            },
            error @ MemoryAllocationError::Charge(_) => error,
        }
    }

    pub(super) fn legacy_io_terminal(error: MemoryAllocationError) -> SystemError {
        match error {
            MemoryAllocationError::System(error) => error,
            MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
        }
    }
}
