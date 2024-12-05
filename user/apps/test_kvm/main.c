
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
//#include <linux/kvm.h>

typedef __signed__ char __s8;
typedef unsigned char __u8;

typedef __signed__ short __s16;
typedef unsigned short __u16;

typedef __signed__ int __s32;
typedef unsigned int __u32;

#ifdef __GNUC__
__extension__ typedef __signed__ long long __s64;
__extension__ typedef unsigned long long __u64;
#else
typedef __signed__ long long __s64;
typedef unsigned long long __u64;
#endif

//from linux/kvm.h
#define KVM_CREATE_VM             _IO(KVMIO,   0x01) /* returns a VM fd */
#define KVM_CREATE_VCPU           _IO(KVMIO,   0x41)
#define KVM_GET_VCPU_MMAP_SIZE    _IO(KVMIO,   0x04) /* in bytes */

#define KVM_RUN                   _IO(KVMIO,   0x80)
#define KVM_GET_REGS              _IOR(KVMIO,  0x81, struct kvm_regs)
#define KVM_SET_REGS              _IOW(KVMIO,  0x82, struct kvm_regs)
#define KVM_GET_SREGS             _IOR(KVMIO,  0x83, struct kvm_sregs)
#define KVM_SET_SREGS             _IOW(KVMIO,  0x84, struct kvm_sregs)

#define KVMIO 0xAE
#define KVM_SET_USER_MEMORY_REGION _IOW(KVMIO, 0x46, \
					struct kvm_userspace_memory_region)
/* Architectural interrupt line count. */
#define KVM_NR_INTERRUPTS 256
struct kvm_hyperv_exit {
#define KVM_EXIT_HYPERV_SYNIC          1
#define KVM_EXIT_HYPERV_HCALL          2
#define KVM_EXIT_HYPERV_SYNDBG         3
	__u32 type;
	__u32 pad1;
	union {
		struct {
			__u32 msr;
			__u32 pad2;
			__u64 control;
			__u64 evt_page;
			__u64 msg_page;
		} synic;
		struct {
			__u64 input;
			__u64 result;
			__u64 params[2];
		} hcall;
		struct {
			__u32 msr;
			__u32 pad2;
			__u64 control;
			__u64 status;
			__u64 send_page;
			__u64 recv_page;
			__u64 pending_page;
		} syndbg;
	} u;
};
struct kvm_debug_exit_arch {
	__u32 exception;
	__u32 pad;
	__u64 pc;
	__u64 dr6;
	__u64 dr7;
};
/* for KVM_SET_USER_MEMORY_REGION */
struct kvm_userspace_memory_region {
	__u32 slot;
	__u32 flags;
	__u64 guest_phys_addr;
	__u64 memory_size; /* bytes */
	__u64 userspace_addr; /* start of the userspace allocated memory */
};
struct kvm_xen_exit {
#define KVM_EXIT_XEN_HCALL          1
	__u32 type;
	union {
		struct {
			__u32 longmode;
			__u32 cpl;
			__u64 input;
			__u64 result;
			__u64 params[6];
		} hcall;
	} u;
};
/* for KVM_GET_REGS and KVM_SET_REGS */
struct kvm_regs {
	/* out (KVM_GET_REGS) / in (KVM_SET_REGS) */
	__u64 rax, rbx, rcx, rdx;
	__u64 rsi, rdi, rsp, rbp;
	__u64 r8,  r9,  r10, r11;
	__u64 r12, r13, r14, r15;
	__u64 rip, rflags;
};
struct my_kvm_segment {
	__u64 base;
	__u32 limit;
	__u16 selector;
	__u8  type;
	__u8  present, dpl, db, s, l, g, avl;
	__u8  unusable;
	__u8  padding;
};
struct kvm_dtable {
	__u64 base;
	__u16 limit;
	__u16 padding[3];
};
/* for KVM_GET_SREGS and KVM_SET_SREGS */
struct kvm_sregs {
	/* out (KVM_GET_SREGS) / in (KVM_SET_SREGS) */
	struct my_kvm_segment cs, ds, es, fs, gs, ss;
	struct my_kvm_segment tr, ldt;
	struct kvm_dtable gdt, idt;
	__u64 cr0, cr2, cr3, cr4, cr8;
	__u64 efer;
	__u64 apic_base;
	__u64 interrupt_bitmap[(KVM_NR_INTERRUPTS + 63) / 64];
};

