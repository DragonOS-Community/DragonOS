#include "screen_manager.h"
#include <common/kprint.h>
#include <common/spinlock.h>
#include <common/string.h>
#include <driver/multiboot2/multiboot2.h>
#include <driver/uart/uart.h>
#include <driver/video/video.h>
#include <mm/mm.h>
#include <mm/slab.h>

extern struct scm_buffer_info_t video_frame_buffer_info;
static struct List scm_framework_list;
static spinlock_t scm_register_lock;                   // 框架注册锁
static spinlock_t scm_screen_own_lock = {1};           // 改变屏幕归属者时，需要对该锁加锁
static struct scm_ui_framework_t *__current_framework; // 当前拥有屏幕控制权的框架
static uint32_t scm_ui_max_id = 0;
static bool __scm_alloc_enabled = false;         // 允许动态申请内存的标志位
static bool __scm_double_buffer_enabled = false; // 允许双缓冲的标志位
/**
 * @brief 创建新的帧缓冲区
 *
 * @param type 帧缓冲区类型
 * @return struct scm_buffer_info_t* 新的帧缓冲区结构体
 */
static struct scm_buffer_info_t *__create_buffer(uint64_t type)
{
    // 若未启用双缓冲，则直接返回帧缓冲区
    if (unlikely(__scm_double_buffer_enabled == false))
        return &video_frame_buffer_info;

    struct scm_buffer_info_t *buf = (struct scm_buffer_info_t *)kmalloc(sizeof(struct scm_buffer_info_t), 0);
    if (buf == NULL)
        return (void *)-ENOMEM;
    memset(buf, 0, sizeof(struct scm_buffer_info_t));
    buf->bit_depth = video_frame_buffer_info.bit_depth;
    buf->flags = SCM_BF_DB;

    if (type & SCM_BF_PIXEL)
        buf->flags |= SCM_BF_PIXEL;
    else
        buf->flags |= SCM_BF_TEXT;
    buf->height = video_frame_buffer_info.height;
    buf->width = video_frame_buffer_info.width;
    buf->size = video_frame_buffer_info.size;

    struct Page *p = alloc_pages(ZONE_NORMAL, PAGE_2M_ALIGN(video_frame_buffer_info.size) / PAGE_2M_SIZE, 0);
    if (p == NULL)
        goto failed;
    buf->vaddr = (uint64_t)phys_2_virt(p->addr_phys);
    return buf;
failed:;
    kfree(buf);
    return (void *)-ENOMEM;
}

/**
 * @brief 销毁双缓冲区
 *
 * @param buf
 * @return int
 */
static int __destroy_buffer(struct scm_buffer_info_t *buf)
{
    // 不能销毁帧缓冲区对象
    if (unlikely(buf == &video_frame_buffer_info || buf == NULL))
        return -EINVAL;
    if (unlikely(buf->vaddr == NULL))
        return -EINVAL;
    if (unlikely(verify_area(buf->vaddr, buf->size) == true))
        return -EINVAL;
    // 是否双缓冲区
    if (buf->flags & SCM_BF_FB)
        return -EINVAL;

    // 释放内存页
    free_pages(Phy_to_2M_Page(virt_2_phys(buf->vaddr)), PAGE_2M_ALIGN(video_frame_buffer_info.size) / PAGE_2M_SIZE);
    return 0;
}

/**
 * @brief 初始化屏幕管理模块
 *
 */
void scm_init()
{
    list_init(&scm_framework_list);
    spin_init(&scm_register_lock);
    spin_init(&scm_screen_own_lock);
    io_mfence();
    scm_ui_max_id = 0;
    __scm_alloc_enabled = false;         // 禁用动态申请内存
    __scm_double_buffer_enabled = false; // 禁用双缓冲
    __current_framework = NULL;
}
/**
 * @brief 检查ui框架结构体中的参数设置是否合法
 *
 * @param name 框架名称
 * @param type 框架类型
 * @param ops 框架的操作
 * @return int
 */
static int __check_ui_param(const char *name, const uint8_t type, const struct scm_ui_framework_operations_t *ops)
{
    if (name == NULL)
        return -EINVAL;
    if ((type == SCM_FRAMWORK_TYPE_GUI || type == SCM_FRAMWORK_TYPE_TEXT) == 0)
        return -EINVAL;
    if (ops == NULL)
        return -EINVAL;
    if (ops->install == NULL || ops->uninstall == NULL || ops->enable == NULL || ops->disable == NULL ||
        ops->change == NULL)
        return -EINVAL;

    return 0;
}
/**
 * @brief 向屏幕管理器注册UI框架（动态获取框架对象结构体）
 *
 * @param name 框架名
 * @param type 类型
 * @param ops 框架操作方法
 * @return int
 */
