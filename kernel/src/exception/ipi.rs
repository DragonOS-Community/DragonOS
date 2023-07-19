#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum IpiKind {
    KickCpu,
    FlushTLB,
    StartUp,
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
    Specified(usize),
}
