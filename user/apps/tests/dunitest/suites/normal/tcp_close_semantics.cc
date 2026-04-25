#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <limits.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#include <cstdlib>
#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    FdGuard() = default;
    explicit FdGuard(int fd) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;

    FdGuard(FdGuard&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }

    FdGuard& operator=(FdGuard&& other) noexcept {
        if (this != &other) {
            Reset();
            fd_ = other.fd_;
            other.fd_ = -1;
        }
        return *this;
    }

    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    int Release() {
        int fd = fd_;
        fd_ = -1;
        return fd;
    }

    void Reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

  private:
    int fd_ = -1;
};

struct TcpPair {
    FdGuard listen_fd;
    FdGuard client_fd;
    FdGuard server_fd;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

bool InitTcpPair(TcpPair* pair) {
    pair->listen_fd.Reset();
    pair->client_fd.Reset();
    pair->server_fd.Reset();

    int listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        ADD_FAILURE() << "socket(listen) failed: " << ErrnoString(errno);
        return false;
    }
    pair->listen_fd.Reset(listen_fd);

    int reuse = 1;
    if (setsockopt(pair->listen_fd.Get(), SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) != 0) {
        ADD_FAILURE() << "setsockopt(SO_REUSEADDR) failed: " << ErrnoString(errno);
        return false;
    }

    sockaddr_in addr {};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(0);

