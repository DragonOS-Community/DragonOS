#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

void ExpectPeer(int fd, pid_t pid, uid_t uid, gid_t gid) {
    struct ucred cred{};
    socklen_t len = sizeof(cred);
    ASSERT_EQ(0, getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &cred, &len)) << strerror(errno);
    EXPECT_EQ(sizeof(cred), len);
    EXPECT_EQ(pid, cred.pid);
    EXPECT_EQ(uid, cred.uid);
    EXPECT_EQ(gid, cred.gid);
}

struct AbstractAddress {
    sockaddr_un addr{};
    socklen_t len;

    explicit AbstractAddress(int suffix) {
        addr.sun_family = AF_UNIX;
        int written = snprintf(addr.sun_path + 1, sizeof(addr.sun_path) - 1,
                               "dkc039-%d-%d", getpid(), suffix);
        len = offsetof(sockaddr_un, sun_path) + 1 + written;
    }
};

bool WriteFile(const char* path, const char* text) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return false;
    const size_t len = strlen(text);
    bool ok = write(fd, text, len) == static_cast<ssize_t>(len);
    close(fd);
    return ok;
}

TEST(UnixPeerCred, UnconnectedAndSocketpair) {
    for (int type : {SOCK_STREAM, SOCK_SEQPACKET, SOCK_DGRAM}) {
        int unconnected = socket(AF_UNIX, type, 0);
        ASSERT_GE(unconnected, 0) << strerror(errno);
        ExpectPeer(unconnected, 0, static_cast<uid_t>(-1), static_cast<gid_t>(-1));
        close(unconnected);

        int pair[2];
        ASSERT_EQ(0, socketpair(AF_UNIX, type, 0, pair)) << strerror(errno);
        ExpectPeer(pair[0], getpid(), geteuid(), getegid());
        ExpectPeer(pair[1], getpid(), geteuid(), getegid());
        close(pair[0]);
        close(pair[1]);
    }
}

TEST(UnixPeerCred, ConnectAndAcceptSnapshot) {
    for (int type : {SOCK_STREAM, SOCK_SEQPACKET}) {
        AbstractAddress address(type);
        int listener = socket(AF_UNIX, type, 0);
        ASSERT_GE(listener, 0);
        ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address.addr), address.len));
        ASSERT_EQ(0, listen(listener, 2));

        int result_pipe[2];
        ASSERT_EQ(0, pipe(result_pipe));
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            close(result_pipe[0]);
            close(listener);
            int client = socket(AF_UNIX, type, 0);
            struct ucred peer{};
            socklen_t len = sizeof(peer);
            int result = client >= 0 &&
                         connect(client, reinterpret_cast<sockaddr*>(&address.addr), address.len) == 0 &&
                         getsockopt(client, SOL_SOCKET, SO_PEERCRED, &peer, &len) == 0 &&
                         len == sizeof(peer) && peer.pid == getppid() &&
                         peer.uid == geteuid() && peer.gid == getegid();
            if (client >= 0) close(client);
            const ssize_t written = write(result_pipe[1], &result, sizeof(result));
            _exit(result && written == static_cast<ssize_t>(sizeof(result)) ? 0 : 1);
        }
        close(result_pipe[1]);
        int accepted = accept(listener, nullptr, nullptr);
        ASSERT_GE(accepted, 0) << strerror(errno);
        ExpectPeer(accepted, child, geteuid(), getegid());
        int child_result = 0;
        EXPECT_EQ(static_cast<ssize_t>(sizeof(child_result)),
                  read(result_pipe[0], &child_result, sizeof(child_result)));
        EXPECT_EQ(1, child_result);
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        close(accepted);
        close(listener);
        close(result_pipe[0]);
    }
}

