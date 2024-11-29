
Hello MyLinux
sh: can't access tty; job control turned off
(kernel) =>./my_test_kvm_static 
vmfd 4
map mem 0x75735e279000
KVM_SET_USER_MEMORY_REGION 0x4020ae46
[    7.189916] VM Exit Reason: 1 //External interrupt
[    7.190102] exit_handler_index: 1
[    7.190287] EPT pointer = 0xfffffc000661205e
[    7.190543] VM Exit Reason: 0x30//48::EPT VIOLATION
[    7.190719] exit_handler_index: 30
[    7.190913] EPT pointer = 0xfffffc000661205e

[    2.265187] kvm_page_fault:
[    2.265371]   addr: 0x0
[    2.265516]   error_code: 0x10
[    2.265685]   exec: 1
[    2.265812]   write: 0
[    2.265942]   present: 0
[    2.266089]   rsvd: 0
[    2.266216]   user: 0
[    2.266348]   prefetch: 0
[    2.266491]   is_tdp: 1
[    2.266623]   nx_huge_page_workaround_enabled: 0
[    2.266868]   max_level: 3
[    2.267012]   req_level: 1
[    2.267156]   gfn: 0
[    2.267274]   pfn: 12747
[    2.267410]   hva: 132350336286720

[    2.267615] kvm_page_fault:
[    2.267774]   addr: 0x0
[    2.267914]   error_code: 0x10
[    2.268084]   exec: 1
[    2.268216]   write: 0
[    2.268351]   present: 0
[    2.268504]   rsvd: 0
[    2.268640]   user: 0
[    2.268777]   prefetch: 0
[    2.268928]   is_tdp: 1
[    2.269086]   nx_huge_page_workaround_enabled: 0
[    2.269335]   max_level: 3
[    2.269561]   req_level: 1
[    2.269709]   gfn: 0
[    2.269851]   pfn: 12747
[    2.270009]   hva: 132350336286720

[    7.193373] VM Exit Reason: 1e
[    7.193557] exit_handler_index: 1e
[    7.193766] EPT pointer = 0xfffffc000661205e
Guest CR3: 0x0
run->exit_reason= 0x2
a
KVM_EXIT_IO: run->io.port = 217 
[    7.194568] VM Exit Reason: 1e
[    7.194750] exit_handler_index: 1e
[    7.194949] EPT pointer = 0xfffffc000661205e
Guest CR3: 0x0
run->exit_reason= 0x2

KVM_EXIT_IO: run->io.port = 217 
[    7.195757] VM Exit Reason: c
[    7.195935] exit_handler_index: c
[    7.196121] EPT pointer = 0xfffffc000661205e
Guest CR3: 0x0
run->exit_reason= 0x5
KVM_EXIT_HLT 
(kernel) =>


   33.881551] VMCS 000000008e180ab4, last attempted VM-entry on CPU 0
[   34.439920] *** Guest State ***
[   34.440253] CR0: actual=0x0000000000000030, shadow=0x0000000060000010, gh_mask=fffffffffffefff7
[   34.441171] CR4: actual=0x0000000000002040, shadow=0x0000000000000000, gh_mask=fffffffffffef871
[   34.442096] CR3 = 0x0000000000000000
[   34.442499] PDPTR0 = 0x0000000000000000  PDPTR1 = 0x0000000000000000
[   34.443180] PDPTR2 = 0x0000000000000000  PDPTR3 = 0x0000000000000000
[   34.443870] RSP = 0x0000000000200000  RIP = 0x0000000000000000
[   34.444512] RFLAGS=0x00010002         DR7 = 0x0000000000000400

[   34.445132] Sysenter RSP=0000000000000000 CS:RIP=0000:0000000000000000
[   34.445833] CS:   sel=0x0000, attr=0x0009b, limit=0x0000ffff, base=0x0000000000000000
[   34.446675] DS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[   34.447520] SS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[   34.448348] ES:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[   34.449169] FS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[   34.449990] GS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[   34.450810] GDTR:                           limit=0x0000ffff, base=0x0000000000000000
[   34.451634] LDTR: sel=0x0000, attr=0x00082, limit=0x0000ffff, base=0x0000000000000000
[   34.452453] IDTR:                           limit=0x0000ffff, base=0x0000000000000000

[   34.453268] TR:   sel=0x0000, attr=0x0008b, limit=0x0000ffff, base=0x0000000000000000
[   34.454076] EFER= 0x0000000000000000
[   34.454466] PAT = 0x0007040600070406
[   34.454852] DebugCtl = 0x0000000000000000  DebugExceptions = 0x0000000000000000
[   34.455615] Interruptibility = 00000000  ActivityState = 00000000

