use ida::IdAllocator;
use system_error::SystemError;

/// Linux-compatible SysV IPC id allocator.
///
/// A user-visible IPC id is encoded as `(seq << IPC_ID_SEQ_SHIFT) | idx`.
/// The low bits address the object table, and the high bits distinguish stale
/// userspace ids after an index is reused.
#[derive(Debug)]
pub struct IpcIdAllocator {
    ida: IdAllocator,
    seq: usize,
    last_idx: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct IpcId {
    pub raw: usize,
    pub idx: usize,
    pub seq: usize,
}

impl IpcIdAllocator {
    pub const IPC_ID_INDEX_BITS: usize = 15;
    pub const IPC_ID_IDX_MASK: usize = (1usize << Self::IPC_ID_INDEX_BITS) - 1;
    pub const IPC_ID_SEQ_SHIFT: usize = Self::IPC_ID_INDEX_BITS;
    pub const IPC_ID_SEQ_MAX: usize = (i32::MAX as usize) >> Self::IPC_ID_SEQ_SHIFT;

    pub fn new(max_ids: usize) -> Result<Self, SystemError> {
        if max_ids == 0 || max_ids > Self::IPC_ID_IDX_MASK + 1 {
            return Err(SystemError::EINVAL);
        }

        Ok(Self {
            ida: IdAllocator::new(0, max_ids).ok_or(SystemError::EINVAL)?,
            seq: 0,
            last_idx: None,
        })
    }

    pub fn alloc(&mut self) -> Result<IpcId, SystemError> {
        let idx = self.ida.alloc().ok_or(SystemError::ENOSPC)?;
        if let Some(last_idx) = self.last_idx {
            if idx <= last_idx {
                self.seq += 1;
                if self.seq >= Self::IPC_ID_SEQ_MAX {
                    self.seq = 0;
                }
            }
        }
        self.last_idx = Some(idx);

        Ok(IpcId {
            raw: Self::build_raw(idx, self.seq),
            idx,
            seq: self.seq,
        })
    }

    pub fn free_idx(&mut self, idx: usize) {
        self.ida.free(idx);
    }

    pub fn decode(raw: usize) -> Result<IpcId, SystemError> {
        if raw > i32::MAX as usize {
            return Err(SystemError::EINVAL);
        }

        let idx = raw & Self::IPC_ID_IDX_MASK;
        let seq = raw >> Self::IPC_ID_SEQ_SHIFT;
        Ok(IpcId { raw, idx, seq })
    }

    #[inline]
    pub fn build_raw(idx: usize, seq: usize) -> usize {
        (seq << Self::IPC_ID_SEQ_SHIFT) | idx
    }
}
