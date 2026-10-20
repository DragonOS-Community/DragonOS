//! Allocation-free CPU sets private to RCU. Membership changes are serialized
//! by RCU state locks; copied GP/action snapshots never allocate.
use crate::{mm::percpu::PerCpu, smp::cpu::ProcessorId};

const MASK_WORDS: usize = (PerCpu::MAX_CPU_NUM as usize).div_ceil(u64::BITS as usize);

/// Fixed CPU membership shared by GP coordination and IRQ progress snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RcuCpuMask {
    words: [u64; MASK_WORDS],
}

impl RcuCpuMask {
    pub(super) const fn new() -> Self {
        Self {
            words: [0; MASK_WORDS],
        }
    }

    pub(super) fn from_cpu(cpu: ProcessorId) -> Self {
        let mut result = Self::new();
        result.set(cpu, true);
        result
    }

    /// Return the previous bit, preserving the dynamic CpuMask contract.
    pub(super) fn set(&mut self, cpu: ProcessorId, value: bool) -> Option<bool> {
        let index = cpu.data() as usize;
        if index >= PerCpu::MAX_CPU_NUM as usize {
            return None;
        }
        let word = index / u64::BITS as usize;
        let bit = 1u64 << (index % u64::BITS as usize);
        let previous = self.words[word] & bit != 0;
        if value {
            self.words[word] |= bit;
        } else {
            self.words[word] &= !bit;
        }
        Some(previous)
    }

    pub(super) fn get(&self, cpu: ProcessorId) -> Option<bool> {
        let index = cpu.data() as usize;
        if index >= PerCpu::MAX_CPU_NUM as usize {
            return None;
        }
        let word = index / u64::BITS as usize;
        let bit = 1u64 << (index % u64::BITS as usize);
        Some(self.words[word] & bit != 0)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    pub(super) fn intersect(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word &= *other_word;
        }
    }

    pub(super) fn iter_cpu(&self) -> RcuCpuMaskIter<'_> {
        RcuCpuMaskIter {
            mask: self,
            next: 0,
        }
    }
}

pub(super) struct RcuCpuMaskIter<'a> {
    mask: &'a RcuCpuMask,
    next: u32,
}

impl Iterator for RcuCpuMaskIter<'_> {
    type Item = ProcessorId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < PerCpu::MAX_CPU_NUM {
            let cpu = ProcessorId::new(self.next);
            self.next += 1;
            if self.mask.get(cpu).unwrap_or(false) {
                return Some(cpu);
            }
        }
        None
    }
}

/// Boundary checks also run inside the feature's real heap-denial scope.
pub(super) fn run_selftests() -> Result<(), &'static str> {
    let first = ProcessorId::new(0);
    let last = ProcessorId::new(PerCpu::MAX_CPU_NUM - 1);
    let invalid = ProcessorId::new(PerCpu::MAX_CPU_NUM);
    let mut mask = RcuCpuMask::new();
    if !mask.is_empty()
        || mask.iter_cpu().next().is_some()
        || mask.get(invalid).is_some()
        || mask.set(invalid, true).is_some()
        || !mask.is_empty()
    {
        return Err("RCU fixed mask empty/invalid CPU boundary failed");
    }
    if mask.set(first, true) != Some(false)
        || mask.set(first, true) != Some(true)
        || mask.set(last, true).is_none()
    {
        return Err("RCU fixed mask lost previous-bit semantics");
    }
    let original = mask;
    if mask.set(first, false) != Some(true)
        || !original.get(first).unwrap_or(false)
        || mask.get(first) != Some(false)
    {
        return Err("RCU fixed mask copy/clear changed its source");
    }
    for index in 0..PerCpu::MAX_CPU_NUM {
        mask.set(ProcessorId::new(index), true);
    }
    if mask.iter_cpu().count() != PerCpu::MAX_CPU_NUM as usize
        || !mask
            .iter_cpu()
            .enumerate()
            .all(|(index, cpu)| index == cpu.data() as usize)
    {
        return Err("RCU fixed mask iterator included tail bits or skipped CPUs");
    }
    mask.intersect(&original);
    if mask != original {
        return Err("RCU fixed mask intersection changed membership");
    }
    mask.set(first, false);
    mask.set(last, false);
    if !mask.is_empty() {
        return Err("RCU fixed mask clear left stale members");
    }
    Ok(())
}
