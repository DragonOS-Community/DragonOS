// 请在 guest(DragonOS) 中使用这个文件，与 host_vsock_test.c 配套使用
/*
  使用方式：
  1. 只测 guest 本地回环（不依赖 host）
  # 在 DragonOS guest 内
  vsock_test selftest
  # 或指定端口
  vsock_test selftest 40500

  2. Host 连接 Guest（guest 当 server）
  # guest 内先监听
  vsock_test guest-listen 40501 hello-from-host guest-ack
  # host 上连接（guest-cid=3）
  ./host_vsock_test client 3 40501 hello-from-host guest-ack

  3. Guest 连接 Host（host 当 server）
  # host 先监听
  ./host_vsock_test server 40502 hello-from-guest host-ack
  # guest 连接 host（host cid=2）
  vsock_test guest-connect 2 40502 hello-from-guest host-ack
*/

#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <linux/vm_sockets.h>

#ifndef VMADDR_CID_LOCAL
#define VMADDR_CID_LOCAL 1U
#endif

#define DEFAULT_SELFTEST_PORT 40500U
#define DEFAULT_BACKLOG 8

static void usage(const char *prog)
{
    fprintf(stderr, "Usage:\n");
    fprintf(stderr, "  %s selftest [port]\n", prog);
    fprintf(stderr, "  %s guest-listen <port> [expect_msg] [reply]\n", prog);
    fprintf(stderr, "  %s guest-connect <cid> <port> [msg] [expect_reply]\n", prog);
}

static int parse_u32(const char *s, uint32_t *out)
{
    char *end = NULL;
    unsigned long v = strtoul(s, &end, 10);
    if (s[0] == '\0' || end == NULL || *end != '\0' || v > 0xffffffffUL) {
        return -1;
    }
    *out = (uint32_t)v;
    return 0;
}

static int create_vsock_stream(void)
{
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("socket(AF_VSOCK, SOCK_STREAM)");
    }
    return fd;
}

static int bind_and_listen(int fd, uint32_t cid, uint32_t port, int backlog)
{
    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = cid;
    addr.svm_port = port;

    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind");
        return -1;
    }

    if (listen(fd, backlog) < 0) {
        perror("listen");
        return -1;
    }

    return 0;
}

static int connect_to(int fd, uint32_t cid, uint32_t port)
{
    struct sockaddr_vm peer;
    memset(&peer, 0, sizeof(peer));
    peer.svm_family = AF_VSOCK;
    peer.svm_cid = cid;
    peer.svm_port = port;

    if (connect(fd, (struct sockaddr *)&peer, sizeof(peer)) < 0) {
        perror("connect");
        return -1;
    }
    return 0;
}

static int send_all(int fd, const char *buf, size_t len)
{
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(fd, buf + off, len - off, 0);
        if (n < 0) {
            perror("send");
            return -1;
        }
        if (n == 0) {
            fprintf(stderr, "send returned 0 unexpectedly\n");
            return -1;
        }
        off += (size_t)n;
    }
    return 0;
}

static int recv_once_str(int fd, char *buf, size_t cap, ssize_t *out_n)
{
    ssize_t n = recv(fd, buf, cap - 1, 0);
    if (n < 0) {
        perror("recv");
        return -1;
    }
    buf[n] = '\0';
    *out_n = n;
    return 0;
}

static int poll_expect(int fd, short events, short must_have, int timeout_ms)
{
    struct pollfd pfd;
    int ret;
    pfd.fd = fd;
    pfd.events = events;
    pfd.revents = 0;

    ret = poll(&pfd, 1, timeout_ms);
    if (ret < 0) {
        perror("poll");
        return -1;
    }
    if (ret == 0) {
        fprintf(stderr, "poll timeout (events=0x%x)\n", (unsigned)events);
        return -1;
    }
    if ((pfd.revents & must_have) != must_have) {
        fprintf(stderr, "poll revents mismatch, got=0x%x need=0x%x\n",
                (unsigned)pfd.revents, (unsigned)must_have);
        return -1;
    }
    return 0;
}

static int run_selftest(uint32_t port)
{
    const char *c2s = "hello-same-cid";
    const char *s2c = "ack-same-cid";
    char buf[256];
    ssize_t n;
    int listenfd = -1;
    int clientfd = -1;
    int connfd = -1;
    int rc = 1;
    struct sockaddr_vm peer;
    socklen_t peer_len = sizeof(peer);

    fprintf(stdout, "[selftest] start, port=%u\n", port);

    listenfd = create_vsock_stream();
    if (listenfd < 0) {
        goto out;
    }
    if (bind_and_listen(listenfd, VMADDR_CID_ANY, port, DEFAULT_BACKLOG) < 0) {
        goto out;
    }

    clientfd = create_vsock_stream();
    if (clientfd < 0) {
        goto out;
    }
    if (connect_to(clientfd, VMADDR_CID_LOCAL, port) < 0) {
        goto out;
    }

    if (poll_expect(clientfd, POLLOUT, POLLOUT, 1000) < 0) {
        goto out;
    }

    connfd = accept(listenfd, (struct sockaddr *)&peer, &peer_len);
    if (connfd < 0) {
        perror("accept");
        goto out;
    }
    fprintf(stdout, "[selftest] accepted peer cid=%u port=%u\n", peer.svm_cid, peer.svm_port);

    if (send_all(clientfd, c2s, strlen(c2s)) < 0) {
        goto out;
    }
    if (recv_once_str(connfd, buf, sizeof(buf), &n) < 0) {
        goto out;
    }
    if ((size_t)n != strlen(c2s) || strcmp(buf, c2s) != 0) {
        fprintf(stderr, "[selftest] c->s payload mismatch: \"%s\"\n", buf);
        goto out;
    }

    if (send_all(connfd, s2c, strlen(s2c)) < 0) {
        goto out;
    }
    if (recv_once_str(clientfd, buf, sizeof(buf), &n) < 0) {
        goto out;
    }
    if ((size_t)n != strlen(s2c) || strcmp(buf, s2c) != 0) {
        fprintf(stderr, "[selftest] s->c payload mismatch: \"%s\"\n", buf);
        goto out;
    }

    if (shutdown(clientfd, SHUT_WR) < 0) {
        perror("shutdown(client, SHUT_WR)");
        goto out;
    }
    n = recv(connfd, buf, sizeof(buf), 0);
    if (n < 0) {
        perror("recv after SHUT_WR");
        goto out;
    }
    if (n != 0) {
        fprintf(stderr, "[selftest] expected EOF after SHUT_WR, got %zd bytes\n", n);
        goto out;
    }

    rc = 0;
    fprintf(stdout, "[selftest] PASS\n");

out:
    if (connfd >= 0) {
        close(connfd);
    }
    if (clientfd >= 0) {
        close(clientfd);
    }
    if (listenfd >= 0) {
        close(listenfd);
    }
    return rc;
}

