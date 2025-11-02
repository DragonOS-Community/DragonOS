/*
 * test_waitid.c - 测试waitid系统调用的功能实现
 *
 * 这个测试程序验证waitid系统调用的各种功能，包括：
 * 1. 等待子进程退出 (WEXITED)
 * 2. 等待子进程停止 (WSTOPPED)
 * 3. 等待子进程继续 (WCONTINUED)
 * 4. 非阻塞模式 (WNOHANG)
 * 5. 只观测不回收模式 (WNOWAIT)
 * 6. 不同进程选择器 (P_ALL, P_PID, P_PGID)
 */

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <signal.h>
#include <string.h>
#include <errno.h>
#include <assert.h>

#define TEST_PASSED 0
#define TEST_FAILED 1

// 打印siginfo_t结构的内容
static void print_siginfo(const siginfo_t *info) {
    printf("  siginfo_t: signo=%d, errno=%d, code=%d, pid=%d, uid=%d, status=%d\n",
           info->si_signo, info->si_errno, info->si_code,
           info->si_pid, info->si_uid, info->si_status);
}

// 测试基本退出功能
static int test_basic_exit(void) {
    printf("测试1: 基本退出功能 (WEXITED)\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return TEST_FAILED;
    }

    if (pid == 0) {
        // 子进程
        sleep(1);
        exit(42);
    }

    // 父进程
    siginfo_t info;
    memset(&info, 0, sizeof(info));

    int ret = waitid(P_PID, (id_t)pid, &info, WEXITED);
    if (ret != 0) {
        perror("waitid");
        return TEST_FAILED;
    }

    printf("  成功等待子进程退出\n");
    print_siginfo(&info);

    if (info.si_signo != SIGCHLD) {
        printf("  错误: si_signo应为SIGCHLD(%d)，实际为%d\n", SIGCHLD, info.si_signo);
        return TEST_FAILED;
    }

    if (info.si_code != CLD_EXITED) {
        printf("  错误: si_code应为CLD_EXITED(%d)，实际为%d\n", CLD_EXITED, info.si_code);
        return TEST_FAILED;
    }

    if (info.si_status != 42) {
        printf("  错误: si_status应为42，实际为%d\n", info.si_status);
        return TEST_FAILED;
    }

    printf("  测试1通过\n\n");
    return TEST_PASSED;
}

// 测试非阻塞模式
static int test_nonblocking(void) {
    printf("测试2: 非阻塞模式 (WNOHANG)\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return TEST_FAILED;
    }

    if (pid == 0) {
        // 子进程，稍后退出
        sleep(2);
        exit(0);
    }

    // 父进程立即检查（非阻塞）
    siginfo_t info;
    memset(&info, 0, sizeof(info));

    int ret = waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG);
    if (ret != 0) {
        perror("waitid");
        return TEST_FAILED;
    }

    if (info.si_signo != 0) {
        printf("  错误: 非阻塞模式下si_signo应为0，实际为%d\n", info.si_signo);
        return TEST_FAILED;
    }

    printf("  非阻塞检查：无事件（正确）\n");

    // 现在阻塞等待
    ret = waitid(P_PID, (id_t)pid, &info, WEXITED);
    if (ret != 0) {
        perror("waitid");
        return TEST_FAILED;
    }

    if (info.si_signo != SIGCHLD || info.si_code != CLD_EXITED) {
        printf("  错误: 阻塞等待失败\n");
        return TEST_FAILED;
    }

    printf("  阻塞等待：成功\n");
    printf("  测试2通过\n\n");
    return TEST_PASSED;
}

