      
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <string.h>
// #include <linux/kvm.h>

#define KVM_S390_GET_SKEYS_NONE   1
#define KVM_S390_SKEYS_MAX        1048576

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

/* For KVM_EXIT_INTERNAL_ERROR */
/* Emulate instruction failed. */
#define KVM_INTERNAL_ERROR_EMULATION	1
/* Encounter unexpected simultaneous exceptions. */
#define KVM_INTERNAL_ERROR_SIMUL_EX	2
/* Encounter unexpected vm-exit due to delivery event. */
#define KVM_INTERNAL_ERROR_DELIVERY_EV	3
/* Encounter unexpected vm-exit reason */
#define KVM_INTERNAL_ERROR_UNEXPECTED_EXIT_REASON	4

/* Flags that describe what fields in emulation_failure hold valid data. */
#define KVM_INTERNAL_ERROR_EMULATION_FLAG_INSTRUCTION_BYTES (1ULL << 0)

typedef uint32_t __u32;
typedef uint16_t __u16;
typedef uint8_t __u8;
typedef uint64_t __u64;

struct kvm_userspace_memory_region {
    uint32_t slot; // 要在哪个slot上注册内存区间
    // flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    // KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    uint32_t flags;
    uint64_t guest_phys_addr; // 虚机内存区间起始物理地址
    uint64_t memory_size;     // 虚机内存区间大小
    uint64_t userspace_addr;  // 虚机内存区间对应的主机虚拟地址
};

struct kvm_regs {
	/* out (KVM_GET_REGS) / in (KVM_SET_REGS) */
	uint64_t rax, rbx, rcx, rdx;
	uint64_t rsi, rdi, rsp, rbp;
	uint64_t r8,  r9,  r10, r11;
	uint64_t r12, r13, r14, r15;
	uint64_t rip, rflags;
};

struct kvm_segment {
	uint64_t base;
	uint32_t limit;
	uint16_t selector;
	uint8_t  type;
	uint8_t  present, dpl, db, s, l, g, avl;
	uint8_t  unusable;
	uint8_t  padding;
};

struct kvm_dtable {
	uint64_t base;
	uint16_t limit;
	uint16_t padding[3];
};

struct kvm_sregs {
	/* out (KVM_GET_SREGS) / in (KVM_SET_SREGS) */
	struct kvm_segment cs, ds, es, fs, gs, ss;
	struct kvm_segment tr, ldt;
	struct kvm_dtable gdt, idt;
	uint64_t cr0, cr2, cr3, cr4, cr8;
	uint64_t efer;
	uint64_t apic_base;
	uint64_t interrupt_bitmap[(256 + 63) / 64];
};

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


struct kvm_debug_exit_arch {
	__u32 exception;
	__u32 pad;
	__u64 pc;
	__u64 dr6;
	__u64 dr7;
};

/* for KVM_RUN, returned by mmap(vcpu_fd, offset=0) */
struct kvm_run {
	/* in */
	uint8_t request_interrupt_window;
	uint8_t immediate_exit;
	uint8_t padding1[6];

	/* out */
	uint32_t exit_reason;
	uint8_t ready_for_interrupt_injection;
	uint8_t if_flag;
	uint16_t flags;

