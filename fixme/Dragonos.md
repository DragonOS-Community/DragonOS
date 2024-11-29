EPT_VIOLATION遇到问题：
1. 编译linux内核源码，匹配相同流程的vcpu Vmcs的差异，发现某些error_code和bits对不上
[ DEBUG ] (src/arch/x86_64/vm/mmu/mmu_internal.rs:323)	 
KvmPageFault { 
    addr: PhysAddr(0x0),
    error_code: 17,//0x11 多了RWX_MASK
    prefetch: false, 
    exec: true, 
    write: false, 
    present: true, 
    rsvd: false, 
    user: false, 
    is_tdp: true, 
    nx_huge_page_workaround_enabled: false, 
    huge_page_disallowed: false, max_level: 3, 
    req_level: 1, 
    goal_level: 1, 
    gfn: 0, 
    slot: 
    Some(LockedKvmMemSlot { inner: RwLock { lock: 0, data: UnsafeCell { .. } } }), mmu_seq: 0, pfn: 111275, hva: 65536, map_writable: false, write_fault_to_shadow_pgtable: false }

[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:359)	 vmexit handler: VMEXIT_INSTR_LEN: 0x3!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:363)	 vmexit handler: VMEXIT_INSTR_INFO: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:381)	 vmexit handler: EXCEPTION_BITMAP: 0x64042!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:385)	 vmexit handler: PAGE_FAULT_ERR_CODE_MASK: 0x9!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:389)	 vmexit handler: PAGE_FAULT_ERR_CODE_MATCH: 0x1!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:393)	 vmexit handler: EPTP_LIST_ADDR_FULL: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:396)	 vmexit handler: VM_INSTRUCTION_ERROR: 0x1c!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:399)	 vmexit handler: EXIT_REASON: 48! //EPT VIOLATION
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:402)	 vmexit handler: VMEXIT_INTERRUPTION_INFO: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:405)	 vmexit handler: VMEXIT_INTERRUPTION_ERR_CODE: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:408)	 vmexit handler: IDT_VECTORING_INFO: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:411)	 vmexit handler: IDT_VECTORING_ERR_CODE: 0x0!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:414)	 vmexit handler: VMEXIT_INSTRUCTION_LEN: 0x3!
[ DEBUG ] (src/arch/x86_64/vm/vmx/exit.rs:417)	 vmexit handler: VMEXIT_INSTRUCTION_INFO: 0x0!


	 vmx_update_host_rsp
	 vmx_spec_ctrl_restore_host todo!
    VMCS addr: 0xffff80001b2cf000, last attempted VM-entry on CPU ProcessorId(0)
*** Guest State ***
$	 CR0: actual = 0x30     11 0000
        Bit 4 (ET - Extension Type): 1，指示协处理器的类型（x87 协处理器）。
        Bit 5 (NE - Numeric Error): 1，控制浮点异常的处理。
     , shadow = 0x60000010, gh_mask = 0xe0040037
$	 CR4: actual = 0x2000
         VMXE (bit 13): 设置为 1,代表启用VMX
     , shadow = 0x0, gh_mask = 0x767871
$	 CR3: actual = 0x0
$	 PDPTR0 = 0x0, PDPTR1 = 0x0
$	 PDPTR2 = 0x0, PDPTR3 = 0x0
	 RSP = 0x200000, RIP = 0x7d # 为什么有值
$	 RFLAGS = 0x10002, DR7 = 0x400
$	 Sysenter RSP = 0x0, CS:RIP = 0x0:0x0
	 CS:  sel = 0x0, attr = 0x93, limit = 0xffff, base = 0x0
	 DS:  sel = 0x0, attr = 0x9b, limit = 0xffff, base = 0x0
