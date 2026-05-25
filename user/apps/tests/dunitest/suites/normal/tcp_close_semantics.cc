#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <limits.h>
#include <netinet/in.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <unistd.h>

#include <atomic>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

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

bool InitDualStackTcpPair(TcpPair* pair) {
    pair->listen_fd.Reset();
    pair->client_fd.Reset();
    pair->server_fd.Reset();

    int listen_fd = socket(AF_INET6, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        ADD_FAILURE() << "socket(AF_INET6 listen) failed: " << ErrnoString(errno);
        return false;
    }
    pair->listen_fd.Reset(listen_fd);

    int v6only = 0;
    if (setsockopt(pair->listen_fd.Get(), IPPROTO_IPV6, IPV6_V6ONLY, &v6only, sizeof(v6only)) !=
        0) {
        ADD_FAILURE() << "setsockopt(IPV6_V6ONLY) failed: " << ErrnoString(errno);
        return false;
    }

    int reuse = 1;
    if (setsockopt(pair->listen_fd.Get(), SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) != 0) {
        ADD_FAILURE() << "setsockopt(SO_REUSEADDR) failed: " << ErrnoString(errno);
        return false;
    }

    sockaddr_in6 addr {};
    addr.sin6_family = AF_INET6;
    addr.sin6_port = htons(0);
    if (inet_pton(AF_INET6, "::ffff:127.0.0.1", &addr.sin6_addr) != 1) {
        ADD_FAILURE() << "inet_pton(::ffff:127.0.0.1) failed";
        return false;
    }

    if (bind(pair->listen_fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
        ADD_FAILURE() << "bind(AF_INET6 dual-stack listen) failed: " << ErrnoString(errno);
        return false;
    }

    socklen_t addr_len = sizeof(addr);
    if (getsockname(pair->listen_fd.Get(), reinterpret_cast<sockaddr*>(&addr), &addr_len) != 0) {
        ADD_FAILURE() << "getsockname(AF_INET6 listen) failed: " << ErrnoString(errno);
        return false;
    }

    if (listen(pair->listen_fd.Get(), 5) != 0) {
        ADD_FAILURE() << "listen(AF_INET6 dual-stack) failed: " << ErrnoString(errno);
        return false;
    }

    int client_fd = socket(AF_INET6, SOCK_STREAM, 0);
    if (client_fd < 0) {
        ADD_FAILURE() << "socket(AF_INET6 client) failed: " << ErrnoString(errno);
        return false;
    }
    pair->client_fd.Reset(client_fd);

    if (connect(pair->client_fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
        ADD_FAILURE() << "connect(AF_INET6 dual-stack) failed: " << ErrnoString(errno);
        return false;
    }

    int server_fd = accept(pair->listen_fd.Get(), nullptr, nullptr);
    if (server_fd < 0) {
        ADD_FAILURE() << "accept(AF_INET6 dual-stack) failed: " << ErrnoString(errno);
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

thread_local sigjmp_buf g_mm_signal_jmp;
thread_local volatile sig_atomic_t g_mm_signal_active = 0;

void MmSignalHandler(int sig, siginfo_t* si, void* uc) {
    (void)sig;
    (void)si;
    (void)uc;
    if (g_mm_signal_active) {
        siglongjmp(g_mm_signal_jmp, 1);
    }
    _exit(99);
}

struct MmSmokeCtx {
    volatile uint8_t* base = nullptr;
    size_t len = 0;
    std::atomic<int>* run = nullptr;
};

void* MmSmokeWriter(void* arg) {
    auto* ctx = reinterpret_cast<MmSmokeCtx*>(arg);
    size_t off = 0;
    size_t spins = 0;

    if (sigsetjmp(g_mm_signal_jmp, 1) != 0) {
        sched_yield();
    }
    g_mm_signal_active = 1;

    while (ctx->run->load(std::memory_order_acquire)) {
        ctx->base[off] = static_cast<uint8_t>(off);
        off += 13;
        if (off >= ctx->len) {
            off = 0;
        }
        if ((++spins & 0xff) == 0) {
            sched_yield();
        }
    }

    g_mm_signal_active = 0;
    return nullptr;
}

void ExpectMmSignalPathResponsiveAfterTcpStorm() {
    constexpr size_t kPageSize = 4096;
    constexpr size_t kPages = 4;
    constexpr size_t kLen = kPages * kPageSize;
    constexpr int kIters = 128;

    struct sigaction old_segv {};
    struct sigaction old_bus {};
    struct sigaction sa {};
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sa.sa_sigaction = MmSignalHandler;
    sigemptyset(&sa.sa_mask);

    ASSERT_EQ(0, sigaction(SIGSEGV, &sa, &old_segv))
        << "sigaction(SIGSEGV) failed: " << ErrnoString(errno);
    ASSERT_EQ(0, sigaction(SIGBUS, &sa, &old_bus))
        << "sigaction(SIGBUS) failed: " << ErrnoString(errno);

    auto restore_handlers = [&]() {
        sigaction(SIGSEGV, &old_segv, nullptr);
        sigaction(SIGBUS, &old_bus, nullptr);
    };

    auto* buf = static_cast<uint8_t*>(
        mmap(nullptr, kLen, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, buf) << "mmap failed: " << ErrnoString(errno);
    std::memset(buf, 0, kLen);

    std::atomic<int> run{1};
    MmSmokeCtx ctx {
        .base = buf,
        .len = kLen,
        .run = &run,
    };
    pthread_t writer {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, MmSmokeWriter, &ctx))
        << "pthread_create failed: " << ErrnoString(errno);

    int rc = 0;
    for (int i = 0; i < kIters; ++i) {
        if (mprotect(buf, kLen, PROT_READ) != 0) {
            rc = errno;
            break;
        }

        if (sigsetjmp(g_mm_signal_jmp, 1) == 0) {
            g_mm_signal_active = 1;
            *reinterpret_cast<volatile uint8_t*>(buf) = 0x5a;
            rc = EFAULT;
            break;
        }
        g_mm_signal_active = 0;

        if (mprotect(buf, kLen, PROT_READ | PROT_WRITE) != 0) {
            rc = errno;
            break;
        }
    }
    g_mm_signal_active = 0;

    run.store(0, std::memory_order_release);
    ASSERT_EQ(0, pthread_join(writer, nullptr))
        << "pthread_join failed: " << ErrnoString(errno);
    EXPECT_EQ(0, munmap(buf, kLen)) << "munmap failed: " << ErrnoString(errno);
    restore_handlers();

    EXPECT_EQ(0, rc) << "post TCP-storm mm/signal smoke failed: " << ErrnoString(rc);
}

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

void* ResetDuringCloseWorker(void*) {
    TcpPair pair;
    if (!InitTcpPair(&pair)) {
        return reinterpret_cast<void*>(1);
    }

    struct PollCloseArgs {
        int fd = -1;
        int result = 0;
    } poll_args {
        .fd = pair.client_fd.Get(),
        .result = 0,
    };

    auto poll_and_close = [](void* arg) -> void* {
        auto* args = reinterpret_cast<PollCloseArgs*>(arg);
        pollfd pfd {
            .fd = args->fd,
            .events = POLLIN | POLLHUP,
            .revents = 0,
        };
        const int poll_ret = poll(&pfd, 1, 5000);
        if (poll_ret != 1) {
            args->result = 1;
            return nullptr;
        }
        if (close(args->fd) != 0) {
            args->result = 2;
            return nullptr;
        }
        args->fd = -1;
        return nullptr;
    };

    pthread_t closer {};
    if (pthread_create(&closer, nullptr, poll_and_close, &poll_args) != 0) {
        return reinterpret_cast<void*>(2);
    }

    constexpr char kData[] = "abc";
    if (write(pair.server_fd.Get(), kData, 3) != 3) {
        pthread_join(closer, nullptr);
        return reinterpret_cast<void*>(3);
    }

    usleep(10000);

    if (close(pair.server_fd.Release()) != 0) {
        pthread_join(closer, nullptr);
        return reinterpret_cast<void*>(4);
    }

    if (pthread_join(closer, nullptr) != 0) {
        return reinterpret_cast<void*>(5);
    }
    if (poll_args.result != 0) {
        return reinterpret_cast<void*>(6 + poll_args.result);
    }
    pair.client_fd.Release();
    return nullptr;
}

void* ReversedDualStackResetDuringCloseWorker(void*) {
    TcpPair pair;
    if (!InitDualStackTcpPair(&pair)) {
        return reinterpret_cast<void*>(1);
    }

    struct PollCloseArgs {
        int fd = -1;
        int result = 0;
    } poll_args {
        .fd = pair.client_fd.Get(),
        .result = 0,
    };

    auto poll_and_close = [](void* arg) -> void* {
        auto* args = reinterpret_cast<PollCloseArgs*>(arg);
        pollfd pfd {
            .fd = args->fd,
            .events = POLLIN | POLLHUP,
            .revents = 0,
        };
        const int poll_ret = poll(&pfd, 1, 5000);
        if (poll_ret != 1) {
            args->result = 1;
            return nullptr;
        }
        if (close(args->fd) != 0) {
            args->result = 2;
            return nullptr;
        }
        args->fd = -1;
        return nullptr;
    };

    pthread_t closer {};
    if (pthread_create(&closer, nullptr, poll_and_close, &poll_args) != 0) {
        return reinterpret_cast<void*>(2);
    }

    constexpr char kData[] = "abc";
    if (write(pair.server_fd.Get(), kData, 3) != 3) {
        pthread_join(closer, nullptr);
        return reinterpret_cast<void*>(3);
    }

    usleep(10000);

    if (close(pair.server_fd.Release()) != 0) {
        pthread_join(closer, nullptr);
        return reinterpret_cast<void*>(4);
    }

    if (pthread_join(closer, nullptr) != 0) {
        return reinterpret_cast<void*>(5);
    }
    if (poll_args.result != 0) {
        return reinterpret_cast<void*>(6 + poll_args.result);
    }
    pair.client_fd.Release();
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

TEST(TcpCloseSemantics, ConcurrentResetDuringCloseDoesNotStall) {
    signal(SIGPIPE, SIG_IGN);

    constexpr int kThreadCount = 100;
    std::vector<pthread_t> threads(kThreadCount);

    for (int i = 0; i < kThreadCount; ++i) {
        ASSERT_EQ(0, pthread_create(&threads[i], nullptr, ResetDuringCloseWorker, nullptr))
            << "pthread_create failed: " << ErrnoString(errno);
    }

    for (int i = 0; i < kThreadCount; ++i) {
        void* result = nullptr;
        ASSERT_EQ(0, pthread_join(threads[i], &result))
            << "pthread_join failed: " << ErrnoString(errno);
        EXPECT_EQ(nullptr, result) << "worker " << i << " failed";
    }

    ExpectMmSignalPathResponsiveAfterTcpStorm();
}

TEST(TcpCloseSemantics, ReversedDualStackResetDuringCloseDoesNotStall) {
    signal(SIGPIPE, SIG_IGN);

    constexpr int kThreadCount = 100;
    std::vector<pthread_t> threads(kThreadCount);

    for (int i = 0; i < kThreadCount; ++i) {
        ASSERT_EQ(0,
                  pthread_create(
                      &threads[i], nullptr, ReversedDualStackResetDuringCloseWorker, nullptr))
            << "pthread_create failed: " << ErrnoString(errno);
    }

    for (int i = 0; i < kThreadCount; ++i) {
        void* result = nullptr;
        ASSERT_EQ(0, pthread_join(threads[i], &result))
            << "pthread_join failed: " << ErrnoString(errno);
        EXPECT_EQ(nullptr, result) << "worker " << i << " failed";
    }

    ExpectMmSignalPathResponsiveAfterTcpStorm();
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
