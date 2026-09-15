#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/fsuid.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SO_REUSEPORT
#define SO_REUSEPORT 15
#endif

#define IO_TIMEOUT_MS 1500
#define TOTAL_TIMEOUT_SEC 30
#define DISTRIBUTION_SAMPLES 80

static int passed;
static int failed;
static int skipped;

static void fail_errno(const char *what)
{
    fprintf(stderr, "    %s: %s\n", what, strerror(errno));
}

static void result(const char *name, int ok)
{
    if (ok) {
        printf("[PASS] %s\n", name);
        passed++;
    } else {
        printf("[FAIL] %s\n", name);
        failed++;
    }
}

static void skip(const char *name, const char *reason)
{
    printf("[SKIP] %s: %s\n", name, reason);
    skipped++;
}

static int set_bool_opt(int fd, int option, int enabled)
{
    return setsockopt(fd, SOL_SOCKET, option, &enabled, sizeof(enabled));
}

static void close_fd(int *fd)
{
    if (*fd >= 0) {
        close(*fd);
        *fd = -1;
    }
}

static int bind_addr(int fd, int family, bool wildcard, uint16_t port)
{
    if (family == AF_INET) {
        struct sockaddr_in addr;

        memset(&addr, 0, sizeof(addr));
        addr.sin_family = AF_INET;
        addr.sin_port = htons(port);
        addr.sin_addr.s_addr = htonl(wildcard ? INADDR_ANY : INADDR_LOOPBACK);
        return bind(fd, (struct sockaddr *)&addr, sizeof(addr));
    }

    struct sockaddr_in6 addr6;

    memset(&addr6, 0, sizeof(addr6));
    addr6.sin6_family = AF_INET6;
    addr6.sin6_port = htons(port);
    addr6.sin6_addr = wildcard ? in6addr_any : in6addr_loopback;
    return bind(fd, (struct sockaddr *)&addr6, sizeof(addr6));
}

static int socket_port(int fd, int family, uint16_t *port)
{
    if (family == AF_INET) {
        struct sockaddr_in addr;
        socklen_t len = sizeof(addr);

        if (getsockname(fd, (struct sockaddr *)&addr, &len) < 0)
            return -1;
        *port = ntohs(addr.sin_port);
        return 0;
    }

    struct sockaddr_in6 addr6;
    socklen_t len = sizeof(addr6);

    if (getsockname(fd, (struct sockaddr *)&addr6, &len) < 0)
        return -1;
    *port = ntohs(addr6.sin6_port);
    return 0;
}

