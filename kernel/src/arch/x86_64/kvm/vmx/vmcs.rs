use bitflags::bitflags;
use num_derive::FromPrimitive;

pub const PAGE_SIZE: usize = 0x1000;

#[repr(C, align(4096))]
#[derive(Clone, Debug)]
pub struct VMCSRegion {
    pub revision_id: u32,
    pub abort_indicator: u32,
    data: [u8; PAGE_SIZE - 8],
}

// (Intel Manual: 25.11.2 VMREAD, VMWRITE, and Encodings of VMCS Fields)
#[derive(FromPrimitive)]
enum VmcsAccessType {
    FULL = 0,
    HIGH = 1,
}

#[derive(FromPrimitive)]
enum VmcsType {
    CONTROL = 0,
    VMEXIT = 1,
    GUEST = 2,
    HOST = 3,
}

#[derive(FromPrimitive)]
enum VmcsWidth {
    BIT16 = 0,
    BIT64 = 1,
    BIT32 = 2,
    NATURAL = 3,
}

#[derive(FromPrimitive)]
#[allow(non_camel_case_types)]
// (Intel Manual: APPENDIX B FIELD ENCODING IN VMCS)
pub enum VmcsFields {
    // [CONTROL] fields
    // 16-bit control fields
    CTRL_VIRT_PROC_ID = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT16, 0) as isize,
    CTRL_POSTED_INTR_N_VECTOR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT16, 1) as isize,
    CTRL_EPTP_INDEX = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT16, 2) as isize,
    // 64-bit control fields
    CTRL_IO_BITMAP_A_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 0) as isize,
    CTRL_IO_BITMAP_B_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 1) as isize,
    CTRL_MSR_BITMAP_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 2) as isize, // control whether RDMSR or WRMSR cause VM exit
    CTRL_VMEXIT_MSR_STORE_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 3) as isize,
    CTRL_VMEXIT_MSR_LOAD_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 4) as isize,
    CTRL_VMENTRY_MSR_LOAD_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 5) as isize,
    CTRL_EXECUTIVE_VMCS_PTR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 6) as isize,
    CTRL_PML_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 7) as isize,
    CTRL_TSC_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 8) as isize,
    CTRL_VIRT_APIC_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 9) as isize,
    CTRL_APIC_ACCESS_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 10) as isize,
    CTRL_POSTED_INTR_DESC_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 11) as isize,
    CTRL_VMFUNC_CTRL = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 12) as isize,
    CTRL_EPTP_PTR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 13) as isize,
    CTRL_EOI_EXIT_BITMAP_0 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 14) as isize,
    CTRL_EOI_EXIT_BITMAP_1 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 15) as isize,
    CTRL_EOI_EXIT_BITMAP_2 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 16) as isize,
    CTRL_EOI_EXIT_BITMAP_3 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 17) as isize,
    CTRL_EPT_LIST_ADDR = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 18) as isize,
    CTRL_VMREAD_BITMAP_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 19) as isize,
    CTRL_VMWRITE_BITMAP_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 20) as isize,
    CTRL_VIRT_EXECPT_INFO_ADDR =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 21) as isize,
    CTRL_XSS_EXITING_BITMAP =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 22) as isize,
    CTRL_ENCLS_EXITING_BITMAP =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 23) as isize,
    CTRL_TSC_MULTIPLIER = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT64, 25) as isize,
    // 32-bit control fields
    CTRL_PIN_BASED_VM_EXEC_CTRLS =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 0) as isize, // control async event handling (i.e. interrupts)
    CTRL_PRIMARY_PROCESSOR_VM_EXEC_CTRLS =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 1) as isize, // control sync event handling (i.e. instruction exits)
    CTRL_EXPECTION_BITMAP = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 2) as isize, // bitmap to control exceptions that cause a VM exit
    CTRL_PAGE_FAULT_ERR_CODE_MASK =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 3) as isize,
    CTRL_PAGE_FAULT_ERR_CODE_MATCH =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 4) as isize,
    CTRL_CR3_TARGET_COUNT = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 5) as isize,
    CTRL_PRIMARY_VM_EXIT_CTRLS =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 6) as isize,
    CTRL_VM_EXIT_MSR_STORE_COUNT =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 7) as isize,
    CTRL_VM_EXIT_MSR_LOAD_COUNT =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 8) as isize,
    CTRL_VM_ENTRY_CTRLS = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 9) as isize,
    CTRL_VM_ENTRY_MSR_LOAD_COUNT =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 10) as isize,
    CTRL_VM_ENTRY_INTR_INFO_FIELD =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 11) as isize,
    CTRL_VM_ENTRY_EXCEPTION_ERR_CODE =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 12) as isize,
    CTRL_VM_ENTRY_INSTR_LEN =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 13) as isize,
    CTRL_TPR_THRESHOLD = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 14) as isize,
    CTRL_SECONDARY_PROCESSOR_VM_EXEC_CTRLS =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 15) as isize,
    CTRL_PLE_GAP = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 16) as isize,
    CTRL_PLE_WINDOW = encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::BIT32, 17) as isize,
    // natural control fields
    CTRL_CR0_GUEST_HOST_MASK =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 0) as isize, // control executions of insts that access cr0
    CTRL_CR4_GUEST_HOST_MASK =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 1) as isize,
    CTRL_CR0_READ_SHADOW =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 2) as isize, // control executions of insts that access cr0
    CTRL_CR4_READ_SHADOW =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 3) as isize,
    CTRL_CR3_TARGET_VALUE_0 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 4) as isize,
    CTRL_CR3_TARGET_VALUE_1 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 5) as isize,
    CTRL_CR3_TARGET_VALUE_2 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 6) as isize,
    CTRL_CR3_TARGET_VALUE_3 =
        encode_vmcs_field_full(VmcsType::CONTROL, VmcsWidth::NATURAL, 7) as isize,

    // [VMEXIT] fields read-only
    // No 16-bit vmexit fields
    // 64-bit vmexit fields
    VMEXIT_GUEST_PHY_ADDR = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT64, 0) as isize,
    // 32-bit vmexit fields
    VMEXIT_INSTR_ERR = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 0) as isize,
    VMEXIT_EXIT_REASON = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 1) as isize,
    VMEXIT_INT_INFO = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 2) as isize,
    VMEXIT_INT_ERR_CODE = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 3) as isize,
    VMEXIT_IDT_VECTOR_INFO = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 4) as isize,
    VMEXIT_IDT_VECTOR_ERR_CODE =
        encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 5) as isize,
    VMEXIT_INSTR_LEN = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 6) as isize,
    VMEXIT_INSTR_INFO = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::BIT32, 7) as isize,
    // natural vmexit fields
    VMEXIT_QUALIFICATION = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 0) as isize,
    VMEXIT_IO_RCX = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 1) as isize,
    VMEXIT_IO_RSX = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 2) as isize,
    VMEXIT_IO_RDI = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 3) as isize,
    VMEXIT_IO_RIP = encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 4) as isize,
    VMEXIT_GUEST_LINEAR_ADDR =
        encode_vmcs_field_full(VmcsType::VMEXIT, VmcsWidth::NATURAL, 5) as isize,

    // [GUEST] fields
    // 16-bit guest fields
    GUEST_ES_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 0) as isize,
    GUEST_CS_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 1) as isize,
    GUEST_SS_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 2) as isize,
    GUEST_DS_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 3) as isize,
    GUEST_FS_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 4) as isize,
    GUEST_GS_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 5) as isize,
    GUEST_LDTR_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 6) as isize,
    GUEST_TR_SELECTOR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 7) as isize,
    GUEST_INTR_STATUS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 8) as isize,
    GUEST_PML_INDEX = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT16, 9) as isize,
    // 64-bit guest fields
    GUEST_VMCS_LINK_PTR = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 0) as isize,
    GUEST_DEBUGCTL = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 1) as isize,
    GUEST_PAT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 2) as isize,
    GUEST_EFER = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 3) as isize,
    GUEST_PERF_GLOBAL_CTRL = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 4) as isize,
    GUEST_PDPTE0 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 5) as isize,
    GUEST_PDPTE1 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 6) as isize,
    GUEST_PDPTE2 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 7) as isize,
    GUEST_PDPTE3 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT64, 8) as isize,
    // 32-bit guest fields
    GUEST_ES_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 0) as isize,
    GUEST_CS_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 1) as isize,
    GUEST_SS_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 2) as isize,
    GUEST_DS_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 3) as isize,
    GUEST_FS_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 4) as isize,
    GUEST_GS_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 5) as isize,
    GUEST_LDTR_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 6) as isize,
    GUEST_TR_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 7) as isize,
    GUEST_GDTR_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 8) as isize,
    GUEST_IDTR_LIMIT = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 9) as isize,
    GUEST_ES_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 10) as isize,
    GUEST_CS_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 11) as isize,
    GUEST_SS_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 12) as isize,
    GUEST_DS_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 13) as isize,
    GUEST_FS_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 14) as isize,
    GUEST_GS_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 15) as isize,
    GUEST_LDTR_ACCESS_RIGHTS =
        encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 16) as isize,
    GUEST_TR_ACCESS_RIGHTS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 17) as isize,
    GUEST_INTERRUPTIBILITY_STATE =
        encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 18) as isize,
    GUEST_ACTIVITY_STATE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 19) as isize,
    GUEST_SMBASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 20) as isize,
    GUEST_SYSENTER_CS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::BIT32, 21) as isize,
    GUEST_VMX_PREEMPT_TIMER_VALUE = 0x482E as isize,
    // natural guest fields
    GUEST_CR0 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 0) as isize,
    GUEST_CR3 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 1) as isize,
    GUEST_CR4 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 2) as isize,
    GUEST_ES_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 3) as isize,
    GUEST_CS_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 4) as isize,
    GUEST_SS_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 5) as isize,
    GUEST_DS_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 6) as isize,
    GUEST_FS_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 7) as isize,
    GUEST_GS_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 8) as isize,
    GUEST_LDTR_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 9) as isize,
    GUEST_TR_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 10) as isize,
    GUEST_GDTR_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 11) as isize,
    GUEST_IDTR_BASE = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 12) as isize,
    GUEST_DR7 = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 13) as isize,
    GUEST_RSP = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 14) as isize,
    GUEST_RIP = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 15) as isize,
    GUEST_RFLAGS = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 16) as isize,
    GUEST_PENDING_DBG_EXCEPTIONS =
        encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 17) as isize,
    GUEST_SYSENTER_ESP = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 18) as isize,
    GUEST_SYSENTER_EIP = encode_vmcs_field_full(VmcsType::GUEST, VmcsWidth::NATURAL, 19) as isize,

    // [HOST] fields
    // host 16 bit fields
    HOST_ES_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 0) as isize,
    HOST_CS_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 1) as isize,
    HOST_SS_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 2) as isize,
    HOST_DS_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 3) as isize,
    HOST_FS_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 4) as isize,
    HOST_GS_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 5) as isize,
    HOST_TR_SELECTOR = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT16, 6) as isize,
    // host 64 bit fields
    HOST_PAT = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT64, 0) as isize,
    HOST_EFER = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT64, 1) as isize,
    HOST_PERF_GLOBAL_CTRL = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT64, 2) as isize,
    // host 32 bit fields
    HOST_SYSENTER_CS = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::BIT32, 0) as isize,
    // host natural fields
    HOST_CR0 = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 0) as isize,
    HOST_CR3 = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 1) as isize,
    HOST_CR4 = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 2) as isize,
    HOST_FS_BASE = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 3) as isize,
    HOST_GS_BASE = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 4) as isize,
    HOST_TR_BASE = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 5) as isize,
    HOST_GDTR_BASE = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 6) as isize,
    HOST_IDTR_BASE = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 7) as isize,
    HOST_SYSENTER_ESP = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 8) as isize,
    HOST_SYSENTER_EIP = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 9) as isize,
    HOST_RSP = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 10) as isize,
    HOST_RIP = encode_vmcs_field_full(VmcsType::HOST, VmcsWidth::NATURAL, 11) as isize,
}

