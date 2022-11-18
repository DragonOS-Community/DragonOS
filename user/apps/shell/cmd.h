#pragma once

// cwd字符串的最大大小
#define SHELL_CWD_MAX_SIZE  256
#define INPUT_BUFFER_SIZE 512

/**
 * @brief shell内建命令结构体
 * 
 */
struct built_in_cmd_t
{
    char *name;
    int (*func)(int argc, char **argv);
};

extern struct built_in_cmd_t shell_cmds[];
/**
 * @brief 寻找对应的主命令编号
 *
 * @param main_cmd 主命令
 * @return int 主命令编号
 */
int shell_find_cmd(char *main_cmd);


/**
 * @brief 运行shell内建的命令
 *
 * @param index 主命令编号
 * @param argc 参数数量
 * @param argv 参数列表
 */
void shell_run_built_in_command(int index, int argc, char **argv);

/**
 * @brief cd命令:进入文件夹
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_cd(int argc, char **argv);

/**
 * @brief 查看文件夹下的文件列表
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_ls(int argc, char **argv);

/**
 * @brief 显示当前工作目录的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_pwd(int argc, char **argv);

/**
 * @brief 查看文件内容的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_cat(int argc, char **argv);

/**
 * @brief 创建空文件的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_touch(int argc, char **argv);

/**
 * @brief 删除命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_rm(int argc, char **argv);

/**
 * @brief 创建文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_mkdir(int argc, char **argv);

/**
 * @brief 删除文件夹的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_rmdir(int argc, char **argv);

/**
 * @brief 执行新的程序的命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_exec(int argc, char **argv);

/**
 * @brief 重启命令
 *
 * @param argc
 * @param argv
 * @return int
 */
int shell_cmd_reboot(int argc, char **argv);

/**
 * @brief 关于软件
 * 
 * @param argc 
 * @param argv 
 * @return int 
 */
int shell_cmd_about(int argc, char **argv);

/**
 * @brief 显示系统内存空间信息的命令
 * 
 * @param argc 
 * @param argv 
 * @return int 
 */
int shell_cmd_free(int argc, char **argv);

/**
 * @brief 解析shell命令
 *
 * @param buf 输入缓冲区
 * @param argc 返回值：参数数量
 * @param argv 返回值：参数列表
 * @return int
 */
int parse_command(char *buf, int *argc, char ***argv);