static int make_bound(int family, bool wildcard, uint16_t port,
                      int reuseaddr, int reuseport, uint16_t *bound_port)
{
    int fd = socket(family, SOCK_STREAM, 0);

    if (fd < 0)
        return -1;
    if ((reuseaddr && set_bool_opt(fd, SO_REUSEADDR, 1) < 0) ||
        (reuseport && set_bool_opt(fd, SO_REUSEPORT, 1) < 0) ||
        bind_addr(fd, family, wildcard, port) < 0 ||
        (bound_port && socket_port(fd, family, bound_port) < 0)) {
        int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

static int connect_loopback(int family, uint16_t port)
{
    int fd = socket(family, SOCK_STREAM, 0);
    int flags;
    int rc;

    if (fd < 0)
        return -1;
    flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0)
        goto error;
    if (bind_addr(fd, family, false, 0) < 0) {
        int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    if (family == AF_INET) {
        struct sockaddr_in addr;

        memset(&addr, 0, sizeof(addr));
        addr.sin_family = AF_INET;
        addr.sin_port = htons(port);
        addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        rc = connect(fd, (struct sockaddr *)&addr, sizeof(addr));
        if (rc < 0 && errno != EINPROGRESS)
            goto error;
    } else {
        struct sockaddr_in6 addr6;

        memset(&addr6, 0, sizeof(addr6));
        addr6.sin6_family = AF_INET6;
        addr6.sin6_port = htons(port);
        addr6.sin6_addr = in6addr_loopback;
        rc = connect(fd, (struct sockaddr *)&addr6, sizeof(addr6));
        if (rc < 0 && errno != EINPROGRESS)
            goto error;
    }
    if (rc < 0) {
        struct pollfd pfd;
        socklen_t error_len;
        int socket_error = 0;

        pfd.fd = fd;
        pfd.events = POLLOUT;
        pfd.revents = 0;
        do {
            rc = poll(&pfd, 1, IO_TIMEOUT_MS);
        } while (rc < 0 && errno == EINTR);
        if (rc == 0) {
            errno = ETIMEDOUT;
            goto error;
        }
        if (rc < 0)
            goto error;
        error_len = sizeof(socket_error);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &socket_error, &error_len) < 0)
            goto error;
        if (socket_error != 0) {
            errno = socket_error;
            goto error;
        }
    }
    if (fcntl(fd, F_SETFL, flags) < 0)
        goto error;
    return fd;

error: {
        int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
}

/* Returns the listener index, or -1 after a bounded wait. */
static int accept_selected(const int *listeners, int count, int *accepted)
{
    struct pollfd pfds[4];
    int i;
    int ready;

    if (count > (int)(sizeof(pfds) / sizeof(pfds[0]))) {
        errno = EINVAL;
        return -1;
    }
    for (i = 0; i < count; i++) {
        pfds[i].fd = listeners[i];
        pfds[i].events = POLLIN;
        pfds[i].revents = 0;
    }

    do {
        ready = poll(pfds, count, IO_TIMEOUT_MS);
    } while (ready < 0 && errno == EINTR);
    if (ready <= 0) {
        if (ready == 0)
            errno = ETIMEDOUT;
        return -1;
    }
    for (i = 0; i < count; i++) {
        if (pfds[i].revents & POLLIN) {
            *accepted = accept(listeners[i], NULL, NULL);
            return *accepted < 0 ? -1 : i;
        }
    }
    for (i = 0; i < count; i++)
        fprintf(stderr, "    listener[%d] poll revents=0x%x\n", i,
                pfds[i].revents);
    errno = EIO;
    return -1;
}

static int one_connection(const int *listeners, int count, int family,
                          uint16_t port)
{
    int client = -1;
    int accepted = -1;
    int selected = -1;

    client = connect_loopback(family, port);
    if (client < 0)
        goto out;
    selected = accept_selected(listeners, count, &accepted);
out:
    close_fd(&accepted);
    close_fd(&client);
    return selected;
}

static int make_reuseport_pair(int family, bool wildcard, int backlog0,
                               int backlog1, int listeners[2], uint16_t *port)
{
    listeners[0] = make_bound(family, wildcard, 0, 0, 1, port);
    if (listeners[0] < 0)
        return -1;
    listeners[1] = make_bound(family, wildcard, *port, 0, 1, NULL);
    if (listeners[1] < 0)
        goto error;
    if (listen(listeners[0], backlog0) < 0 || listen(listeners[1], backlog1) < 0)
        goto error;
    return 0;

error:
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
    return -1;
}

static void test_poll_hup_cleared_by_listen(void)
{
    const char *name = "bind/listen clears pre-bind POLLHUP state";
    struct pollfd pfd;
    int listener = -1;
    uint16_t port = 0;
    int rc;
    int ok = 0;

    listener = socket(AF_INET, SOCK_STREAM, 0);
    if (listener < 0)
        goto out;
    pfd.fd = listener;
    pfd.events = POLLIN;
    pfd.revents = 0;
    rc = poll(&pfd, 1, 0);
    if (rc < 0)
        goto out;
    printf("    pre-bind poll: rc=%d revents=0x%x\n", rc, pfd.revents);

    if (set_bool_opt(listener, SO_REUSEPORT, 1) < 0 ||
        bind_addr(listener, AF_INET, false, 0) < 0 ||
        socket_port(listener, AF_INET, &port) < 0 || listen(listener, 4) < 0)
        goto out;
    pfd.revents = 0;
    rc = poll(&pfd, 1, 0);
    printf("    empty-listener poll: rc=%d revents=0x%x\n", rc, pfd.revents);
    ok = rc == 0 && pfd.revents == 0;
out:
    if (!ok)
        fail_errno("poll event transition");
    result(name, ok);
    close_fd(&listener);
}

static bool is_v4_mapped_loopback(const struct sockaddr_storage *storage)
{
    const struct sockaddr_in6 *addr6 = (const struct sockaddr_in6 *)storage;
    uint32_t v4;

    if (storage->ss_family != AF_INET6 || !IN6_IS_ADDR_V4MAPPED(&addr6->sin6_addr))
        return false;
    memcpy(&v4, &addr6->sin6_addr.s6_addr[12], sizeof(v4));
    return v4 == htonl(INADDR_LOOPBACK);
}

static void test_dualstack_accept_addresses(void)
{
    const char *name = "IPv6 wildcard dual-stack accept returns mapped IPv6 endpoints";
    struct sockaddr_storage accepted_peer = {0};
    struct sockaddr_storage accepted_local = {0};
    struct sockaddr_storage queried_peer = {0};
    struct sockaddr_in client_local = {0};
    socklen_t accepted_peer_len = sizeof(accepted_peer);
    socklen_t accepted_local_len = sizeof(accepted_local);
    socklen_t queried_peer_len = sizeof(queried_peer);
    socklen_t client_local_len = sizeof(client_local);
    struct pollfd pfd;
    int listener = -1;
    int client = -1;
    int accepted = -1;
    uint16_t port = 0;
    int ok = 0;

    listener = make_bound(AF_INET6, true, 0, 0, 1, &port);
    if (listener < 0 || listen(listener, 4) < 0)
        goto out;
    client = connect_loopback(AF_INET, port);
    if (client < 0)
        goto out;
    pfd.fd = listener;
    pfd.events = POLLIN;
    pfd.revents = 0;
    if (poll(&pfd, 1, IO_TIMEOUT_MS) != 1 || !(pfd.revents & POLLIN))
        goto out;
    accepted = accept(listener, (struct sockaddr *)&accepted_peer,
                      &accepted_peer_len);
    if (accepted < 0 ||
        getsockname(accepted, (struct sockaddr *)&accepted_local,
                    &accepted_local_len) < 0 ||
        getpeername(accepted, (struct sockaddr *)&queried_peer,
                    &queried_peer_len) < 0 ||
        getsockname(client, (struct sockaddr *)&client_local,
                    &client_local_len) < 0)
        goto out;

    ok = accepted_peer_len == sizeof(struct sockaddr_in6) &&
         accepted_local_len == sizeof(struct sockaddr_in6) &&
         queried_peer_len == sizeof(struct sockaddr_in6) &&
         is_v4_mapped_loopback(&accepted_peer) &&
         is_v4_mapped_loopback(&accepted_local) &&
         is_v4_mapped_loopback(&queried_peer) &&
         ((struct sockaddr_in6 *)&accepted_peer)->sin6_port ==
             client_local.sin_port &&
         ((struct sockaddr_in6 *)&queried_peer)->sin6_port ==
             client_local.sin_port &&
         ((struct sockaddr_in6 *)&accepted_local)->sin6_port == htons(port);
out:
    if (!ok) {
        fprintf(stderr,
                "    accepted_len=%u local_len=%u peer_len=%u families=%d/%d/%d\n",
                (unsigned)accepted_peer_len, (unsigned)accepted_local_len,
                (unsigned)queried_peer_len, accepted_peer.ss_family,
                accepted_local.ss_family, queried_peer.ss_family);
        fail_errno("dual-stack accepted endpoints");
    }
    result(name, ok);
    close_fd(&accepted);
    close_fd(&client);
    close_fd(&listener);
}

static void test_shared_accept(int family, const char *name)
{
    int listeners[2] = {-1, -1};
    uint16_t port = 0;
    int seen[2] = {0, 0};
    int i;

    if (make_reuseport_pair(family, false, 4, 4, listeners, &port) < 0) {
        if (family == AF_INET6 &&
            (errno == EAFNOSUPPORT || errno == EADDRNOTAVAIL || errno == ENOPROTOOPT)) {
            skip(name, "IPv6 or SO_REUSEPORT is unavailable");
            return;
        }
        fail_errno("create reuseport listeners");
        result(name, 0);
        return;
    }
    for (i = 0; i < 48 && (!seen[0] || !seen[1]); i++) {
        int selected = one_connection(listeners, 2, family, port);

        if (selected < 0) {
            fail_errno("connect/accept");
            break;
        }
        seen[selected]++;
    }
    result(name, seen[0] && seen[1]);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
}

static void test_backlog_not_weight(void)
{
    const char *name = "backlog 1:8 does not weight reuseport distribution";
    int listeners[2] = {-1, -1};
    int count[2] = {0, 0};
    uint16_t port = 0;
    int i;

    if (make_reuseport_pair(AF_INET, false, 1, 8, listeners, &port) < 0) {
        fail_errno("create backlog listeners");
        result(name, 0);
        return;
    }
    for (i = 0; i < DISTRIBUTION_SAMPLES; i++) {
        int selected = one_connection(listeners, 2, AF_INET, port);

        if (selected < 0) {
            fail_errno("distribution connect/accept");
            break;
        }
        count[selected]++;
    }
    printf("    distribution: backlog=1 %d, backlog=8 %d\n", count[0], count[1]);
    result(name, i == DISTRIBUTION_SAMPLES && count[0] >= 20 && count[1] >= 20);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
}

static void test_reuseaddr_listen_conflict(void)
{
    const char *name = "SO_REUSEADDR permits double bind but rejects second listen";
    int first = -1;
    int second = -1;
    uint16_t port = 0;
    int ok = 0;

    first = make_bound(AF_INET, false, 0, 1, 0, &port);
    if (first < 0)
        goto out;
    second = make_bound(AF_INET, false, port, 1, 0, NULL);
    if (second < 0)
        goto out;
    if (listen(first, 4) < 0)
        goto out;
    errno = 0;
    ok = listen(second, 4) < 0 && errno == EADDRINUSE;
out:
    if (!ok)
        fail_errno("reuseaddr bind/listen matrix");
    result(name, ok);
    close_fd(&first);
    close_fd(&second);
}

static void test_exact_over_wildcard(void)
{
    const char *name = "exact loopback listener wins over wildcard";
    int listeners[2] = {-1, -1};
    uint16_t port = 0;
    int i;
    int ok = 1;

    listeners[0] = make_bound(AF_INET, true, 0, 0, 1, &port);
    if (listeners[0] < 0)
        ok = 0;
    if (ok)
        listeners[1] = make_bound(AF_INET, false, port, 0, 1, NULL);
    if (listeners[1] < 0)
        ok = 0;
    if (ok && (listen(listeners[0], 4) < 0 || listen(listeners[1], 4) < 0))
        ok = 0;
    for (i = 0; ok && i < 12; i++) {
        if (one_connection(listeners, 2, AF_INET, port) != 1)
            ok = 0;
    }
    if (!ok)
        fail_errno("exact/wildcard selection");
    result(name, ok);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
}

static void test_close_listeners_then_rebind(void)
{
    const char *name = "accepted connection survives listener close and port can rebind";
    int listeners[2] = {-1, -1};
    int client = -1;
    int accepted = -1;
    int rebound = -1;
    uint16_t port = 0;
    int selected;
    int ok = 0;

    if (make_reuseport_pair(AF_INET, false, 4, 4, listeners, &port) < 0)
        goto out;
    client = connect_loopback(AF_INET, port);
    if (client < 0)
        goto out;
    selected = accept_selected(listeners, 2, &accepted);
    if (selected < 0)
        goto out;
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);

    /* Keep the established pair open, so TIME_WAIT cannot explain a failure. */
    rebound = make_bound(AF_INET, false, port, 0, 1, NULL);
    if (rebound < 0 || listen(rebound, 4) < 0)
        goto out;
    ok = 1;
out:
    if (!ok)
        fail_errno("close listeners/rebind");
    result(name, ok);
    close_fd(&rebound);
    close_fd(&accepted);
    close_fd(&client);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
}