[   33.892855] *** Host State ***
[   33.893027] RIP = 0xffffffff8203c8ee  RSP = 0xffffc90000657c48
[   33.893376] CS=0010 SS=0018 DS=0000 ES=0000 FS=0000 GS=0000 TR=0040
[   33.893721] FSBase=0000000031a743c0 GSBase=ffff88803ea00000 TRBase=fffffe0000003000
[   33.894132] GDTBase=fffffe0000001000 IDTBase=fffffe0000000000
[   33.894443] CR0=0000000080050033 1000 0000 0000 0101 0000 0000 0011 0011

Bit 0 (PE - Protection Enable): 1，启用保护模式。
Bit 1 (MP - Monitor Coprocessor): 1，控制协处理器的操作。
Bit 4 (ET - Extension Type): 1，指示协处理器的类型（x87 协处理器）。
Bit 5 (NE - Numeric Error): 1，控制浮点异常的处理。
Bit 16 (WP - Write Protect): 1，控制用户模式下对只读页面的写入保护。
Bit 18 (AM - Alignment Mask): 1，控制对齐检查。
Bit 31 (PG - Paging): 1，启用分页。

CR3=00000000063fe000 
    CR4=0000000000752ef0    0111 0101 0010 1110 1111 0000
        Bit 4 (PSE - Page Size Extensions): 1，启用页大小扩展。
        Bit 5 (PAE - Physical Address Extension): 1，启用物理地址扩展。
        Bit 6 (MCE - Machine Check Enable): 1，启用机器检查。
        Bit 7 (PGE - Page Global Enable): 1，启用全局页。
        Bit 9 (OSFXSR - Operating System Support for FXSAVE and FXRSTOR instructions): 1启用操作系统对 FXSAVE 和 FXRSTOR 指令的支持。
        Bit 10 (OSXMMEXCPT - Operating System Support for Unmasked SIMD Floating-Point Exceptions): 1，启用操作系统对未屏蔽的 SIMD 浮点异常的支持。
        Bit 11 (UMIP - User-Mode Instruction Prevention): 0，未启用用户模式指令预防。
        Bit 13 (VMXE - VMX Enable): 1，启用 VMX（虚拟机扩展）。
        Bit 16 (FSGSBASE - Enable RDFSBASE, WRFSBASE, RDGSBASE, WRGSBASE instructions): 1，启用 RDFSBASE, WRFSBASE, RDGSBASE, WRGSBASE 指令。
        Bit 18 (OSXSAVE - XSAVE and Processor Extended States Enable): 1，启用 XSAVE 和处理器扩展状态。
        Bit 20 (SMEP - Supervisor Mode Execution Protection): 1，启用监督模式执行保护。
        Bit 21 (SMAP - Supervisor Mode Access Prevention): 1，启用监督模式访问预防。
        Bit 22 (PKE - Protection Key Enable): 0，未启用保护键。

启用监督模式访问预防。
[   33.894856] Sysenter RSP=fffffe0000003000 CS:RIP=0010:ffffffff82201960
[   33.895276] EFER= 0x0000000000000d01
[   33.895517] PAT = 0x0407050600070106
[   33.895765] *** Control State ***
[   33.895985] CPUBased=0xb5986dfa SecondaryExec=0x020128e2 TertiaryExec=0x0000000000000000


[   33.896503] PinBased=0x0000007f EntryControls=0000d1ff ExitControls=002befff

[   33.896969] ExceptionBitmap=00060042 PFECmask=00000000 PFECmatch=00000000
[   33.897415] VMEntry: intr_info=00000000 errcode=00000000 ilen=00000000
[   33.897837] VMExit: intr_info=00000000 errcode=00000000 ilen=00000000
[   33.898221]         reason=80000021 qualification=0000000000000000
[   33.898617] IDTVectoring: info=00000000 errcode=00000000
[   33.898970] TSC Offset = 0xffffff351a3ae18e
[   33.899245] TSC Multiplier = 0x0001000000000000
[   33.899543] EPT pointer = 0x00000000065d305e
[   33.899830] Virtual processor ID = 0x0001


[    7.280281] EPT pointer (physical) = 0x0000000006612000
[    7.280574] EPT pointer (virtual) = 0x00000000a06935fd
 100100000111
[    7.280857] PML4E[0] = 0x0000000006611907 (physical) = 0x0000000080e968db (virtual)
[    7.281278]   PDPTE[0] = 0x0000000006610907 (physical) = 0x0000000023b763c3 (virtual)
[    7.281723]     PDE[0] = 0x000000000660f907 (physical) = 0x00000000192face4 (virtual)
[    7.282155]       PTE[0] = 0x06000000031cab77 (physical) = 0x00000000bba5feb4 (virtual)
[    7.282589]         PTE data at 00000000bba5feb4: can not access