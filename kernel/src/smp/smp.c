#include "smp.h"
#include <common/cpu.h>
#include <common/kprint.h>
#include <common/spinlock.h>
#include <driver/interrupt/apic/apic.h>
#include <exception/gate.h>
#include <mm/slab.h>
#include <process/process.h>

#include <process/preempt.h>
#include <sched/sched.h>
#include <driver/acpi/acpi.h>
#include <driver/interrupt/apic/apic.h>
#include "ipi.h"

static void __smp_kick_cpu_handler(uint64_t irq_num, uint64_t param, struct pt_regs *regs);
static void __smp__flush_tlb_ipi_handler(uint64_t irq_num, uint64_t param, struct pt_regs *regs);

static spinlock_t multi_core_starting_lock = {1}; // 多核启动锁

static struct acpi_Processor_Local_APIC_Structure_t *proc_local_apic_structs[MAX_SUPPORTED_PROCESSOR_NUM];
static uint32_t total_processor_num = 0;
static int current_starting_cpu = 0;

static int num_cpu_started = 1;
extern void rs_smp_init_idle();

// 在head.S中定义的，APU启动时，要加载的页表
// 由于内存管理模块初始化的时候，重置了页表，因此我们要把当前的页表传给APU
extern uint64_t __APU_START_CR3;

// kick cpu 功能所使用的中断向量号
#define KICK_CPU_IRQ_NUM 0xc8
#define FLUSH_TLB_IRQ_NUM 0xc9

void smp_init()
{
    spin_init(&multi_core_starting_lock); // 初始化多核启动锁
    // 设置多核启动时，要加载的页表
    __APU_START_CR3 = (uint64_t)get_CR3();

    ul tmp_vaddr[MAX_SUPPORTED_PROCESSOR_NUM] = {0};

    apic_get_ics(ACPI_ICS_TYPE_PROCESSOR_LOCAL_APIC, tmp_vaddr, &total_processor_num);

    // kdebug("processor num=%d", total_processor_num);
    for (int i = 0; i < total_processor_num; ++i)
    {
        io_mfence();
        proc_local_apic_structs[i] = (struct acpi_Processor_Local_APIC_Structure_t *)(tmp_vaddr[i]);
    }

    // 将引导程序复制到物理地址0x20000处
    memcpy((unsigned char *)phys_2_virt(0x20000), _apu_boot_start,
           (unsigned long)&_apu_boot_end - (unsigned long)&_apu_boot_start);
    io_mfence();
    // 设置多核IPI中断门
    for (int i = 200; i < 210; ++i)
        set_intr_gate(i, 0, SMP_interrupt_table[i - 200]);
    memset((void *)SMP_IPI_desc, 0, sizeof(irq_desc_t) * SMP_IRQ_NUM);

    io_mfence();

    io_mfence();
    ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x00, ICR_INIT, ICR_ALL_EXCLUDE_Self, 0x00);

    kdebug("total_processor_num=%d", total_processor_num);
    // 注册接收kick_cpu功能的处理函数。（向量号200）
    ipi_regiserIPI(KICK_CPU_IRQ_NUM, NULL, &__smp_kick_cpu_handler, NULL, NULL, "IPI kick cpu");
    ipi_regiserIPI(FLUSH_TLB_IRQ_NUM, NULL, &__smp__flush_tlb_ipi_handler, NULL, NULL, "IPI flush tlb");

    int core_to_start = 0;
    // total_processor_num = 3;
    for (int i = 0; i < total_processor_num; ++i) // i从1开始，不初始化bsp
    {
        io_mfence();

        // 跳过BSP
        kdebug("[core %d] acpi processor UID=%d, APIC ID=%d, flags=%#010lx", i,
               proc_local_apic_structs[i]->ACPI_Processor_UID, proc_local_apic_structs[i]->local_apic_id,
               proc_local_apic_structs[i]->flags);
        if (proc_local_apic_structs[i]->local_apic_id == 0)
        {
            // --total_processor_num;
            continue;
        }
        if (!((proc_local_apic_structs[i]->flags & 0x1) || (proc_local_apic_structs[i]->flags & 0x2)))
        {
            // --total_processor_num;
            kdebug("processor %d cannot be enabled.", proc_local_apic_structs[i]->ACPI_Processor_UID);
            continue;
        }
        ++core_to_start;
        // continue;
        io_mfence();
        spin_lock(&multi_core_starting_lock);
        preempt_enable(); // 由于ap处理器的pcb与bsp的不同，因此ap处理器放锁时，bsp的自旋锁持有计数不会发生改变,需要手动恢复preempt
                          // count
        current_starting_cpu = proc_local_apic_structs[i]->ACPI_Processor_UID;
        io_mfence();
        // 为每个AP处理器分配栈空间
        cpu_core_info[current_starting_cpu].stack_start = (uint64_t)kmalloc(STACK_SIZE, 0) + STACK_SIZE;
        cpu_core_info[current_starting_cpu].ist_stack_start = (uint64_t)(kmalloc(STACK_SIZE, 0)) + STACK_SIZE;
        io_mfence();
        memset((void *)cpu_core_info[current_starting_cpu].stack_start - STACK_SIZE, 0, STACK_SIZE);
        memset((void *)cpu_core_info[current_starting_cpu].ist_stack_start - STACK_SIZE, 0, STACK_SIZE);
        io_mfence();

        // 设置ap处理器的中断栈及内核栈中的cpu_id
        ((struct process_control_block *)(cpu_core_info[current_starting_cpu].stack_start - STACK_SIZE))->cpu_id =
            proc_local_apic_structs[i]->local_apic_id;
        ((struct process_control_block *)(cpu_core_info[current_starting_cpu].ist_stack_start - STACK_SIZE))->cpu_id =
            proc_local_apic_structs[i]->local_apic_id;

        cpu_core_info[current_starting_cpu].tss_vaddr = (uint64_t)&initial_tss[current_starting_cpu];

        memset(&initial_tss[current_starting_cpu], 0, sizeof(struct tss_struct));
        // kdebug("core %d, set tss", current_starting_cpu);
        set_tss_descriptor(10 + (current_starting_cpu * 2), (void *)(cpu_core_info[current_starting_cpu].tss_vaddr));
        io_mfence();
        set_tss64(
            (uint *)cpu_core_info[current_starting_cpu].tss_vaddr, cpu_core_info[current_starting_cpu].stack_start,
            cpu_core_info[current_starting_cpu].stack_start, cpu_core_info[current_starting_cpu].stack_start,
            cpu_core_info[current_starting_cpu].ist_stack_start, cpu_core_info[current_starting_cpu].ist_stack_start,
            cpu_core_info[current_starting_cpu].ist_stack_start, cpu_core_info[current_starting_cpu].ist_stack_start,
            cpu_core_info[current_starting_cpu].ist_stack_start, cpu_core_info[current_starting_cpu].ist_stack_start,
            cpu_core_info[current_starting_cpu].ist_stack_start);
        io_mfence();

        // kdebug("core %d, to send start up", current_starting_cpu);
        // 连续发送两次start-up IPI
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x20, ICR_Start_up, ICR_No_Shorthand,
                     proc_local_apic_structs[i]->local_apic_id);
        io_mfence();
        ipi_send_IPI(DEST_PHYSICAL, IDLE, ICR_LEVEL_DE_ASSERT, EDGE_TRIGGER, 0x20, ICR_Start_up, ICR_No_Shorthand,
                     proc_local_apic_structs[i]->local_apic_id);
        // kdebug("core %d, send start up ok", current_starting_cpu);
    }
    io_mfence();
    while (num_cpu_started != (core_to_start + 1))
        pause();

    kinfo("Cleaning page table remapping...\n");

    // 由于ap处理器初始化过程需要用到0x00处的地址，因此初始化完毕后才取消内存地址的重映射
    rs_unmap_at_low_addr();
    kdebug("init proc's preempt_count=%ld", current_pcb->preempt_count);
    kinfo("Successfully cleaned page table remapping!\n");
}

