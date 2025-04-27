#![allow(dead_code)]

pub const VMX_EPT_MT_EPTE_SHIFT: u64 = 3;
pub const VMX_EPTP_PWL_MASK: u64 = 0x38;
pub const VMX_EPTP_PWL_4: u64 = 0x18;
pub const VMX_EPTP_PWL_5: u64 = 0x20;
pub const VMX_EPTP_AD_ENABLE_BIT: u64 = 1 << 6;
pub const VMX_EPTP_MT_MASK: u64 = 0x7;
pub const VMX_EPTP_MT_WB: u64 = 0x6;
pub const VMX_EPTP_MT_UC: u64 = 0x0;
pub const VMX_EPT_READABLE_MASK: u64 = 0x1;
pub const VMX_EPT_WRITABLE_MASK: u64 = 0x2;
pub const VMX_EPT_EXECUTABLE_MASK: u64 = 0x4;
pub const VMX_EPT_IPAT_BIT: u64 = 1 << 6;
pub const VMX_EPT_ACCESS_BIT: u64 = 1 << 8;
pub const VMX_EPT_DIRTY_BIT: u64 = 1 << 9;
pub const VMX_EPT_RWX_MASK: u64 =
    VMX_EPT_READABLE_MASK | VMX_EPT_WRITABLE_MASK | VMX_EPT_EXECUTABLE_MASK;
pub const VMX_EPT_MT_MASK: u64 = 7 << VMX_EPT_MT_EPTE_SHIFT;
