#include <stdio.h>
#include <signal.h>
#include <unistd.h>
#include <stdlib.h>

// 信号处理函数
void handle_signal(int signal)
{
    if (signal == SIGINT)
    {
        printf("Caught SIGINT (Ctrl+C). Exiting gracefully...\n");
        exit(0); // 终止程序
    }
}

int main()
{
    // 注册信号处理函数
    signal(SIGINT, handle_signal);

    // 模拟一个长时间运行的进程
    while (1)
    {
        printf("Running... Press Ctrl+C to stop.\n");
        sleep(5);
    }

    return 0;
}
