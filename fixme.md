### fixme:
PageLevel的类型
MTRR 是 x86 架构中的一组寄存器，用于控制不同内存区域的缓存属性。通过配置 MTRR，可以优化系统性能和兼容性。操作系统在启动时通常会配置 MTRR，以确保不同内存区域具有适当的缓存属性。

初次EPT_VIOLATION的时候，gpa=0，要建立从gpa到hpa的映射，也就是ept映射，处理完各个寄存器以及mmu等状态后
- do_page_fault 初始化page_fault信息，能知道gfn

- gfn_to_memslot 找到包含 gfn 的 memslot 的指针，放在page_fault.slot里面

- __gfn_to_hva_many 得到hva（照着之前的kvm写的）（要用到page_fault的slot）

- hva_to_pfn 得到pfn，可以说相当于知道了hpa（照着之前的kvm写的），放在 page_fault.pfn里面

找到ept root物理地址 kernel/src/arch/x86_64/mm/mod.rs:184

### 疑问？
- 内核里面应该有相似的多级页表查询/映射的机制，是不是可以借鉴或者复用 kernel/src/mm/page.rs:712 kvm:kernel/src/arch/x86_64/kvm/vmx/ept.rs:91

- 我感觉得到ept root 物理地址(不知道存哪了，可能在真正要)后，按照索引在ept页表往下查，然后缺页就alloc块给它然后加入页表建立映射（gpa->hpa），直到找到目标层的level，[linux实现](https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/mmu/tdp_mmu.c?fi=kvm_tdp_mmu_map#952)

- __va和virt_2_phys是一样的吗？

- mm.h的作用


### Debug
tdp_page_fault :at src/arch/x86_64/vm/mmu/mmu_internal.rs:233
enter_guest :   at src/arch/x86_64/vm/kvm_host/vcpu.rs:840
handle_ept_violation :at src/arch/x86_64/vm/vmx/exit.rs:278
try_handle_exit: at kernel/src/arch/x86_64/vm/vmx/exit.rs:250
vmlaunch : at kernel/src/arch/x86_64/vm/vmx/vmenter.S:103
page fault :kernel/src/arch/x86_64/vm/mmu/mmu_internal.rs:105

kernel/src/mm/kernel_mapper.rs