// 测试停止和继续功能
static int test_stop_continue(void) {
    printf("测试3: 停止和继续功能 (WSTOPPED, WCONTINUED)\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return TEST_FAILED;
    }

    if (pid == 0) {
        // 子进程
        while (1) {
            printf("  子进程运行中...\n");
            sleep(1);
        }
        exit(0);
    }

    // 父进程
    sleep(1); // 让子进程运行一会儿

    // 停止子进程
    printf("  发送SIGSTOP停止子进程\n");
    if (kill(pid, SIGSTOP) != 0) {
        perror("kill SIGSTOP");
        return TEST_FAILED;
    }

    siginfo_t info;
    memset(&info, 0, sizeof(info));

    // 等待停止事件
    int ret = waitid(P_PID, (id_t)pid, &info, WSTOPPED);
    if (ret != 0) {
        perror("waitid WSTOPPED");
        return TEST_FAILED;
    }

    printf("  成功检测到停止事件\n");
    print_siginfo(&info);

    if (info.si_signo != SIGCHLD || info.si_code != CLD_STOPPED) {
        printf("  错误: 停止事件信息不正确\n");
        return TEST_FAILED;
    }

    // 继续子进程
    printf("  发送SIGCONT继续子进程\n");
    if (kill(pid, SIGCONT) != 0) {
        perror("kill SIGCONT");
        return TEST_FAILED;
    }

    memset(&info, 0, sizeof(info));

    // 等待继续事件
    ret = waitid(P_PID, (id_t)pid, &info, WCONTINUED);
    if (ret != 0) {
        perror("waitid WCONTINUED");
        return TEST_FAILED;
    }

    printf("  成功检测到继续事件\n");
    print_siginfo(&info);

    if (info.si_signo != SIGCHLD || info.si_code != CLD_CONTINUED) {
        printf("  错误: 继续事件信息不正确\n");
        return TEST_FAILED;
    }

    // 终止子进程
    kill(pid, SIGTERM);
    waitid(P_PID, (id_t)pid, &info, WEXITED);

    printf("  测试3通过\n\n");
    return TEST_PASSED;
}

// 测试只观测不回收模式
static int test_nowait(void) {
    printf("测试4: 只观测不回收模式 (WNOWAIT)\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return TEST_FAILED;
    }

    if (pid == 0) {
        // 子进程
        sleep(1);
        exit(99);
    }

    // 父进程
    siginfo_t info;

    // 第一次观测（不回收）
    memset(&info, 0, sizeof(info));
    int ret = waitid(P_PID, (id_t)pid, &info, WEXITED | WNOWAIT);
    if (ret != 0) {
        perror("waitid WNOWAIT 1");
        return TEST_FAILED;
    }

    printf("  第一次观测（不回收）:\n");
    print_siginfo(&info);

    if (info.si_signo != SIGCHLD || info.si_code != CLD_EXITED || info.si_status != 99) {
        printf("  错误: 第一次观测信息不正确\n");
        return TEST_FAILED;
    }

    // 第二次观测（应该还能看到相同信息）
    memset(&info, 0, sizeof(info));
    ret = waitid(P_PID, (id_t)pid, &info, WEXITED | WNOWAIT);
    if (ret != 0) {
        perror("waitid WNOWAIT 2");
        return TEST_FAILED;
    }

    printf("  第二次观测（不回收）:\n");
    print_siginfo(&info);

    if (info.si_signo != SIGCHLD || info.si_code != CLD_EXITED || info.si_status != 99) {
        printf("  错误: 第二次观测信息不正确\n");
        return TEST_FAILED;
    }

    // 最终回收 - 使用waitpid而不是waitid
    int status;
    pid_t reaped = waitpid(pid, &status, 0);
    if (reaped != pid) {
        perror("waitpid final");
        return TEST_FAILED;
    }

    printf("  最终回收 (waitpid): pid=%d, status=%d\n", reaped, WEXITSTATUS(status));

    // 再次检查应该没有事件了
    memset(&info, 0, sizeof(info));
    ret = waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG);
    printf("  回收后检查 - waitid返回: %d, errno=%d\n", ret, errno);
    printf("  回收后检查: ");
    print_siginfo(&info);

    if (ret != 0) {
        // 在某些系统上，回收后waitid可能返回错误而不是无事件
        printf("  注意: 回收后waitid返回错误 (errno=%d)，这是可接受的行为\n", errno);
    } else {
        if (info.si_signo != 0) {
            printf("  错误: 回收后仍有事件\n");
            return TEST_FAILED;
        }
    }

    printf("  测试4通过\n\n");
    return TEST_PASSED;
}

