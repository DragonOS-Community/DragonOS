#pragma once
#include <common/glib.h>
#include <stdbool.h>
#include <libs/libUI/screen_manager.h>

/**
 * @brief 重新初始化显示驱动，需先低级初始化才能高级初始化
 * @param level 初始化等级
 * false -> 低级初始化：不使用double buffer
 * true ->高级初始化：增加double buffer的支持
 * @return int
 */
int video_reinitialize(bool level);

/**
 * @brief 初始化显示驱动
 *
 * @return int
 */
int video_init();

/**
 * @brief 设置帧缓冲区刷新目标
 * 
 * @param buf 
 * @return int 
 */
int video_set_refresh_target(struct scm_buffer_info_t *buf);

extern uint64_t video_refresh_expire_jiffies;
extern uint64_t video_last_refresh_pid;

extern void video_refresh_framebuffer();