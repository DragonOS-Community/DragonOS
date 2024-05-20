use crate::virt::vm::user_api::UapiKvmSegment;

pub const DE_VECTOR: usize = 0;
pub const DB_VECTOR: usize = 1;
pub const BP_VECTOR: usize = 3;
pub const OF_VECTOR: usize = 4;
pub const BR_VECTOR: usize = 5;
pub const UD_VECTOR: usize = 6;
pub const NM_VECTOR: usize = 7;
pub const DF_VECTOR: usize = 8;
pub const TS_VECTOR: usize = 10;
pub const NP_VECTOR: usize = 11;
pub const SS_VECTOR: usize = 12;
pub const GP_VECTOR: usize = 13;
pub const PF_VECTOR: usize = 14;
pub const MF_VECTOR: usize = 16;
pub const AC_VECTOR: usize = 17;
pub const MC_VECTOR: usize = 18;
pub const XM_VECTOR: usize = 19;
pub const VE_VECTOR: usize = 20;

pub const KVM_SYNC_X86_REGS: u64 = 1 << 0;
pub const KVM_SYNC_X86_SREGS: u64 = 1 << 1;
pub const KVM_SYNC_X86_EVENTS: u64 = 1 << 2;

pub const KVM_SYNC_X86_VALID_FIELDS: u64 =
    KVM_SYNC_X86_REGS | KVM_SYNC_X86_SREGS | KVM_SYNC_X86_EVENTS;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmSegmentRegs {
    pub cs: UapiKvmSegment,
    pub ds: UapiKvmSegment,
    pub es: UapiKvmSegment,
    pub fs: UapiKvmSegment,
    pub gs: UapiKvmSegment,
    pub ss: UapiKvmSegment,
    pub tr: UapiKvmSegment,
    pub ldt: UapiKvmSegment,
    pub gdt: UapiKvmDtable,
    pub idt: UapiKvmDtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    pub interrupt_bitmap: [u64; 4usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmDtable {
    pub base: u64,
    pub limit: u16,
    pub padding: [u16; 3usize],
}
