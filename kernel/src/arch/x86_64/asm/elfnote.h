/*
 * arch/x86/include/asm/elfnote.h
 *
 * ELF Note macros for PVH boot support
 * Based on Linux kernel implementation
 */

#ifndef _ASM_X86_ELFNOTE_H
#define _ASM_X86_ELFNOTE_H

/*
 * Xen ELF Note Types
 * Based on include/xen/interface/elfnote.h from Xen
 */
#define XEN_ELFNOTE_INFO           0
#define XEN_ELFNOTE_ENTRY          1
#define XEN_ELFNOTE_HYPERCALL_PAGE 2
#define XEN_ELFNOTE_VIRT_BASE      3
#define XEN_ELFNOTE_PADDR_OFFSET   4
#define XEN_ELFNOTE_XEN_VERSION    5
#define XEN_ELFNOTE_GUEST_OS       6
#define XEN_ELFNOTE_GUEST_VERSION  7
#define XEN_ELFNOTE_LOADER         8
#define XEN_ELFNOTE_PAE_MODE       9
#define XEN_ELFNOTE_FEATURES      10
#define XEN_ELFNOTE_BSD_SYMTAB    11
#define XEN_ELFNOTE_HV_START_LOW  12
#define XEN_ELFNOTE_L1_MFN_VALID  13
#define XEN_ELFNOTE_SUSPEND_CANCEL 14
#define XEN_ELFNOTE_INIT_P2M      15
#define XEN_ELFNOTE_MOD_START_PFN 16
#define XEN_ELFNOTE_SUPPORTED_FEATURES 17

/*
 * Physical entry point into the kernel (32-bit)
 *
 * 32bit entry point into the kernel. When requested to launch the
 * guest kernel in an HVM container, Xen will use this entry point to
 * launch the guest in 32bit protected mode with paging disabled.
 */
#define XEN_ELFNOTE_PHYS32_ENTRY 18

/*
 * Physical loading constraints for PVH kernels
 *
 * The presence of this note indicates the kernel supports relocating itself.
 *
 * The note may include up to three 32bit values to place constraints on the
 * guest physical loading addresses and alignment for a PVH kernel.  Values
 * are read in the following order:
 *  - a required start alignment (default 0x200000)
 *  - a minimum address for the start of the image (default 0)
 *  - a maximum address for the last byte of the image (default 0xffffffff)
 */
#define XEN_ELFNOTE_PHYS32_RELOC 19

/*
 * Helper macros to generate ELF Note structures
 *
 * These macros create an ELF note with the proper format expected by
 * Cloud Hypervisor and other VMMs that support PVH boot.
 */

/*
 * Start an ELF note
 * name: The note owner (e.g., "Xen")
 * type: The note type (e.g., XEN_ELFNOTE_PHYS32_ENTRY)
 * flags: Section flags (typically "a" for alloc)
 */
#define ELFNOTE_START(name, type, flags)              \
    .pushsection .note.name, flags, @note;            \
    .balign 4;                                        \
    .long 2f - 1f;        /* namesz */                \
    .long 4484f - 3f;      /* descsz */                \
    .long type;                                   \
1:  .asciz #name;                                     \
2:  .balign 4;                                       \
3:

/*
 * End an ELF note
 */
#define ELFNOTE_END                                     \
4484:.balign 4;                                        \
    .popsection;

/*
 * Create a complete ELF note
 * name: The note owner
 * type: The note type number
 * desc: The description data (can be multiple instructions)
 */
#define ELFNOTE(name, type, desc)              \
    ELFNOTE_START(name, type, "a")             \
    desc;                                     \
    ELFNOTE_END

/*
 * Convenience macros for Xen notes
 */
#define XEN_ELFNOTE_NOTE(type, desc) \
    ELFNOTE(Xen, type, desc)

/*
 * Create the PHYS32_ENTRY note
 * entry_sym: The symbol for the PVH entry point
 * This should be: entry_sym - __START_KERNEL_map (for Linux-style)
 * or just the physical address directly
 */
#define XEN_PVH_ENTRY_NOTE(entry_sym) \
    XEN_ELFNOTE_NOTE(XEN_ELFNOTE_PHYS32_ENTRY, .long entry_sym)

/*
 * Create the PHYS32_RELOC note
 * align: Required alignment (e.g., CONFIG_PHYSICAL_ALIGN)
 * min_addr: Minimum load address (e.g., LOAD_PHYSICAL_ADDR)
 * max_addr: Maximum end address (e.g., KERNEL_IMAGE_SIZE - 1)
 */
#define XEN_PVH_RELOC_NOTE(align, min_addr, max_addr) \
    XEN_ELFNOTE_NOTE(XEN_ELFNOTE_PHYS32_RELOC,     \
        .long align;                                \
        .long min_addr;                              \
        .long max_addr)

#endif /* _ASM_X86_ELFNOTE_H */