TEST(UnixPeerCred, DatagramConnectDoesNotSetPeerIdentity) {
    int receiver = socket(AF_UNIX, SOCK_DGRAM, 0);
    int sender = socket(AF_UNIX, SOCK_DGRAM, 0);
    ASSERT_GE(receiver, 0);
    ASSERT_GE(sender, 0);
    AbstractAddress address(100);
    ASSERT_EQ(0, bind(receiver, reinterpret_cast<sockaddr*>(&address.addr), address.len));
    ASSERT_EQ(0, connect(sender, reinterpret_cast<sockaddr*>(&address.addr), address.len));
    ExpectPeer(sender, 0, static_cast<uid_t>(-1), static_cast<gid_t>(-1));
    close(sender);
    close(receiver);
}

TEST(UnixPeerCred, FailedListenAndConnectDoNotInventIdentity) {
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0);
    errno = 0;
    EXPECT_EQ(-1, listen(listener, 1));
    EXPECT_EQ(EINVAL, errno);
    ExpectPeer(listener, 0, static_cast<uid_t>(-1), static_cast<gid_t>(-1));

    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(client, 0);
    AbstractAddress absent(103);
    errno = 0;
    EXPECT_EQ(-1, connect(client, reinterpret_cast<sockaddr*>(&absent.addr), absent.len));
    EXPECT_EQ(ECONNREFUSED, errno);
    ExpectPeer(client, 0, static_cast<uid_t>(-1), static_cast<gid_t>(-1));
    close(client);
    close(listener);
}

TEST(UnixPeerCred, RepeatedListenCapturesNewEffectiveUid) {
    if (geteuid() != 0) GTEST_SKIP() << "requires root to change euid";
    AbstractAddress address(101);
    int ready_pipe[2];
    ASSERT_EQ(0, pipe(ready_pipe));
    pid_t server = fork();
    ASSERT_GE(server, 0);
    if (server == 0) {
        close(ready_pipe[0]);
        int listener = socket(AF_UNIX, SOCK_STREAM, 0);
        bool ok = listener >= 0 &&
                  bind(listener, reinterpret_cast<sockaddr*>(&address.addr), address.len) == 0 &&
                  listen(listener, 1) == 0 && seteuid(12345) == 0 &&
                  listen(listener, 1) == 0 && seteuid(0) == 0;
        char ready = ok ? 'Y' : 'N';
        const ssize_t written = write(ready_pipe[1], &ready, 1);
        if (ok && written == 1) {
            int accepted = accept(listener, nullptr, nullptr);
            ok = accepted >= 0;
            if (accepted >= 0) close(accepted);
        }
        if (listener >= 0) close(listener);
        _exit(ok ? 0 : 1);
    }
    close(ready_pipe[1]);
    char ready = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &ready, 1));
    ASSERT_EQ('Y', ready);
    int client = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(client, 0);
    ASSERT_EQ(0, connect(client, reinterpret_cast<sockaddr*>(&address.addr), address.len));
    ExpectPeer(client, server, 12345, getegid());
    close(client);
    close(ready_pipe[0]);
    int status = 0;
    ASSERT_EQ(server, waitpid(server, &status, 0));
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

TEST(UnixPeerCred, ConnectCapturesEffectiveUidBeforeAccept) {
    if (geteuid() != 0) GTEST_SKIP() << "requires root to change euid";
    AbstractAddress address(102);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address.addr), address.len));
    ASSERT_EQ(0, listen(listener, 1));
    int ready_pipe[2];
    ASSERT_EQ(0, pipe(ready_pipe));
    pid_t client_pid = fork();
    ASSERT_GE(client_pid, 0);
    if (client_pid == 0) {
        close(ready_pipe[0]);
        close(listener);
        int client = socket(AF_UNIX, SOCK_STREAM, 0);
        bool ok = client >= 0 && seteuid(12345) == 0 &&
                  connect(client, reinterpret_cast<sockaddr*>(&address.addr), address.len) == 0 &&
                  seteuid(0) == 0;
        char ready = ok ? 'Y' : 'N';
        const ssize_t written = write(ready_pipe[1], &ready, 1);
        if (client >= 0) close(client);
        _exit(ok && written == 1 ? 0 : 1);
    }
    close(ready_pipe[1]);
    char ready = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &ready, 1));
    ASSERT_EQ('Y', ready);
    int accepted = accept(listener, nullptr, nullptr);
    ASSERT_GE(accepted, 0);
    ExpectPeer(accepted, client_pid, 12345, getegid());
    close(accepted);
    close(listener);
    close(ready_pipe[0]);
    int status = 0;
    ASSERT_EQ(client_pid, waitpid(client_pid, &status, 0));
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

