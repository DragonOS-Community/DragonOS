//! Logical pipe slots own copying, consumption and empty-packet boundaries.
use super::*;
use crate::{filesystem::vfs::file::File, syscall::user_buffer::UserBuffer};

struct ReadHold<'a> {
    pipe: &'a PipeRuntime,
    _transaction: PipeWriterTransaction<'a>,
    freed_slots: bool,
}
impl Drop for ReadHold<'_> {
    fn drop(&mut self) {
        let mut inner = self.pipe.inner.lock();
        inner.splice_hold = None;
        let writer_wake = if self.freed_slots {
            PipeRuntime::writer_wake_action(&mut inner)
        } else {
            PipeWriterWake::None
        };
        let reader_wake = if inner.has_slots() {
            WakeMode::One
        } else if inner.writer == 0 {
            WakeMode::All
        } else {
            WakeMode::None
        };
        drop(inner);
        self.pipe
            .read_wait_queue
            .wake_one_tagged(HOLD_RELEASE_WAIT_TAG);
        self.pipe.wake_pipe_waiters(reader_wake, writer_wake);
        if self.freed_slots {
            self.pipe.notify_writers(EPollEventType::empty());
            self.pipe.write_fasync_items.send_sigio(FASYNC_POLL_OUT);
        }
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
        'read: loop {
            match self.wait_readable(flags.contains(FileFlags::O_NONBLOCK) || total != 0) {
                Ok(true) => {}
                Ok(false) => return Ok(total),
                Err(_) if total != 0 => return Ok(total),
                Err(error) => return Err(error),
            }
            let transaction = self.begin_writer_transaction();
            let mut inner = self.inner.lock();
            if inner.read_reserved() || !inner.has_slots() {
                drop(inner);
                drop(transaction);
                continue;
            }
            let slots = inner.data.segment_count();
            inner.splice_hold = Some(inner.valid_cnt.max(0) as usize);
            drop(inner);
            let mut hold = ReadHold {
                pipe: self,
                _transaction: transaction,
                freed_slots: false,
            };
            for _ in 0..slots {
                let mut inner = self.inner.lock();
                let segment = inner.data.first_segment().expect("reserved pipe slot");
                if segment.len == 0 {
                    inner.data.pop_front();
                    hold.freed_slots = true;
                    if segment.packet {
                        return Ok(total);
                    }
                    continue;
                }
                if scratch.is_none() {
                    // Empty slots need no payload storage. Charge recovery
                    // must run outside the read reservation and transaction.
                    drop(inner);
                    drop(hold);
                    let actor = crate::cgroup::memory::object_for_task(
                        &crate::process::ProcessManager::current_pcb(),
                    );
                    scratch = Some(File::recover_io(|| {
                        PipeBuffer::try_new_bytes(len.min(MMArch::PAGE_SIZE), &actor)
                    })?);
                    continue 'read;
                }
                let count = segment.len.min(len - total);
                let bytes = scratch.as_mut().expect("prepared read scratch");
                let start = inner.read_pos as usize;
                let first = count.min(inner.buf_size - start);
                bytes[..first].copy_from_slice(&inner.data[start..start + first]);
                if count > first {
                    bytes[first..count].copy_from_slice(&inner.data[..count - first]);
                }
                drop(inner);
                match writer.write_to_user(total, &bytes[..count]) {
                    Ok(n) if n == count => {}
                    Ok(_) | Err(_) if total != 0 => return Ok(total),
                    Ok(_) => return Err(SystemError::EFAULT),
                    Err(error) => return Err(error),
                }
                let mut inner = self.inner.lock();
                let before = inner.data.segment_count();
                inner.consume(if segment.packet { segment.len } else { count });
                hold.freed_slots |= inner.data.segment_count() < before;
                total += count;
                if segment.packet || total == len || count < segment.len {
                    return Ok(total);
                }
            }
            drop(hold);
            if total != 0 {
                return Ok(total);
            }
        }
    }
}