/* for KVM_GET/SET_VCPU_EVENTS */
struct kvm_vcpu_events {
	struct {
		__u8 injected;
		__u8 nr;
		__u8 has_error_code;
		__u8 pending;
		__u32 error_code;
	} exception;
	struct {
		__u8 injected;
		__u8 nr;
		__u8 soft;
		__u8 shadow;
	} interrupt;
	struct {
		__u8 injected;
		__u8 pending;
		__u8 masked;
		__u8 pad;
	} nmi;
	__u32 sipi_vector;
	__u32 flags;
	struct {
		__u8 smm;
		__u8 pending;
		__u8 smm_inside_nmi;
		__u8 latched_init;
	} smi;
	__u8 reserved[27];
	__u8 exception_has_payload;
	__u64 exception_payload;
};
/* kvm_sync_regs struct included by kvm_run struct */
struct kvm_sync_regs {
	/* Members of this structure are potentially malicious.
	 * Care must be taken by code reading, esp. interpreting,
	 * data fields from them inside KVM to prevent TOCTOU and
	 * double-fetch types of vulnerabilities.
	 */
	struct kvm_regs regs;
	struct kvm_sregs sregs;
	struct kvm_vcpu_events events;
};

/* for KVM_RUN, returned by mmap(vcpu_fd, offset=0) */
struct kvm_run {
	/* in */
	__u8 request_interrupt_window;
	__u8 immediate_exit;
	__u8 padding1[6];

	/* out */
	__u32 exit_reason;
	__u8 ready_for_interrupt_injection;
	__u8 if_flag;
	__u16 flags;

