use crate::process::KernelStack;

pub mod bitops;
pub mod boot;

/* KSave registers */
pub const LOONGARCH_CSR_KS0: usize = 0x30;
pub const LOONGARCH_CSR_KS1: usize = 0x31;
pub const LOONGARCH_CSR_KS2: usize = 0x32;
pub const LOONGARCH_CSR_KS3: usize = 0x33;
pub const LOONGARCH_CSR_KS4: usize = 0x34;
pub const LOONGARCH_CSR_KS5: usize = 0x35;
pub const LOONGARCH_CSR_KS6: usize = 0x36;
pub const LOONGARCH_CSR_KS7: usize = 0x37;
pub const LOONGARCH_CSR_KS8: usize = 0x38;

/// Current mode info
pub const LOONGARCH_CSR_CRMD: usize = 0x0;
/// Prev-exception mode info
pub const LOONGARCH_CSR_PRMD: usize = 0x1;

/// Extended unit enable
pub const LOONGARCH_CSR_EUEN: usize = 0x2;

/// Exception config
pub const LOONGARCH_CSR_ECFG: usize = 0x4;
/// Exception status
pub const LOONGARCH_CSR_ESTAT: usize = 0x5;

/// Exception return address.
pub const LOONGARCH_CSR_ERA: usize = 0x6;

/// Bad virtual address.
pub const LOONGARCH_CSR_BADV: usize = 0x7;

/* Exception allocated KS0, KS1 and KS2 statically */
pub const EXCEPTION_KS0: usize = LOONGARCH_CSR_KS0;
pub const EXCEPTION_KS1: usize = LOONGARCH_CSR_KS1;

/* Percpu-data base allocated KS3 statically */
pub const PERCPU_BASE_KS: usize = LOONGARCH_CSR_KS3;
pub const PERCPU_KSAVE_MASK: usize = 1 << 3;

pub const _THREAD_MASK: usize = KernelStack::SIZE - 1;
