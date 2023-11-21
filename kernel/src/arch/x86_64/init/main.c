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
#include <libs/lib_ui/screen_manager.h>
#include <libs/lib_ui/textui.h>
#include <sched/sched.h>
#include <smp/ipi.h>

#include <filesystem/vfs/VFS.h>

#include "driver/acpi/acpi.h"
#include "driver/disk/ata.h"
#include "driver/keyboard/ps2_keyboard.h"
#include "driver/mouse/ps2_mouse.h"
#include "driver/multiboot2/multiboot2.h"
#include <time/timer.h>

#include <arch/x86_64/driver/apic/apic_timer.h>
#include <virt/kvm/kvm.h>
#include <debug/bug.h>

extern int rs_driver_init();
extern void rs_softirq_init();
extern void rs_mm_init();
extern void rs_kthread_init();
extern void rs_init_intertrait();
extern void rs_init_before_mem_init();
extern int rs_setup_arch();
extern void rs_futex_init();
extern int rs_hpet_init();
extern int rs_hpet_enable();
extern int rs_tsc_init();
extern void rs_clocksource_boot_finish();
extern void rs_timekeeping_init();
extern void rs_process_init();
extern void rs_textui_init();
extern void rs_pci_init();

ul bsp_idt_size, bsp_gdt_size;

#pragma GCC push_options
#pragma GCC optimize("O0")
struct gdtr gdtp;
struct idtr idtp;
ul _stack_start;
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
  rs_init_before_mem_init();

  _stack_start =
      head_stack_start; // 保存init
                        // proc的栈基地址（由于之后取消了地址重映射，因此必须在这里重新保存）
  kdebug("_stack_start=%#018lx", _stack_start);

  set_current_core_tss(_stack_start, 0);
  rs_load_current_core_tss();

  cpu_core_info[0].stack_start = _stack_start;

  // 初始化中断描述符表
  sys_vector_init();
  //  初始化内存管理单元
  // mm_init();
  rs_mm_init();
  // 内存管理单元初始化完毕后，需要立即重新初始化显示驱动。
  // 原因是，系统启动初期，framebuffer被映射到48M地址处，
  // mm初始化完毕后，若不重新初始化显示驱动，将会导致错误的数据写入内存，从而造成其他模块崩溃
  // 对显示模块进行低级初始化，不启用double buffer

  io_mfence();
  scm_reinit();
  rs_textui_init();

  rs_init_intertrait();
  // kinfo("vaddr:%#018lx", video_frame_buffer_info.vaddr);
  io_mfence();
  vfs_init();

  rs_driver_init();

  acpi_init();

  rs_setup_arch();
  io_mfence();
  irq_init();
  rs_process_init();
  sched_init();

  sti();
  io_mfence();

  rs_softirq_init();

  syscall_init();
  io_mfence();

  rs_timekeeping_init();
  io_mfence();

  rs_timer_init();
  io_mfence();

  rs_jiffies_init();
  io_mfence();

  rs_kthread_init();
  io_mfence();

  io_mfence();
  rs_clocksource_boot_finish();

  io_mfence();

  cpu_init();

  ps2_keyboard_init();
  io_mfence();

  rs_pci_init();

  // 这里必须加内存屏障，否则会出错
  io_mfence();
  smp_init();

  io_mfence();
  rs_futex_init();
  cli();
  rs_hpet_init();
  rs_hpet_enable();
  rs_tsc_init();

  io_mfence();

  kvm_init();

  io_mfence();
  // 系统初始化到此结束，剩下的初始化功能应当放在初始内核线程中执行

  apic_timer_init();
  // while(1);
  io_mfence();
  sti();
  while (1)
    ;
}

// 操作系统内核从这里开始执行
void Start_Kernel(void)
{

  // 获取multiboot2的信息
  uint64_t mb2_info, mb2_magic;
  __asm__ __volatile__("movq %%r15, %0    \n\t"
                       "movq %%r14, %1  \n\t"
                       "movq %%r13, %2  \n\t"
                       "movq %%r12, %3  \n\t"
                       : "=r"(mb2_info), "=r"(mb2_magic), "=r"(bsp_gdt_size),
                         "=r"(bsp_idt_size)::"memory");
  reload_gdt();
  reload_idt();

  mb2_info &= 0xffffffff;
  mb2_magic &= 0xffffffff;
  multiboot2_init(mb2_info, mb2_magic);
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