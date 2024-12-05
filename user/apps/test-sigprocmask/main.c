#include <stdio.h>
#include <stdlib.h>
#include <signal.h>
#include <unistd.h>

void signal_handler(int signo) {
    if (signo == SIGINT) {
        printf("\nReceived SIGINT (Ctrl+C)\n");
    }
}

void print_signal_mask(const char *msg, const sigset_t *mask) {
    printf("%s: ", msg);
    for (int signo = 1; signo < NSIG; ++signo) {
        if (sigismember(mask, signo)) {
            printf("%d ", signo);
        }
    }
    printf("\n");
}

int main() {
    sigset_t new_mask, old_mask;
    sigemptyset(&old_mask);

    // 注册 SIGINT 的信号处理函数
    if (signal(SIGINT, signal_handler) == SIG_ERR) {
        perror("signal");
        exit(EXIT_FAILURE);
    }

    // 初始化新的信号集，并将 SIGINT 添加到其中
    sigemptyset(&new_mask);
    sigaddset(&new_mask, SIGINT);

    // 打印 new_mask 的值
    print_signal_mask("new_mask", &new_mask);

    // 屏蔽 SIGINT
    if (sigprocmask(SIG_BLOCK, &new_mask, &old_mask) < 0) {
        perror("sigprocmask - SIG_BLOCK");
        exit(EXIT_FAILURE);
    }

    // 打印 old_mask 的值
    print_signal_mask("old_mask", &old_mask);

    printf("SIGINT is now blocked. Try pressing Ctrl+C...\n");

    // 等待 5 秒，以便测试 SIGINT 是否被屏蔽
    sleep(5);
    printf("\nIf you pressed Ctrl+C, SIGINT was blocked, and no message should have appeared.\n");

    // 恢复原来的信号屏蔽字
    if (sigprocmask(SIG_SETMASK, &old_mask, NULL) < 0) {
        perror("sigprocmask - SIG_SETMASK");
        exit(EXIT_FAILURE);
    }
    printf("SIGINT is now unblocked. Try pressing Ctrl+C again...\n");

    // 等待 5 秒，以便测试 SIGINT 是否解除屏蔽
    sleep(5);

    printf("Exiting program.\n");
    return 0;
}


// #include <signal.h>
// #include <stdio.h>
// #include <stdlib.h>
// #include <unistd.h>

// void print_current_signal_mask(const char *msg) {
//     sigset_t current_mask;

//     // 获取当前线程的信号屏蔽字
//     if (sigprocmask(SIG_SETMASK, NULL, &current_mask) < 0) {
//         perror("sigprocmask - SIG_SETMASK");
//         return;
//     }

//     // 打印信号屏蔽字
//     printf("%s: ", msg);
//     for (int signo = 1; signo < NSIG; ++signo) {
//         if (sigismember(&current_mask, signo)) {
//             printf("%d ", signo);
//         }
//     }
//     printf("\n");
// }

// void signal_handler(int signo) {
//     if (signo == SIGINT) {
//         printf("\nReceived SIGINT (Ctrl+C)\n");
//     }
// }

// void print_signal_mask(const char *msg, const sigset_t *mask) {
//     printf("%s: ", msg);
//     for (int signo = 1; signo < NSIG; ++signo) {
//         if (sigismember(mask, signo)) {
//             printf("%d ", signo);
//         }
//     }
//     printf("\n");
// }

// void check_signal_in_mask(int signo, const char *msg) {
//     sigset_t current_mask;
//     sigprocmask(SIG_SETMASK, NULL, &current_mask);
//     if (sigismember(&current_mask, signo)) {
//         printf("%s: Signal %d is in the mask.\n", msg, signo);
//     } else {
//         printf("%s: Signal %d is not in the mask.\n", msg, signo);
//     }
// }

// int main() {
//     sigset_t new_mask, old_mask;
//     sigemptyset(&old_mask);

//     // 注册 SIGINT 的信号处理函数
//     if (signal(SIGINT, signal_handler) == SIG_ERR) {
//         perror("signal");
//         exit(EXIT_FAILURE);
//     }

//     // 初始化新的信号集，并将 SIGINT 添加到其中
//     sigemptyset(&new_mask);
//     sigaddset(&new_mask, SIGINT);

//     // 打印 new_mask 的值
//     print_signal_mask("new_mask", &new_mask);

//     print_current_signal_mask("Before blocking SIGINT");

//     // 屏蔽 SIGINT
//     if (sigprocmask(SIG_BLOCK, &new_mask, &old_mask) < 0) {
//         perror("sigprocmask - SIG_BLOCK");
//         exit(EXIT_FAILURE);
//     }

//     print_current_signal_mask("Blocking SIGINT");


//     // 检查 SIGINT 是否被屏蔽
//     check_signal_in_mask(SIGINT, "After blocking SIGINT");

//     printf("SIGINT is now blocked. Try pressing Ctrl+C...\n");

//     // 等待 5 秒，以便测试 SIGINT 是否被屏蔽
//     sleep(5);
//     printf("\nIf you pressed Ctrl+C, SIGINT was blocked, and no message should "
//            "have appeared.\n");

//     // 恢复原来的信号屏蔽字
//     if (sigprocmask(SIG_SETMASK, &old_mask, NULL) < 0) {
//         perror("sigprocmask - SIG_SETMASK");
//         exit(EXIT_FAILURE);
//     }

//     // 检查 SIGINT 是否被解除屏蔽
//     check_signal_in_mask(SIGINT, "After unblocking SIGINT");

//     printf("SIGINT is now unblocked. Try pressing Ctrl+C again...\n");

//     // 等待 5 秒，以便测试 SIGINT 是否解除屏蔽
//     sleep(5);

//     printf("Exiting program.\n");
//     return 0;
// }
