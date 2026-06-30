#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define INJECT_PATH "/proc/sys/vm/oom_fault_inject"
#define BIG_PAGES 16384
#define PAGE_SIZE 4096

static int write_inject_config(pid_t tgid, unsigned long fail_after, unsigned long fail_times)
{
    char buf[128];
    int fd;
    int len;

    fd = open(INJECT_PATH, O_WRONLY);
    if (fd < 0) {
        perror("open oom_fault_inject");
        return -1;
    }

    len = snprintf(buf, sizeof(buf), "%d %lu %lu\n", tgid, fail_after, fail_times);
    if (write(fd, buf, len) != len) {
        perror("write oom_fault_inject");
        close(fd);
        return -1;
    }

    close(fd);
    return 0;
}

static void cleanup_inject_config(void)
{
    (void)write_inject_config(0, 0, 0);
}

static void *fault_one_page(void)
{
    volatile uint8_t *p;

    p = mmap(NULL, PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        return NULL;
    }

    p[0] = 0x5a;
    return (void *)p;
}

static void *map_and_touch_pages(size_t pages)
{
    volatile uint8_t *p;
    size_t len = pages * PAGE_SIZE;
    size_t i;

    p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        return MAP_FAILED;
    }

    for (i = 0; i < len; i += PAGE_SIZE) {
        p[i] = (uint8_t)(i / PAGE_SIZE);
    }

    return (void *)p;
}

static void victim_child_main(void)
{
    if (map_and_touch_pages(BIG_PAGES) == MAP_FAILED) {
        perror("victim mmap");
        _exit(2);
    }

    for (;;) {
        pause();
    }
}

static void trigger_selfkill_child_main(void)
{
    if (map_and_touch_pages(BIG_PAGES) == MAP_FAILED) {
        perror("selfkill mmap");
        _exit(2);
    }

    if (write_inject_config(getpid(), 0, 1) < 0) {
        _exit(2);
    }

    (void)fault_one_page();
    _exit(3);
}

static void trigger_retry_child_main(void)
{
    void *p;

    if (write_inject_config(getpid(), 0, 1) < 0) {
        _exit(2);
    }

    p = fault_one_page();
    if (p == NULL) {
        _exit(3);
    }

    memset(p, 0xa5, PAGE_SIZE);
    _exit(0);
}

static int expect_sigkill(pid_t pid, const char *name)
{
    int status;

    if (waitpid(pid, &status, 0) < 0) {
        perror("waitpid");
        return -1;
    }

    if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGKILL) {
        fprintf(stderr, "%s: expected SIGKILL, got status=0x%x\n", name, status);
        return -1;
    }

    return 0;
}

static int expect_exit0(pid_t pid, const char *name)
{
    int status;

    if (waitpid(pid, &status, 0) < 0) {
        perror("waitpid");
        return -1;
    }

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "%s: expected exit 0, got status=0x%x\n", name, status);
        return -1;
    }

    return 0;
}

static int test_current_is_victim(void)
{
    pid_t child = fork();
    int ret;

    if (child < 0) {
        perror("fork");
        return -1;
    }
    if (child == 0) {
        trigger_selfkill_child_main();
    }

    ret = expect_sigkill(child, "current_is_victim");
    cleanup_inject_config();
    return ret;
}

static int test_other_process_is_victim(void)
{
    pid_t victim;
    pid_t trigger;

    victim = fork();
    if (victim < 0) {
        perror("fork victim");
        return -1;
    }
    if (victim == 0) {
        victim_child_main();
    }

    usleep(100000);

    trigger = fork();
    if (trigger < 0) {
        perror("fork trigger");
        kill(victim, SIGKILL);
        waitpid(victim, NULL, 0);
        cleanup_inject_config();
        return -1;
    }
    if (trigger == 0) {
        trigger_retry_child_main();
    }

    if (expect_exit0(trigger, "trigger_retry") < 0) {
        kill(victim, SIGKILL);
        waitpid(victim, NULL, 0);
        cleanup_inject_config();
        return -1;
    }

    int ret = expect_sigkill(victim, "other_process_victim");
    cleanup_inject_config();
    return ret;
}

int main(void)
{
    cleanup_inject_config();

    if (test_current_is_victim() < 0) {
        cleanup_inject_config();
        return 1;
    }
    if (test_other_process_is_victim() < 0) {
        cleanup_inject_config();
        return 1;
    }

    cleanup_inject_config();
    puts("test_oom_killer: PASS");
    return 0;
}
