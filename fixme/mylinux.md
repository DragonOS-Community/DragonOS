
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
[    2.779110] *** Guest State ***
[    2.779288] CR0: actual=0x0000000000000030, shadow=0x0000000060000010, gh_mask=fffffffffffefff7
[    2.779761] CR4: actual=0x0000000000002040, shadow=0x0000000000000000, gh_mask=fffffffffffef871
[    2.780234] CR3 = 0x0000000000000000
[    2.780439] PDPTR0 = 0x0000000000000000  PDPTR1 = 0x0000000000000000
[    2.780790] PDPTR2 = 0x0000000000000000  PDPTR3 = 0x0000000000000000
[    2.781140] RSP = 0x0000000000200000  RIP = 0x0000000000000000
[    2.781464] RFLAGS=0x00010002         DR7 = 0x0000000000000400
[    2.781788] Sysenter RSP=0000000000000000 CS:RIP=0000:0000000000000000
[    2.782153] CS:   sel=0x0000, attr=0x0009b, limit=0x0000ffff, base=0x0000000000000000
[    2.782607] DS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[    2.783048] SS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[    2.783486] ES:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[    2.783922] FS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[    2.784355] GS:   sel=0x0000, attr=0x00093, limit=0x0000ffff, base=0x0000000000000000
[    2.784784] GDTR:                           limit=0x0000ffff, base=0x0000000000000000
[    2.785215] LDTR: sel=0x0000, attr=0x00082, limit=0x0000ffff, base=0x0000000000000000
[    2.785646] IDTR:                           limit=0x0000ffff, base=0x0000000000000000
[    2.786081] TR:   sel=0x0000, attr=0x0008b, limit=0x0000ffff, base=0x0000000000000000
[    2.786515] EFER= 0x0000000000000000
[    2.786717] PAT = 0x0007040600070406
[    2.786923] DebugCtl = 0x0000000000000000  DebugExceptions = 0x0000000000000000
[    2.787324] Interruptibility = 00000000  ActivityState = 00000000
[    2.787675] *** Host State ***
[    2.787853] RIP = 0xffffffff8203d8ee  RSP = 0xffffc900006439f8
[    2.788178] CS=0010 SS=0018 DS=0000 ES=0000 FS=0000 GS=0000 TR=0040
[    2.788522] FSBase=000000003919f3c0 GSBase=ffff88803ea00000 TRBase=fffffe0000003000
[    2.788940] GDTBase=fffffe0000001000 IDTBase=fffffe0000000000
[    2.789260] CR0=0000000080050033 __va::CR3=00000000a894e9cc CR4=0000000000752ef0
[    2.789671] Sysenter RSP=fffffe0000003000 CS:RIP=0010:ffffffff82201960
[    2.790034] EFER= 0x0000000000000d01
[    2.790235] PAT = 0x0407050600070106
[    2.790439] *** Control State ***
[    2.790626] CPUBased=0xb5986dfa SecondaryExec=0x020128e2 TertiaryExec=0x0000000000000000
[    2.791067] PinBased=0x0000007f EntryControls=0000d1ff ExitControls=002befff
[    2.791465] ExceptionBitmap=00060042 PFECmask=00000000 PFECmatch=00000000
[    2.791840] VMEntry: intr_info=00000000 errcode=00000000 ilen=00000000
[    2.792201] VMExit: intr_info=00000000 errcode=00000000 ilen=00000003
[    2.792570]         reason=00000030 qualification=0000000000000184
[    2.792911] IDTVectoring: info=00000000 errcode=00000000
[    2.793208] TSC Offset = 0xfffffffddc262e77
[    2.793444] TSC Multiplier = 0x0001000000000000
[    2.793700] __va::EPT pointer = 0x0000000093b59c89
[    2.793970] Virtual processor ID = 0x0001
[    2.794193] kvm_tdp_mmu_map ret = 4
[    2.794446] VM Exit Reason:30
[    2.794624] exit_handler_index: 30
Guest CR3: 0x0
run->exit_reason= 0x2
aKVM_EXIT_IO: run->io.port = 217 
[    2.795391] VM Exit Reason:30
[    2.795563] exit_handler_index: 30
Guest CR3: 0x0
run->exit_reason= 0x2

KVM_EXIT_IO: run->io.port = 217 
[    2.796299] VM Exit Reason:12
[    2.796472] exit_handler_index: 12
Guest CR3: 0x0
run->exit_reason= 0x5
KVM_EXIT_HLT 


[    7.280281] EPT pointer (physical) = 0x0000000006612000
[    7.280574] EPT pointer (virtual) = 0x00000000a06935fd
 100100000111
[    7.280857] PML4E[0] = 0x0000000006611907 (physical) = 0x0000000080e968db (virtual)
[    7.281278]   PDPTE[0] = 0x0000000006610907 (physical) = 0x0000000023b763c3 (virtual)
[    7.281723]     PDE[0] = 0x000000000660f907 (physical) = 0x00000000192face4 (virtual)
[    7.282155]       PTE[0] = 0x06000000031cab77 (physical) = 0x00000000bba5feb4 (virtual)
[    7.282589]         PTE data at 00000000bba5feb4: can not access