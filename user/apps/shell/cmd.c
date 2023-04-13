#include "cmd.h"
#include "cmd_help.h"
#include "cmd_test.h"
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <libsystem/syscall.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

// 当前工作目录（在main_loop中初始化）
char *shell_current_path = NULL;
/**
 * @brief shell 内建函数的主命令与处理函数的映射表
 *
 */
struct built_in_cmd_t shell_cmds[] = {
    {"cd", shell_cmd_cd},         {"cat", shell_cmd_cat},     {"exec", shell_cmd_exec},   {"ls", shell_cmd_ls},
    {"mkdir", shell_cmd_mkdir},   {"pwd", shell_cmd_pwd},     {"rm", shell_cmd_rm},       {"rmdir", shell_cmd_rmdir},
    {"reboot", shell_cmd_reboot}, {"touch", shell_cmd_touch}, {"about", shell_cmd_about}, {"free", shell_cmd_free},
    {"help", shell_help},         {"pipe", shell_pipe_test},  {"kill", shell_cmd_kill},

};
// 总共的内建命令数量
const static int total_built_in_cmd_num = sizeof(shell_cmds) / sizeof(struct built_in_cmd_t);

/**
 * @brief 将cwd与文件名进行拼接，得到最终的文件绝对路径
 *
 * @param filename 文件名
 * @param result_path_len 结果字符串的大小
 * @return char* 结果字符串
 */
static char *get_target_filepath(const char *filename, int *result_path_len)
{
    char *file_path = NULL;
    if (filename[0] != '/')
    {
        int cwd_len = strlen(shell_current_path);

        // 计算文件完整路径的长度
        *result_path_len = cwd_len + strlen(filename);

        file_path = (char *)malloc(*result_path_len + 2);

        memset(file_path, 0, *result_path_len + 2);

        strncpy(file_path, shell_current_path, cwd_len);

        // 在文件路径中加入斜杠
        if (cwd_len > 1)
            file_path[cwd_len] = '/';

        // 拼接完整路径
        strcat(file_path, filename);
    }
    else
    {
        *result_path_len = strlen(filename);
        file_path = (char *)malloc(*result_path_len + 2);

        memset(file_path, 0, *result_path_len + 2);

        strncpy(file_path, filename, *result_path_len);
        if (filename[(*result_path_len) - 1] != '/')
            file_path[*result_path_len] = '/';
    }

    return file_path;
}

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

int shell_cmd_cd(int argc, char **argv)
{

    int current_dir_len = strlen(shell_current_path);
    if (argc < 2)
    {
        shell_help_cd();
        goto done;
    }
    // 进入当前文件夹
    if (!strcmp(".", argv[1]))
        goto done;

    // 进入父目录
    if (!strcmp("..", argv[1]))
    {

        // 当前已经是根目录
        if (!strcmp("/", shell_current_path))
            goto done;

        // 返回到父目录
        int index = current_dir_len - 1;
        for (; index > 1; --index)
        {
            if (shell_current_path[index] == '/')
                break;
        }
        shell_current_path[index] = '\0';

        // printf("switch to \" %s \"\n", shell_current_path);
        goto done;
    }

    int dest_len = strlen(argv[1]);
    // 路径过长
    if (dest_len >= SHELL_CWD_MAX_SIZE - 1)
    {
        printf("ERROR: Path too long!\n");
        goto fail;
    }

    if (argv[1][0] == '/')
    {
        // ======进入绝对路径=====
        int ec = chdir(argv[1]);
        if (ec == -1)
            ec = errno;
        if (ec == 0)
        {
            // 获取新的路径字符串
            char *new_path = (char *)malloc(dest_len + 2);
            memset(new_path, 0, dest_len + 2);
            strncpy(new_path, argv[1], dest_len);

            // 释放原有的路径字符串的内存空间
            free(shell_current_path);

            shell_current_path = new_path;

            shell_current_path[dest_len] = '\0';
            return 0;
        }
        else
            goto fail;
        ; // 出错则直接忽略
    }
    else // ======进入相对路径=====
    {
        int dest_offset = 0;
        if (dest_len > 2)
        {
            if (argv[1][0] == '.' && argv[1][1] == '/') // 相对路径
                dest_offset = 2;
        }

        int new_len = current_dir_len + dest_len - dest_offset;

        if (new_len >= SHELL_CWD_MAX_SIZE - 1)
        {
            printf("ERROR: Path too long!\n");
            goto fail;
        }

        // 拼接出新的字符串
        char *new_path = (char *)malloc(new_len + 2);
        memset(new_path, 0, sizeof(new_path));
        strncpy(new_path, shell_current_path, current_dir_len);

        if (current_dir_len > 1)
            new_path[current_dir_len] = '/';
        strcat(new_path, argv[1] + dest_offset);
        int x = chdir(new_path);
        if (x == 0) // 成功切换目录
        {
            free(shell_current_path);
            // printf("new_path=%s, newlen= %d\n", new_path, new_len);
            new_path[new_len + 1] = '\0';
            shell_current_path = new_path;
            goto done;
        }
        else
        {
            free(new_path);
            printf("ERROR: Cannot switch to directory: %s\n", new_path);
            goto fail;
        }
    }

fail:;
done:;
    // 释放参数所占的内存
    free(argv);
    return 0;
}

