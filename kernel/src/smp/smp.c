#include "smp.h"
#include <common/cpu.h>
#include <common/kprint.h>
#include <common/spinlock.h>
#include <mm/slab.h>
#include <process/process.h>

#include <process/preempt.h>
#include <sched/sched.h>
#include <driver/acpi/acpi.h>
#include <arch/arch.h>

/* x86-64 specific MSRs */
#define MSR_EFER 0xc0000080         /* extended feature register */
#define MSR_STAR 0xc0000081         /* legacy mode SYSCALL target */
#define MSR_LSTAR 0xc0000082        /* long mode SYSCALL target */
#define MSR_SYSCALL_MASK 0xc0000084 /* EFLAGS mask for syscall */

static spinlock_t multi_core_starting_lock = {1}; // 多核启动锁

static uint32_t total_processor_num = 0;
static int current_starting_cpu = 0;

int num_cpu_started = 1;

extern void smp_ap_start();
extern uint64_t rs_get_idle_stack_top(uint32_t cpu_id);
extern int rs_ipi_send_smp_startup(uint32_t apic_id);
extern void rs_ipi_send_smp_init();
extern void rs_init_syscall_64();

// 在head.S中定义的，APU启动时，要加载的页表
// 由于内存管理模块初始化的时候，重置了页表，因此我们要把当前的页表传给APU
extern uint64_t __APU_START_CR3;

struct X86CpuInfo
{
    uint32_t apic_id;
    uint32_t core_id;
    char can_boot;
};

extern uint64_t rs_smp_get_cpus(struct X86CpuInfo *res);
static struct X86CpuInfo __cpu_info[MAX_SUPPORTED_PROCESSOR_NUM] = {0};

// kick cpu 功能所使用的中断向量号
#define KICK_CPU_IRQ_NUM 0xc8
#define FLUSH_TLB_IRQ_NUM 0xc9

void smp_init()
{
    spin_init(&multi_core_starting_lock); // 初始化多核启动锁
#if ARCH(I386) || ARCH(X86_64)
    // 设置多核启动时，要加载的页表
    __APU_START_CR3 = (uint64_t)get_CR3();

    // kdebug("processor num=%d", total_processor_num);
    total_processor_num = rs_smp_get_cpus(__cpu_info);

    // 将引导程序复制到物理地址0x20000处
    memcpy((unsigned char *)phys_2_virt(0x20000), _apu_boot_start,
           (unsigned long)&_apu_boot_end - (unsigned long)&_apu_boot_start);
    io_mfence();


    io_mfence();

    io_mfence();
    rs_ipi_send_smp_init();

    kdebug("total_processor_num=%d", total_processor_num);
   
    int core_to_start = 0;
    // total_processor_num = 3;
    for (int i = 0; i < total_processor_num; ++i) // i从1开始，不初始化bsp
    {
        io_mfence();

        // 跳过BSP
        kdebug("[core %d] acpi processor UID=%d, APIC ID=%d, can_boot=%d", i,
               __cpu_info[i].core_id, __cpu_info[i].apic_id,
               __cpu_info[i].can_boot);
        if (__cpu_info[i].apic_id == 0)
        {
            // --total_processor_num;
            continue;
        }
        if (__cpu_info[i].can_boot == false)
        {
            // --total_processor_num;
            kdebug("processor %d cannot be enabled.", __cpu_info[i].core_id);
            continue;
        }
        ++core_to_start;
        // continue;
        io_mfence();
        spin_lock_no_preempt(&multi_core_starting_lock);
        current_starting_cpu = __cpu_info[i].apic_id;
        io_mfence();
        // 为每个AP处理器分配栈空间
        cpu_core_info[current_starting_cpu].stack_start = (uint64_t)rs_get_idle_stack_top(current_starting_cpu);

        io_mfence();

        kdebug("core %d, to send start up", __cpu_info[i].apic_id);
        // 连续发送两次start-up IPI

        int r = rs_ipi_send_smp_startup(__cpu_info[i].apic_id);
        if (r)
        {
            kerror("Failed to send startup ipi to cpu: %d", __cpu_info[i].apic_id);
        }
        io_mfence();
        rs_ipi_send_smp_startup(__cpu_info[i].apic_id);

        io_mfence();
    }
    io_mfence();
    while (num_cpu_started != (core_to_start + 1))
        pause();

    kinfo("Cleaning page table remapping...\n");

    // 由于ap处理器初始化过程需要用到0x00处的地址，因此初始化完毕后才取消内存地址的重映射
    rs_unmap_at_low_addr();
    kinfo("Successfully cleaned page table remapping!\n");
#endif
    io_mfence();
}

/**
 * @brief AP处理器启动后执行的第一个函数
 *
 */
void smp_ap_start_stage2()
{

    ksuccess("AP core %d successfully started!", current_starting_cpu);
    io_mfence();
    ++num_cpu_started;
    io_mfence();
#if ARCH(I386) || ARCH(X86_64)
    rs_apic_init_ap();

    // ============ 为ap处理器初始化IDLE进程 =============

    barrier();

    io_mfence();
    spin_unlock_no_preempt(&multi_core_starting_lock);

    rs_init_syscall_64();
    
    rs_init_current_core_sched();
#endif

    sti();
    sched();

    while (1)
    {
        // kdebug("123");
        hlt();
    }

    while (1)
    {
        printk_color(BLACK, WHITE, "CPU:%d IDLE process.\n", rs_current_cpu_id());
    }
    while (1) // 这里要循环hlt，原因是当收到中断后，核心会被唤醒，处理完中断之后不会自动hlt
        hlt();
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