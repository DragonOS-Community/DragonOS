pub mod vcpu;
pub mod vmx_asm_wrapper;
pub mod vmexit;
pub mod vmcs;
pub mod ept;
pub mod mmu;
pub mod kvm_emulation;

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