// (Intel Manual: 25.6 VM-EXECUTION CONTROL FIELDS)
bitflags! {
    // (Intel Manual: 25.6.1 Pin-Based VM-Execution Controls)
    #[allow(non_camel_case_types)]
    pub struct VmxPinBasedExecuteCtrl: u32 {
        const EXTERNAL_INTERRUPT_EXITING = 1 << 0; // external interrupts cause VM exits
        const NMI_EXITING = 1 << 3; // non-maskable interrupts (NMIs) cause VM exits.
        const VIRTUAL_NMIS = 1 << 5; // NMIs are never blocked and the “blocking by NMI” bit (bit 3) in the interruptibility-state field indicates “virtual-NMI blocking”
        const VMX_PREEMPTION_TIMER = 1 << 6; // the VMX-preemption timer counts down in VMX non-root operation
        const PROCESS_POSTED_INTERRUPTS = 1 << 7; // he processor treats interrupts with the posted-interrupt notification vector
    }

    // (Intel Manual: 25.6.2 Processor-Based VM-Execution Controls)
    #[allow(non_camel_case_types)]
    pub struct VmxPrimaryProcessBasedExecuteCtrl: u32{
        const INTERRUPT_WINDOW_EXITING = 1 << 2; // VM exits on interrupt window RFLAGS.IF = 1
        const USE_TSC_OFFSETTING = 1 << 3; // TSC offsetting is enabled
        const HLT_EXITING = 1 << 7;
        const INVLPG_EXITING = 1 << 9;
        const MWAIT_EXITING = 1 << 10;
        const RDPMC_EXITING = 1 << 11;
        const RDTSC_EXITING = 1 << 12;
        const CR3_LOAD_EXITING = 1 << 15;
        const CR3_STR_EXITING = 1 << 16;
        const CR8_LOAD_EXITING = 1 << 19;
        const CR8_STR_EXITING = 1 << 20;
        const USE_TPR_SHADOW = 1 << 21;
        const NMI_WINDOW_EXITING = 1 << 22;
        const MOV_DR_EXITING = 1 << 23;
        const UNCOND_IO_EXITING = 1 << 24;
        const USE_IO_BITMAPS = 1 << 25;
        const MONITOR_TRAP_FLAG = 1 << 27;
        const USE_MSR_BITMAPS = 1 << 28;
        const MONITOR_EXITING = 1 << 29;
        const PAUSE_EXITING = 1 << 30;
        const ACTIVATE_SECONDARY_CONTROLS = 1 << 31;
    }

    // (Intel Manual: 25.6.2 Processor-Based VM-Execution Controls)
    pub struct VmxSecondaryProcessBasedExecuteCtrl: u32{
        const VIRT_APIC_ACCESS = 1 << 0;
        const ENABLE_EPT = 1 << 1;
        const DESCRIPTOR_TABLE_EXITING = 1 << 2;
        const ENABLE_RDTSCP = 1 << 3;
        const VIRT_X2APIC_MODE = 1 << 4;
        const ENABLE_VPID = 1 << 5;
        const WBINVD_EXITING = 1 << 6;
        const UNRESTRICTED_GUEST = 1 << 7;
        const APCI_REGISTER_VIRT = 1 << 8;
        const VIRT_INTR_DELIVERY = 1 << 9;
        const PAUSE_LOOP_EXITING = 1 << 10;
        const RDRAND_EXITING = 1 << 11;
        const ENABLE_INVPCID = 1 << 12;
        const ENABLE_VM_FUNCTIONS = 1 << 13;
        const VMCS_SHADOWING = 1 << 14;
        const ENABLE_ENCLS_EXITING = 1 << 15;
        const RDSEED_EXITING = 1 << 16;
        const ENABLE_PML = 1 << 17;
        const EPT_VIOLATION_VE = 1 << 18;
        const CONCEAL_VMX_FROM_PT = 1 << 19;
        const ENABLE_XSAVES_XRSTORS = 1 << 20;
        const PASID_TRANSLATION = 1 << 21;
        const MODE_BASED_EPT_EXEC = 1 << 22;
        const SUB_PAGE_WRITE_PERM = 1 << 23;
        const PT_USE_GUEST_PYH_ADDR = 1 << 24;
        const USE_TSC_SCALING = 1 << 25;
        const ENABLE_USER_WAIT_PAUSE = 1 << 26;
        const ENABLE_PCONFIG = 1 << 27;
        const ENABLE_ENCLV_EXITING = 1 << 28;
        const VMM_BUS_LOCK_DETECTION = 1 << 30;
        const INST_TIMEOUT = 1 << 31;
    }

    // (Intel Manual: 25.7.1 VM-Exit Controls)
    #[allow(non_camel_case_types)]
    pub struct VmxPrimaryExitCtrl: u32 {
        const SAVE_DBG_CTRLS = 1 << 2;
        const HOST_ADDR_SPACE_SIZE = 1 << 9; // determines if a virtual processor will be in 64-bit mode after a VM exit
        const LOAD_IA32_PERF_GLOBAL_CTRL = 1 << 12;
        const ACK_INTERRUPT_ON_EXIT = 1 << 15;
        const SAVE_IA32_PAT = 1 << 18;
        const LOAD_IA32_PAT = 1 << 19;
        const SAVE_IA32_EFER = 1 << 20;
        const LOAD_IA32_EFER = 1 << 21;
        const SAVE_VMX_PREEMPT_TIMER_VALUE = 1 << 22;
        const CLEAR_IA32_BNDCFGS = 1 << 23;
        const CONCEAL_VMX_FROM_PT = 1 << 24;
        const CLEAR_IA32_RTIT_CTL = 1 << 25;
        const CLEAR_IA32_LBR_CTL = 1 << 26;
        const CLEAR_UINV = 1 << 27;
        const LOAD_CET_STATE = 1 << 28;
        const LOAD_PKRS = 1 << 29;
        const SAVE_IA32_PERF_GLOBAL_CTL = 1 << 30;
        const ACTIVATE_SECONDARY_CONTROLS = 1 << 31;
    }

    // (Intel Manual: 25.8.1 VM-Entry Controls)
    #[allow(non_camel_case_types)]
    pub struct VmxEntryCtrl: u32 {
        const LOAD_DBG_CTRLS = 1 << 2;
        const IA32E_MODE_GUEST = 1 << 9;
        const ENTRY_TO_SMM = 1 << 10;
        const DEACTIVATE_DUAL_MONITOR = 1 << 11;
        const LOAD_IA32_PERF_GLOBAL_CTRL = 1 << 13;
        const LOAD_IA32_PAT = 1 << 14;
        const LOAD_IA32_EFER = 1 << 15;
        const LOAD_IA32_BNDCFGS = 1 << 16;
        const CONCEAL_VMX_FROM_PT = 1 << 17;
        const LOAD_IA32_RTIT_CTL = 1 << 18;
        const LOAD_UINV = 1 << 19;
        const LOAD_CET_STATE = 1 << 20;
        const LOAD_PKRS = 1 << 21;
        const LOAD_IA32_PERF_GLOBAL_CTL = 1 << 22;
    }

}