$	 SS:  sel = 0x0, attr = 0x93, limit = 0xffff, base = 0x0
$    ES:  sel = 0x0, attr = 0x93, limit = 0xffff, base = 0x0
$	 FS:  sel = 0x0, attr = 0x93, limit = 0xffff, base = 0x0
$	 GS:  sel = 0x0, attr = 0x93, limit = 0xffff, base = 0x0
$	 GDTR:  limit = 0xffff, base = 0x0
$	 LDTR:  sel = 0x0, attr = 0x82, limit = 0xffff, base = 0x0
$	 IDTR:  limit = 0xffff, base = 0x0
$	 TR:  sel = 0x0, attr = 0x8b, limit = 0xffff, base = 0x0
$	 EFER = 0x0
	 PAT = 0x0
$	 DebugCtl = 0x0, DebugExceptions = 0x0
$	 Interruptibility = 0x0, ActivityState = 0x0
	 
*** Host State ***
$	 RIP = 0xffff80000100b06e, RSP = 0xffff80001a9df988
	 CS = 0x8, SS = 0x28, DS = 0x0, ES = 0x0, FS = 0x0, GS = 0x0, TR = 0x50
	 FSBase = 0x408778, GSBase = 0x0, TRBase = 0xffff800001ee3078
	 GDTBase = 0xffff800000137010, IDTBase = 0xffff80000013738c
	 CR0 = 0x80000033  10000000000000000000000000110011
        PE (bit 0): 设置为 1，启用保护模式。
        MP (bit 1): 设置为 1，控制协处理器的操作。
        ET (bit 4): 设置为 1，表示系统使用的是387浮点协处理器
        NE (bit 5):  控制浮点异常的处理。
        PG (bit 31): 设置为 1，启用分页。

$    , CR3 = 0x1a968000, 
     CR4 = 0x2620  10011000100000
        PAE (Physical Address Extension, bit 5): 启用物理地址扩展。
        OSFXSR (Operating System Support for FXSAVE and FXRSTOR instructions, bit 9):。启用操作系统对 FXSAVE 和 FXRSTOR 指令的支持。
        OSXMMEXCPT (Operating System Support for Unmasked SIMD Floating-Point Exceptions, bit 10)，启用操作系统对未屏蔽的 SIMD 浮点异常的支持。
    VMXE (VMX Enable, bit 13): 启用 VMX（虚拟机扩展）。
	 Sysenter RSP = 0x0, CS:RIP=0x0:0x0
	 EFER = 0x501
	 PAT = 0x7040600070406  # 这在linux是Guest PAT
	 
*** Control State ***
    CPUBased = USE_TSC_OFFSETTING  3 | HLT_EXITING 7 | MWAIT_EXITING 10| RDPMC_EXITING 11 | CR8_LOAD_EXITING  16| CR8_STORE_EXITING 19 | UNCOND_IO_EXITING 24 | USE_MSR_BITMAPS 28| MONITOR_EXITING 29 | SECONDARY_CONTROLS 31,    

    10110001000010010000110010001000  b1090c88

    SecondaryExec = 0x8ea,
$    TertiaryExec = 0(Unused)
 
    PinBased = EXTERNAL_INTERRUPT_EXITING 0 | NMI_EXITING 3 | VIRTUAL_NMIS 5,   101000 0x28

    EntryControls = LOAD_DEBUG_CONTROLS 2 | LOAD_IA32_PAT 14 | LOAD_IA32_EFER 15, 1100000000000100  c004
    ExitControls = SAVE_DEBUG_CONTROLS 2 | HOST_ADDRESS_SPACE_SIZE 9 | ACK_INTERRUPT_ON_EXIT 15 | LOAD_IA32_PAT 19 | LOAD_IA32_EFER 21  0x288204
$	ExceptionBitmap = 0x60042, PFECmask = 0x0, PFECmatch = 0x0
$	 VMEntry: intr_info = 0x0, errcode = 0x0, ilen = 0x0
	 VMExit: intr_info = 0x0, errcode = 0x0, ilen = 0x3 # ??
	         reason = 0x30, qualification = 0x182
	 IDTVectoring: info = 0x8000030d, errcode = 0x0
	 TSC Offset = 0x0
$	 EPT pointer = 0x1b2e901e //少了huge page
	 Virtual processor ID = 0x1