static void test_close_one_member(void)
{
    const char *name = "remaining reuseport member accepts after peer close";
    int listeners[2] = {-1, -1};
    uint16_t port = 0;
    int ok = 0;

    if (make_reuseport_pair(AF_INET, false, 4, 4, listeners, &port) < 0)
        goto out;
    close_fd(&listeners[0]);
    ok = one_connection(&listeners[1], 1, AF_INET, port) == 0;
out:
    if (!ok)
        fail_errno("remaining member accept");
    result(name, ok);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
}

static void test_shutdown_relisten(void)
{
    const char *name = "shutdown releases old port and re-listen uses a new port";
    int listener = -1;
    int rebound = -1;
    int old_client = -1;
    int new_client = -1;
    int accepted = -1;
    uint16_t old_port = 0;
    uint16_t new_port = 0;
    int ok = 0;

    listener = make_bound(AF_INET, false, 0, 0, 1, &old_port);
    if (listener < 0 || listen(listener, 4) < 0)
        goto out;
    if (shutdown(listener, SHUT_RDWR) < 0 || listen(listener, 4) < 0)
        goto out;
    if (socket_port(listener, AF_INET, &new_port) < 0 ||
        new_port == 0 || new_port == old_port)
        goto out;

    errno = 0;
    old_client = connect_loopback(AF_INET, old_port);
    if (old_client >= 0 || errno != ECONNREFUSED)
        goto out;
    new_client = connect_loopback(AF_INET, new_port);
    if (new_client < 0 || accept_selected(&listener, 1, &accepted) != 0)
        goto out;

    rebound = make_bound(AF_INET, false, old_port, 0, 0, NULL);
    if (rebound < 0 || listen(rebound, 4) < 0)
        goto out;
    ok = 1;
out:
    if (!ok)
        fail_errno("shutdown/re-listen");
    result(name, ok);
    close_fd(&rebound);
    close_fd(&accepted);
    close_fd(&new_client);
    close_fd(&old_client);
    close_fd(&listener);
}