#[derive(FromPrimitive)]
#[allow(non_camel_case_types)]
pub enum VmxExitReason {
    EXCEPTION_OR_NMI = 0,
    EXTERNAL_INTERRUPT = 1,
    TRIPLE_FAULT = 2,
    INIT_SIGNAL = 3,
    SIPI = 4,
    IO_SMI = 5,
    OTHER_SMI = 6,
    INTERRUPT_WINDOW = 7,
    NMI_WINDOW = 8,
    TASK_SWITCH = 9,
    CPUID = 10,
    GETSEC = 11,
    HLT = 12,
    INVD = 13,
    INVLPG = 14,
    RDPMC = 15,
    RDTSC = 16,
    RSM = 17,
    VMCALL = 18,
    VMCLEAR = 19,
    VMLAUNCH = 20,
    VMPTRLD = 21,
    VMPTRST = 22,
    VMREAD = 23,
    VMRESUME = 24,
    VMWRITE = 25,
    VMXOFF = 26,
    VMXON = 27,
    CR_ACCESS = 28,
    DR_ACCESS = 29,
    IO_INSTRUCTION = 30,
    RDMSR = 31,
    WRMSR = 32,
    VM_ENTRY_FAILURE_INVALID_GUEST_STATE = 33,
    VM_ENTRY_FAILURE_MSR_LOADING = 34,
    MWAIT = 36,
    MONITOR_TRAP_FLAG = 37,
    MONITOR = 39,
    PAUSE = 40,
    VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT = 41,
    TPR_BELOW_THRESHOLD = 43,
    APIC_ACCESS = 44,
    VIRTUALIZED_EOI = 45,
    ACCESS_GDTR_OR_IDTR = 46,
    ACCESS_LDTR_OR_TR = 47,
    EPT_VIOLATION = 48,
    EPT_MISCONFIG = 49,
    INVEPT = 50,
    RDTSCP = 51,
    VMX_PREEMPTION_TIMER_EXPIRED = 52,
    INVVPID = 53,
    WBINVD = 54,
    XSETBV = 55,
    APIC_WRITE = 56,
    RDRAND = 57,
    INVPCID = 58,
    VMFUNC = 59,
    ENCLS = 60,
    RDSEED = 61,
    PML_FULL = 62,
    XSAVES = 63,
    XRSTORS = 64,
}

