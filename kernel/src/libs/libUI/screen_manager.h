#pragma once
#include <common/sys/types.h>
#include <common/glib.h>

// 帧缓冲区标志位
#define SCM_BF_FB (1 << 0)    // 当前buffer是设备显存中的帧缓冲区
#define SCM_BF_DB (1 << 1)    // 当前buffer是双缓冲
#define SCM_BF_TEXT (1 << 2)  // 使用文本模式
#define SCM_BF_PIXEL (1 << 3) // 使用图像模式

// ui框架类型
#define SCM_FRAMWORK_TYPE_TEXT (uint8_t)0
#define SCM_FRAMWORK_TYPE_GUI (uint8_t)1

/**
 * @brief 帧缓冲区信息结构体
 *
 */
struct scm_buffer_info_t
{
    uint32_t width;     // 帧缓冲区宽度（pixel或columns）
    uint32_t height;    // 帧缓冲区高度（pixel或lines）
    uint32_t size;      // 帧缓冲区大小（bytes）
    uint32_t bit_depth; // 像素点位深度

    uint64_t vaddr; // 帧缓冲区的地址
    uint64_t flags; // 帧缓冲区标志位
};

/**
 * @brief 初始化屏幕管理模块
 *
 */
extern void scm_init();

/**
 * @brief 当内存管理单元被初始化之后，重新处理帧缓冲区问题
 *
 */
extern void scm_reinit();

/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
extern int scm_enable_double_buffer();

/**
 * @brief 允许往窗口打印信息
 *
 */
extern void scm_enable_put_to_window();
/**
 * @brief 禁止往窗口打印信息
 *
 */
extern void scm_disable_put_to_window();
