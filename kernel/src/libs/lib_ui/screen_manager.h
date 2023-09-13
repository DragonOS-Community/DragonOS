#pragma once

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
