#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <limits.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <net/if.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cstddef>
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

constexpr intptr_t kPollDuringCloseFailed = 7;
constexpr intptr_t kCloseAfterPollFailed = 8;

struct ResetDuringCloseArgs {
    bool (*init_pair)(TcpPair*);
    int poll_result = 0;
    int poll_errno = 0;
    short poll_revents = 0;
    std::chrono::milliseconds poll_elapsed{};
    int queued_bytes = -1;
    int fionread_errno = 0;
    int tcp_state = -1;
    socklen_t tcp_info_length = 0;
    int tcp_info_errno = 0;
};

void* ResetDuringCloseWorker(void* arg) {
    auto& worker = *static_cast<ResetDuringCloseArgs*>(arg);
    TcpPair pair;
    if (!worker.init_pair(&pair)) {
        return reinterpret_cast<void*>(1);
    }

    struct PollCloseArgs {
        int fd;
        ResetDuringCloseArgs& worker;
        intptr_t result = 0;
    } poll_args{pair.client_fd.Get(), worker};

    auto poll_and_close = [](void* arg) -> void* {
        auto* args = reinterpret_cast<PollCloseArgs*>(arg);
        pollfd pfd {
            .fd = args->fd,
            .events = POLLIN | POLLHUP,
            .revents = 0,
        };
        const auto start = std::chrono::steady_clock::now();
        const int poll_ret = poll(&pfd, 1, 5000);
        if (poll_ret != 1) {
            auto& observation = args->worker;
            observation.poll_errno = poll_ret < 0 ? errno : 0;
            observation.poll_result = poll_ret;
            observation.poll_revents = pfd.revents;
            observation.poll_elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - start);
            int queued_bytes = 0;
            if (ioctl(args->fd, FIONREAD, &queued_bytes) == 0) {
                observation.queued_bytes = queued_bytes;
            } else {
                observation.fionread_errno = errno;
            }
            tcp_info info{};
            socklen_t length = sizeof(info);
            if (getsockopt(args->fd, IPPROTO_TCP, TCP_INFO, &info, &length) == 0) {
                observation.tcp_info_length = length;
                if (length >= offsetof(tcp_info, tcpi_state) + sizeof(info.tcpi_state)) {
                    observation.tcp_state = info.tcpi_state;
                }
            } else {
                observation.tcp_info_errno = errno;
            }
            args->result = kPollDuringCloseFailed;
            return nullptr;
        }
        if (close(args->fd) != 0) {
            args->result = kCloseAfterPollFailed;
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
        return reinterpret_cast<void*>(poll_args.result);
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

void ExpectCorkedCloseDeliversAcceptedBytes(bool backpressure) {
    TcpPair pair;
    ASSERT_TRUE(InitTcpPair(&pair));

    const int sender = pair.client_fd.Get();
    const int receiver = pair.server_fd.Get();
    const int enabled = 1;
    ASSERT_EQ(0, setsockopt(sender, IPPROTO_TCP, TCP_CORK, &enabled, sizeof(enabled)));
    const timeval deadline{.tv_sec = 4, .tv_usec = 0};
    ASSERT_EQ(0, setsockopt(receiver, SOL_SOCKET, SO_RCVTIMEO, &deadline, sizeof(deadline)));

    std::vector<char> payload(backpressure ? 256 * 1024 : 24);
    for (size_t i = 0; i < payload.size(); ++i) {
        payload[i] = static_cast<char>((i ^ (i >> 8)) & 0xff);
    }
    size_t accepted = 0;
    if (backpressure) {
        const int capacity = 2304;
        ASSERT_EQ(0, setsockopt(sender, SOL_SOCKET, SO_SNDBUF, &capacity, sizeof(capacity)));
        ASSERT_EQ(0, setsockopt(receiver, SOL_SOCKET, SO_RCVBUF, &capacity, sizeof(capacity)));
        // Fill the send path while the peer leaves its receive window full.
        while (accepted < payload.size()) {
            const ssize_t n = send(sender, payload.data() + accepted,
                                   payload.size() - accepted, MSG_DONTWAIT | MSG_NOSIGNAL);
            if (n == -1) {
                ASSERT_EQ(EAGAIN, errno) << ErrnoString(errno);
                break;
            }
            ASSERT_GT(n, 0);
            accepted += static_cast<size_t>(n);
        }
        ASSERT_GT(accepted, 0u);
        ASSERT_LT(accepted, payload.size()) << "expected send backpressure before close";
    } else {
        ASSERT_EQ(static_cast<ssize_t>(payload.size()),
                  send(sender, payload.data(), payload.size(), MSG_NOSIGNAL));
        accepted = payload.size();
    }
    ASSERT_EQ(0, close(pair.client_fd.Release()));

    std::vector<char> received(accepted);
    size_t total = 0;
    while (total < accepted) {
        const ssize_t n = read(receiver, received.data() + total, accepted - total);
        ASSERT_GT(n, 0) << "received " << total << " of " << accepted << " accepted bytes";
        total += static_cast<size_t>(n);
    }
    payload.resize(accepted);
    EXPECT_EQ(payload, received);
    char extra;
    EXPECT_EQ(0, read(receiver, &extra, sizeof(extra))) << ErrnoString(errno);
}

TEST(TcpCloseSemantics, CloseDeliversCorkedBytesBeforeEof) {
    ExpectCorkedCloseDeliversAcceptedBytes(false);
}

TEST(TcpCloseSemantics, CloseDeliversBackpressuredCorkedBytesBeforeEof) {
    ExpectCorkedCloseDeliversAcceptedBytes(true);
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

TEST(TcpCloseSemantics, ZeroLingerDoesNotOvertakeSentDualStackData) {
    signal(SIGPIPE, SIG_IGN);

    // The accepted socket is the sender, matching gVisor's reversed dual-stack
    // case. Repeated short connections exercise contention with the TCP poller.
    for (int iteration = 0; iteration < 100; ++iteration) {
        SCOPED_TRACE(iteration);
        TcpPair pair;
        ASSERT_TRUE(InitDualStackTcpPair(&pair));

        linger linger_opt {
            .l_onoff = 1,
            .l_linger = 0,
        };
        ASSERT_EQ(0,
                  setsockopt(pair.server_fd.Get(),
                             SOL_SOCKET,
                             SO_LINGER,
                             &linger_opt,
                             sizeof(linger_opt)));

        char sent[1000] = {};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)),
                  write(pair.server_fd.Get(), sent, sizeof(sent)))
            << ErrnoString(errno);
        pair.server_fd.Reset();

        // As in the reference test, a peer write may race with the reset.
        const ssize_t peer_write = write(pair.client_fd.Get(), sent, sizeof(sent));
        (void)peer_write;
        char received[sizeof(sent)] = {};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(received)),
                  read(pair.client_fd.Get(), received, sizeof(received)))
            << ErrnoString(errno);
    }
}

