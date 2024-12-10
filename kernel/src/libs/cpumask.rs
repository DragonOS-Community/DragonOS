use core::ops::BitAnd;

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

    /// # from_cpu - 从指定的CPU创建CPU掩码
    ///
    /// 该函数用于根据给定的CPU标识创建一个CPU掩码，只有指定的CPU被设置为激活状态。
    ///
    /// ## 参数
    /// - `cpu`: `ProcessorId`，指定要设置为激活状态的CPU。
    ///
    /// ## 返回值
    /// - `Self`: 返回一个新的`CpuMask`实例，其中只有指定的CPU被设置为激活状态。
    pub fn from_cpu(cpu: ProcessorId) -> Self {
        let mut mask = Self::new();
        mask.set(cpu, true);
        mask
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
            index: None,
            set: true,
            begin: true,
        }
    }

    /// 迭代所有未被置位的cpu
    pub fn iter_zero_cpu(&self) -> CpuMaskIter {
        CpuMaskIter {
            mask: self,
            index: None,
            set: false,
            begin: true,
        }
    }

    pub fn inner(&self) -> &AllocBitmap {
        &self.bmp
    }

    pub fn bitand_assign(&mut self, rhs: &CpuMask) {
        self.bmp.bitand_assign(&rhs.bmp);
    }
}

impl BitAnd for &CpuMask {
    type Output = CpuMask;

    fn bitand(self, rhs: &CpuMask) -> Self::Output {
        let bmp = &self.bmp & &rhs.bmp;
        CpuMask { bmp }
    }
}

impl Default for CpuMask {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CpuMaskIter<'a> {
    mask: &'a CpuMask,
    index: Option<ProcessorId>,
    set: bool,
    begin: bool,
}

impl Iterator for CpuMaskIter<'_> {
    type Item = ProcessorId;

    fn next(&mut self) -> Option<ProcessorId> {
        if self.index.is_none() && self.begin {
            if self.set {
                self.index = self.mask.first();
            } else {
                self.index = self.mask.first_zero();
            }

            self.begin = false;
        }
        let result = self.index;
        if self.set {
            self.index = self.mask.next_index(self.index?);
        } else {
            self.index = self.mask.next_zero_index(self.index?);
        }

        result
    }
}

impl core::fmt::Debug for CpuMask {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CpuMask")
            .field("bmp", &format!("size: {}", self.bmp.size()))
            .finish()
    }
}
