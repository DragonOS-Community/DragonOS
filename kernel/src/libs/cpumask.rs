use bitmap::{traits::BitMapOps, AllocBitmap};

use crate::mm::percpu::PerCpu;

pub struct CpuMask {
    bmp: AllocBitmap,
}

#[allow(dead_code)]
impl CpuMask {
    pub fn new() -> Self {
        let bmp = AllocBitmap::new(PerCpu::MAX_CPU_NUM);
        Self { bmp }
    }

    /// 获取CpuMask中的第一个cpu
    pub fn first(&self) -> Option<usize> {
        self.bmp.first_index()
    }

    /// 获取CpuMask中第一个未被置位的cpu
    pub fn first_zero(&self) -> Option<usize> {
        self.bmp.first_false_index()
    }

    /// 获取CpuMask中的最后一个被置位的cpu
    pub fn last(&self) -> Option<usize> {
        self.bmp.last_index()
    }

    /// 获取指定cpu之后第一个为1的位的cpu
    pub fn next_index(&self, cpu: usize) -> Option<usize> {
        self.bmp.next_index(cpu)
    }

    /// 获取指定cpu之后第一个为未被置位的cpu
    pub fn next_zero_index(&self, cpu: usize) -> Option<usize> {
        self.bmp.next_false_index(cpu)
    }

    /// 迭代所有被置位的cpu
    pub fn iter_cpu(&self) -> CpuMaskIter {
        CpuMaskIter {
            mask: self,
            index: 0,
            set: true,
        }
    }

    /// 迭代所有未被置位的cpu
    pub fn iter_zero_cpu(&self) -> CpuMaskIter {
        CpuMaskIter {
            mask: self,
            index: 0,
            set: false,
        }
    }
}

pub struct CpuMaskIter<'a> {
    mask: &'a CpuMask,
    index: usize,
    set: bool,
}

impl<'a> Iterator for CpuMaskIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == 0 {
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
