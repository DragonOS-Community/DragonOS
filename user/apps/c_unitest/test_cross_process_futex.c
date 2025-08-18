#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <linux/futex.h>
#include <sys/syscall.h>
#include <time.h>
#include <errno.h>
#include <stdint.h>

#define FUTEX_WAIT_REQUEUE_PI 11
#define FUTEX_CMP_REQUEUE_PI 12

static long futex(uint32_t *uaddr, int futex_op, uint32_t val, const struct timespec *timeout, uint32_t *uaddr2, uint32_t val3) {
    return syscall(SYS_futex, uaddr, futex_op, val, timeout, uaddr2, val3);
}

int main() {
    // 创建共享内存区域
    uint32_t *shared_futex = mmap(NULL, sizeof(uint32_t), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared_futex == MAP_FAILED) {
        perror("mmap");
        exit(1);
    }

    // 初始化futex
    *shared_futex = 0;

    printf("Parent: Setting up robust futex list...\n");
    
    // 设置robust futex列表
    struct robust_list_head {
        struct robust_list *list;
        long futex_offset;
        struct robust_list *list_op_pending;
    };
    
    struct robust_list_head robust_head = {
        .list = (struct robust_list *)shared_futex,
        .futex_offset = 0,
        .list_op_pending = NULL
    };
    
    // 设置当前进程的robust list
    if (syscall(SYS_set_robust_list, &robust_head, sizeof(robust_head)) != 0) {
        perror("set_robust_list");
        exit(1);
    }

    pid_t pid = fork();
    
    if (pid == 0) {
        // 子进程
        printf("Child: Waiting for futex...\n");
        
        // 子进程也设置自己的robust list
        struct robust_list_head child_robust_head = {
            .list = (struct robust_list *)shared_futex,
            .futex_offset = 0,
            .list_op_pending = NULL
        };
        
        if (syscall(SYS_set_robust_list, &child_robust_head, sizeof(child_robust_head)) != 0) {
            perror("child set_robust_list");
            exit(1);
        }
        
        // 锁定futex
        __sync_lock_test_and_set(shared_futex, getpid());
        
        printf("Child: Acquired futex, sleeping for 1 second...\n");
        sleep(1);
        
        // 释放futex
        __sync_lock_release(shared_futex);
        
        printf("Child: Released futex, exiting...\n");
        exit(0);
    } else if (pid > 0) {
        // 父进程
        printf("Parent: Child PID = %d\n", pid);
        
        // 等待子进程
        int status;
        waitpid(pid, &status, 0);
        
        printf("Parent: Child exited, cleaning up...\n");
        
        // 尝试访问futex（这可能会触发VMA not mapped错误）
        printf("Parent: Futex value = %u\n", *shared_futex);
        
        // 清理
        munmap(shared_futex, sizeof(uint32_t));
        
        printf("Parent: Done\n");
    } else {
        perror("fork");
        exit(1);
    }
    
    return 0;
}