TEST(UnixPeerCred, LengthAndFaultSemantics) {
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    struct ucred expected{getpid(), geteuid(), getegid()};
    for (socklen_t requested : {0u, 1u, 4u, 8u, 11u, 12u, 13u, 8192u}) {
        unsigned char buffer[16];
        memset(buffer, 0xa5, sizeof(buffer));
        socklen_t len = requested;
        ASSERT_EQ(0, getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, buffer, &len))
            << "requested=" << requested << " " << strerror(errno);
        EXPECT_EQ(requested < sizeof(expected) ? requested : sizeof(expected), len);
        EXPECT_EQ(0, memcmp(buffer, &expected, len));
        for (size_t i = len; i < sizeof(buffer); ++i) EXPECT_EQ(0xa5, buffer[i]);
    }
    socklen_t len = 0;
    EXPECT_EQ(0, getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, nullptr, &len));
    EXPECT_EQ(0u, len);
    len = sizeof(expected);
    errno = 0;
    EXPECT_EQ(-1, getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, nullptr, &len));
    EXPECT_EQ(EFAULT, errno);
    close(pair[0]);
    close(pair[1]);
}

TEST(UnixPeerCred, PeerOutsidePidNamespaceIsInvisible) {
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (unshare(CLONE_NEWPID) != 0)
            _exit(errno == EPERM || errno == ENOSYS ? 77 : 2);
        pid_t inner = fork();
        if (inner < 0) _exit(2);
        if (inner == 0) {
            struct ucred cred{};
            socklen_t len = sizeof(cred);
            bool ok = getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, &cred, &len) == 0 &&
                      len == sizeof(cred) && cred.pid == 0 &&
                      cred.uid == geteuid() && cred.gid == getegid();
            _exit(ok ? 0 : 3);
        }
        int status = 0;
        _exit(waitpid(inner, &status, 0) == inner && WIFEXITED(status) &&
                      WEXITSTATUS(status) == 0
                  ? 0
                  : 4);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    close(pair[0]);
    close(pair[1]);
    ASSERT_TRUE(WIFEXITED(status));
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "CLONE_NEWPID unavailable";
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(UnixPeerCred, EffectiveIdsMapIntoQueryingUserNamespace) {
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const uid_t outer_uid = getuid();
        const gid_t outer_gid = getgid();
        if (unshare(CLONE_NEWUSER) != 0)
            _exit(errno == EPERM || errno == ENOSYS ? 77 : 2);
        char uid_map[64];
        char gid_map[64];
        snprintf(uid_map, sizeof(uid_map), "12345 %u 1\n", outer_uid);
        snprintf(gid_map, sizeof(gid_map), "12345 %u 1\n", outer_gid);
        if (!WriteFile("/proc/self/uid_map", uid_map) ||
            !WriteFile("/proc/self/setgroups", "deny\n") ||
            !WriteFile("/proc/self/gid_map", gid_map)) {
            _exit(3);
        }
        struct ucred cred{};
        socklen_t len = sizeof(cred);
        bool ok = getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, &cred, &len) == 0 &&
                  len == sizeof(cred) && cred.uid == 12345 && cred.gid == 12345;
        _exit(ok ? 0 : 2);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    close(pair[0]);
    close(pair[1]);
    ASSERT_TRUE(WIFEXITED(status));
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "user namespace mapping unavailable";
    EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
