use crate::smp::cpu::ProcessorId;

use super::HardwareIrqNumber;

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IpiKind {
    KickCpu,
    FlushTLB,
    /// 指定中断向量号
    SpecVector(HardwareIrqNumber),
}

/// IPI投递目标
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[allow(dead_code)]
pub enum IpiTarget {
    /// 当前CPU
    Current,
    /// 所有CPU
    All,
    /// 除了当前CPU以外的所有CPU
    Other,
    /// 指定的CPU
    Specified(ProcessorId),
}
