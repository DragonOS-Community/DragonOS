#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <sys/syscall.h>
#include <errno.h>
#include <string.h>
#include <sys/wait.h>

// tgkill系统调用号定义
#ifndef SYS_tgkill
#define SYS_tgkill 234
#endif

// tgkill函数声明
int tgkill(int tgid, int tid, int sig) {
    return syscall(SYS_tgkill, tgid, tid, sig);
}

// 测试结果统计
static int total_tests = 0;
static int passed_tests = 0;
static int failed_tests = 0;
static char failed_test_names[100][256];
static int failed_test_count = 0;

// 测试辅助宏
#define TEST_ASSERT(condition, test_name) do { \
    total_tests++; \
    if (condition) { \
        passed_tests++; \
        printf("PASS - %s\n", test_name); \
    } else { \
        failed_tests++; \
        if (failed_test_count < 100) { \
            snprintf(failed_test_names[failed_test_count], sizeof(failed_test_names[failed_test_count]), "%s", test_name); \
            failed_test_count++; \
        } \
        printf("FAIL - %s\n", test_name); \
    } \
} while(0)

// 信号处理函数
void signal_handler(int sig) {
    printf("子进程收到信号 %d\n", sig);
}

// 测试tgkill基本功能
void test_tgkill_basic() {
    printf("=== 测试tgkill基本功能 ===\n");
    
    pid_t pid = fork();
    if (pid == 0) {
        // 子进程
        signal(SIGUSR1, signal_handler);
        printf("子进程 PID=%d, TGID=%d 等待信号...\n", getpid(), getpid());
        pause(); // 等待信号
        printf("子进程收到信号，退出\n");
        exit(0);
    } else if (pid > 0) {
        // 父进程
        sleep(1); // 等待子进程设置信号处理
        
        int tgid = pid;
        int tid = pid;
        int sig = SIGUSR1;
        
        printf("父进程发送信号 %d 到 TGID=%d, TID=%d\n", sig, tgid, tid);
        
        int ret = tgkill(tgid, tid, sig);
        TEST_ASSERT(ret == 0, "tgkill基本功能测试");
        
        wait(NULL); // 等待子进程结束
    } else {
        perror("fork failed");
        exit(1);
    }
}

// 测试tgkill参数验证
void test_tgkill_validation() {
    printf("\n=== 测试tgkill参数验证 ===\n");
    
    // 测试无效的tgid
    int ret = tgkill(0, 1, SIGUSR1);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "测试无效tgid (0)");
    
    // 测试无效的tid
    ret = tgkill(1, 0, SIGUSR1);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "测试无效tid (0)");
    
    // 测试不存在的进程
    ret = tgkill(99999, 99999, SIGUSR1);
    TEST_ASSERT(ret == -1 && errno == ESRCH, "测试不存在的进程");
}

// 测试tgkill探测模式 (sig=0)
void test_tgkill_probe() {
    printf("\n=== 测试tgkill探测模式 ===\n");
    
    pid_t pid = fork();
    if (pid == 0) {
        // 子进程
        printf("子进程 PID=%d 运行中...\n", getpid());
        sleep(3); // 运行一段时间
        exit(0);
    } else if (pid > 0) {
        // 父进程
        sleep(1); // 等待子进程启动
        
        int tgid = pid;
        int tid = pid;
        
        int ret = tgkill(tgid, tid, 0); // sig=0 探测模式
        TEST_ASSERT(ret == 0, "探测进程是否存在");
        
        // 等待子进程结束
        wait(NULL);
        
        // 再次探测已结束的进程
        ret = tgkill(tgid, tid, 0);
        TEST_ASSERT(ret == -1 && errno == ESRCH, "探测已结束的进程");
    } else {
        perror("fork failed");
        exit(1);
    }
}

// 测试tgkill线程组归属验证
void test_tgkill_thread_group() {
    printf("\n=== 测试tgkill线程组归属验证 ===\n");
    
    pid_t pid = fork();
    if (pid == 0) {
        // 子进程
        printf("子进程 PID=%d, TGID=%d 运行中...\n", getpid(), getpid());
        sleep(2);
        exit(0);
    } else if (pid > 0) {
        // 父进程
        sleep(1); // 等待子进程启动
        
        int correct_tgid = pid;
        int correct_tid = pid;
        int wrong_tgid = pid + 1; // 错误的TGID
        
        int ret = tgkill(correct_tgid, correct_tid, 0);
        TEST_ASSERT(ret == 0, "测试正确的TGID");
        
        ret = tgkill(wrong_tgid, correct_tid, 0);
        TEST_ASSERT(ret == -1 && errno == ESRCH, "测试错误的TGID");
        
        wait(NULL);
    } else {
        perror("fork failed");
        exit(1);
    }
}

int main() {
    printf("开始tgkill系统调用测试\n");
    printf("当前进程 PID=%d, TGID=%d\n", getpid(), getpid());
    
    test_tgkill_basic();
    test_tgkill_validation();
    test_tgkill_probe();
    test_tgkill_thread_group();
    
    printf("\n=== tgkill测试完成 ===\n");
    printf("\n=== 测试结果总结 ===\n");
    printf("总测试数: %d\n", total_tests);
    printf("通过: %d\n", passed_tests);
    printf("失败: %d\n", failed_tests);
    printf("成功率: %.1f%%\n", total_tests > 0 ? (float)passed_tests / total_tests * 100 : 0);
    
    if (failed_tests > 0) {
        printf("\n失败的测试用例:\n");
        for (int i = 0; i < failed_test_count; i++) {
            printf("  - %s\n", failed_test_names[i]);
        }
    } else {
        printf("\n所有测试用例都通过了！\n");
    }
    
    return failed_tests > 0 ? 1 : 0;
}