/**
 * @brief 查看文件夹下的文件列表
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_ls(int argc, char **argv)
{
    struct DIR *dir = opendir(shell_current_path);

    if (dir == NULL)
        return -1;

    struct dirent *buf = NULL;
    // printf("dir=%#018lx\n", dir);

    while (1)
    {
        buf = readdir(dir);
        if (buf == NULL)
            break;

        int color = COLOR_WHITE;
        if (buf->d_type == DT_DIR)
            color = COLOR_YELLOW;
        else if (buf->d_type == DT_REG)
            color = COLOR_INDIGO;
        else if (buf->d_type == DT_BLK || buf->d_type == DT_CHR)
            color = COLOR_GREEN;

        char output_buf[256] = {0};

        sprintf(output_buf, "%s   ", buf->d_name);
        put_string(output_buf, color, COLOR_BLACK);
    }
    printf("\n");
    closedir(dir);

    if (argv != NULL)
        free(argv);

    return 0;
}

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
    if (argv != NULL)
        free(argv);
    return 0;
}

/**
 * @brief 查看文件内容的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_cat(int argc, char **argv)
{
    int path_len = 0;
    char *file_path = get_target_filepath(argv[1], &path_len);

    // 打开文件
    int fd = open(file_path, 0);
    if (fd <= 0)
    {
        printf("ERROR: Cannot open file: %s, fd=%d\n", file_path, fd);
        return -1;
    }
    // 获取文件总大小
    int file_size = lseek(fd, 0, SEEK_END);
    // 将文件指针切换回文件起始位置
    lseek(fd, 0, SEEK_SET);

    char *buf = (char *)malloc(512);

    while (file_size > 0)
    {
        memset(buf, 0, 512);
        int l = read(fd, buf, 511);
        if (l < 0)
        {
            printf("ERROR: Cannot read file: %s\n", file_path);
            return -1;
        }
        buf[l] = '\0';

        file_size -= l;
        printf("%s", buf);
    }
    close(fd);
    free(buf);
    free(file_path);
    if (argv != NULL)
        free(argv);
    return 0;
}

/**
 * @brief 创建空文件的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_touch(int argc, char **argv)
{
    int path_len = 0;
    char *file_path;
    bool alloc_full_path = false;
    if (argv[1][0] == '/')
        file_path = argv[1];
    else
    {
        file_path = get_target_filepath(argv[1], &path_len);
        alloc_full_path = true;
    }

    // 打开文件
    int fd = open(file_path, O_CREAT);
    switch (fd)
    {
    case -ENOENT:
        put_string("Parent dir not exists.\n", COLOR_RED, COLOR_BLACK);
        break;

    default:
        break;
    }
    close(fd);
    if (argv != NULL)
        free(argv);
    if (alloc_full_path)
        free(file_path);
    return 0;
}

/**
 * @brief 创建文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_mkdir(int argc, char **argv)
{
    int result_path_len = -1;
    char *full_path = NULL;
    bool alloc_full_path = false;
    if (argv[1][0] == '/')
        full_path = argv[1];
    else
    {
        full_path = get_target_filepath(argv[1], &result_path_len);
        alloc_full_path = true;
    }
    // printf("mkdir: full_path = %s\n", full_path);
    int retval = mkdir(full_path, 0);

    if (argv != NULL)
        free(argv);
    if (alloc_full_path)
        free(full_path);
    return retval;
}

/**
 * @brief 删除文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_rmdir(int argc, char **argv)
{
    char *full_path = NULL;
    int result_path_len = -1;
    bool alloc_full_path = false;

    if (argv[1][0] == '/')
        full_path = argv[1];
    else
    {
        full_path = get_target_filepath(argv[1], &result_path_len);
        alloc_full_path = true;
    }
    int retval = rmdir(full_path);
    if (retval != 0)
        printf("Failed to remove %s, retval=%d\n", full_path, retval);
    // printf("rmdir: path=%s, retval=%d\n", full_path, retval);
    if (argv != NULL)
        free(argv);
    if (alloc_full_path)
        free(full_path);
    return retval;
}

/**
 * @brief 删除文件的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_rm(int argc, char **argv)
{
    char *full_path = NULL;
    int result_path_len = -1;
    int retval = 0;
    bool alloc_full_path = false;

    if (argv[1][0] == '/')
        full_path = argv[1];
    else
    {
        full_path = get_target_filepath(argv[1], &result_path_len);
        alloc_full_path = true;
    }

    retval = rm(full_path);
    // printf("rmdir: path=%s, retval=%d\n", full_path, retval);
    if (retval != 0)
        printf("Failed to remove %s, retval=%d\n", full_path, retval);
    if (alloc_full_path)
        free(full_path);
    if (argv != NULL)
        free(argv);
    return retval;
}

/**
 * @brief 执行新的程序的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_exec(int argc, char **argv)
{
    pid_t pid = fork();
    int retval = 0;
    // printf("  pid=%d  \n",pid);

    if (pid == 0)
    {

        // 子进程
        int path_len = 0;
        char *file_path = get_target_filepath(argv[1], &path_len);
        // printf("before execv, path=%s, argc=%d\n", file_path, argc);
        execv(file_path, argv);
        // printf("after execv, path=%s, argc=%d\n", file_path, argc);
        free(argv);
        free(file_path);

        exit(-1);
    }
    else
    {
        // 如果不指定后台运行,则等待退出
        if (strcmp(argv[argc - 1], "&") != 0)
            waitpid(pid, &retval, 0);
        else
            printf("[1] %d\n", pid); // 输出子进程的pid
        
        free(argv);
    }
}

int shell_cmd_about(int argc, char **argv)
{
    if (argv != NULL)
        free(argv);
    int aac = 0;
    char **aav;

    unsigned char input_buffer[INPUT_BUFFER_SIZE] = {0};

    strcpy(input_buffer, "exec /bin/about.elf\0");

    parse_command(input_buffer, &aac, &aav);

    return shell_cmd_exec(aac, aav);
}

int shell_cmd_kill(int argc, char **argv)
{
    int retval = 0;
    if (argc < 2)
    {
        printf("Usage: Kill <pid>\n");
        retval = -EINVAL;
        goto out;
    }
    retval = kill(atoi(argv[1]), SIGKILL);
out:;
    free(argv);
    return retval;
}

/**
 * @brief 重启命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_reboot(int argc, char **argv)
{
    return syscall_invoke(SYS_REBOOT, 0, 0, 0, 0, 0, 0, 0, 0);
}

int shell_cmd_free(int argc, char **argv)
{
    int retval = 0;
    if (argc == 2 && strcmp("-m", argv[1]) != 0)
    {
        retval = -EINVAL;
        printf("Invalid argument: %s\n", argv[1]);
        goto done;
    }

    struct mstat_t mst = {0};
    retval = mstat(&mst);
    if (retval != 0)
    {
        printf("Failed: retval=%d", retval);
        goto done;
    }

    printf("\ttotal\tused\tfree\tshared\tcache\tavailable\n");
    printf("Mem:\t");
    if (argc == 1) // 按照kb显示
    {
        printf("%ld\t%ld\t%ld\t%ld\t%ld\t%ld\t\n", mst.total >> 10, mst.used >> 10, mst.free >> 10, mst.shared >> 10,
               mst.cache_used >> 10, mst.available >> 10);
    }
    else // 按照MB显示
    {
        printf("%ld\t%ld\t%ld\t%ld\t%ld\t%ld\t\n", mst.total >> 20, mst.used >> 20, mst.free >> 20, mst.shared >> 20,
               mst.cache_used >> 20, mst.available >> 20);
    }

done:;
    if (argv != NULL)
        free(argv);
    return retval;
}

/**
 * @brief 解析shell命令
 *
 * @param buf 输入缓冲区
 * @param argc 返回值：参数数量
 * @param argv 返回值：参数列表
 * @return int 主命令的编号
 */
