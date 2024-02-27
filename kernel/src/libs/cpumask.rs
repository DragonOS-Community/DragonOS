use bitmap::{traits::BitMapOps, AllocBitmap};

use crate::{mm::percpu::PerCpu, smp::cpu::ProcessorId};

#[derive(Clone)]
pub struct CpuMask {
    bmp: AllocBitmap,
}

#[allow(dead_code)]
impl CpuMask {
    pub fn new() -> Self {
        let bmp = AllocBitmap::new(PerCpu::MAX_CPU_NUM as usize);
        Self { bmp }
    }

    /// 获取CpuMask中的第一个cpu
    pub fn first(&self) -> Option<ProcessorId> {
        self.bmp
            .first_index()
            .map(|index| ProcessorId::new(index as u32))
    }

    /// 获取CpuMask中第一个未被置位的cpu
    pub fn first_zero(&self) -> Option<ProcessorId> {
        self.bmp
            .first_false_index()
            .map(|index| ProcessorId::new(index as u32))
    }

    /// 获取CpuMask中的最后一个被置位的cpu
    pub fn last(&self) -> Option<ProcessorId> {
        self.bmp
            .last_index()
            .map(|index| ProcessorId::new(index as u32))
    }

    /// 获取指定cpu之后第一个为1的位的cpu
    pub fn next_index(&self, cpu: ProcessorId) -> Option<ProcessorId> {
        self.bmp
            .next_index(cpu.data() as usize)
            .map(|index| ProcessorId::new(index as u32))
    }

    /// 获取指定cpu之后第一个为未被置位的cpu
    pub fn next_zero_index(&self, cpu: ProcessorId) -> Option<ProcessorId> {
        self.bmp
            .next_false_index(cpu.data() as usize)
            .map(|index| ProcessorId::new(index as u32))
    }

    pub fn set(&mut self, cpu: ProcessorId, value: bool) -> Option<bool> {
        self.bmp.set(cpu.data() as usize, value)
    }

    pub fn get(&self, cpu: ProcessorId) -> Option<bool> {
        self.bmp.get(cpu.data() as usize)
    }

    pub fn is_empty(&self) -> bool {
        self.bmp.is_empty()
    }

    /// 迭代所有被置位的cpu
    pub fn iter_cpu(&self) -> CpuMaskIter {
        CpuMaskIter {
            mask: self,
            index: ProcessorId::new(0),
            set: true,
        }
    }

    /// 迭代所有未被置位的cpu
    pub fn iter_zero_cpu(&self) -> CpuMaskIter {
        CpuMaskIter {
            mask: self,
            index: ProcessorId::new(0),
            set: false,
        }
    }
}

pub struct CpuMaskIter<'a> {
    mask: &'a CpuMask,
    index: ProcessorId,
    set: bool,
}

impl<'a> Iterator for CpuMaskIter<'a> {
    type Item = ProcessorId;

    fn next(&mut self) -> Option<ProcessorId> {
        if self.index.data() == 0 {
            if self.set {
                self.index = self.mask.first()?;
            } else {
                self.index = self.mask.first_zero()?;
            }
        }

        if self.set {
            self.index = self.mask.next_index(self.index)?;
        } else {
            self.index = self.mask.next_zero_index(self.index)?;
        }
        Some(self.index)
    }
}

impl core::fmt::Debug for CpuMask {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CpuMask")
            .field("bmp", &format!("size: {}", self.bmp.size()))
            .finish()
    }
}
