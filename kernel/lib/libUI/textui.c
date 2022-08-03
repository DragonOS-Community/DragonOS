#include "textui.h"

#include "screen_manager.h"
#include "driver/uart/uart.h"
#include <common/string.h>
#include <common/printk.h>

struct scm_ui_framework_t textui_framework;

int textui_install_handler(struct scm_buffer_info_t *buf)
{
    return printk_init(buf);
}

int textui_uninstall_handler(void *args)
{
}

int textui_enable_handler(void *args)
{
}

int textui_disable_handler(void *args)
{
}

int textui_change_handler(struct scm_buffer_info_t *buf)
{
    memcpy((void*)buf->vaddr, (void*)(textui_framework.buf->vaddr), textui_framework.buf->size);
    textui_framework.buf = buf;
    set_pos_VBE_FB_addr((uint*)buf->vaddr);
    return 0;
}

struct scm_ui_framework_operations_t textui_ops =
    {
        .install = &textui_install_handler,
        .uninstall = &textui_uninstall_handler,
        .change = &textui_change_handler,
        .enable = &textui_enable_handler,
        .disable = &textui_disable_handler,
};

/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
int textui_init()
{
    memset(&textui_framework, 0, sizeof(textui_framework));
    io_mfence();
    char name[] = "textUI";
    strcpy(textui_framework.name, name);

    textui_framework.ui_ops = &textui_ops;
    textui_framework.type = SCM_FRAMWORK_TYPE_TEXT;
    uart_send_str(COM1, "12121");
    int retval = scm_register(&textui_framework);
    if (retval != 0)
    {
        uart_send_str(COM1, "text ui init failed");
        while (1)
            pause();
    }
    uart_send_str(COM1, "text ui initialized");
    return 0;
}