	/* in (pre_kvm_run), out (post_kvm_run) */
	__u64 cr8;
	__u64 apic_base;

#ifdef __KVM_S390
	/* the processor status word for s390 */
	__u64 psw_mask; /* psw upper half */
	__u64 psw_addr; /* psw lower half */
#endif
	union {
		/* KVM_EXIT_UNKNOWN */
		struct {
			__u64 hardware_exit_reason;
		} hw;
		/* KVM_EXIT_FAIL_ENTRY */
		struct {
			__u64 hardware_entry_failure_reason;
			__u32 cpu;
		} fail_entry;
		/* KVM_EXIT_EXCEPTION */
		struct {
			__u32 exception;
			__u32 error_code;
		} ex;
		/* KVM_EXIT_IO */
		struct {
#define KVM_EXIT_IO_IN  0
#define KVM_EXIT_IO_OUT 1
			__u8 direction;
			__u8 size; /* bytes */
			__u16 port;
			__u32 count;
			__u64 data_offset; /* relative to kvm_run start */
		} io;
		/* KVM_EXIT_DEBUG */
		struct {
			struct kvm_debug_exit_arch arch;
		} debug;
		/* KVM_EXIT_MMIO */
		struct {
			__u64 phys_addr;
			__u8  data[8];
			__u32 len;
			__u8  is_write;
		} mmio;
		/* KVM_EXIT_HYPERCALL */
		struct {
			__u64 nr;
			__u64 args[6];
			__u64 ret;
			__u32 longmode;
			__u32 pad;
		} hypercall;
		/* KVM_EXIT_TPR_ACCESS */
		struct {
			__u64 rip;
			__u32 is_write;
			__u32 pad;
		} tpr_access;
		/* KVM_EXIT_S390_SIEIC */
		struct {
			__u8 icptcode;
			__u16 ipa;
			__u32 ipb;
		} s390_sieic;
		/* KVM_EXIT_S390_RESET */
#define KVM_S390_RESET_POR       1
#define KVM_S390_RESET_CLEAR     2
#define KVM_S390_RESET_SUBSYSTEM 4
#define KVM_S390_RESET_CPU_INIT  8
#define KVM_S390_RESET_IPL       16
		__u64 s390_reset_flags;
		/* KVM_EXIT_S390_UCONTROL */
		struct {
			__u64 trans_exc_code;
			__u32 pgm_code;
		} s390_ucontrol;
		/* KVM_EXIT_DCR (deprecated) */
		struct {
			__u32 dcrn;
			__u32 data;
			__u8  is_write;
		} dcr;
		/* KVM_EXIT_INTERNAL_ERROR */
		struct {
			__u32 suberror;
			/* Available with KVM_CAP_INTERNAL_ERROR_DATA: */
			__u32 ndata;
			__u64 data[16];
		} internal;
		/*
		 * KVM_INTERNAL_ERROR_EMULATION
		 *
		 * "struct emulation_failure" is an overlay of "struct internal"
		 * that is used for the KVM_INTERNAL_ERROR_EMULATION sub-type of
		 * KVM_EXIT_INTERNAL_ERROR.  Note, unlike other internal error
		 * sub-types, this struct is ABI!  It also needs to be backwards
		 * compatible with "struct internal".  Take special care that
		 * "ndata" is correct, that new fields are enumerated in "flags",
		 * and that each flag enumerates fields that are 64-bit aligned
		 * and sized (so that ndata+internal.data[] is valid/accurate).
		 */
		struct {
			__u32 suberror;
			__u32 ndata;
			__u64 flags;
			__u8  insn_size;
			__u8  insn_bytes[15];
		} emulation_failure;
		/* KVM_EXIT_OSI */
		struct {
			__u64 gprs[32];
		} osi;
		/* KVM_EXIT_PAPR_HCALL */
		struct {
			__u64 nr;
			__u64 ret;
			__u64 args[9];
		} papr_hcall;
		/* KVM_EXIT_S390_TSCH */
		struct {
			__u16 subchannel_id;
			__u16 subchannel_nr;
			__u32 io_int_parm;
			__u32 io_int_word;
			__u32 ipb;
			__u8 dequeued;
		} s390_tsch;
		/* KVM_EXIT_EPR */
		struct {
			__u32 epr;
		} epr;
		/* KVM_EXIT_SYSTEM_EVENT */
		struct {
#define KVM_SYSTEM_EVENT_SHUTDOWN       1
#define KVM_SYSTEM_EVENT_RESET          2
#define KVM_SYSTEM_EVENT_CRASH          3
			__u32 type;
			__u64 flags;
		} system_event;
		/* KVM_EXIT_S390_STSI */
		struct {
			__u64 addr;
			__u8 ar;
			__u8 reserved;
			__u8 fc;
			__u8 sel1;
			__u16 sel2;
		} s390_stsi;
		/* KVM_EXIT_IOAPIC_EOI */
		struct {
			__u8 vector;
		} eoi;
		/* KVM_EXIT_HYPERV */
		struct kvm_hyperv_exit hyperv;
		/* KVM_EXIT_ARM_NISV */
		struct {
			__u64 esr_iss;
			__u64 fault_ipa;
		} arm_nisv;
		/* KVM_EXIT_X86_RDMSR / KVM_EXIT_X86_WRMSR */
		struct {
			__u8 error; /* user -> kernel */
			__u8 pad[7];
#define KVM_MSR_EXIT_REASON_INVAL	(1 << 0)
#define KVM_MSR_EXIT_REASON_UNKNOWN	(1 << 1)
#define KVM_MSR_EXIT_REASON_FILTER	(1 << 2)
			__u32 reason; /* kernel -> user */
			__u32 index; /* kernel -> user */
			__u64 data; /* kernel <-> user */
		} msr;
		/* KVM_EXIT_XEN */
		struct kvm_xen_exit xen;
		/* Fix the size of the union. */
		char padding[256];
	};