	/* in (pre_kvm_run), out (post_kvm_run) */
	uint64_t cr8;
	uint64_t apic_base;

#ifdef __KVM_S390
	/* the processor status word for s390 */
	uint64_t psw_mask; /* psw upper half */
	uint64_t psw_addr; /* psw lower half */
#endif
	union {
		/* KVM_EXIT_UNKNOWN */
		struct {
			uint64_t hardware_exit_reason;
		} hw;
		/* KVM_EXIT_FAIL_ENTRY */
		struct {
			uint64_t hardware_entry_failure_reason;
			uint32_t cpu;
		} fail_entry;
		/* KVM_EXIT_EXCEPTION */
		struct {
			uint32_t exception;
			uint32_t error_code;
		} ex;
		/* KVM_EXIT_IO */
		struct {
#define KVM_EXIT_IO_IN  0
#define KVM_EXIT_IO_OUT 1
			uint8_t direction;
			uint8_t size; /* bytes */
			uint16_t port;
			uint32_t count;
			uint64_t data_offset; /* relative to kvm_run start */
		} io;
		/* KVM_EXIT_DEBUG */
		struct {
			struct kvm_debug_exit_arch arch;
		} debug;
		/* KVM_EXIT_MMIO */
		struct {
			uint64_t phys_addr;
			uint8_t  data[8];
			uint32_t len;
			uint8_t  is_write;
		} mmio;
		/* KVM_EXIT_HYPERCALL */
		struct {
			uint64_t nr;
			uint64_t args[6];
			uint64_t ret;
			uint32_t longmode;
			uint32_t pad;
		} hypercall;
		/* KVM_EXIT_TPR_ACCESS */
		struct {
			uint64_t rip;
			uint32_t is_write;
			uint32_t pad;
		} tpr_access;
		/* KVM_EXIT_S390_SIEIC */
		struct {
			uint8_t icptcode;
			uint16_t ipa;
			uint32_t ipb;
		} s390_sieic;
		/* KVM_EXIT_S390_RESET */
#define KVM_S390_RESET_POR       1
#define KVM_S390_RESET_CLEAR     2
#define KVM_S390_RESET_SUBSYSTEM 4
#define KVM_S390_RESET_CPU_INIT  8
#define KVM_S390_RESET_IPL       16
		uint64_t s390_reset_flags;
		/* KVM_EXIT_S390_UCONTROL */
		struct {
			uint64_t trans_exc_code;
			uint32_t pgm_code;
		} s390_ucontrol;
		/* KVM_EXIT_DCR (deprecated) */
		struct {
			uint32_t dcrn;
			uint32_t data;
			uint8_t  is_write;
		} dcr;
		/* KVM_EXIT_INTERNAL_ERROR */
		struct {
			uint32_t suberror;
			/* Available with KVM_CAP_INTERNAL_ERROR_DATA: */
			uint32_t ndata;
			uint64_t data[16];
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
			uint32_t suberror;
			uint32_t ndata;
			uint64_t flags;
			uint8_t  insn_size;
			uint8_t  insn_bytes[15];
		} emulation_failure;
		/* KVM_EXIT_OSI */
		struct {
			uint64_t gprs[32];
		} osi;
		/* KVM_EXIT_PAPR_HCALL */
		struct {
			uint64_t nr;
			uint64_t ret;
			uint64_t args[9];
		} papr_hcall;
		/* KVM_EXIT_S390_TSCH */
		struct {
			uint16_t subchannel_id;
			uint16_t subchannel_nr;
			uint32_t io_int_parm;
			uint32_t io_int_word;
			uint32_t ipb;
			uint8_t dequeued;
		} s390_tsch;
		/* KVM_EXIT_EPR */
		struct {
			uint32_t epr;
		} epr;
		/* KVM_EXIT_SYSTEM_EVENT */
		struct {
#define KVM_SYSTEM_EVENT_SHUTDOWN       1
#define KVM_SYSTEM_EVENT_RESET          2
#define KVM_SYSTEM_EVENT_CRASH          3
			uint32_t type;
			uint64_t flags;
		} system_event;
		/* KVM_EXIT_S390_STSI */
		struct {
			uint64_t addr;
			uint8_t ar;
			uint8_t reserved;
			uint8_t fc;
			uint8_t sel1;
			uint16_t sel2;
		} s390_stsi;
		/* KVM_EXIT_IOAPIC_EOI */
		struct {
			uint8_t vector;
		} eoi;
		/* KVM_EXIT_HYPERV */
		struct kvm_hyperv_exit hyperv;
		/* KVM_EXIT_ARM_NISV */
		struct {
			uint64_t esr_iss;
			uint64_t fault_ipa;
		} arm_nisv;
		/* KVM_EXIT_X86_RDMSR / KVM_EXIT_X86_WRMSR */
		struct {
			uint8_t error; /* user -> kernel */
			uint8_t pad[7];
#define KVM_MSR_EXIT_REASON_INVAL	(1 << 0)
#define KVM_MSR_EXIT_REASON_UNKNOWN	(1 << 1)
#define KVM_MSR_EXIT_REASON_FILTER	(1 << 2)
			uint32_t reason; /* kernel -> user */
			uint32_t index; /* kernel -> user */
			uint64_t data; /* kernel <-> user */
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
	uint64_t kvm_valid_regs;
	uint64_t kvm_dirty_regs;
	union {
		struct kvm_sync_regs regs;
		char padding[SYNC_REGS_SIZE_BYTES];
	} s;
};

#define KVM_CREATE_VM 0xAE01
#define KVM_CREATE_VCPU 0xAE41
#define KVM_SET_USER_MEMORY_REGION 0xAE46
#define KVM_GET_VCPU_MMAP_SIZE 0xAE04
#define KVM_GET_REGS 0xAE81
#define KVM_SET_REGS 0xAE82
#define KVM_GET_SREGS 0xAE83
#define KVM_SET_SREGS 0xAE84

#define KVM_RUN 0xAE80

int kvm(uint8_t code[], size_t code_len) {
    // step 1, open /dev/kvm
  int kvmfd = open("/dev/kvm", O_RDWR|O_CLOEXEC);
  if(kvmfd == -1) {
    printf("failed to open /dev/kvm\n");
    return 0;
  }

  // step 2, create VM
  int vmfd = ioctl(kvmfd, KVM_CREATE_VM, 0);
    printf("vmfd %d\n",vmfd);
  // step 3, set up user memory region
  size_t mem_size = 0x4000; // size of user memory you want to assign
  void *mem = mmap(0, mem_size, PROT_READ|PROT_WRITE,
                   MAP_SHARED|MAP_ANONYMOUS, -1, 0);

  printf("map mem %p\n",mem);
  int user_entry = 0x0;
  memcpy((void*)((size_t)mem + user_entry), code, code_len);
  struct kvm_userspace_memory_region region = {
    .slot = 0,
    .flags = 0,
    .guest_phys_addr = 0,
    .memory_size = mem_size,
    .userspace_addr = (size_t)mem
  };
  ioctl(vmfd, KVM_SET_USER_MEMORY_REGION, &region);
  /* end of step 3 */

  // step 4, create vCPU
  int vcpufd = ioctl(vmfd, KVM_CREATE_VCPU, 0);

  // step 5, set up memory for vCPU
  size_t vcpu_mmap_size = ioctl(kvmfd, KVM_GET_VCPU_MMAP_SIZE, NULL);
  struct kvm_run* run = (struct kvm_run*) mmap(0, vcpu_mmap_size, PROT_READ | PROT_WRITE, MAP_SHARED, vcpufd, 0);

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

   // step 7, execute vm and handle exit reason
  while (1) {
    ioctl(vcpufd, KVM_RUN, NULL);
    switch (run->exit_reason) {
    case KVM_EXIT_HLT:
      fputs("KVM_EXIT_HLT", stderr);
      return 0;
    case KVM_EXIT_IO:
      /* TODO: check port and direction here */
      putchar(*(((char *)run) + run->io.data_offset));
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

int main() {
    uint8_t code[] = "\xB0\x61\xBA\x17\x02\xEE\xB0\n\xEE\xF4";
    kvm(code, sizeof(code));
    return 0;
}

    