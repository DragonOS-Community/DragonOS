target remote localhost:1234
file bin/kernel/kernel.elf
set follow-fork-mode child

b kernel/src/arch/x86_64/vm/mmu/mmu_internal.rs:310
b kernel/src/arch/x86_64/vm/vmx/ept/mod.rs:274
b kernel/src/arch/x86_64/vm/vmx/ept/mod.rs:177
b kernel/src/arch/x86_64/vm/kvm_host/vcpu.rs:1248