// 测试进程组功能
static int test_process_group(void) {
    printf("测试5: 进程组功能 (P_PGID)\n");

    // 创建新的进程组
    pid_t pgid = getpid();
    if (setpgid(0, 0) != 0) {
        perror("setpgid");
        return TEST_FAILED;
    }

    pid_t pid1 = fork();
    if (pid1 < 0) {
        perror("fork 1");
        return TEST_FAILED;
    }

    if (pid1 == 0) {
        // 子进程1
        setpgid(0, pgid);
        sleep(1);
        exit(1);
    }

    pid_t pid2 = fork();
    if (pid2 < 0) {
        perror("fork 2");
        return TEST_FAILED;
    }

    if (pid2 == 0) {
        // 子进程2
        setpgid(0, pgid);
        sleep(2);
        exit(2);
    }

    // 父进程等待进程组中的子进程
    siginfo_t info;
    int count = 0;

    while (count < 2) {
        memset(&info, 0, sizeof(info));
        int ret = waitid(P_PGID, (id_t)pgid, &info, WEXITED);
        if (ret != 0) {
            perror("waitid P_PGID");
            return TEST_FAILED;
        }

        if (info.si_signo == SIGCHLD && info.si_code == CLD_EXITED) {
            printf("  等待到进程组中的子进程: pid=%d, status=%d\n",
                   info.si_pid, info.si_status);
            count++;
        }
    }

    printf("  成功等待到进程组中的所有子进程\n");
    printf("  测试5通过\n\n");
    return TEST_PASSED;
}

// 测试等待所有子进程
static int test_wait_all(void) {
    printf("测试6: 等待所有子进程 (P_ALL)\n");

    pid_t pids[3];

    for (int i = 0; i < 3; i++) {
        pids[i] = fork();
        if (pids[i] < 0) {
            perror("fork");
            return TEST_FAILED;
        }

        if (pids[i] == 0) {
            // 子进程
            sleep(i + 1);
            exit(100 + i);
        }
    }

    // 父进程等待所有子进程
    siginfo_t info;
    int count = 0;

    while (count < 3) {
        memset(&info, 0, sizeof(info));
        int ret = waitid(P_ALL, 0, &info, WEXITED);
        if (ret != 0) {
            perror("waitid P_ALL");
            return TEST_FAILED;
        }

        if (info.si_signo == SIGCHLD && info.si_code == CLD_EXITED) {
            printf("  等待到子进程: pid=%d, status=%d\n",
                   info.si_pid, info.si_status);
            count++;
        }
    }

    printf("  成功等待到所有子进程\n");
    printf("  测试6通过\n\n");
    return TEST_PASSED;
}

// 测试错误参数
static int test_error_cases(void) {
    printf("测试7: 错误参数测试\n");

    siginfo_t info;

    // 测试无效的which参数
    int ret = waitid((idtype_t)999, 0, &info, WEXITED);
    if (ret == 0) {
        printf("  错误: 无效的which参数应该失败\n");
        return TEST_FAILED;
    }
    printf("  无效which参数正确返回错误 (errno=%d)\n", errno);

    // 测试无效的options（不包含任何事件位）
    ret = waitid(P_ALL, 0, &info, WNOHANG);
    if (ret == 0) {
        printf("  错误: 无效的options参数应该失败\n");
        return TEST_FAILED;
    }
    printf("  无效options参数正确返回错误 (errno=%d)\n", errno);

    // 测试不存在的进程
    ret = waitid(P_PID, 99999, &info, WEXITED | WNOHANG);
    if (ret != 0) {
        // 在某些系统上，不存在的进程可能返回错误而不是无事件
        printf("  注意: 不存在的进程返回错误 (errno=%d)，这是可接受的行为\n", errno);
    } else {
        if (info.si_signo != 0) {
            printf("  错误: 不存在的进程si_signo应该为0\n");
            return TEST_FAILED;
        }
        printf("  不存在的进程正确返回无事件\n");
    }

    printf("  测试7通过\n\n");
    return TEST_PASSED;
}

int main(void) {
    printf("开始测试waitid系统调用功能\n");
    printf("================================\n\n");

    int passed = 0;
    int total = 7;

    // 运行所有测试
    if (test_basic_exit() == TEST_PASSED) passed++;
    if (test_nonblocking() == TEST_PASSED) passed++;
    if (test_stop_continue() == TEST_PASSED) passed++;
    if (test_nowait() == TEST_PASSED) passed++;
    if (test_process_group() == TEST_PASSED) passed++;
    if (test_wait_all() == TEST_PASSED) passed++;
    if (test_error_cases() == TEST_PASSED) passed++;

    printf("================================\n");
    printf("测试完成: %d/%d 通过\n", passed, total);

    if (passed == total) {
        printf("所有测试通过！waitid系统调用功能正常。\n");
        return 0;
    } else {
        printf("部分测试失败，请检查实现。\n");
        return 1;
    }
}