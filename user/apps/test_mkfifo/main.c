#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define BUFFER_SIZE 256
#define PIPE_NAME "/bin/fifo"

int main()
{
    pid_t pid;
    int pipe_fd;
    char buffer[BUFFER_SIZE];
    int bytes_read;
    int status;

    // 创建命名管道
    mkfifo(PIPE_NAME, 0666);

    // 创建子进程
    pid = fork();

    if (pid < 0)
    {
        fprintf(stderr, "Fork failed\n");
        return 1;
    }
    else if (pid == 0)
    {
        // 子进程

        // 打开管道以供读取
        pipe_fd = open(PIPE_NAME, O_RDONLY);

        // 从管道中读取数据
        bytes_read = read(pipe_fd, buffer, BUFFER_SIZE);
        if (bytes_read > 0)
        {
            printf("Child process received message: %s\n", buffer);
        }

        // 关闭管道文件描述符
        close(pipe_fd);

        // 删除命名管道
        unlink(PIPE_NAME);

        exit(0);
    }
    else
    {
        // 父进程

        // 打开管道以供写入
        pipe_fd = open(PIPE_NAME, O_WRONLY);

        // 向管道写入数据
        const char *message = "Hello from parent process";
        write(pipe_fd, message, strlen(message) + 1);

        // 关闭管道文件描述符
        close(pipe_fd);

        // 等待子进程结束
        waitpid(pid, &status, 0);

        if (WIFEXITED(status))
        {
            printf("Child process exited with status: %d\n", WEXITSTATUS(status));
        }
    }

    return 0;
}