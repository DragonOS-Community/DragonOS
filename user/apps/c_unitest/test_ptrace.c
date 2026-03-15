/*
# test_ptrace.c测试在Linux下的行为
=== Testing PTRACE_TRACEME ===
Child ready for tracing
Child stopped by signal 19 (Stopped (signal))
Child exited with status 0
=== Testing PTRACE_ATTACH/DETACH ===
target process 100 waiting...
Tracer attaching to target 100
target stopped by signal 19 (Stopped (signal))
Tracer detaching from target
target received 18 (Continued)
target exited with status 0
=== Testing PTRACE_SYSCALL ===
Child initial stop by signal 19 (Stopped (signal))
Syscall entry detected: nr=39
Syscall exit detected: nr=39
Child called getpid()
Child exited normally
=== Testing PTRACE_PEEKDATA ===
Child:  msg_addr=0x49b643, heap_addr=0x23339c80, heap_val=0x66ccff
Parent: msg_addr=0x49b643, heap_addr=0x23339c80
Read message: PTRACE_PEEKDATA_testing
Original heap value: 0x66ccff
Modified heap value: 0xee0000
*/

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

// 根据CPU架构定义系统调用号位置
#if defined(__x86_64__)
#define REG_SYSCALL_NR         orig_rax
#define REG_DATA_PASS          r14
#define ARCH_SET_PASS_REG(val) asm volatile("mov %0, %%r14" : : "r"(val))
#elif defined(__aarch64__)
#define REG_SYSCALL_NR         regs[8]
#define REG_DATA_PASS          regs[19]
#define ARCH_SET_PASS_REG(val) asm volatile("mov x19, %0" : : "r"(val))
#elif defined(__riscv) || defined(__riscv__)
#define REG_SYSCALL_NR         a7
#define REG_DATA_PASS          s1
#define ARCH_SET_PASS_REG(val) asm volatile("mv s1, %0" : : "r"(val))
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

static long peek_word(pid_t pid, const void* addr) {
    errno = 0;
    long data = ptrace(PTRACE_PEEKDATA, pid, addr, 0);
    if (data == -1 && errno != 0) {
        fprintf(stderr, "ptrace(PEEKDATA, %p) failed: %s\n", addr, strerror(errno));
        exit(EXIT_FAILURE);
    }
    return data;
}

static void poke_word(pid_t pid, void* addr, void* data) {
    if (ptrace(PTRACE_POKEDATA, pid, addr, data) == -1) {
        fprintf(stderr, "ptrace(POKEDATA, %p) failed: %s\n", addr, strerror(errno));
        exit(EXIT_FAILURE);
    }
}

static long read_syscall_nr(pid_t pid) {
    struct user_regs_struct regs;
    CHK_SYSCALL(ptrace(PTRACE_GETREGS, pid, NULL, &regs));
    return regs.REG_SYSCALL_NR;
}

static void sigcont_handler(int sig) {
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
        syscall(SYS_getpid);
        printf("Child called getpid()\n");
        exit(EXIT_SUCCESS);
    } else {
        // 等待子进程第一次停止
        int status;
        CHK_SYSCALL(waitpid(child, &status, 0));

        if (!WIFSTOPPED(status)) {
            printf("Child did not stop as expected (status=%d)\n", status);
            return;
        }
        printf("Child initial stop by signal %d (%s)\n", WSTOPSIG(status), strsignal(WSTOPSIG(status)));
        const long expected_syscall = __NR_getpid;
        // 启用系统调用跟踪
        CHK_SYSCALL(ptrace(PTRACE_SYSCALL, child, NULL, NULL));
        // CHK_SYSCALL(ptrace(PTRACE_SETOPTIONS, child, NULL, (void*)PTRACE_O_TRACESYSGOOD));
        // 等待系统调用入口事件
        CHK_SYSCALL(waitpid(child, &status, 0));

        if (WIFSTOPPED(status)) {
            long nr_entry = read_syscall_nr(child);
            printf("Syscall entry detected: nr=%ld%s\n", nr_entry, nr_entry == expected_syscall ? "" : " (unexpected)");
            // 继续执行
            CHK_SYSCALL(ptrace(PTRACE_SYSCALL, child, NULL, NULL));
            // 等待系统调用出口事件
            CHK_SYSCALL(waitpid(child, &status, 0));
            if (WIFSTOPPED(status)) {
                long nr_exit = read_syscall_nr(child);
                printf(
                    "Syscall exit detected: nr=%ld%s\n", nr_exit, nr_exit == expected_syscall ? "" : " (unexpected)");
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
        static const char* message = "PTRACE_PEEKDATA_testing";
        long* heap_data = (long*)malloc(sizeof(long));
        *heap_data = 0x66CCFF;
        // 直接写入共享内存结构
        struct {
            const char* msg;
            long* heap;
        } addr_info = {message, heap_data};
        printf("Child:  msg_addr=%p, heap_addr=%p, heap_val=%#lx\n", addr_info.msg, addr_info.heap, *addr_info.heap);
        CHK_SYSCALL(ptrace(PTRACE_TRACEME, 0, NULL, NULL));
        ARCH_SET_PASS_REG(&addr_info); // 将结构体地址放入寄存器供父进程读取
        raise(SIGSTOP);                // 父进程检查点
        free(heap_data);
        exit(EXIT_SUCCESS); // 不会执行到这里

    } else {
        int status;
        struct user_regs_struct regs;
        CHK_SYSCALL(waitpid(child, &status, 0));
        if (WIFSTOPPED(status)) {
            CHK_SYSCALL(ptrace(PTRACE_GETREGS, child, NULL, &regs));
            uintptr_t addr_info_addr = regs.REG_DATA_PASS;
            struct {
                const char* msg;
                long* heap;
            } addr_info;
            for (size_t i = 0; i < sizeof(addr_info) / sizeof(long); ++i) {
                long* dest = ((long*)&addr_info) + i;
                *dest = peek_word(child, (void*)(addr_info_addr + i * sizeof(long)));
            }
            uintptr_t msg_addr = (uintptr_t)addr_info.msg;
            uintptr_t heap_addr = (uintptr_t)addr_info.heap;
            printf("Parent: msg_addr=%#lx, heap_addr=%#lx\n", msg_addr, heap_addr);
            char buf[32] = {0};
            for (int i = 0; i < 4; i++) {
                long word = peek_word(child, (void*)(msg_addr + i * sizeof(long)));
                memcpy(buf + i * sizeof(long), &word, sizeof(long));
            }
            printf("Read message: %s\n", buf);

            // 读取并修改堆内存
            long heap_value = peek_word(child, (void*)heap_addr);
            printf("Original heap value: %#lx\n", heap_value);
            poke_word(child, (void*)heap_addr, (void*)0xEE0000);
            long new_value = peek_word(child, (void*)heap_addr);
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
