//
// Created by longjin on 2022/1/20.
//

#include "common/glib.h"
#include "common/kprint.h"
#include "common/printk.h"
#include "exception/gate.h"
#include "exception/irq.h"
#include "exception/trap.h"
#include "mm/mm.h"
#include "mm/slab.h"
#include "process/process.h"
#include "smp/smp.h"
#include "syscall/syscall.h"
#include <exception/softirq.h>
#include <libs/libUI/screen_manager.h>
#include <libs/libUI/textui.h>
#include <sched/sched.h>
#include <smp/ipi.h>

#include <filesystem/VFS/VFS.h>
#include <filesystem/devfs/devfs.h>
#include <filesystem/fat32/fat32.h>

#include "driver/acpi/acpi.h"
#include "driver/disk/ahci/ahci.h"
#include "driver/disk/ata.h"
#include "driver/keyboard/ps2_keyboard.h"
#include "driver/mouse/ps2_mouse.h"
#include "driver/multiboot2/multiboot2.h"
#include "driver/pci/pci.h"
#include "driver/tty/tty.h"
#include <driver/timers/HPET/HPET.h>
#include <driver/timers/rtc/rtc.h>
#include <driver/uart/uart.h>
#include <driver/usb/usb.h>
#include <driver/video/video.h>
#include <time/timer.h>

#include <driver/interrupt/apic/apic_timer.h>

ul bsp_idt_size, bsp_gdt_size;

#pragma GCC push_options
#pragma GCC optimize("O0")
struct gdtr gdtp;
struct idtr idtp;
void reload_gdt()
{

    gdtp.size = bsp_gdt_size - 1;
    gdtp.gdt_vaddr = (ul)phys_2_virt((ul)&GDT_Table);

    asm volatile("lgdt (%0)   \n\t" ::"r"(&gdtp) : "memory");
}

void reload_idt()
{

    idtp.size = bsp_idt_size - 1;
    idtp.idt_vaddr = (ul)phys_2_virt((ul)&IDT_Table);
    // kdebug("gdtvaddr=%#018lx", p.gdt_vaddr);
    // kdebug("gdt size=%d", p.size);

    asm volatile("lidt (%0)   \n\t" ::"r"(&idtp) : "memory");
}

// 初始化系统各模块
void system_initialize()
{

    uart_init(COM1, 115200);
    video_init();

    scm_init();
    textui_init();
    kinfo("Kernel Starting...");

    // 重新加载gdt和idt
    ul tss_item_addr = (ul)phys_2_virt(0x7c00);

    _stack_start = head_stack_start; // 保存init proc的栈基地址（由于之后取消了地址重映射，因此必须在这里重新保存）
    kdebug("_stack_start=%#018lx", _stack_start);

    load_TR(10); // 加载TR寄存器
    set_tss64((uint *)&initial_tss[0], _stack_start, _stack_start, _stack_start, tss_item_addr, tss_item_addr,
              tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr);

    cpu_core_info[0].stack_start = _stack_start;
    cpu_core_info[0].tss_vaddr = (uint64_t)&initial_tss[0];
    // kdebug("cpu_core_info[0].tss_vaddr=%#018lx", cpu_core_info[0].tss_vaddr);
    // kdebug("cpu_core_info[0].stack_start%#018lx", cpu_core_info[0].stack_start);

    // 初始化中断描述符表
    sys_vector_init();

    //  初始化内存管理单元
    mm_init();

    // 内存管理单元初始化完毕后，需要立即重新初始化显示驱动。
    // 原因是，系统启动初期，framebuffer被映射到48M地址处，
    // mm初始化完毕后，若不重新初始化显示驱动，将会导致错误的数据写入内存，从而造成其他模块崩溃
    // 对显示模块进行低级初始化，不启用double buffer
    scm_reinit();

    // =========== 重新设置initial_tss[0]的ist
    uchar *ptr = (uchar *)kzalloc(STACK_SIZE, 0) + STACK_SIZE;
    ((struct process_control_block *)(ptr - STACK_SIZE))->cpu_id = 0;

    initial_tss[0].ist1 = (ul)ptr;
    initial_tss[0].ist2 = (ul)ptr;
    initial_tss[0].ist3 = (ul)ptr;
    initial_tss[0].ist4 = (ul)ptr;
    initial_tss[0].ist5 = (ul)ptr;
    initial_tss[0].ist6 = (ul)ptr;
    initial_tss[0].ist7 = (ul)ptr;
    // ===========================

    acpi_init();

    // 初始化中断模块
    sched_init();
    irq_init();

    softirq_init();
    current_pcb->cpu_id = 0;
    current_pcb->preempt_count = 0;
    // 先初始化系统调用模块
    syscall_init();
    io_mfence();
    //  再初始化进程模块。顺序不能调转
    sched_init();
    io_mfence();

    timer_init();

    // 这里必须加内存屏障，否则会出错
    io_mfence();
    smp_init();
    io_mfence();

    vfs_init();
    devfs_init();
    cpu_init();
    ps2_keyboard_init();
    tty_init();
    // ps2_mouse_init();
    // ata_init();
    pci_init();
    io_mfence();

    // test_slab();
    // test_mm();

    // process_init();
    HPET_init();
    io_mfence();
    HPET_measure_freq();
    io_mfence();
    // current_pcb->preempt_count = 0;
    // kdebug("cpu_get_core_crysral_freq()=%ld", cpu_get_core_crysral_freq());

    process_init();
    // 启用double buffer
    // scm_enable_double_buffer();  // 因为时序问题, 该函数调用被移到 initial_kernel_thread
    io_mfence();
    // fat32_init();
    HPET_enable();

    io_mfence();
    // 系统初始化到此结束，剩下的初始化功能应当放在初始内核线程中执行
    apic_timer_init();
    io_mfence();

    // 这里不能删除，否则在O1会报错
    // while (1)
    //     pause();
}

//操作系统内核从这里开始执行
void Start_Kernel(void)
{

    // 获取multiboot2的信息
    uint64_t mb2_info, mb2_magic;
    __asm__ __volatile__("movq %%r15, %0    \n\t"
                         "movq %%r14, %1  \n\t"
                         "movq %%r13, %2  \n\t"
                         "movq %%r12, %3  \n\t"
                         : "=r"(mb2_info), "=r"(mb2_magic), "=r"(bsp_gdt_size), "=r"(bsp_idt_size)::"memory");
    reload_gdt();
    reload_idt();

    // 重新设置TSS描述符
    set_tss_descriptor(10, (void *)(&initial_tss[0]));

    mb2_info &= 0xffffffff;
    mb2_magic &= 0xffffffff;
    multiboot2_magic = (uint)mb2_magic;
    multiboot2_boot_info_addr = mb2_info + PAGE_OFFSET;
    io_mfence();
    system_initialize();
    io_mfence();

    // idle
    while (1)
    {
        // 如果调用的时候，启用了中断，则hlt。否则认为是bug
        if (get_rflags() & 0x200)
        {
            // kdebug("hlt");
            hlt();
        }
        else
        {
            BUG_ON(1);
            pause();
        }
    }
}
#pragma GCC pop_options