TEST(TcpCloseSemantics, AbortiveCloseDoesNotOvertakeQueuedResponse) {
    for (bool unread_request : {false, true}) {
        SCOPED_TRACE(unread_request);
        TcpPair pair;
        ASSERT_TRUE(InitTcpPair(&pair));
        if (unread_request) {
            constexpr char request[] = "request";
            ASSERT_EQ(static_cast<ssize_t>(sizeof(request)),
                      write(pair.client_fd.Get(), request, sizeof(request)));
            pollfd pending{.fd = pair.server_fd.Get(), .events = POLLIN, .revents = 0};
            ASSERT_EQ(1, poll(&pending, 1, 1000));
            ASSERT_NE(0, pending.revents & POLLIN);
        } else {
            const linger abortive{.l_onoff = 1, .l_linger = 0};
            ASSERT_EQ(0, setsockopt(pair.server_fd.Get(), SOL_SOCKET, SO_LINGER,
                                    &abortive, sizeof(abortive)));
        }

        // Verify that a short response on this established loopback
        // connection reaches the peer before the abortive close.
        char response[1000];
        std::memset(response, 'r', sizeof(response));
        ASSERT_EQ(static_cast<ssize_t>(sizeof(response)),
                  write(pair.server_fd.Get(), response, sizeof(response)));
        ASSERT_EQ(0, close(pair.server_fd.Release()));

        const timeval deadline{.tv_sec = 2, .tv_usec = 0};
        ASSERT_EQ(0, setsockopt(pair.client_fd.Get(), SOL_SOCKET, SO_RCVTIMEO,
                                &deadline, sizeof(deadline)));
        char received[sizeof(response)] = {};
        size_t total = 0;
        while (total < sizeof(received)) {
            const ssize_t n =
                read(pair.client_fd.Get(), received + total, sizeof(received) - total);
            ASSERT_GT(n, 0) << ErrnoString(errno);
            total += static_cast<size_t>(n);
        }
        EXPECT_EQ(0, std::memcmp(response, received, sizeof(received)));
        char extra;
        ASSERT_EQ(-1, read(pair.client_fd.Get(), &extra, sizeof(extra)));
        EXPECT_EQ(ECONNRESET, errno);
    }
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

void ExpectConcurrentResetDuringClose(bool (*init_pair)(TcpPair*)) {
    signal(SIGPIPE, SIG_IGN);

    constexpr int kThreadCount = 100;
    std::vector<pthread_t> threads(kThreadCount);
    std::vector<ResetDuringCloseArgs> args(kThreadCount, ResetDuringCloseArgs{init_pair});
    int started = 0;
    for (; started < kThreadCount; ++started) {
        const int error = pthread_create(&threads[started], nullptr, ResetDuringCloseWorker,
                                         &args[started]);
        if (error != 0) {
            ADD_FAILURE() << "pthread_create failed: " << ErrnoString(error);
            break;
        }
    }

    for (int i = 0; i < started; ++i) {
        void* result = nullptr;
        const int error = pthread_join(threads[i], &result);
        EXPECT_EQ(0, error) << "pthread_join failed: " << ErrnoString(error);
        if (error != 0) {
            continue;
        }
        if (result == reinterpret_cast<void*>(kPollDuringCloseFailed)) {
            const auto& observation = args[i];
            ADD_FAILURE() << "worker " << i << ": poll returned " << observation.poll_result
                          << ", errno=" << observation.poll_errno
                          << ", revents=" << observation.poll_revents
                          << ", elapsed_ms=" << observation.poll_elapsed.count()
                          << ", queued_bytes=" << observation.queued_bytes
                          << ", fionread_errno=" << observation.fionread_errno
                          << ", tcp_state=" << observation.tcp_state
                          << ", tcp_info_length=" << observation.tcp_info_length
                          << ", tcp_info_errno=" << observation.tcp_info_errno;
        } else {
            EXPECT_EQ(nullptr, result) << "worker " << i << " failed";
        }
    }

    if (started == kThreadCount) {
        ExpectMmSignalPathResponsiveAfterTcpStorm();
    }
}

TEST(TcpCloseSemantics, ConcurrentResetDuringCloseDoesNotStall) {
    ExpectConcurrentResetDuringClose(InitTcpPair);
}

TEST(TcpCloseSemantics, ReversedDualStackResetDuringCloseDoesNotStall) {
    ExpectConcurrentResetDuringClose(InitDualStackTcpPair);
}

TEST(TcpCloseSemantics, ColdNamespaceConcurrentBindAndConnectAdmission) {
    // A private namespace starts with an empty protocol table. Keep every
    // successful pair alive until all attempts complete so close cannot hide
    // a prepare/commit capacity race by returning vacancies early.
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(30);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0)
            _exit(errno == EPERM ? 77 : 1);
        FdGuard control(socket(AF_INET, SOCK_STREAM, 0));
        if (control.Get() < 0) _exit(2);
        ifreq request {};
        strcpy(request.ifr_name, "lo");
        if (ioctl(control.Get(), SIOCGIFFLAGS, &request) != 0) _exit(3);
        request.ifr_flags |= IFF_UP;
        if (ioctl(control.Get(), SIOCSIFFLAGS, &request) != 0) _exit(4);
        struct Admission {
            std::atomic<bool> start {false};
            std::atomic<int> arrived {0};
            int workers = 0;
        } admission;
        auto worker = [](void* argument) -> void* {
            auto* admission = static_cast<Admission*>(argument);
            while (!admission->start.load(std::memory_order_acquire)) sched_yield();
            TcpPair pair;
            bool success = InitTcpPair(&pair);
            admission->arrived.fetch_add(1, std::memory_order_release);
            while (admission->arrived.load(std::memory_order_acquire) != admission->workers)
                sched_yield();
            return success ? nullptr : reinterpret_cast<void*>(1);
        };
        constexpr int kWorkers = 100;
        pthread_t threads[kWorkers] {};
        while (admission.workers < kWorkers) {
            if (pthread_create(&threads[admission.workers], nullptr, worker, &admission) != 0)
                break;
            ++admission.workers;
        }
        admission.start.store(true, std::memory_order_release);
        bool success = admission.workers == kWorkers;
        for (int i = 0; i < admission.workers; ++i) {
            void* result = nullptr;
            if (pthread_join(threads[i], &result) != 0 || result != nullptr) success = false;
        }
        _exit(success ? 0 : 5);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 77)
        GTEST_SKIP() << "user/network namespace creation is not permitted";
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
