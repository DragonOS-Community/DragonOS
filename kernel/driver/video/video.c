#include "video.h"
#include <mm/mm.h>
#include <common/printk.h>
#include <driver/multiboot2/multiboot2.h>
#include <driver/timers/timer.h>
#include <common/kprint.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <process/spinlock.h>
// 每个时刻只能有1个进程新增定时任务
spinlock_t video_timer_func_add_lock;

#define REFRESH_INTERVAL 15 // 启动刷新帧缓冲区任务的时间间隔

ul VBE_FB_phys_addr; // 由bootloader传来的帧缓存区的物理地址
struct screen_info_t
{
    int width, height;
    uint64_t length;
    uint64_t fb_vaddr, fb_paddr;
    uint64_t double_fb_vaddr;
} sc_info;

/**
 * @brief VBE帧缓存区的地址重新映射
 * 将帧缓存区映射到地址SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE处
 */
void init_frame_buffer(bool level)
{
    kinfo("Re-mapping VBE frame buffer...");

    uint64_t global_CR3 = (uint64_t)get_CR3();

    if (level == false)
    {
        struct multiboot_tag_framebuffer_info_t info;
        int reserved;

        multiboot2_iter(multiboot2_get_Framebuffer_info, &info, &reserved);

        sc_info.fb_vaddr = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + FRAME_BUFFER_MAPPING_OFFSET;

        sc_info.fb_paddr = info.framebuffer_addr;
        sc_info.width = info.framebuffer_width;
        sc_info.height = info.framebuffer_height;

        sc_info.length = 1UL * sc_info.width * sc_info.height;
        mm_map_proc_page_table(global_CR3, true, sc_info.fb_vaddr, sc_info.fb_paddr, get_VBE_FB_length() << 2, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false, true);
        set_pos_VBE_FB_addr((uint *)sc_info.fb_vaddr);
    }
    else // 高级初始化，增加双缓冲区的支持
    {
        // 申请双重缓冲区
        struct Page *p = alloc_pages(ZONE_NORMAL, PAGE_2M_ALIGN(sc_info.length << 2) / PAGE_2M_SIZE, 0);
        sc_info.double_fb_vaddr = (uint64_t)phys_2_virt(p->addr_phys);
        mm_map_proc_page_table(global_CR3, true, sc_info.double_fb_vaddr, p->addr_phys, PAGE_2M_ALIGN(sc_info.length << 2), PAGE_KERNEL_PAGE, false, true);

        // 将原有的数据拷贝到double buffer里面
        memcpy((void *)sc_info.double_fb_vaddr, (void *)sc_info.fb_vaddr, sc_info.length << 2);
        set_pos_VBE_FB_addr((uint *)sc_info.double_fb_vaddr);
    }

    flush_tlb();
    kinfo("VBE frame buffer successfully Re-mapped!");
}

/**
 * @brief 刷新帧缓冲区
 *
 */
static void video_refresh_framebuffer()
{

    // kdebug("pid%d flush fb", current_pcb->pid);

    memcpy((void *)sc_info.fb_vaddr, (void *)sc_info.double_fb_vaddr, (sc_info.length << 2));

    // 新增下一个刷新定时任务
    struct timer_func_list_t *tmp = (struct timer_func_list_t *)kmalloc(sizeof(struct timer_func_list_t), 0);
    spin_lock(&video_timer_func_add_lock);
    timer_func_init(tmp, &video_refresh_framebuffer, NULL, 10 * REFRESH_INTERVAL);
    timer_func_add(tmp);
    spin_unlock(&video_timer_func_add_lock);
}

/**
 * @brief 初始化显示模块，需先低级初始化才能高级初始化
 * @param level 初始化等级
 * false -> 低级初始化：不使用double buffer
 * true ->高级初始化：增加double buffer的支持
 * @return int
 */
int video_init(bool level)
{
    init_frame_buffer(level);
    if (level)
    {
        spin_init(&video_timer_func_add_lock);
        // 启用双缓冲后，使能printk滚动动画
        // printk_enable_animation();
        // 初始化第一个屏幕刷新任务
        struct timer_func_list_t *tmp = (struct timer_func_list_t *)kmalloc(sizeof(struct timer_func_list_t), 0);
        timer_func_init(tmp, &video_refresh_framebuffer, NULL, REFRESH_INTERVAL);
        timer_func_add(tmp);
    }
}