int parse_command(char *buf, int *argc, char ***argv)
{
    // printf("parse command\n");
    int index = 0; // 当前访问的是buf的第几位
    // 去除命令前导的空格
    while (index < INPUT_BUFFER_SIZE && buf[index] == ' ')
        ++index;

    // 计算参数数量
    for (int i = index; i < (INPUT_BUFFER_SIZE - 1); ++i)
    {
        // 到达了字符串末尾
        if (!buf[i])
            break;
        if (buf[i] != ' ' && (buf[i + 1] == ' ' || buf[i + 1] == '\0'))
            ++(*argc);
    }

    // printf("\nargc=%d\n", *argc);

    // 为指向每个指令的指针分配空间
    *argv = (char **)malloc(sizeof(char **) * (*argc));
    memset(*argv, 0, sizeof(char **) * (*argc));
    // 将每个命令都单独提取出来
    for (int i = 0; i < *argc && index < INPUT_BUFFER_SIZE; ++i)
    {
        // 提取出命令，以空格作为分割
        *((*argv) + i) = &buf[index];
        while (index < (INPUT_BUFFER_SIZE - 1) && buf[index] && buf[index] != ' ')
            ++index;
        buf[index++] = '\0';

        // 删除命令间多余的空格
        while (index < INPUT_BUFFER_SIZE && buf[index] == ' ')
            ++index;

        // printf("%s\n", (*argv)[i]);
    }
    // 以第一个命令作为主命令，查找其在命令表中的编号
    return shell_find_cmd(**argv);
}