/**
 * @brief AP处理器启动后执行的第一个函数
 *
 */
void smp_ap_start()
{

    //  切换栈基地址
    //  uint64_t stack_start = (uint64_t)kmalloc(STACK_SIZE, 0) + STACK_SIZE;
    __asm__ __volatile__("movq %0, %%rbp \n\t" ::"m"(cpu_core_info[current_starting_cpu].stack_start)
                         : "memory");
    __asm__ __volatile__("movq %0, %%rsp \n\t" ::"m"(cpu_core_info[current_starting_cpu].stack_start)
                         : "memory");

    ksuccess("AP core %d successfully started!", current_starting_cpu);
    io_mfence();
    ++num_cpu_started;

    apic_init_ap_core_local_apic();

    // ============ 为ap处理器初始化IDLE进程 =============
    memset(current_pcb, 0, sizeof(struct process_control_block));

    barrier();
    current_pcb->state = PROC_RUNNING;
    current_pcb->flags = PF_KTHREAD;
    current_pcb->address_space = NULL;
    rs_smp_init_idle();

    list_init(&current_pcb->list);
    current_pcb->addr_limit = KERNEL_BASE_LINEAR_ADDR;
    current_pcb->priority = 2;
    current_pcb->virtual_runtime = 0;

    current_pcb->thread = (struct thread_struct *)(current_pcb + 1); // 将线程结构体放置在pcb后方
    current_pcb->thread->rbp = cpu_core_info[current_starting_cpu].stack_start;
    current_pcb->thread->rsp = cpu_core_info[current_starting_cpu].stack_start;
    current_pcb->thread->fs = KERNEL_DS;
    current_pcb->thread->gs = KERNEL_DS;
    current_pcb->cpu_id = current_starting_cpu;

    initial_proc[proc_current_cpu_id] = current_pcb;
    barrier();
    load_TR(10 + current_starting_cpu * 2);
    current_pcb->preempt_count = 0;

    sched_set_cpu_idle(current_starting_cpu, current_pcb);

    io_mfence();
    spin_unlock(&multi_core_starting_lock);
    preempt_disable(); // 由于ap处理器的pcb与bsp的不同，因此ap处理器放锁时，需要手动恢复preempt count
    io_mfence();
    current_pcb->flags |= PF_NEED_SCHED;

    apic_timer_ap_core_init();
    sti();
    sched();

    while (1)
    {
        // kdebug("123");
        hlt();
    }

    while (1)
    {
        printk_color(BLACK, WHITE, "CPU:%d IDLE process.\n", proc_current_cpu_id);
    }
    while (1) // 这里要循环hlt，原因是当收到中断后，核心会被唤醒，处理完中断之后不会自动hlt
        hlt();
}

/**
 * @brief kick_cpu 核心间通信的处理函数
 *
 * @param irq_num
 * @param param
 * @param regs
 */
static void __smp_kick_cpu_handler(uint64_t irq_num, uint64_t param, struct pt_regs *regs)
{
    if (user_mode(regs))
        return;
    sched();
}

static void __smp__flush_tlb_ipi_handler(uint64_t irq_num, uint64_t param, struct pt_regs *regs)
{
    if (user_mode(regs))
        return;
    flush_tlb();
}

/**
 * @brief 获取当前全部的cpu数目
 *
 * @return uint32_t
 */
uint32_t smp_get_total_cpu()
{
    return num_cpu_started;
}