#![allow(dead_code)]

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

#[allow(dead_code)]
pub mod kvm_exit {
    pub const KVM_EXIT_UNKNOWN: u32 = 0;
    pub const KVM_EXIT_EXCEPTION: u32 = 1;
    pub const KVM_EXIT_IO: u32 = 2;
    pub const KVM_EXIT_HYPERCALL: u32 = 3;
    pub const KVM_EXIT_DEBUG: u32 = 4;
    pub const KVM_EXIT_HLT: u32 = 5;
    pub const KVM_EXIT_MMIO: u32 = 6;
    pub const KVM_EXIT_IRQ_WINDOW_OPEN: u32 = 7;
    pub const KVM_EXIT_SHUTDOWN: u32 = 8;
    pub const KVM_EXIT_FAIL_ENTRY: u32 = 9;
    pub const KVM_EXIT_INTR: u32 = 10;
    pub const KVM_EXIT_SET_TPR: u32 = 11;
    pub const KVM_EXIT_TPR_ACCESS: u32 = 12;
    pub const KVM_EXIT_S390_SIEIC: u32 = 13;
    pub const KVM_EXIT_S390_RESET: u32 = 14;
    pub const KVM_EXIT_DCR: u32 = 15;
    pub const KVM_EXIT_NMI: u32 = 16;
    pub const KVM_EXIT_INTERNAL_ERROR: u32 = 17;
    pub const KVM_EXIT_OSI: u32 = 18;
    pub const KVM_EXIT_PAPR_HCALL: u32 = 19;
    pub const KVM_EXIT_S390_UCONTROL: u32 = 20;
    pub const KVM_EXIT_WATCHDOG: u32 = 21;
    pub const KVM_EXIT_S390_TSCH: u32 = 22;
    pub const KVM_EXIT_EPR: u32 = 23;
    pub const KVM_EXIT_SYSTEM_EVENT: u32 = 24;
    pub const KVM_EXIT_S390_STSI: u32 = 25;
    pub const KVM_EXIT_IOAPIC_EOI: u32 = 26;
    pub const KVM_EXIT_HYPERV: u32 = 27;
    pub const KVM_EXIT_ARM_NISV: u32 = 28;
    pub const KVM_EXIT_X86_RDMSR: u32 = 29;
    pub const KVM_EXIT_X86_WRMSR: u32 = 30;
    pub const KVM_EXIT_DIRTY_RING_FULL: u32 = 31;
    pub const KVM_EXIT_AP_RESET_HOLD: u32 = 32;
    pub const KVM_EXIT_X86_BUS_LOCK: u32 = 33;
    pub const KVM_EXIT_XEN: u32 = 34;
    pub const KVM_EXIT_RISCV_SBI: u32 = 35;
    pub const KVM_EXIT_RISCV_CSR: u32 = 36;
    pub const KVM_EXIT_NOTIFY: u32 = 37;
}
