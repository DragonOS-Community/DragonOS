//! User reads keep the logical pipe-buffer copy and consumption in one transaction.
use super::*;
use crate::{filesystem::vfs::file::File, syscall::user_buffer::UserBuffer};

/// A snapshot excludes other consumers; the existing writer mutex excludes
/// append/resize while user faults can sleep without a pipe spinlock held.
struct ReadHold<'a> {
    pipe: &'a PipeRuntime,
    _transaction: PipeWriterTransaction<'a>,
    consumed: usize,
}

impl Drop for ReadHold<'_> {
    fn drop(&mut self) {
        self.pipe.splice_finish_hold(self.consumed);
    }
}

impl PipeRuntime {
    pub(super) fn read_user(
        &self,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let flags = match &*data {
            FilePrivateData::Pipefs(data) => data.flags,
            _ => return Err(SystemError::EBADF),
        };
        drop(data);
        if len == 0 {
            return Ok(0);
        }
        let mut scratch = None;
        let mut total = 0;
        loop {
            match self.wait_readable(flags.contains(FileFlags::O_NONBLOCK) || total != 0) {
                Ok(true) => {}
                Ok(false) => return Ok(total),
                Err(_) if total != 0 => return Ok(total),
                Err(error) => return Err(error),
            }
            if scratch.is_none() {
                let actor = crate::cgroup::memory::object_for_task(
                    &crate::process::ProcessManager::current_pcb(),
                );
                // No pipe lock or consumption is retained across charge recovery.
                scratch = Some(File::recover_io(|| {
                    PipeBuffer::try_new_bytes(len.min(MMArch::PAGE_SIZE), &actor)
                })?);
            }
            let transaction = self.begin_writer_transaction();
            let mut inner = self.inner.lock();
            if inner.splice_hold != 0 || inner.valid_cnt == 0 {
                drop(inner);
                drop(transaction);
                continue;
            }
            let snapshot_len = inner.valid_cnt as usize;
            inner.splice_hold = snapshot_len;
            drop(inner);
            let mut hold = ReadHold {
                pipe: self,
                _transaction: transaction,
                consumed: 0,
            };
            let mut segment_index = 0;
            // Keep one snapshot reserved for the whole syscall. Other readers
            // cannot take intervening buffers while a later user page faults.
            while total < len && hold.consumed < snapshot_len {
                let inner = self.inner.lock();
                let segment = inner
                    .data
                    .get_segment(segment_index)
                    .expect("reserved pipe buffer");
                let count = segment.len.min(len - total);
                let scratch = scratch.as_mut().expect("prepared scratch");
                let start = (inner.read_pos as usize + hold.consumed) % inner.buf_size;
                let first = count.min(inner.buf_size - start);
                scratch[..first].copy_from_slice(&inner.data[start..start + first]);
                if count > first {
                    scratch[first..count].copy_from_slice(&inner.data[..count - first]);
                }
                drop(inner);
                // A partial copy does not consume this logical buffer, even
                // across iovecs. Earlier complete buffers commit on hold drop.
                match writer.write_to_user(total, &scratch[..count]) {
                    Ok(n) if n == count => {
                        hold.consumed += if segment.packet { segment.len } else { count };
                        total += count;
                    }
                    Ok(_) | Err(_) if total != 0 => return Ok(total),
                    Ok(_) => return Err(SystemError::EFAULT),
                    Err(error) => return Err(error),
                }
                if segment.packet || count < segment.len {
                    break;
                }
                segment_index += 1;
            }
            return Ok(total);
        }
    }
}