static int dynamic_option_case(int before, int after, bool expect_peer)
{
    int first = -1;
    int second = -1;
    uint16_t port = 0;
    int ok = 0;

    first = make_bound(AF_INET, false, 0, 0, before, &port);
    if (first < 0)
        goto out;
    if (set_bool_opt(first, SO_REUSEPORT, after) < 0)
        goto out;
    second = make_bound(AF_INET, false, port, 0, 1, NULL);
    if (second < 0) {
        ok = !expect_peer && errno == EADDRINUSE;
        goto out;
    }
    if (listen(first, 4) < 0) {
        ok = !expect_peer && errno == EADDRINUSE;
        goto out;
    }
    errno = 0;
    if (listen(second, 4) == 0)
        ok = expect_peer;
    else
        ok = !expect_peer && errno == EADDRINUSE;
out:
    if (!ok)
        fail_errno("dynamic SO_REUSEPORT admission");
    close_fd(&first);
    close_fd(&second);
    return ok;
}

static void test_dynamic_options(void)
{
    result("post-bind SO_REUSEPORT 0->1 admits a reuseport peer",
           dynamic_option_case(0, 1, true));
    result("post-bind SO_REUSEPORT 1->0 rejects peer at bind/listen",
           dynamic_option_case(1, 0, false));
}

