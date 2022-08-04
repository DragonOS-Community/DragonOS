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
 * @brief 上层ui框架应当实现的接口
 *
 */
struct scm_ui_framework_operations_t
{
    int (*install)(struct scm_buffer_info_t *buf); // 安装ui框架的回调函数
    int (*uninstall)(void *args);                  // 卸载ui框架的回调函数
    int (*enable)(void *args);                     // 启用ui框架的回调函数
    int (*disable)(void *args);                    // 禁用ui框架的回调函数
    int (*change)(struct scm_buffer_info_t *buf);  // 改变ui框架的帧缓冲区的回调函数
};
struct scm_ui_framework_t
{
    struct List list;
    uint16_t id;
    char name[16];
    uint8_t type;
    struct scm_ui_framework_operations_t *ui_ops;
    struct scm_buffer_info_t *buf;
};

/**
 * @brief 初始化屏幕管理模块
 *
 */
void scm_init();

/**
 * @brief 当内存管理单元被初始化之后，重新处理帧缓冲区问题
 * 
 */
void scm_reinit();

/**
 * @brief 向屏幕管理器注册UI框架（动态获取框架对象结构体）
 *
 * @param name 框架名
 * @param type 类型
 * @param ops 框架操作方法
 * @return int
 */
int scm_register_alloc(const char *name, const uint8_t type, struct scm_ui_framework_operations_t *ops);

/**
 * @brief 向屏幕管理器注册UI框架（静态设置的框架对象）
 *
 * @param ui 框架结构体指针
 * @return int 错误码
 */
int scm_register(struct scm_ui_framework_t *ui);

/**
 * @brief 向屏幕管理器卸载UI框架
 *
 * @param ui ui框架结构体
 * @return int
 */
int scm_unregister(struct scm_ui_framework_t *ui);

/**
 * @brief 向屏幕管理器卸载动态创建的UI框架
 *
 * @param ui ui框架结构体
 * @return int
 */
int scm_unregister_alloc(struct scm_ui_framework_t *ui);

/**
 * @brief 允许动态申请内存
 *
 * @return int
 */
int scm_enable_alloc();

/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
int scm_enable_double_buffer();

/**
 * @brief 启用某个ui框架，将它的帧缓冲区渲染到屏幕上
 *
 * @param ui 要启动的ui框架
 * @return int 返回码
 */
int scm_framework_enable(struct scm_ui_framework_t *ui);