    if (bind(pair->listen_fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
        ADD_FAILURE() << "bind(listen) failed: " << ErrnoString(errno);
        return false;
    }

    socklen_t addr_len = sizeof(addr);
    if (getsockname(pair->listen_fd.Get(), reinterpret_cast<sockaddr*>(&addr), &addr_len) != 0) {
        ADD_FAILURE() << "getsockname(listen) failed: " << ErrnoString(errno);
        return false;
    }

    if (listen(pair->listen_fd.Get(), 1) != 0) {
        ADD_FAILURE() << "listen failed: " << ErrnoString(errno);
        return false;
    }

    int client_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (client_fd < 0) {
        ADD_FAILURE() << "socket(client) failed: " << ErrnoString(errno);
        return false;
    }
    pair->client_fd.Reset(client_fd);

    if (connect(pair->client_fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
        ADD_FAILURE() << "connect failed: " << ErrnoString(errno);
        return false;
    }

    int server_fd = accept(pair->listen_fd.Get(), nullptr, nullptr);
    if (server_fd < 0) {
        ADD_FAILURE() << "accept failed: " << ErrnoString(errno);
        return false;
    }
    pair->server_fd.Reset(server_fd);

    return true;
}

void ExpectPeerEofAfterShutdownWr(int fd) {
    char byte = '\0';
    errno = 0;
    const ssize_t n = recv(fd, &byte, sizeof(byte), 0);
    const int saved_errno = errno;
    ASSERT_GE(n, 0) << "recv failed: " << ErrnoString(saved_errno);
    ASSERT_EQ(0, n) << "expected EOF after SHUT_WR, got " << n;
}

struct WriteResult {
    ssize_t first_ret = -1;
    int first_errno = 0;
    ssize_t second_ret = -1;
    int second_errno = 0;
};

struct WriterArgs {
    int fd = -1;
    size_t len = 0;
    WriteResult* result = nullptr;
};

void* WriterThread(void* arg) {
    auto* args = reinterpret_cast<WriterArgs*>(arg);
    char* buf = static_cast<char*>(std::malloc(args->len));
    if (buf == nullptr) {
        return nullptr;
    }
    std::memset(buf, 'a', args->len);

    errno = 0;
    args->result->first_ret = write(args->fd, buf, args->len);
    args->result->first_errno = errno;

    errno = 0;
    args->result->second_ret = write(args->fd, buf, args->len);
    args->result->second_errno = errno;

    std::free(buf);
    return nullptr;
}

TEST(TcpCloseSemantics, BlockPartialWriteClosedReturnsReset) {
    signal(SIGPIPE, SIG_IGN);
    TcpPair pair;
    ASSERT_TRUE(InitTcpPair(&pair));

    int sndbuf_req = INT_MAX;
    ASSERT_EQ(0,
              setsockopt(
                  pair.client_fd.Get(), SOL_SOCKET, SO_SNDBUF, &sndbuf_req, sizeof(sndbuf_req)))
        << "setsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);

    int actual_sndbuf = 0;
    socklen_t optlen = sizeof(actual_sndbuf);
    ASSERT_EQ(0,
              getsockopt(
                  pair.client_fd.Get(), SOL_SOCKET, SO_SNDBUF, &actual_sndbuf, &optlen))
        << "getsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);
    ASSERT_GT(actual_sndbuf, 0);

    WriteResult result {};
    WriterArgs args {
        .fd = pair.client_fd.Get(),
        .len = static_cast<size_t>(actual_sndbuf) * 2,
        .result = &result,
    };

    pthread_t writer {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriterThread, &args))
        << "pthread_create failed: " << ErrnoString(errno);

    sleep(1);

    pair.server_fd.Reset();

    ASSERT_EQ(0, pthread_join(writer, nullptr))
        << "pthread_join failed: " << ErrnoString(errno);

    EXPECT_GT(result.first_ret, 0) << "first write should partially succeed";
    EXPECT_EQ(-1, result.second_ret) << "second write should fail";
    EXPECT_TRUE(result.second_errno == EPIPE || result.second_errno == ECONNRESET)
        << "expected EPIPE/ECONNRESET, got " << ErrnoString(result.second_errno);
}

TEST(TcpCloseSemantics, ShutdownWrThenClosePreservesGracefulClose) {
    signal(SIGPIPE, SIG_IGN);
    TcpPair pair;
    ASSERT_TRUE(InitTcpPair(&pair));

    ASSERT_EQ(0, shutdown(pair.client_fd.Get(), SHUT_WR))
        << "shutdown(SHUT_WR) failed: " << ErrnoString(errno);
    ExpectPeerEofAfterShutdownWr(pair.server_fd.Get());

    sleep(1);

    pair.client_fd.Reset();

    sleep(1);

    char byte = 'x';
    errno = 0;
    const ssize_t wr = send(pair.server_fd.Get(), &byte, sizeof(byte), 0);
    EXPECT_EQ(static_cast<ssize_t>(sizeof(byte)), wr)
        << "send after peer graceful close failed: " << ErrnoString(errno);
}

TEST(TcpCloseSemantics, ShutdownWrThenZeroLingerCloseAborts) {
    signal(SIGPIPE, SIG_IGN);
    TcpPair pair;
    ASSERT_TRUE(InitTcpPair(&pair));

    linger linger_opt {
        .l_onoff = 1,
        .l_linger = 0,
    };
    ASSERT_EQ(0,
              setsockopt(pair.client_fd.Get(),
                         SOL_SOCKET,
                         SO_LINGER,
                         &linger_opt,
                         sizeof(linger_opt)))
        << "setsockopt(SO_LINGER) failed: " << ErrnoString(errno);

    ASSERT_EQ(0, shutdown(pair.client_fd.Get(), SHUT_WR))
        << "shutdown(SHUT_WR) failed: " << ErrnoString(errno);
    ExpectPeerEofAfterShutdownWr(pair.server_fd.Get());

    sleep(1);

    pair.client_fd.Reset();

    sleep(1);

    char byte = 'z';
    errno = 0;
    const ssize_t wr = send(pair.server_fd.Get(), &byte, sizeof(byte), 0);
    const int saved_errno = errno;
    EXPECT_EQ(-1, wr) << "zero-linger close should abort the peer";
    EXPECT_TRUE(saved_errno == EPIPE || saved_errno == ECONNRESET)
        << "expected EPIPE/ECONNRESET, got " << ErrnoString(saved_errno);
}

TEST(TcpCloseSemantics, ShutdownWrThenUnreadDataCloseAborts) {
    signal(SIGPIPE, SIG_IGN);
    TcpPair pair;
    ASSERT_TRUE(InitTcpPair(&pair));

    ASSERT_EQ(0, shutdown(pair.client_fd.Get(), SHUT_WR))
        << "shutdown(SHUT_WR) failed: " << ErrnoString(errno);
    ExpectPeerEofAfterShutdownWr(pair.server_fd.Get());

    char byte = 'a';
    errno = 0;
    const ssize_t first_send = send(pair.server_fd.Get(), &byte, sizeof(byte), 0);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(byte)), first_send)
        << "first send before peer close failed: " << ErrnoString(errno);

    sleep(1);

    pair.client_fd.Reset();

    sleep(1);

    byte = 'b';
    errno = 0;
    const ssize_t second_send = send(pair.server_fd.Get(), &byte, sizeof(byte), 0);
    const int saved_errno = errno;
    EXPECT_EQ(-1, second_send) << "peer close with unread data should abort";
    EXPECT_TRUE(saved_errno == EPIPE || saved_errno == ECONNRESET)
        << "expected EPIPE/ECONNRESET, got " << ErrnoString(saved_errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
