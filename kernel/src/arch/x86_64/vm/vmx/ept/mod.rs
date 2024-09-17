use crate::libs::rwlock::RwLock;

// pub const VMX_EPT_MT_EPTE_SHIFT:u64 = 3;
pub const VMX_EPT_RWX_MASK: u64 = 0x7 << 3;

// Exit Qualifications for EPT Violations
pub const EPT_VIOLATION_ACC_READ_BIT: u64 = 0;
pub const EPT_VIOLATION_ACC_WRITE_BIT: u64 = 1;
pub const EPT_VIOLATION_ACC_INSTR_BIT: u64 = 2;
pub const EPT_VIOLATION_RWX_SHIFT: u64 = 3;
pub const EPT_VIOLATION_GVA_IS_VALID_BIT: u64 = 7;
pub const EPT_VIOLATION_GVA_TRANSLATED_BIT: u64 = 8;

bitflags! {
    pub struct EptViolationExitQual :u64{
        const ACC_READ = 1 << EPT_VIOLATION_ACC_READ_BIT;
        const ACC_WRITE = 1 << EPT_VIOLATION_ACC_WRITE_BIT;
        const ACC_INSTR = 1 << EPT_VIOLATION_ACC_INSTR_BIT;
        const RWX_MASK = VMX_EPT_RWX_MASK << EPT_VIOLATION_RWX_SHIFT;
        const GVA_IS_VALID = 1 << EPT_VIOLATION_GVA_IS_VALID_BIT;
        const GVA_TRANSLATED = 1 << EPT_VIOLATION_GVA_TRANSLATED_BIT;
    }
}
struct EptPageTable {
    // EPT 页表数据结构
}

struct EptManager {
    ept: RwLock<EptPageTable>,
    // 其他字段
}
