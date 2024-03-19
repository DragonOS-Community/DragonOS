pub const CSR_SSTATUS: usize = 0x100;
pub const CSR_SSCRATCH: usize = 0x140;
pub const CSR_SEPC: usize = 0x141;
pub const CSR_SCAUSE: usize = 0x142;
pub const CSR_STVAL: usize = 0x143;

// === Status register flags ===

/// Previously Supervisor
pub const SR_SPP: usize = 0x00000100;
/// Supervisor User Memory Access
pub const SR_SUM: usize = 0x00040000;

/// Floating-Point Status
pub const SR_FS: usize = 0x00006000;
/// Vector status
pub const SR_VS: usize = 0x00000600;

/// Vector and Floating-Point Unit
pub const SR_FS_VS: usize = SR_FS | SR_VS;
