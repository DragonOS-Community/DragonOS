#include "cmd_test.h"
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>

#define buf_SIZE 256 // 定义消息的最大长度
int shell_pipe_test(int argc, char **argv)
{
    int fd[2], i, n;

    pid_t pid;
    int ret = pipe(fd); // 创建一个管道
    if (ret < 0)
    {
        printf("pipe error");
        exit(1);
    }
    pid = fork(); // 创建一个子进程
    if (pid < 0)
    {
        printf("fork error");
        exit(1);
    }
    if (pid == 0)
    {                 // 子进程
        close(fd[1]); // 关闭管道的写端
        for (i = 0; i < 3; i++)
        { // 循环三次
            char buf[buf_SIZE] = {0};
            n = read(fd[0], buf, buf_SIZE); // 从管道的读端读取一条消息
            if (n > 0)
            {

                printf("Child process received message: %s\n", buf); // 打印收到的消息
                if (strcmp(buf, "quit") == 0)
                {                                     // 如果收到的消息是"quit"
                    printf("Child process exits.\n"); // 打印退出信息
                    break;                            // 跳出循环
                }
                else
                {                                                    // 如果收到的消息不是"quit"
                    printf("Child process is doing something...\n"); // 模拟子进程做一些操作
                    usleep(100);
                }
            }
        }
        close(fd[0]); // 关闭管道的读端
        exit(0);
    }
    else
    {                 // 父进程
        close(fd[0]); // 关闭管道的读端
        for (i = 0; i < 3; i++)
        { // 循环三次
            char *msg = "hello world";
            if (i == 1)
            {
                msg = "how are you";
                usleep(1000);
            }
            if (i == 2)
            {
                msg = "quit";
                usleep(1000);
            }
            n = strlen(msg);
            printf("Parent process send:%s\n", msg);

            write(fd[1], msg, n); // 向管道的写端写入一条消息
            if (strcmp(msg, "quit") == 0)
            {                                      // 如果发送的消息是"quit"
                printf("Parent process exits.\n"); // 打印退出信息
                break;                             // 跳出循环
            }
        }
        close(fd[1]); // 关闭管道的写端
        wait(NULL);   // 等待子进程结束
    }
    return 0;
}
int shell_pipe2_test(int argc, char **argv)
{
    int fd[2], i, n;

    pid_t pid;
    int ret = pipe2(fd, O_NONBLOCK); // 创建一个管道
    if (ret < 0)
    {
        printf("pipe error\n");
        exit(1);
    }
    pid = fork(); // 创建一个子进程
    if (pid < 0)
    {
        printf("fork error\n");
        exit(1);
    }
    if (pid == 0)
    {                 // 子进程
        close(fd[1]); // 关闭管道的写端
        for (i = 0; i < 10; i++)
        {
            char buf[buf_SIZE] = {0};
            n = read(fd[0], buf, buf_SIZE); // 从管道的读端读取一条消息
            if (n > 0)
            {

                printf("Child process received message: %s\n", buf); // 打印收到的消息
                if (strcmp(buf, "quit") == 0)
                {                                     // 如果收到的消息是"quit"
                    printf("Child process exits.\n"); // 打印退出信息
                    break;                            // 跳出循环
                }
                else
                {                                                    // 如果收到的消息不是"quit"
                    printf("Child process is doing something...\n"); // 模拟子进程做一些操作
                    // usleep(1000);
                }
            }
            else
            {
                printf("read error,buf is empty\n");
            }
        }
        close(fd[0]); // 关闭管道的读端
        exit(0);
    }
    else
    {                 // 父进程
        close(fd[0]); // 关闭管道的读端
        for (i = 0; i < 100; i++)
        {
            char *msg = "hello world";
            if (i < 99 & i > 0)
            {
                msg = "how are you";
                // usleep(1000);
            }
            if (i == 99)
            {
                msg = "quit";
                // usleep(1000);
            }
            n = strlen(msg);
            printf("Parent process send:%s\n", msg);

            int r = write(fd[1], msg, n); // 向管道的写端写入一条消息
            if (r < 0)
            {
                printf("write error,buf is full\n");
            }
            if (strcmp(msg, "quit") == 0)
            {                                      // 如果发送的消息是"quit"
                printf("Parent process exits.\n"); // 打印退出信息
                break;                             // 跳出循环
            }
        }
        close(fd[1]); // 关闭管道的写端
        wait(NULL);   // 等待子进程结束
    }
    return 0;
}
