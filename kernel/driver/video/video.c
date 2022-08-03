#include "video.h"
#include <mm/mm.h>
#include <common/printk.h>
#include <driver/multiboot2/multiboot2.h>
#include <time/timer.h>
#include <common/kprint.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <common/spinlock.h>
#include <exception/softirq.h>
#include <driver/uart/uart.h>
#include <common/time.h>



uint64_t video_refresh_expire_jiffies = 0;
uint64_t video_last_refresh_pid = -1;

struct scm_buffer_info_t video_frame_buffer_info = {0};
static struct multiboot_tag_framebuffer_info_t __fb_info;
static struct scm_buffer_info_t *video_refresh_target = NULL;

#define REFRESH_INTERVAL 15UL // 启动刷新帧缓冲区任务的时间间隔

ul VBE_FB_phys_addr; // 由bootloader传来的帧缓存区的物理地址

/**
 * @brief VBE帧缓存区的地址重新映射
 * 将帧缓存区映射到地址SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE处
 */
void init_frame_buffer()
{
    kinfo("Re-mapping VBE frame buffer...");

    uint64_t global_CR3 = (uint64_t)get_CR3();

    struct multiboot_tag_framebuffer_info_t info;
    int reserved;

    video_frame_buffer_info.vaddr = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + FRAME_BUFFER_MAPPING_OFFSET;
    mm_map_proc_page_table(global_CR3, true, video_frame_buffer_info.vaddr, __fb_info.framebuffer_addr, video_frame_buffer_info.size, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false, true, false);

    flush_tlb();
    kinfo("VBE frame buffer successfully Re-mapped!");
}

/**
 * @brief 刷新帧缓冲区
 *
 */
void video_refresh_framebuffer(void *data)
{
    video_refresh_expire_jiffies = cal_next_n_ms_jiffies(REFRESH_INTERVAL << 1);
    if (unlikely(video_refresh_target == NULL))
        return;
    memcpy((void *)video_frame_buffer_info.vaddr, (void *)video_refresh_target->vaddr, video_refresh_target->size);
}

/**
 * @brief 初始化显示模块，需先低级初始化才能高级初始化
 * @param level 初始化等级
 * false -> 低级初始化：不使用double buffer
 * true ->高级初始化：增加double buffer的支持
 * @return int
 */
int video_reinitialize(bool level)
{
    if (level == false)
        init_frame_buffer();
    else
    {
        // 启用屏幕刷新软中断
        register_softirq(VIDEO_REFRESH_SIRQ, &video_refresh_framebuffer, NULL);

        video_refresh_expire_jiffies = cal_next_n_ms_jiffies(10 * REFRESH_INTERVAL);

        raise_softirq(VIDEO_REFRESH_SIRQ);
    }
    return 0;
}

/**
 * @brief 设置帧缓冲区刷新目标
 *
 * @param buf
 * @return int
 */
int video_set_refresh_target(struct scm_buffer_info_t *buf)
{

    unregister_softirq(VIDEO_REFRESH_SIRQ);
    int counter = 100;
    while ((get_softirq_pending() & (1 << VIDEO_REFRESH_SIRQ)) && counter > 0)
    {
        --counter;
        usleep(1000);
    }
    video_refresh_target = buf;
    register_softirq(VIDEO_REFRESH_SIRQ, &video_refresh_framebuffer, NULL);
    raise_softirq(VIDEO_REFRESH_SIRQ);
}

/**
 * @brief 初始化显示驱动
 *
 * @return int
 */
int video_init()
{

    memset(&video_frame_buffer_info, 0, sizeof(struct scm_buffer_info_t));
    memset(&__fb_info, 0, sizeof(struct multiboot_tag_framebuffer_info_t));
    video_refresh_target = NULL;

    io_mfence();
    // 从multiboot2获取帧缓冲区信息
    int reserved;
    multiboot2_iter(multiboot2_get_Framebuffer_info, &__fb_info, &reserved);
    io_mfence();

    // 初始化帧缓冲区信息结构体
    if (__fb_info.framebuffer_type == 2)
    {
        video_frame_buffer_info.bit_depth = 8; // type=2时，width和height是按照字符数来表示的，因此depth=8
        video_frame_buffer_info.flags |= SCM_BF_TEXT;
    }
    else
    {
        video_frame_buffer_info.bit_depth = __fb_info.framebuffer_bpp;
        video_frame_buffer_info.flags |= SCM_BF_PIXEL;
    }

    video_frame_buffer_info.flags |= SCM_BF_FB;
    video_frame_buffer_info.width = __fb_info.framebuffer_width;
    video_frame_buffer_info.height = __fb_info.framebuffer_height;
    io_mfence();

    video_frame_buffer_info.size = video_frame_buffer_info.width * video_frame_buffer_info.height * ((video_frame_buffer_info.bit_depth + 7) / 8);
    // 先临时映射到该地址，稍后再重新映射
    video_frame_buffer_info.vaddr = 0xffff800003000000;
    mm_map_phys_addr(video_frame_buffer_info.vaddr, __fb_info.framebuffer_addr, video_frame_buffer_info.size, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);

    io_mfence();
    char init_text2[] = "Video driver initialized.";
    for (int i = 0; i < sizeof(init_text2) - 1; ++i)
        uart_send(COM1, init_text2[i]);
    return 0;
}