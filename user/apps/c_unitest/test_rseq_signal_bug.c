/**
 * Test to demonstrate the rseq+signal reentrancy bug
 *
 * This test demonstrates the bug where:
 * 1. Register rseq successfully
 * 2. Set rseq_cs to invalid memory (0xdeadbeefdeadbeef)
 * 3. Send signal to self
 * 4. Kernel's error handling sends SIGSEGV while already in signal handler
 * 5. Process crashes with "Segmentation fault"
 */

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <sys/syscall.h>
#include <errno.h>
#include <string.h>
#include <stdint.h>

#ifndef SYS_rseq
#define SYS_rseq 334
#endif

#define RSEQ_FLAG_UNREGISTER 1

struct rseq {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t padding[3];
};

static struct rseq g_rseq __attribute__((aligned(32))) = {0};

static int rseq_register(struct rseq *rseq, uint32_t len, uint32_t sig, int flags)
{
    return syscall(SYS_rseq, rseq, len, flags, sig);
}

static volatile int signal_handled = 0;

static void signal_handler(int sig)
{
    printf("[HANDLER] Signal %d received\n", sig);
    signal_handled = 1;
}

int main(void)
{
    struct sigaction sa;
    int ret;

    printf("=== rseq+signal reentrancy bug demonstration ===\n\n");

    // Set up signal handler
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = signal_handler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);

    if (sigaction(SIGUSR1, &sa, NULL) != 0) {
        perror("sigaction failed");
        return 1;
    }

    // Register rseq
    memset(&g_rseq, 0, sizeof(g_rseq));
    g_rseq.cpu_id = -1;

    printf("[1] Registering rseq...\n");
    ret = rseq_register(&g_rseq, 32, 0x53534551, 0);
    if (ret != 0) {
        if (errno == ENOSYS) {
            printf("[SKIP] rseq not implemented\n");
            return 0;
        }
        printf("[ERROR] rseq registration failed: %s\n", strerror(errno));
        return 1;
    }
    printf("[OK] rseq registered\n");

    // Set invalid rseq_cs pointer
    printf("\n[2] Setting rseq_cs to invalid pointer (0xdeadbeefdeadbeef)...\n");
    g_rseq.rseq_cs = 0xdeadbeefdeadbeefUL;

    // Trigger signal delivery
    printf("[3] Sending SIGUSR1 to trigger signal handling...\n");
    printf("     This will cause kernel to read invalid rseq_cs\n");
    printf("     Expected (buggy behavior): Process crashes with 'Segmentation fault'\n");
    printf("     Expected (correct behavior): Signal handler executes, process continues\n\n");

    kill(getpid(), SIGUSR1);

    // Small delay to allow signal processing
    for (volatile int i = 0; i < 1000000; i++);

    // If we reach here, the bug didn't trigger or is fixed
    if (signal_handled) {
        printf("\n[SUCCESS] Signal handler was executed! Bug appears to be fixed.\n");
    } else {
        printf("\n[UNEXPECTED] Process survived but signal handler was not called.\n");
    }

    // Cleanup
    printf("\n[4] Cleaning up: unregistering rseq...\n");
    g_rseq.rseq_cs = 0; // Clear invalid pointer before unregister
    ret = rseq_register(&g_rseq, 32, 0x53534551, RSEQ_FLAG_UNREGISTER);
    if (ret != 0) {
        printf("[WARNING] rseq unregistration failed: %s\n", strerror(errno));
    } else {
        printf("[OK] rseq unregistered\n");
    }

    printf("\n=== Test completed ===\n");
    return 0;
}
