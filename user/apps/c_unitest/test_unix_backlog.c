/* Finite regression for AF_UNIX backlog progress and connect error propagation.
 * Build: musl-gcc -static -O2 -Wall -Wextra test_unix_backlog.c -o test_unix_backlog
 * Run without arguments for all cases, or pass accept/grow/close/signal/cycles/errors.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child = -1;
static void fail(const char *what)
{
    perror(what);
    if (child > 0) {
        kill(child, SIGKILL);
        waitpid(child, NULL, 0);
    }
    exit(1);
}
#define CHECK(x) do { if (!(x)) fail(#x); } while (0)
static void handler(int sig) { (void)sig; }
static int readable(int fd, int milliseconds)
{
    struct pollfd p = { .fd = fd, .events = POLLIN };
    int rc;
    do { rc = poll(&p, 1, milliseconds); } while (rc < 0 && errno == EINTR);
    CHECK(rc >= 0);
    return rc && (p.revents & POLLIN);
}
static int make_listener(struct sockaddr_un *addr)
{
    static unsigned int serial;
    memset(addr, 0, sizeof(*addr));
    addr->sun_family = AF_UNIX;
    snprintf(addr->sun_path + 1, sizeof(addr->sun_path) - 1,
             "backlog-%ld-%u", (long)getpid(), ++serial);
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(fd >= 0);
    CHECK(bind(fd, (struct sockaddr *)addr, sizeof(*addr)) == 0);
    CHECK(listen(fd, 0) == 0);
    return fd;
}
static int connected_client(struct sockaddr_un *addr)
{
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(fd >= 0);
    CHECK(connect(fd, (struct sockaddr *)addr, sizeof(*addr)) == 0);
    return fd;
}
/* Wait for the connector to actually sleep, rather than assume a delay proves it.
 * The ready pipe is written immediately before connect; no other blocking call
 * follows it in the child until the result is written to the empty result pipe.
 */
static void wait_sleeping(pid_t pid)
{
    char path[64], line[512];
    snprintf(path, sizeof(path), "/proc/%ld/stat", (long)pid);
    for (int i = 0; i < 5000; ++i) {
        FILE *f = fopen(path, "r");
        if (f) {
            char *got = fgets(line, sizeof(line), f);
            fclose(f);
            char *end = got ? strrchr(line, ')') : NULL;
            if (end && end[1] == ' ' && (end[2] == 'S' || end[2] == 'D'))
                return;
        }
        usleep(1000);
    }
    errno = ETIMEDOUT;
    fail("connector did not enter sleep");
}
static void blocked_case(const char *mode)
{
    struct sockaddr_un addr;
    int listener = make_listener(&addr);
    int first = connected_client(&addr);
    int ready[2], result[2];
    CHECK(pipe(ready) == 0);
    CHECK(pipe(result) == 0);
    child = fork();
    CHECK(child >= 0);
    if (!child) {
        close(listener);
        close(first);
        close(ready[0]);
        close(result[0]);
        struct sigaction sa = { .sa_handler = handler };
        sigemptyset(&sa.sa_mask);
        if (sigaction(SIGUSR1, &sa, NULL)) _exit(10);
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0 || write(ready[1], "r", 1) != 1) _exit(11);
        int rc = connect(fd, (struct sockaddr *)&addr, sizeof(addr));
        int values[2] = {rc, errno};
        if (write(result[1], values, sizeof(values)) != sizeof(values)) _exit(12);
        close(fd);
        _exit(0);
    }
    close(ready[1]);
    close(result[1]);
    CHECK(readable(ready[0], 5000));
    char marker;
    CHECK(read(ready[0], &marker, 1) == 1);
    wait_sleeping(child);
    CHECK(!readable(result[0], 100));
    if (!strcmp(mode, "accept")) {
        int accepted = accept(listener, NULL, NULL);
        CHECK(accepted >= 0);
        close(accepted);
    } else if (!strcmp(mode, "grow")) {
        CHECK(listen(listener, 1) == 0);
    } else if (!strcmp(mode, "close")) {
        close(listener);
        listener = -1;
    } else {
        CHECK(kill(child, SIGUSR1) == 0);
    }
    CHECK(readable(result[0], 5000));
    int values[2];
    CHECK(read(result[0], values, sizeof(values)) == sizeof(values));
    if (!strcmp(mode, "close") || !strcmp(mode, "signal")) {
        CHECK(values[0] == -1);
        CHECK(values[1] == (!strcmp(mode, "close") ? ECONNREFUSED : EINTR));
    } else {
        CHECK(values[0] == 0);
        /* Each successful connect must have its own accept-ready entry. */
        int count = !strcmp(mode, "grow") ? 2 : 1;
        for (int i = 0; i < count; ++i) {
            CHECK(readable(listener, 5000));
            int accepted = accept(listener, NULL, NULL);
            CHECK(accepted >= 0);
            close(accepted);
        }
        CHECK(!readable(listener, 0));
    }
    int status;
    CHECK(waitpid(child, &status, 0) == child);
    child = -1;
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(ready[0]);
    close(result[0]);
    close(first);
    if (listener >= 0) close(listener);
    printf("PASS %s\n", mode);
}
static void cycles(void)
{
    struct sockaddr_un addr;
    int listener = make_listener(&addr);
    for (int i = 0; i < 1024; ++i) {
        int fd = connected_client(&addr);
        CHECK(readable(listener, 5000));
        int peer = accept(listener, NULL, NULL);
        CHECK(peer >= 0);
        CHECK(write(fd, "x", 1) == 1);
        char c;
        CHECK(read(peer, &c, 1) == 1 && c == 'x');
        close(fd);
        CHECK(read(peer, &c, 1) == 0);
        close(peer);
    }
    close(listener);
    puts("PASS cycles (1024)");
}
static void errors(void)
{
    struct sockaddr_un addr;
    int listener = make_listener(&addr);
    int fd = connected_client(&addr);
    int accepted = accept(listener, NULL, NULL);
    CHECK(accepted >= 0);
    CHECK(connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == -1 && errno == EISCONN);
    close(accepted);
    int pending = connected_client(&addr);
    int second = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(second >= 0);
    CHECK(connect(second, (struct sockaddr *)&addr, sizeof(addr)) == -1 && errno == EAGAIN);
    close(second);
    close(pending);
    close(fd);
    close(listener);
    puts("PASS errors");
}
int main(int argc, char **argv)
{
    setbuf(stdout, NULL);
    const char *names[] = {"accept", "grow", "close", "signal", "errors", "cycles"};
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); ++i) {
        if (argc > 1 && strcmp(argv[1], names[i])) continue;
        if (!strcmp(names[i], "errors")) errors();
        else if (!strcmp(names[i], "cycles")) cycles();
        else blocked_case(names[i]);
    }
    return 0;
}
