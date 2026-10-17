//! One pipe write transaction retains the original logical input boundaries.
use super::*;
use crate::syscall::user_buffer::UserBuffer;

enum Input<'a, 'b> {
    Kernel(&'a [u8]),
    User(&'a UserBuffer<'b>),
}

/// User access may fault and sleep. Exclude consumers without holding `inner`;
/// poll can still observe the reserved slot, as with Linux's published head.
struct InputCopy<'a>(&'a PipeRuntime);
impl Drop for InputCopy<'_> {
    fn drop(&mut self) {
        self.0.inner.lock().input_copy_active = false;
        PipeRuntime::wake_waiters(&self.0.read_wait_queue, WakeMode::One);
    }
}

impl PipeRuntime {
    pub(super) fn write_with_flags(
        &self,
        len: usize,
        bytes: &[u8],
        flags: FileFlags,
    ) -> Result<usize, MemoryAllocationError> {
        if bytes.len() < len {
            return Err(SystemError::EINVAL.into());
        }
        self.write_input(len, Input::Kernel(bytes), flags)
    }

    pub(super) fn write_user_buffer(
        &self,
        len: usize,
        source: &UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        let flags = match &*data {
            FilePrivateData::Pipefs(value) => value.flags,
            _ => return Err(SystemError::EBADF.into()),
        };
        drop(data);
        if source.len() < len {
            return Err(SystemError::EINVAL.into());
        }
        self.write_input(len, Input::User(source), flags)
    }

    fn wait_for_input_hold(&self) -> Result<(), SystemError> {
        self.read_wait_queue
            .wait_event_interruptible_tagged(HOLD_RELEASE_WAIT_TAG, || {
                self.inner.lock().splice_hold.is_none()
            })
            .map_err(|_| SystemError::ERESTARTSYS)
    }

    fn write_input(
        &self,
        len: usize,
        input: Input<'_, '_>,
        flags: FileFlags,
    ) -> Result<usize, MemoryAllocationError> {
        if len == 0 {
            return Ok(0);
        }
        let actor =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let mut transaction = self.begin_writer_transaction();
        let mut inner = self.inner.lock();
        let merge = len & (MMArch::PAGE_SIZE - 1);
        let can_merge = |inner: &InnerPipeInode| {
            merge != 0
                && inner.data.last_segment().is_some_and(|last| {
                    last.mergeable && last.offset + last.len + merge <= MMArch::PAGE_SIZE
                })
        };
        // The entry-only merge opportunity is not reconsidered after waiting.
        let mut allow_merge = true;
        // Check destination conditions before allocation or touching input.
        loop {
            if inner.splice_hold.is_some() {
                drop(inner);
                self.wait_for_input_hold()?;
                inner = self.inner.lock();
                continue;
            }
            if inner.reader == 0 {
                drop(inner);
                Self::send_sigpipe();
                return Err(SystemError::EPIPE.into());
            }
            if (allow_merge && can_merge(&inner)) || !inner.buf_full() {
                break;
            }
            if flags.contains(FileFlags::O_NONBLOCK) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
            }
            let (next, result) = self.wait_for_write_space(inner, &mut transaction, || {});
            allow_merge = false;
            inner = next;
            result?;
        }
        drop(inner);
        // All recoverable allocation precedes even an empty-slot reservation.
        self.prepare_output_storage(&transaction, &actor)?;
        let mut scratch = match input {
            Input::User(_) => Some(PipeBuffer::try_new_bytes(
                len.min(MMArch::PAGE_SIZE),
                &actor,
            )?),
            Input::Kernel(_) => None,
        };
        let mut inner = self.inner.lock();
        let mut total = 0;
        let packet = flags.contains(FileFlags::O_DIRECT);
        let mut first = allow_merge;
        let mut error = None;
        loop {
            if total == len {
                break;
            }
            // A pre-existing splice completion may retire the merge target.
            // New reservations cannot enter once input_copy_active is set.
            if inner.splice_hold.is_some() {
                drop(inner);
                let result = self.wait_for_input_hold();
                inner = self.inner.lock();
                if let Err(err) = result {
                    error = Some(err);
                    break;
                }
                continue;
            }
            if inner.reader == 0 {
                error = Some(SystemError::EPIPE);
                break;
            }
            let merging = first && can_merge(&inner);
            first = false;
            if !merging && inner.buf_full() {
                if flags.contains(FileFlags::O_NONBLOCK) {
                    error = Some(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    break;
                }
                let (next, result) = self.wait_for_write_space(inner, &mut transaction, || {
                    self.publish_write_progress(WakeMode::One)
                });
                inner = next;
                if let Err(err) = result {
                    error = Some(err);
                    break;
                }
                continue;
            }
            let count = if merging {
                merge
            } else {
                (len - total).min(MMArch::PAGE_SIZE)
            };
            if !merging {
                inner.data.push_segment(PipeSegment {
                    offset: 0,
                    len: 0,
                    mergeable: !packet,
                    packet,
                });
            }
            match input {
                Input::Kernel(bytes) => {
                    Self::write_bytes(&mut inner, &bytes[total..total + count], count)
                }
                Input::User(source) => {
                    debug_assert!(inner.splice_hold.is_none());
                    inner.input_copy_active = true;
                    let hold = InputCopy(self);
                    drop(inner);
                    let bytes = scratch.as_mut().expect("prepared user scratch");
                    let copied = source.read_from_user(total, &mut bytes[..count]);
                    inner = self.inner.lock();
                    match copied {
                        Ok(n) if n == count => {
                            Self::write_bytes(&mut inner, &bytes[..count], count)
                        }
                        Ok(_) | Err(_) => {
                            error = Some(SystemError::EFAULT);
                            drop(inner);
                            drop(hold);
                            inner = self.inner.lock();
                            break;
                        }
                    }
                    // Publish bytes and length before releasing the copy hold.
                    inner.data.extend_last(count);
                    total += count;
                    transaction.pass_baton_if_waiters_remain(&inner);
                    drop(inner);
                    drop(hold);
                    inner = self.inner.lock();
                    continue;
                }
            }
            inner.data.extend_last(count);
            total += count;
            transaction.pass_baton_if_waiters_remain(&inner);
        }
        let has_slots = inner.data.segment_count() != 0;
        drop(inner);
        if error == Some(SystemError::EPIPE) {
            Self::send_sigpipe();
        }
        if has_slots {
            self.publish_write_progress(WakeMode::One);
        }
        if total != 0 {
            Ok(total)
        } else if let Some(error) = error {
            Err(error.into())
        } else {
            Ok(0)
        }
    }
}
