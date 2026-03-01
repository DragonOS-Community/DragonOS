// 请在 host 中使用这个文件，与 vsock_test.c 配套使用
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <linux/vm_sockets.h>

#define DEFAULT_BACKLOG 8

static void usage(const char *prog)
{
    fprintf(stderr, "Usage:\n");
    fprintf(stderr, "  %s server <port> [expect_msg] [reply]\n", prog);
    fprintf(stderr, "  %s client <guest_cid> <port> [msg] [expect_reply]\n", prog);
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

static int recv_once_str(int fd, char *buf, size_t cap)
{
    ssize_t n = recv(fd, buf, cap - 1, 0);
    if (n < 0) {
        perror("recv");
        return -1;
    }
    buf[n] = '\0';
    return 0;
}

static int run_server(uint32_t port, const char *expect_msg, const char *reply)
{
    char buf[512];
    int listenfd = -1;
    int connfd = -1;
    int rc = 1;
    struct sockaddr_vm addr;
    struct sockaddr_vm peer;
    socklen_t peer_len = sizeof(peer);

    listenfd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (listenfd < 0) {
        perror("socket");
        return 1;
    }

    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = VMADDR_CID_ANY;
    addr.svm_port = port;

    if (bind(listenfd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind");
        goto out;
    }
    if (listen(listenfd, DEFAULT_BACKLOG) < 0) {
        perror("listen");
        goto out;
    }

    fprintf(stdout, "[host-server] listening on cid=ANY port=%u\n", port);
    connfd = accept(listenfd, (struct sockaddr *)&peer, &peer_len);
    if (connfd < 0) {
        perror("accept");
        goto out;
    }
    fprintf(stdout, "[host-server] accepted peer cid=%u port=%u\n", peer.svm_cid, peer.svm_port);

    if (recv_once_str(connfd, buf, sizeof(buf)) < 0) {
        goto out;
    }
    fprintf(stdout, "[host-server] recv: \"%s\"\n", buf);

    if (expect_msg != NULL && strcmp(expect_msg, buf) != 0) {
        fprintf(stderr, "[host-server] expect \"%s\" but got \"%s\"\n", expect_msg, buf);
        goto out;
    }

    if (reply != NULL && reply[0] != '\0') {
        if (send_all(connfd, reply, strlen(reply)) < 0) {
            goto out;
        }
        fprintf(stdout, "[host-server] sent reply: \"%s\"\n", reply);
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

static int run_client(uint32_t guest_cid, uint32_t port, const char *msg, const char *expect_reply)
{
    char buf[512];
    int fd = -1;
    int rc = 1;
    struct sockaddr_vm peer;

    fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("socket");
        return 1;
    }

    memset(&peer, 0, sizeof(peer));
    peer.svm_family = AF_VSOCK;
    peer.svm_cid = guest_cid;
    peer.svm_port = port;

    if (connect(fd, (struct sockaddr *)&peer, sizeof(peer)) < 0) {
        perror("connect");
        goto out;
    }
    fprintf(stdout, "[host-client] connected to guest cid=%u port=%u\n", guest_cid, port);

    if (send_all(fd, msg, strlen(msg)) < 0) {
        goto out;
    }
    fprintf(stdout, "[host-client] sent: \"%s\"\n", msg);

    if (expect_reply != NULL) {
        if (recv_once_str(fd, buf, sizeof(buf)) < 0) {
            goto out;
        }
        fprintf(stdout, "[host-client] recv: \"%s\"\n", buf);
        if (strcmp(expect_reply, buf) != 0) {
            fprintf(stderr, "[host-client] expect \"%s\" but got \"%s\"\n", expect_reply, buf);
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

    if (argc < 3) {
        usage(argv[0]);
        return 2;
    }

    if (strcmp(argv[1], "server") == 0) {
        if (parse_u32(argv[2], &port) < 0) {
            usage(argv[0]);
            return 2;
        }
        return run_server(
            port,
            (argc >= 4) ? argv[3] : NULL,
            (argc >= 5) ? argv[4] : "host-ack");
    }

    if (strcmp(argv[1], "client") == 0) {
        if (argc < 4 || parse_u32(argv[2], &cid) < 0 || parse_u32(argv[3], &port) < 0) {
            usage(argv[0]);
            return 2;
        }
        return run_client(
            cid,
            port,
            (argc >= 5) ? argv[4] : "hello-from-host",
            (argc >= 6) ? argv[5] : "guest-ack");
    }

    usage(argv[0]);
    return 2;
}
