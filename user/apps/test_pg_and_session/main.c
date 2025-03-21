#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/wait.h>

// 打印进程信息
void print_ids(const char *name) {
    printf("[%s] PID=%d, PPID=%d, PGID=%d, SID=%d\n",
           name,
           getpid(),
           getppid(),
           getpgid(0),  // 获取当前进程的 PGID
           getsid(0));  // 获取当前进程的 SID
}

int main() {
    printf("===== 初始进程信息 =====\n");
    print_ids("Parent");

    // 测试 1: setpgid 创建新进程组
    printf("\n===== 测试 setpgid =====");
    pid_t child1 = fork();
    if (child1 == 0) {
        // 子进程1: 创建新进程组
        printf("\n[Child1] 创建新进程组...\n");
        if (setpgid(0, 0) == -1) {  // 参数 0 表示使用当前 PID 作为新 PGID
            perror("setpgid failed");
            exit(EXIT_FAILURE);
        }
        print_ids("Child1 (new group)");
        exit(EXIT_SUCCESS);
    } else {
        // 父进程等待子进程1结束
        waitpid(child1, NULL, 0);
    }

    // 测试 2: setsid 创建新会话
    printf("\n===== 测试 setsid =====");
    pid_t child2 = fork();
    if (child2 == 0) {
        // 子进程2: 脱离原会话，创建新会话
        printf("\n[Child2] 创建新会话...\n");
        pid_t newsid = setsid();
        if (newsid == -1) {
            perror("setsid failed");
            exit(EXIT_FAILURE);
        }
        printf("New SID = %d\n", newsid);
        print_ids("Child2 (new session)");
        exit(EXIT_SUCCESS);
    } else {
        // 父进程等待子进程2结束
        waitpid(child2, NULL, 0);
    }

    // 测试 3: getpgid/getsid 跨进程验证
    printf("\n===== 跨进程验证 =====");
    pid_t child3 = fork();
    if (child3 == 0) {
        printf("\n[Child3] 修改父进程的 PGID...\n");
        // 尝试修改父进程的 PGID（预期失败，无权限）
        if (setpgid(getppid(), 0) == -1) {
            perror("setpgid(parent) failed (预期错误)");
        }
        print_ids("Child3");
        exit(EXIT_SUCCESS);
    } else {
        waitpid(child3, NULL, 0);
    }

    printf("\n===== 最终父进程信息 =====\n");
    print_ids("Parent");
    return 0;
}