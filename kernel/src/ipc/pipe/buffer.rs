//! A unique byte ring; physical backing dies before its kernel receipt.
use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    mm::{allocation::MemoryAllocationError, MemoryManagementArch},
};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
};
use system_error::SystemError;

/// Page provenance independent of physical byte-ring positions.
#[derive(Debug, Clone, Copy)]
pub(super) struct PipeSegment {
    pub offset: usize,
    pub len: usize,
    pub mergeable: bool,
    pub packet: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PipeBuffer {
    bytes: Vec<u8>,
    segments: VecDeque<PipeSegment>,
    _charge: Option<ObjectCharge>,
    _segment_charge: Option<ObjectCharge>,
}
impl PipeBuffer {
    /// Bytes-only storage for transfer scratch; it owns no pipe provenance.
    pub(crate) fn try_new_bytes(
        size: usize,
        actor: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        if size == 0 {
            return Ok(Self::default());
        }
        let layout = Layout::array::<u8>(size).map_err(|_| SystemError::EOVERFLOW)?;
        let charge = actor.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(size, 0);
        Ok(Self {
            bytes,
            _charge: Some(charge),
            ..Self::default()
        })
    }

    pub(crate) fn try_new(
        size: usize,
        actor: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let mut buffer = Self::try_new_bytes(size, actor)?;
        if size == 0 {
            return Ok(buffer);
        }
        let slots = size.div_ceil(crate::arch::MMArch::PAGE_SIZE);
        let layout = Layout::array::<PipeSegment>(slots).map_err(|_| SystemError::EOVERFLOW)?;
        let charge = actor.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
        buffer
            .segments
            .try_reserve_exact(slots)
            .map_err(|_| SystemError::ENOMEM)?;
        buffer._segment_charge = Some(charge);
        Ok(buffer)
    }
}
impl PipeBuffer {
    pub(super) fn segment_count(&self) -> usize {
        self.segments.len()
    }
    pub(super) fn first_segment(&self) -> Option<PipeSegment> {
        self.segments.front().copied()
    }
    pub(super) fn last_segment(&self) -> Option<PipeSegment> {
        self.segments.back().copied()
    }
    pub(super) fn get_segment(&self, index: usize) -> Option<PipeSegment> {
        self.segments.get(index).copied()
    }
    pub(super) fn segment_at(&self, mut skip: usize) -> Option<PipeSegment> {
        for segment in &self.segments {
            if skip < segment.len {
                return Some(PipeSegment {
                    offset: segment.offset + skip,
                    len: segment.len - skip,
                    ..*segment
                });
            }
            skip -= segment.len;
        }
        None
    }
    pub(super) fn push_segment(&mut self, segment: PipeSegment) {
        assert!(segment.len != 0 && self.segments.len() < self.segments.capacity());
        self.segments.push_back(segment);
    }
    pub(super) fn clear_last_merge(&mut self) {
        self.segments.back_mut().expect("transfer target").mergeable = false;
    }
    pub(super) fn extend_last(&mut self, len: usize) {
        self.segments.back_mut().expect("merge target").len += len;
    }
    pub(super) fn consume(&mut self, mut len: usize) {
        while len != 0 {
            let first = self
                .segments
                .front_mut()
                .expect("pipe bytes have provenance");
            let consumed = len.min(first.len);
            first.offset += consumed;
            first.len -= consumed;
            len -= consumed;
            if first.len == 0 {
                self.segments.pop_front();
            }
        }
    }
    pub(super) fn copy_segments_from(&mut self, source: &Self) {
        assert!(source.segments.len() <= self.segments.capacity());
        self.segments.clear();
        self.segments.extend(source.segments.iter().copied());
    }
}
impl Deref for PipeBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
impl DerefMut for PipeBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}
