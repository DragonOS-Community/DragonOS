#include "cmd.h"
#include <libc/string.h>
#include <libc/stdio.h>
#include <libc/stddef.h>
#include <libsystem/syscall.h>

// 当前工作目录（在main_loop中初始化）
char *shell_current_path = NULL;
/**
 * @brief shell 内建函数的主命令与处理函数的映射表
 *
 */
struct built_in_cmd_t shell_cmds[] =
    {
        {"cd", shell_cmd_cd},
        {"cat", shell_cmd_cat},
        {"exec", shell_cmd_exec},
        {"ls", shell_cmd_ls},
        {"mkdir", shell_cmd_mkdir},
        {"pwd", shell_cmd_pwd},
        {"rm", shell_cmd_rm},
        {"rmdir", shell_cmd_rmdir},
        {"reboot", shell_cmd_reboot},
        {"touch", shell_cmd_touch},

};
// 总共的内建命令数量
const static int total_built_in_cmd_num = sizeof(shell_cmds) / sizeof(struct built_in_cmd_t);

/**
 * @brief 寻找对应的主命令编号
 *
 * @param main_cmd 主命令
 * @return int 成功：主命令编号
 *              失败： -1
 */
int shell_find_cmd(char *main_cmd)
{

    for (int i = 0; i < total_built_in_cmd_num; ++i)
    {
        if (strcmp(main_cmd, shell_cmds[i].name) == 0) // 找到对应的命令号
            return i;
    }
    // 找不到该命令
    return -1;
}

/**
 * @brief 运行shell内建的命令
 *
 * @param index 主命令编号
 * @param argc 参数数量
 * @param argv 参数列表
 */
void shell_run_built_in_command(int index, int argc, char **argv)
{
    if (index >= total_built_in_cmd_num)
        return;
    // printf("run built-in command : %s\n", shell_cmds[index].name);

    shell_cmds[index].func(argc, argv);
}

/**
 * @brief cd命令:进入文件夹
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_cd(int argc, char **argv) {}

/**
 * @brief 查看文件夹下的文件列表
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_ls(int argc, char **argv) {}

/**
 * @brief 显示当前工作目录的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_pwd(int argc, char **argv)
{
    if (shell_current_path)
        printf("%s\n", shell_current_path);
}

/**
 * @brief 查看文件内容的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_cat(int argc, char **argv) {}

/**
 * @brief 创建空文件的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_touch(int argc, char **argv) {}

/**
 * @brief 删除命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_rm(int argc, char **argv) {}

/**
 * @brief 创建文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_mkdir(int argc, char **argv) {}

/**
 * @brief 删除文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_rmdir(int argc, char **argv) {}

/**
 * @brief 执行新的程序的命令
 *
 * @param argc
 * @param argv
 * @return int
 */

// todo:
int shell_cmd_exec(int argc, char **argv) {}

/**
 * @brief 重启命令
 *
 * @param argc
 * @param argv
 * @return int
 */
// todo:
int shell_cmd_reboot(int argc, char **argv)
{
    return syscall_invoke(SYS_REBOOT, 0, 0, 0, 0, 0, 0, 0, 0);
}