static void test_different_uid(void)
{
    const char *name = "different effective UID cannot join reuseport group";
    int listener = -1;
    uint16_t port = 0;
    pid_t child;
    int status;

    if (geteuid() != 0) {
        skip(name, "requires root to create a child with a different effective UID");
        return;
    }
    listener = make_bound(AF_INET, false, 0, 0, 1, &port);
    if (listener < 0 || listen(listener, 4) < 0) {
        fail_errno("create parent listener");
        result(name, 0);
        close_fd(&listener);
        return;
    }
    child = fork();
    if (child < 0) {
        fail_errno("fork");
        result(name, 0);
        close_fd(&listener);
        return;
    }
    if (child == 0) {
        int peer;

        if (setuid(65534) < 0)
            _exit(2);
        peer = make_bound(AF_INET, false, port, 0, 1, NULL);
        if (peer < 0)
            _exit(errno == EADDRINUSE || errno == EACCES ? 0 : 3);
        if (listen(peer, 4) < 0)
            _exit(errno == EADDRINUSE || errno == EACCES ? 0 : 4);
        close(peer);
        _exit(1);
    }
    while (waitpid(child, &status, 0) < 0 && errno == EINTR)
        ;
    if (WIFEXITED(status) && WEXITSTATUS(status) == 2)
        skip(name, "setuid(65534) is unavailable in this environment");
    else
        result(name, WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close_fd(&listener);
}

/* Keep family separate from the transport address: both listen on IPv4
 * loopback, but only AF_INET should receive until that listener closes. */
static int mapped_family_case(bool ipv6_first)
{
    int listeners[2] = {-1, -1};
    int client = -1, accepted = -1, ok = 0;
    uint16_t port = 0;
    struct sockaddr_in6 mapped = {0};

    listeners[0] = make_bound(AF_INET, false, 0, 0, 1, &port);
    listeners[1] = socket(AF_INET6, SOCK_STREAM, 0);
    mapped.sin6_family = AF_INET6;
    mapped.sin6_port = htons(port);
    if (listeners[0] < 0 || listeners[1] < 0 ||
        inet_pton(AF_INET6, "::ffff:127.0.0.1", &mapped.sin6_addr) != 1 ||
        set_bool_opt(listeners[1], SO_REUSEPORT, 1) < 0 ||
        bind(listeners[1], (struct sockaddr *)&mapped, sizeof(mapped)) < 0 ||
        listen(listeners[ipv6_first ? 1 : 0], 4) < 0 ||
        listen(listeners[ipv6_first ? 0 : 1], 4) < 0)
        goto out;
    for (int i = 0; i < DISTRIBUTION_SAMPLES; i++) {
        client = connect_loopback(AF_INET, port);
        if (client < 0 || accept_selected(listeners, 2, &accepted) != 0)
            goto out;
        close_fd(&accepted);
        close_fd(&client);
    }
    close_fd(&listeners[0]);
    client = connect_loopback(AF_INET, port);
    if (client < 0 || accept_selected(&listeners[1], 1, &accepted) != 0)
        goto out;
    ok = 1;
out:
    if (!ok)
        fail_errno("mapped family selection");
    close_fd(&accepted);
    close_fd(&client);
    close_fd(&listeners[0]);
    close_fd(&listeners[1]);
    return ok;
}

static void test_mapped_family_priority(void)
{
    result("AF_INET wins when mapped AF_INET6 listens first", mapped_family_case(true));
    result("AF_INET wins when mapped AF_INET6 listens last", mapped_family_case(false));
}

/* Each child changes its credentials independently. The parent listener's
 * creation-time fsuid is zero; bind/listen must compare socket owners. */
static void test_fsuid_ownership(void)
{
    const char *names[] = {
        "equal euid but different fsuid rejects reuseport",
        "different euid but equal fsuid permits reuseport",
        "restoring fsuid after socket creation does not change its owner",
        "changing fsuid after socket creation does not change its owner",
    };
    if (geteuid() != 0 || setfsuid((uid_t)-1) != 0) {
        for (int i = 0; i < 4; i++)
            skip(names[i], "requires initial root euid and fsuid");
        return;
    }
    for (int i = 0; i < 4; i++) {
        uint16_t port = 0;
        int listener = make_bound(AF_INET, false, 0, 0, 1, &port);
        if (listener < 0 || listen(listener, 4) < 0) {
            result(names[i], 0);
            close_fd(&listener);
            continue;
        }
        pid_t child = fork();
        if (child == 0) {
            int peer;
            /* Both requested fsuids are in real/effective/saved IDs. This
             * does not rely on privileged arbitrary setfsuid support. */
            if (setresuid(65534, i == 1 ? 65534 : 0, 0) < 0)
                _exit(2);
            uid_t creation_fsuid = (i == 0 || i == 2) ? 65534 : 0;
            setfsuid(creation_fsuid);
            if ((uid_t)setfsuid((uid_t)-1) != creation_fsuid)
                _exit(2);
            peer = socket(AF_INET, SOCK_STREAM, 0);
            if (peer < 0 || set_bool_opt(peer, SO_REUSEPORT, 1) < 0)
                _exit(3);
            if (i >= 2) {
                uid_t bind_fsuid = i == 2 ? 0 : 65534;
                setfsuid(bind_fsuid);
                if ((uid_t)setfsuid((uid_t)-1) != bind_fsuid)
                    _exit(2);
            }
            int rc = bind_addr(peer, AF_INET, false, port);
            if (i == 0 || i == 2)
                _exit(rc < 0 && errno == EADDRINUSE ? 0 : 1);
            _exit(rc == 0 && listen(peer, 4) == 0 ? 0 : 1);
        }
        int status = 0;
        pid_t waited;
        do {
            waited = child < 0 ? -1 : waitpid(child, &status, 0);
        } while (waited < 0 && errno == EINTR);
        /* Root-capable tests fail on credential setup errors instead of
         * silently skipping the regression they are intended to cover. */
        result(names[i], waited == child && child > 0 &&
               WIFEXITED(status) && WEXITSTATUS(status) == 0);
        close_fd(&listener);
    }
}

static void timeout_handler(int signo)
{
    (void)signo;
    static const char message[] = "[FAIL] global test timeout\n";
    ssize_t ignored;

    ignored = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)ignored;
    _exit(124);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("[START] TCP SO_REUSEPORT syscall regression\n");
    signal(SIGALRM, timeout_handler);
    alarm(TOTAL_TIMEOUT_SEC);

    test_poll_hup_cleared_by_listen();
    test_dualstack_accept_addresses();
    test_shared_accept(AF_INET, "IPv4 reuseport members both accept");
    test_shared_accept(AF_INET6, "IPv6 reuseport members both accept");
    test_backlog_not_weight();
    test_reuseaddr_listen_conflict();
    test_exact_over_wildcard();
    test_close_listeners_then_rebind();
    test_close_one_member();
    test_shutdown_relisten();
    test_dynamic_options();
    test_different_uid();
    test_mapped_family_priority();
    test_fsuid_ownership();

    alarm(0);
    printf("Summary: PASS=%d FAIL=%d SKIP=%d\n", passed, failed, skipped);
    return failed ? EXIT_FAILURE : EXIT_SUCCESS;
}