	/* 2048 is the size of the char array used to bound/pad the size
	 * of the union that holds sync regs.
	 */
	#define SYNC_REGS_SIZE_BYTES 2048
	/*
	 * shared registers between kvm and userspace.
	 * kvm_valid_regs specifies the register classes set by the host
	 * kvm_dirty_regs specified the register classes dirtied by userspace
	 * struct kvm_sync_regs is architecture specific, as well as the
	 * bits for kvm_valid_regs and kvm_dirty_regs
	 */
	__u64 kvm_valid_regs;
	__u64 kvm_dirty_regs;
	union {
		struct kvm_sync_regs regs;
		char padding[SYNC_REGS_SIZE_BYTES];
	} s;
};


int kvm(uint8_t code[], size_t code_len)
{
  // step 1, open /dev/kvm
  int kvmfd = open("/dev/kvm", O_RDWR | O_CLOEXEC);
  if (kvmfd == -1)
  {
    printf("failed to open /dev/kvm\n");
    return 0;
  }

  // step 2, create VM
  int vmfd = ioctl(kvmfd, KVM_CREATE_VM, 0);
  printf("vmfd %d\n", vmfd);
  // step 3, set up user memory region
  size_t mem_size = 0x10000; // size of user memory you want to assign
  void *mem = mmap(0, mem_size, PROT_READ | PROT_WRITE,
                   MAP_SHARED | MAP_ANONYMOUS, -1, 0);

  printf("map mem %p\n", mem);
  int user_entry = 0x0;
  memcpy((void *)((size_t)mem + user_entry), code, code_len);
  struct kvm_userspace_memory_region region = {
      .slot = 0,
      .flags = 0,
      .guest_phys_addr = 0,
      .memory_size = mem_size,
      .userspace_addr = (size_t)mem};
  ioctl(vmfd, KVM_SET_USER_MEMORY_REGION, &region);
  /* end of step 3 */

  // step 4, create vCPU
  int vcpufd = ioctl(vmfd, KVM_CREATE_VCPU, 0);
  printf("create vcpu,fd: %p\n", vcpufd);
  // step 5, set up memory for vCPU
  size_t vcpu_mmap_size = ioctl(kvmfd, KVM_GET_VCPU_MMAP_SIZE, NULL);
  struct kvm_run *run = (struct kvm_run *)mmap(0, vcpu_mmap_size, PROT_READ | PROT_WRITE, MAP_SHARED, vcpufd, 0);

  // step 6, set up vCPU's registers
  /* standard registers include general-purpose registers and flags */
  struct kvm_regs regs;
  ioctl(vcpufd, KVM_GET_REGS, &regs);
  regs.rip = user_entry;
  regs.rsp = 0x200000; // stack address
  regs.rflags = 0x2; // in x86 the 0x2 bit should always be set
  ioctl(vcpufd, KVM_SET_REGS, &regs); // set registers

  /* special registers include segment registers */
  struct kvm_sregs sregs;
  ioctl(vcpufd, KVM_GET_SREGS, &sregs);
  sregs.cs.base = sregs.cs.selector = 0; // let base of code segment equal to zero
  ioctl(vcpufd, KVM_SET_SREGS, &sregs);
  ioctl(vcpufd, KVM_GET_SREGS, &sregs);
  // step 7, execute vm and handle exit reason
  #define KVM_EXIT_UNKNOWN          0
#define KVM_EXIT_EXCEPTION        1
#define KVM_EXIT_IO               2
#define KVM_EXIT_HYPERCALL        3
#define KVM_EXIT_DEBUG            4
#define KVM_EXIT_HLT              5
#define KVM_EXIT_MMIO             6
#define KVM_EXIT_IRQ_WINDOW_OPEN  7
#define KVM_EXIT_SHUTDOWN         8
#define KVM_EXIT_FAIL_ENTRY       9
#define KVM_EXIT_INTR             10
#define KVM_EXIT_SET_TPR          11
#define KVM_EXIT_TPR_ACCESS       12
#define KVM_EXIT_S390_SIEIC       13
#define KVM_EXIT_S390_RESET       14
#define KVM_EXIT_DCR              15 /* deprecated */
#define KVM_EXIT_NMI              16
#define KVM_EXIT_INTERNAL_ERROR   17
#define KVM_EXIT_OSI              18
#define KVM_EXIT_PAPR_HCALL	  19
#define KVM_EXIT_S390_UCONTROL	  20
#define KVM_EXIT_WATCHDOG         21
#define KVM_EXIT_S390_TSCH        22
#define KVM_EXIT_EPR              23
#define KVM_EXIT_SYSTEM_EVENT     24
#define KVM_EXIT_S390_STSI        25
#define KVM_EXIT_IOAPIC_EOI       26
#define KVM_EXIT_HYPERV           27
#define KVM_EXIT_ARM_NISV         28
#define KVM_EXIT_X86_RDMSR        29
#define KVM_EXIT_X86_WRMSR        30
#define KVM_EXIT_DIRTY_RING_FULL  31
#define KVM_EXIT_AP_RESET_HOLD    32
#define KVM_EXIT_X86_BUS_LOCK     33
#define KVM_EXIT_XEN              34
  while (1)
  {
    ioctl(vcpufd, KVM_RUN, NULL);
    ioctl(vcpufd, KVM_GET_SREGS, &sregs);
    printf("Guest CR3: 0x%llx\n", sregs.cr3);
    switch (run->exit_reason)
    {
    case KVM_EXIT_HLT:
      fputs("KVM_EXIT_HLT \n", stderr);
      return 0;
    case KVM_EXIT_IO:
      /* TODO: check port and direction here */
      putchar(*(((char *)run) + run->io.data_offset));
      printf("KVM_EXIT_IO: run->io.port = %lx \n",
             run->io.port);
      break;
    case KVM_EXIT_FAIL_ENTRY:
      printf("KVM_EXIT_FAIL_ENTRY: hardware_entry_failure_reason = 0x%lx",
             run->fail_entry.hardware_entry_failure_reason);
      return 0;
    case KVM_EXIT_INTERNAL_ERROR:
      printf("KVM_EXIT_INTERNAL_ERROR: suberror = 0x%x",
             run->internal.suberror);
      return 0;
    case KVM_EXIT_SHUTDOWN:
      printf("KVM_EXIT_SHUTDOWN");
      return 0;
    default:
      printf("Unhandled reason: %d", run->exit_reason);
      return 0;
    }
  }
}

  /*汇编指令解释
0xB0 0x61 (mov al, 0x61)
解释：将立即数 0x61（ASCII 字符 'a'）加载到 AL 寄存器中。

0xBA 0x17 0x02 (mov dx, 0x0217)
Linux: ilen = 3 外中断和EPT_VIOLATION
解释：将立即数 0x0217 加载到 DX 寄存器中。

0xEE (out dx, al)
解释：将 AL 寄存器的值输出到 DX 寄存器指定的端口。

0xB0 0x0A (mov al, 0x0A)
解释：将立即数 0x0A（换行符）加载到 AL 寄存器中。

0xEE (out dx, al)
解释：将 AL 寄存器的值输出到 DX 寄存器指定的端口。

0xF4 (hlt)
解释：执行 hlt 指令，使处理器进入休眠状态，直到下一个外部中断到来。*/

int main()
{
	//uint8_t code[] = "\xB0\x61\xBA\x17\x02\xEE\xB0\n\xEE\xF4";
  	//uint8_t code[] = "\xB0\x61\xBA\x17\x02\xEE\xF4";
	uint8_t code[] = "\xB0\x61\xF4";
  kvm(code, sizeof(code));
  return 0;
}
