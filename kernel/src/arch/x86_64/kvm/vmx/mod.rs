pub mod ept;
pub mod kvm_emulation;
pub mod mmu;
pub mod seg;
pub mod vcpu;
pub mod vmcs;
pub mod vmexit;
pub mod vmx_asm_wrapper;

#[allow(dead_code)]
pub enum VcpuRegIndex {
    Rax = 0,
    Rbx = 1,
    Rcx = 2,
    Rdx = 3,
    Rsi = 4,
    Rdi = 5,
    Rsp = 6,
    Rbp = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

bitflags! {
    #[allow(non_camel_case_types)]
    pub struct X86_CR0: u32{
        const CR0_PE = 1 << 0; /* Protection Enable */
        const CR0_MP = 1 << 1; /* Monitor Coprocessor */
        const CR0_EM = 1 << 2; /* Emulation */
        const CR0_TS = 1 << 3; /* Task Switched */
        const CR0_ET = 1 << 4; /* Extension Type */
        const CR0_NE = 1 << 5; /* Numeric Error */
        const CR0_WP = 1 << 16; /* Write Protect */
        const CR0_AM = 1 << 18; /* Alignment Mask */
        const CR0_NW = 1 << 29; /* Not Write-through */
        const CR0_CD = 1 << 30; /* Cache Disable */
        const CR0_PG = 1 << 31; /* Paging */
    }
}