static int run_guest_listen(uint32_t port, const char *expect_msg, const char *reply)
{
    char buf[512];
    ssize_t n;
    int listenfd = -1;
    int connfd = -1;
    int rc = 1;
    struct sockaddr_vm peer;
    socklen_t peer_len = sizeof(peer);

    listenfd = create_vsock_stream();
    if (listenfd < 0) {
        return 1;
    }
    if (bind_and_listen(listenfd, VMADDR_CID_ANY, port, DEFAULT_BACKLOG) < 0) {
        goto out;
    }

    fprintf(stdout, "[guest-listen] listening on cid=ANY port=%u\n", port);

    connfd = accept(listenfd, (struct sockaddr *)&peer, &peer_len);
    if (connfd < 0) {
        perror("accept");
        goto out;
    }
    fprintf(stdout, "[guest-listen] accepted peer cid=%u port=%u\n", peer.svm_cid, peer.svm_port);

    if (recv_once_str(connfd, buf, sizeof(buf), &n) < 0) {
        goto out;
    }
    fprintf(stdout, "[guest-listen] recv: \"%s\"\n", buf);

    if (expect_msg != NULL && strcmp(buf, expect_msg) != 0) {
        fprintf(stderr, "[guest-listen] expect \"%s\" but got \"%s\"\n", expect_msg, buf);
        goto out;
    }

    if (reply != NULL && reply[0] != '\0') {
        if (send_all(connfd, reply, strlen(reply)) < 0) {
            goto out;
        }
        fprintf(stdout, "[guest-listen] sent reply: \"%s\"\n", reply);
    }

    rc = 0;
out:
    if (connfd >= 0) {
        close(connfd);
    }
    if (listenfd >= 0) {
        close(listenfd);
    }
    return rc;
}

static int run_guest_connect(uint32_t cid, uint32_t port, const char *msg, const char *expect_reply)
{
    char buf[512];
    ssize_t n;
    int fd = -1;
    int rc = 1;

    fd = create_vsock_stream();
    if (fd < 0) {
        return 1;
    }
    if (connect_to(fd, cid, port) < 0) {
        goto out;
    }
    fprintf(stdout, "[guest-connect] connected to cid=%u port=%u\n", cid, port);

    if (send_all(fd, msg, strlen(msg)) < 0) {
        goto out;
    }
    fprintf(stdout, "[guest-connect] sent: \"%s\"\n", msg);

    if (expect_reply != NULL) {
        if (recv_once_str(fd, buf, sizeof(buf), &n) < 0) {
            goto out;
        }
        fprintf(stdout, "[guest-connect] recv: \"%s\"\n", buf);
        if (strcmp(buf, expect_reply) != 0) {
            fprintf(stderr, "[guest-connect] expect \"%s\" but got \"%s\"\n", expect_reply, buf);
            goto out;
        }
    }

    rc = 0;
out:
    if (fd >= 0) {
        close(fd);
    }
    return rc;
}

int main(int argc, char **argv)
{
    uint32_t cid;
    uint32_t port;

    if (argc < 2) {
        usage(argv[0]);
        return 2;
    }

    if (strcmp(argv[1], "selftest") == 0) {
        port = DEFAULT_SELFTEST_PORT;
        if (argc >= 3 && parse_u32(argv[2], &port) < 0) {
            fprintf(stderr, "invalid port: %s\n", argv[2]);
            return 2;
        }
        return run_selftest(port);
    }

    if (strcmp(argv[1], "guest-listen") == 0) {
        if (argc < 3 || parse_u32(argv[2], &port) < 0) {
            usage(argv[0]);
            return 2;
        }
        return run_guest_listen(
            port,
            (argc >= 4) ? argv[3] : NULL,
            (argc >= 5) ? argv[4] : "guest-ack");
    }

    if (strcmp(argv[1], "guest-connect") == 0) {
        if (argc < 4 || parse_u32(argv[2], &cid) < 0 || parse_u32(argv[3], &port) < 0) {
            usage(argv[0]);
            return 2;
        }
        return run_guest_connect(
            cid,
            port,
            (argc >= 5) ? argv[4] : "hello-from-guest",
            (argc >= 6) ? argv[5] : "host-ack");
    }

    usage(argv[0]);
    return 2;
}
