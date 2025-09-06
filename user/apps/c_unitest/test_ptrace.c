#include <stdint.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/types.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

// 根据CPU架构定义系统调用号位置
#if defined(__x86_64__) || defined(_M_X64)
#define ORIG_RAX 15 // ORIG_RAX在user_regs_struct中的偏移

#elif defined(__aarch64__) || defined(_M_ARM64)
#define ORIG_RAX 8 // ARM64上系统调用号在regs[8]

#elif defined(__riscv) || defined(__riscv__)
#define ORIG_RAX 0 // RISC-V上系统调用号在a7寄存器

#else
#error "Unsupported architecture for PTRACE_SYSCALL test"
#endif

#define CHK_SYSCALL(call)                                                                                              \
    do {                                                                                                               \
        if ((call) == -1) {                                                                                            \
            fprintf(stderr, "Error at %s:%d: %s failed: %s\n", __FILE__, __LINE__, #call, strerror(errno));            \
            exit(EXIT_FAILURE);                                                                                        \
        }                                                                                                              \
    } while (0)

void sigcont_handler(int sig) {
    printf("target received %d (%s)\n", sig, strsignal(sig));
    exit(EXIT_SUCCESS);
}

// 测试 PTRACE_TRACEME 功能
void test_trace_me() {
    printf("=== Testing PTRACE_TRACEME ===\n");
    pid_t child = fork();
    if (child == 0) {
        // 子进程请求被跟踪
        CHK_SYSCALL(ptrace(PTRACE_TRACEME, 0, NULL, NULL));
        // 强制产生一个信号/系统调用事件
        printf("Child ready for tracing\n");
        getpid();
        raise(SIGSTOP);
        // 正常退出
        exit(EXIT_SUCCESS);
    } else {
        // 等待子进程停止
        int status;
        CHK_SYSCALL(waitpid(child, &status, 0));
        if (WIFSTOPPED(status)) {
            int sig = WSTOPSIG(status);
            printf("Child stopped by signal %d (%s)\n", sig, strsignal(sig));
            // // 获取停止原因
            // long request = ptrace(PTRACE_PEEKUSER, child, (void*)ORIG_RAX, NULL);
            // printf("System call: %ld\n", request);
            // 恢复子进程执行
            CHK_SYSCALL(ptrace(PTRACE_CONT, child, NULL, NULL));
            // 等待子进程退出
            CHK_SYSCALL(waitpid(child, &status, 0));

            if (WIFEXITED(status)) {
                printf("Child exited with status %d\n", WEXITSTATUS(status));
            } else {
                printf("Child did not exit normally (status=%d)\n", status);
            }
        } else if (WIFEXITED(status)) {
            printf("Child exited without stopping (status=%d)\n", WEXITSTATUS(status));
        } else {
            printf("Child did not stop as expected (status=%d)\n", status);
        }
    }
}

// 测试 PTRACE_ATTACH/DETACH 功能
void test_attach_detach() {
    printf("=== Testing PTRACE_ATTACH/DETACH ===\n");
    pid_t target = fork();
    if (target == 0) {
        // 目标进程暂停自己
        printf("target process %d waiting...\n", getpid());
        // 确保分离后有信号处理
        if (signal(SIGCONT, sigcont_handler) == SIG_ERR) {
            perror("Error setting SIGCONT handler");
            exit(EXIT_FAILURE);
        }
        sleep(10);
        // pause(); // 等待信号
        // 永远不会到达这里
        printf("target process resumed\n");
        exit(EXIT_FAILURE);
    } else {
        // 给目标进程时间进入pause状态
        sleep(1);
        printf("Tracer attaching to target %d\n", target);
        // 父进程附加到目标进程
        CHK_SYSCALL(ptrace(PTRACE_ATTACH, target, NULL, NULL));
        // 等待目标进程停止
        int status;
        CHK_SYSCALL(waitpid(target, &status, 0));

        if (WIFSTOPPED(status)) {
            int sig = WSTOPSIG(status);
            printf("target stopped by signal %d (%s)\n", sig, strsignal(sig));
            // 分离目标进程并发送SIGCONT唤醒它
            printf("Tracer detaching from target\n");
            CHK_SYSCALL(ptrace(PTRACE_DETACH, target, NULL, (void*)(long)SIGCONT));
            // 等待目标进程退出
            CHK_SYSCALL(waitpid(target, &status, 0));
            if (WIFEXITED(status)) {
                printf("target exited with status %d\n", WEXITSTATUS(status));
            } else {
                printf("target did not exit normally (status=%d)\n", status);
            }
        } else {
            printf("target did not stop as expected (status=%d)\n", status);
        }
    }
}

// 测试 PTRACE_SYSCALL 功能
void test_syscall_tracing() {
    printf("=== Testing PTRACE_SYSCALL ===\n");
    pid_t child = fork();
    if (child == 0) {
        // 子进程请求被跟踪
        CHK_SYSCALL(ptrace(PTRACE_TRACEME, 0, NULL, NULL));
        // 触发系统调用
        raise(SIGSTOP);
        printf("Child calling getpid()\n");
        getpid();
        exit(EXIT_SUCCESS);
    } else {
        // 等待子进程第一次停止
        int status;
        CHK_SYSCALL(waitpid(child, &status, 0));

        if (!WIFSTOPPED(status)) {
            printf("Child did not stop as expected (status=%d)\n", status);
            return;
        }
        printf("Child initial stop by signal %d\n", WSTOPSIG(status));
        // 启用系统调用跟踪
        CHK_SYSCALL(ptrace(PTRACE_SYSCALL, child, NULL, NULL));
        // 等待系统调用入口事件
        CHK_SYSCALL(waitpid(child, &status, 0));

        if (WIFSTOPPED(status)) {
            printf("Syscall entry detected\n");
            // 继续执行
            CHK_SYSCALL(ptrace(PTRACE_SYSCALL, child, NULL, NULL));
            // 等待系统调用出口事件
            CHK_SYSCALL(waitpid(child, &status, 0));
            if (WIFSTOPPED(status)) {
                printf("Syscall exit detected\n");
            }
        }

        // 恢复子进程执行
        CHK_SYSCALL(ptrace(PTRACE_CONT, child, NULL, NULL));
        // 等待子进程退出
        CHK_SYSCALL(waitpid(child, &status, 0));
        if (WIFEXITED(status)) {
            printf("Child exited normally\n");
        }
    }
}

// 测试内存读取功能
void test_peek_data() {
    printf("=== Testing PTRACE_PEEKDATA ===\n");
    pid_t child = fork();
    if (child == 0) {
        const char* message = "PTRACE_PEEKDATA_testing";
        long* heap_data = (long*)malloc(sizeof(long));
        *heap_data = 0x66CCFF;
        // 直接写入共享内存结构
        struct {
            const char* msg;
            long* heap;
        } addr_info = {message, heap_data};
        asm volatile("mov %0, %%r14" : : "r"(&addr_info));
        printf("Child:  msg_addr=%p, heap_addr=%p, heap_val=%#lx\n", addr_info.msg, addr_info.heap, *addr_info.heap);
        CHK_SYSCALL(ptrace(PTRACE_TRACEME, 0, NULL, NULL));
        raise(SIGSTOP); // 父进程检查点
        pause();
        exit(EXIT_SUCCESS); // 不会执行

    } else {
        int status;
        struct user_regs_struct regs;
        CHK_SYSCALL(waitpid(child, &status, 0));
        if (WIFSTOPPED(status)) {
            CHK_SYSCALL(ptrace(PTRACE_GETREGS, child, NULL, &regs));
            uintptr_t addr_info_addr = regs.r14;
            struct {
                const char* msg;
                long* heap;
            } addr_info;
            for (size_t i = 0; i < sizeof(addr_info) / sizeof(long); ++i) {
                long* dest = ((long*)&addr_info) + i;
                *dest = ptrace(PTRACE_PEEKDATA, child, (void*)(addr_info_addr + i * sizeof(long)), 0);
            }
            uintptr_t msg_addr = (uintptr_t)addr_info.msg;
            uintptr_t heap_addr = (uintptr_t)addr_info.heap;
            printf("Parent: msg_addr=%#lx, heap_addr=%#lx\n", msg_addr, heap_addr);
            printf("Read message: ");
            int bytes_printed = 0;
            for (const char* p = (const char*)msg_addr;; p++) {
                long data = ptrace(PTRACE_PEEKDATA, child, p, 0);
                if (data == -1) {
                    perror("Error reading char");
                    break;
                }
                char c = (char)(data & 0xFF);
                if (c == '\0') {
                    break;
                }
                if (++bytes_printed > 128) {
                    printf("... (truncated)");
                    break;
                }
                if (c >= ' ' && c <= '~') { // 可打印字符
                    putchar(c);
                } else if (c == '\n') { // 特殊字符转义
                    fputs("\\n", stdout);
                } else if (c == '\t') {
                    fputs("\\t", stdout);
                } else { // 不可打印字符
                    printf("\\x%02x", (unsigned char)c);
                }
            }
            printf("\n");

            // 读取并修改堆内存
            long heap_value = ptrace(PTRACE_PEEKDATA, child, (void*)heap_addr, 0);
            printf("Original heap value: %#lx\n", heap_value);
            ptrace(PTRACE_POKEDATA, child, (void*)heap_addr, (void*)0xEE0000);
            long new_value = ptrace(PTRACE_PEEKDATA, child, (void*)heap_addr, 0);
            printf("Modified heap value: %#lx\n", new_value);
        }
        // 结束子进程
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
    }
}

int main() {
    printf("===== Starting ptrace tests =====\n\n");

    test_trace_me();
    test_attach_detach();
    test_syscall_tracing();
    test_peek_data();

    printf("\n===== All ptrace tests completed =====\n");
    return EXIT_SUCCESS;
}