int scm_register_alloc(const char *name, const uint8_t type, struct scm_ui_framework_operations_t *ops)
{
    // 若未启用动态申请，则返回。
    if (unlikely(__scm_alloc_enabled == false))
        return -EAGAIN;

    // 检查参数合法性
    if (__check_ui_param(name, type, ops) != 0)
        return -EINVAL;

    struct scm_ui_framework_t *ui = (struct scm_ui_framework_t *)kmalloc(sizeof(struct scm_ui_framework_t *), 0);
    memset(ui, 0, sizeof(struct scm_ui_framework_t));
    strncpy(ui->name, name, 15);
    ui->type = type;
    ui->ui_ops = ops;
    list_init(&ui->list);

    spin_lock(&scm_register_lock);
    ui->id = scm_ui_max_id++;
    spin_unlock(&scm_register_lock);

    // 创建帧缓冲区
    ui->buf = __create_buffer(ui->type);
    if ((uint64_t)(ui->buf) == (uint64_t)-ENOMEM)
    {
        kfree(ui);
        return -ENOMEM;
    }
    // 把ui框架加入链表
    list_add(&scm_framework_list, &ui->list);

    // 调用ui框架的回调函数以安装ui框架，并将其激活
    ui->ui_ops->install(ui->buf);
    ui->ui_ops->enable(NULL);
    if (__current_framework == NULL)
        return scm_framework_enable(ui);
    return 0;
}

/**
 * @brief 向屏幕管理器注册UI框架（静态设置的框架对象）
 *
 * @param ui 框架结构体指针
 * @return int 错误码
 */
int scm_register(struct scm_ui_framework_t *ui)
{
    if (ui == NULL)
        return -EINVAL;
    if (__check_ui_param(ui->name, ui->type, ui->ui_ops) != 0)
        return -EINVAL;

    list_init(&ui->list);
    spin_lock(&scm_register_lock);
    ui->id = scm_ui_max_id++;
    spin_unlock(&scm_register_lock);

    ui->buf = __create_buffer(ui->type);

    if ((uint64_t)(ui->buf) == (uint64_t)-ENOMEM)
        return -ENOMEM;

    // 把ui框架加入链表
    list_add(&scm_framework_list, &ui->list);

    // 调用ui框架的回调函数以安装ui框架，并将其激活
    ui->ui_ops->install(ui->buf);
    ui->ui_ops->enable(NULL);

    if (__current_framework == NULL)
        return scm_framework_enable(ui);

    return 0;
}

/**
 * @brief 向屏幕管理器卸载UI框架
 *
 * @param ui ui框架结构体
 * @return int
 */
int scm_unregister(struct scm_ui_framework_t *ui)
{
    return 0;
}

/**
 * @brief 向屏幕管理器卸载动态创建的UI框架
 *
 * @param ui ui框架结构体
 * @return int
 */
int scm_unregister_alloc(struct scm_ui_framework_t *ui)
{
    return 0;
}

/**
 * @brief 允许动态申请内存
 *
 * @return int
 */
int scm_enable_alloc()
{
    __scm_alloc_enabled = true;
    return 0;
}

/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
int scm_enable_double_buffer()
{
    if (__scm_double_buffer_enabled == true) // 已经开启了双缓冲区了, 直接退出
        return 0;
    __scm_double_buffer_enabled = true;
    if (list_empty(&scm_framework_list)) // scm 框架链表为空
        return 0;

    // 逐个检查已经注册了的ui框架，将其缓冲区更改为双缓冲
    struct scm_ui_framework_t *ptr = container_of(list_next(&scm_framework_list), struct scm_ui_framework_t, list);
    // 这里的ptr不需要特判空指针吗 问题1
    do
    {
        if (ptr->buf == &video_frame_buffer_info)
        {
            uart_send_str(COM1, "##init double buffer##\n");
            struct scm_buffer_info_t *buf = __create_buffer(SCM_BF_DB | SCM_BF_PIXEL);
            if ((uint64_t)(buf) == (uint64_t)-ENOMEM)
                return -ENOMEM;
            uart_send_str(COM1, "##to change double buffer##\n");

            if (ptr->ui_ops->change(buf) != 0) // 这里的change回调函数不会是空指针吗 问题2
            {

                __destroy_buffer(buf);
                kfree(buf);
            }
        }

    } while (list_next(&ptr->list) != &scm_framework_list); // 枚举链表的每一个ui框架

    // 设置定时刷新的对象
    video_set_refresh_target(__current_framework->buf);
    // 通知显示驱动，启动双缓冲
    video_reinitialize(true);
    uart_send_str(COM1, "##initialized double buffer##\n");
    return 0;
}

/**
 * @brief 启用某个ui框架，将它的帧缓冲区渲染到屏幕上
 *
 * @param ui 要启动的ui框架
 * @return int 返回码
 */
int scm_framework_enable(struct scm_ui_framework_t *ui)
{
    if (ui->buf->vaddr == NULL)
        return -EINVAL;
    spin_lock(&scm_screen_own_lock);
    int retval = 0;
    if (__scm_double_buffer_enabled == true)
    {

        retval = video_set_refresh_target(ui->buf);
        if (retval == 0)
            __current_framework = ui;
    }
    else
        __current_framework = ui;

    spin_unlock(&scm_screen_own_lock);
    return retval;
}

/**
 * @brief 当内存管理单元被初始化之后，重新处理帧缓冲区问题
 *
 */
void scm_reinit()
{
    scm_enable_alloc();
    video_reinitialize(false);

    // 遍历当前所有使用帧缓冲区的框架，更新地址
    // 逐个检查已经注册了的ui框架，将其缓冲区更改为双缓冲
    struct scm_ui_framework_t *ptr = container_of(list_next(&scm_framework_list), struct scm_ui_framework_t, list);
    do
    {
        if (ptr->buf == &video_frame_buffer_info)
        {
            ptr->ui_ops->change(&video_frame_buffer_info);
        }
    } while (list_next(&ptr->list) != &scm_framework_list);
    return;
}