impl From<i32> for VmxExitReason {
    fn from(num: i32) -> Self {
        match num {
            0 => VmxExitReason::EXCEPTION_OR_NMI,
            1 => VmxExitReason::EXTERNAL_INTERRUPT,
            2 => VmxExitReason::TRIPLE_FAULT,
            3 => VmxExitReason::INIT_SIGNAL,
            4 => VmxExitReason::SIPI,
            5 => VmxExitReason::IO_SMI,
            6 => VmxExitReason::OTHER_SMI,
            7 => VmxExitReason::INTERRUPT_WINDOW,
            8 => VmxExitReason::NMI_WINDOW,
            9 => VmxExitReason::TASK_SWITCH,
            10 => VmxExitReason::CPUID,
            11 => VmxExitReason::GETSEC,
            12 => VmxExitReason::HLT,
            13 => VmxExitReason::INVD,
            14 => VmxExitReason::INVLPG,
            15 => VmxExitReason::RDPMC,
            16 => VmxExitReason::RDTSC,
            17 => VmxExitReason::RSM,
            18 => VmxExitReason::VMCALL,
            19 => VmxExitReason::VMCLEAR,
            20 => VmxExitReason::VMLAUNCH,
            21 => VmxExitReason::VMPTRLD,
            22 => VmxExitReason::VMPTRST,
            23 => VmxExitReason::VMREAD,
            24 => VmxExitReason::VMRESUME,
            25 => VmxExitReason::VMWRITE,
            26 => VmxExitReason::VMXOFF,
            27 => VmxExitReason::VMXON,
            28 => VmxExitReason::CR_ACCESS,
            29 => VmxExitReason::DR_ACCESS,
            30 => VmxExitReason::IO_INSTRUCTION,
            31 => VmxExitReason::RDMSR,
            32 => VmxExitReason::WRMSR,
            33 => VmxExitReason::VM_ENTRY_FAILURE_INVALID_GUEST_STATE,
            34 => VmxExitReason::VM_ENTRY_FAILURE_MSR_LOADING,
            36 => VmxExitReason::MWAIT,
            37 => VmxExitReason::MONITOR_TRAP_FLAG,
            39 => VmxExitReason::MONITOR,
            40 => VmxExitReason::PAUSE,
            41 => VmxExitReason::VM_ENTRY_FAILURE_MACHINE_CHECK_EVENT,
            43 => VmxExitReason::TPR_BELOW_THRESHOLD,
            44 => VmxExitReason::APIC_ACCESS,
            45 => VmxExitReason::VIRTUALIZED_EOI,
            46 => VmxExitReason::ACCESS_GDTR_OR_IDTR,
            47 => VmxExitReason::ACCESS_LDTR_OR_TR,
            48 => VmxExitReason::EPT_VIOLATION,
            49 => VmxExitReason::EPT_MISCONFIG,
            50 => VmxExitReason::INVEPT,
            51 => VmxExitReason::RDTSCP,
            52 => VmxExitReason::VMX_PREEMPTION_TIMER_EXPIRED,
            53 => VmxExitReason::INVVPID,
            54 => VmxExitReason::WBINVD,
            55 => VmxExitReason::XSETBV,
            56 => VmxExitReason::APIC_WRITE,
            57 => VmxExitReason::RDRAND,
            58 => VmxExitReason::INVPCID,
            59 => VmxExitReason::VMFUNC,
            60 => VmxExitReason::ENCLS,
            61 => VmxExitReason::RDSEED,
            62 => VmxExitReason::PML_FULL,
            63 => VmxExitReason::XSAVES,
            64 => VmxExitReason::XRSTORS,
            _ => panic!("Invalid VmxExitReason number: {}", num),
        }
    }
}

const fn encode_vmcs_field(
    access_type: VmcsAccessType,
    vmcs_type: VmcsType,
    vmcs_width: VmcsWidth,
    index: u32,
) -> u32 {
    let mut encoding: u32 = 0;
    encoding |= (access_type as u32)
        | (index as u32) << 1
        | (vmcs_type as u32) << 10
        | (vmcs_width as u32) << 13;
    return encoding;
}

const fn encode_vmcs_field_full(vmcs_type: VmcsType, vmcs_width: VmcsWidth, index: u32) -> u32 {
    encode_vmcs_field(VmcsAccessType::FULL, vmcs_type, vmcs_width, index)
}

// fn decode_vmcs_field(field: u32) -> (VmcsAccessType, VmcsType, VmcsWidth, u16){
//     (FromPrimitive::from_u32(field & 1).unwrap() ,
//         FromPrimitive::from_u32((field>>10) & 0x3).unwrap(),
//         FromPrimitive::from_u32((field>>13) & 0x3).unwrap(),
//         ((field>>1) & 0x1ff) as u16
//     )
// }
