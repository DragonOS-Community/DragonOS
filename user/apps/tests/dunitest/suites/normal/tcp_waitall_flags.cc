#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstring>
#include <thread>
#include <vector>

namespace {

class Fd {
 public:
    ~Fd() { Reset(); }
    int Get() const { return fd_; }
    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }
 private:
    int fd_ = -1;
};

constexpr std::array<int, 4> kFlags{
    MSG_WAITALL, MSG_WAITALL | MSG_PEEK, MSG_WAITALL | MSG_TRUNC,
    MSG_WAITALL | MSG_PEEK | MSG_TRUNC};

class TcpWaitallFlags : public ::testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        Fd listener;
        listener.Reset(socket(AF_INET, SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0) << errno;
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address),
                          sizeof(address))) << errno;
        socklen_t length = sizeof(address);
        ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address),
                                 &length)) << errno;
        ASSERT_EQ(0, listen(listener.Get(), 1)) << errno;
        reader_.Reset(socket(AF_INET, SOCK_STREAM, 0));
        ASSERT_GE(reader_.Get(), 0) << errno;
        timeval timeout{3, 0};
        ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVTIMEO,
                               &timeout, sizeof(timeout))) << errno;
        ASSERT_EQ(0, connect(reader_.Get(), reinterpret_cast<sockaddr*>(&address),
                             length)) << errno;
        pollfd ready{listener.Get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 3000));
        writer_.Reset(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(writer_.Get(), 0) << errno;
        ASSERT_EQ(0, setsockopt(writer_.Get(), SOL_SOCKET, SO_SNDTIMEO,
                               &timeout, sizeof(timeout))) << errno;
    }

    void Queue(const char* bytes, size_t length) {
        ASSERT_EQ(static_cast<ssize_t>(length),
                  send(writer_.Get(), bytes, length, MSG_NOSIGNAL)) << errno;
        pollfd ready{reader_.Get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 3000)) << errno;
        ASSERT_NE(0, ready.revents & POLLIN);
    }

    void ShortTimeout() {
        timeval timeout{0, 250000};
        ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVTIMEO,
                               &timeout, sizeof(timeout))) << errno;
    }

    void CheckPayload(const char* bytes, size_t length) {
        if (GetParam() & MSG_TRUNC) {
            EXPECT_TRUE(std::all_of(bytes, bytes + 4, [](char c) { return c == '!'; }));
        } else {
            EXPECT_EQ(0, memcmp(bytes, "abcd", length));
        }
    }

    void CheckQueue(const char* expected, size_t length, bool eof = false) {
        char bytes[16]{};
        errno = 0;
        // A full WAITALL target can complete before the peer's FIN arrives.
        // EOF uses the fixture's finite timeout, not a premature DONTWAIT probe.
        const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), eof ? 0 : MSG_DONTWAIT);
        if (length != 0) {
            ASSERT_EQ(static_cast<ssize_t>(length), result) << errno;
            EXPECT_EQ(0, memcmp(bytes, expected, length));
            if (eof) {
                EXPECT_EQ(0, recv(reader_.Get(), bytes, sizeof(bytes), 0)) << errno;
            }
        } else if (eof) {
            EXPECT_EQ(0, result) << errno;
        } else {
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EAGAIN, errno);
        }
    }

    Fd reader_;
    Fd writer_;
};

TEST_P(TcpWaitallFlags, DelayedSecondPartSatisfiesWholeTarget) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    std::thread sender([&] {
        usleep(80000);
        EXPECT_EQ(2, send(writer_.Get(), "cd", 2, MSG_NOSIGNAL)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), GetParam());
    sender.join();
    ASSERT_EQ(4, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 4));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("abcd", GetParam() & MSG_PEEK ? 4 : 0));
}

TEST_P(TcpWaitallFlags, FinReturnsPrefixWithoutRepeatingPeek) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    std::thread sender([&] {
        usleep(80000);
        EXPECT_EQ(0, shutdown(writer_.Get(), SHUT_WR)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), GetParam());
    sender.join();
    ASSERT_EQ(2, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0, true));
}

TEST_P(TcpWaitallFlags, FinArrivingWithLastPartDoesNotResetPeekCursor) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    std::thread sender([&] {
        usleep(80000);
        EXPECT_EQ(2, send(writer_.Get(), "cd", 2, MSG_NOSIGNAL)) << errno;
        EXPECT_EQ(0, shutdown(writer_.Get(), SHUT_WR)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), GetParam());
    sender.join();
    ASSERT_EQ(4, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 4));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("abcd", GetParam() & MSG_PEEK ? 4 : 0, true));
}

TEST_P(TcpWaitallFlags, TimeoutReturnsPrefixAfterWaiting) {
    ASSERT_NO_FATAL_FAILURE(ShortTimeout());
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    char bytes[4] = {'!', '!', '!', '!'};
    const auto start = std::chrono::steady_clock::now();
    const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), GetParam());
    const auto elapsed = std::chrono::steady_clock::now() - start;
    ASSERT_EQ(2, result) << errno;
    // A broad lower bound distinguishes an immediate short read from the
    // configured wait. There is no scheduler-dependent upper-bound assertion.
    EXPECT_GE(elapsed, std::chrono::milliseconds(100));
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0));
}

TEST_P(TcpWaitallFlags, EmptyTimeoutReturnsAgain) {
    ASSERT_NO_FATAL_FAILURE(ShortTimeout());
    char bytes[4]{};
    errno = 0;
    EXPECT_EQ(-1, recv(reader_.Get(), bytes, sizeof(bytes), GetParam()));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(TcpWaitallFlags, DontwaitReturnsQueuedPrefix) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    char bytes[4] = {'!', '!', '!', '!'};
    ASSERT_EQ(2, recv(reader_.Get(), bytes, sizeof(bytes), GetParam() | MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0));
}

TEST_P(TcpWaitallFlags, NonblockingEmptyReturnsAgain) {
    ASSERT_EQ(0, fcntl(reader_.Get(), F_SETFL, O_NONBLOCK)) << errno;
    char bytes[4]{};
    errno = 0;
    EXPECT_EQ(-1, recv(reader_.Get(), bytes, sizeof(bytes), GetParam()));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(TcpWaitallFlags, VectoredTargetIncludesAllIovecs) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    std::thread sender([&] {
        usleep(80000);
        EXPECT_EQ(2, send(writer_.Get(), "cd", 2, MSG_NOSIGNAL)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    iovec iov[] = {{bytes, 1}, {bytes + 1, 0}, {bytes + 1, 2}, {bytes + 3, 1}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    const ssize_t result = recvmsg(reader_.Get(), &message, GetParam());
    sender.join();
    ASSERT_EQ(4, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 4));
    EXPECT_EQ(0, message.msg_flags);
    ASSERT_NO_FATAL_FAILURE(CheckQueue("abcd", GetParam() & MSG_PEEK ? 4 : 0));
}

TEST_P(TcpWaitallFlags, ThreePartsNeverCountAnOldPeekPrefixTwice) {
    ASSERT_NO_FATAL_FAILURE(Queue("a", 1));
    std::thread sender([&] {
        usleep(50000);
        EXPECT_EQ(1, send(writer_.Get(), "b", 1, MSG_NOSIGNAL)) << errno;
        usleep(50000);
        EXPECT_EQ(2, send(writer_.Get(), "cd", 2, MSG_NOSIGNAL)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    const ssize_t result = recv(reader_.Get(), bytes, sizeof(bytes), GetParam());
    sender.join();
    ASSERT_EQ(4, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 4));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("abcd", GetParam() & MSG_PEEK ? 4 : 0));
}

TEST_P(TcpWaitallFlags, VectoredTimeoutPreservesReturnedPrefix) {
    ASSERT_NO_FATAL_FAILURE(ShortTimeout());
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    char bytes[4] = {'!', '!', '!', '!'};
    iovec iov[] = {{bytes, 1}, {bytes + 1, 3}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    const auto start = std::chrono::steady_clock::now();
    const ssize_t result = recvmsg(reader_.Get(), &message, GetParam());
    const auto elapsed = std::chrono::steady_clock::now() - start;
    ASSERT_EQ(2, result) << errno;
    EXPECT_GE(elapsed, std::chrono::milliseconds(100));
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0));
}

TEST_P(TcpWaitallFlags, VectoredFinReturnsOnlyAvailablePrefix) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    std::thread sender([&] {
        usleep(80000);
        EXPECT_EQ(0, shutdown(writer_.Get(), SHUT_WR)) << errno;
    });
    char bytes[4] = {'!', '!', '!', '!'};
    iovec iov[] = {{bytes, 1}, {bytes + 1, 3}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    const ssize_t result = recvmsg(reader_.Get(), &message, GetParam());
    sender.join();
    ASSERT_EQ(2, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0, true));
}

TEST_P(TcpWaitallFlags, VectoredTargetCrossesStagingWindow) {
    int capacity = 256 * 1024;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVBUF,
                           &capacity, sizeof(capacity))) << errno;
    ASSERT_EQ(0, setsockopt(writer_.Get(), SOL_SOCKET, SO_SNDBUF,
                           &capacity, sizeof(capacity))) << errno;
    constexpr size_t kSize = 96 * 1024;
    std::vector<char> expected(kSize);
    for (size_t i = 0; i < expected.size(); ++i) expected[i] = 'a' + (i % 23);
    std::vector<char> bytes(kSize, '!');
    std::thread sender([&] {
        size_t offset = 0;
        while (offset < expected.size()) {
            const size_t batch = std::min<size_t>(32 * 1024, expected.size() - offset);
            const ssize_t sent = send(writer_.Get(), expected.data() + offset,
                                      batch, MSG_NOSIGNAL);
            if (sent <= 0) {
                ADD_FAILURE() << "large sender failed: " << errno;
                shutdown(writer_.Get(), SHUT_WR);
                return;
            }
            offset += static_cast<size_t>(sent);
            if (offset < expected.size()) usleep(20000);
        }
    });
    iovec iov[] = {{bytes.data(), 65533}, {bytes.data() + 65533, kSize - 65533}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    const ssize_t result = recvmsg(reader_.Get(), &message, GetParam());
    sender.join();
    ASSERT_EQ(static_cast<ssize_t>(kSize), result) << errno;
    if (GetParam() & MSG_TRUNC) {
        EXPECT_TRUE(std::all_of(bytes.begin(), bytes.end(), [](char c) { return c == '!'; }));
    } else {
        EXPECT_EQ(expected, bytes);
    }
    if (GetParam() & MSG_PEEK) {
        size_t offset = 0;
        while (offset < bytes.size()) {
            const ssize_t received = recv(reader_.Get(), bytes.data() + offset,
                                          bytes.size() - offset, MSG_DONTWAIT);
            ASSERT_GT(received, 0) << errno;
            offset += static_cast<size_t>(received);
        }
        EXPECT_EQ(expected, bytes);
    }
    ASSERT_NO_FATAL_FAILURE(CheckQueue("", 0));
}

INSTANTIATE_TEST_SUITE_P(Flags, TcpWaitallFlags, ::testing::ValuesIn(kFlags));

class TcpTruncWaitall : public TcpWaitallFlags {};

TEST_P(TcpTruncWaitall, InaccessibleScalarPayloadIsNeverTouched) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    void* inaccessible = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    EXPECT_EQ(2, recv(reader_.Get(), inaccessible, 2, GetParam())) << errno;
    EXPECT_EQ(0, munmap(inaccessible, 4096));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0));
}

TEST_P(TcpTruncWaitall, InaccessibleVectoredPayloadIsNeverTouched) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    void* inaccessible = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    iovec iov[] = {{inaccessible, 1}, {inaccessible, 1}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    EXPECT_EQ(2, recvmsg(reader_.Get(), &message, GetParam())) << errno;
    EXPECT_EQ(0, munmap(inaccessible, 4096));
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0));
}

INSTANTIATE_TEST_SUITE_P(Trunc, TcpTruncWaitall,
                        ::testing::Values(MSG_WAITALL | MSG_TRUNC,
                                          MSG_WAITALL | MSG_PEEK | MSG_TRUNC));

class TcpPeekWaitall : public TcpWaitallFlags {};

// The destination lives in shared memory so the parent can observe the
// child's already-copied prefix without racing a C++ thread or guessing a
// scheduling delay. Linux resets its private peek cursor when a competing
// consumer moves the receive head, retaining the prefix already copied.
TEST_P(TcpPeekWaitall, CompetingReaderMovesHeadWhilePeekWaits) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* bytes = static_cast<char*>(mapping);
    memset(bytes, '!', 4);
    int report[2];
    ASSERT_EQ(0, pipe(report));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(report[0]);
        const ssize_t result = recv(reader_.Get(), bytes, 4, MSG_WAITALL | MSG_PEEK);
        const ssize_t reported = write(report[1], &result, sizeof(result));
        _exit(reported == static_cast<ssize_t>(sizeof(result)) ? 0 : 1);
    }
    close(report[1]);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    const auto* visible = static_cast<volatile const char*>(mapping);
    while ((visible[0] != 'a' || visible[1] != 'b') &&
           std::chrono::steady_clock::now() < deadline) {
        usleep(1000);
    }
    const bool prefix_visible = visible[0] == 'a' && visible[1] == 'b';
    if (prefix_visible) {
        char consumed[2]{};
        EXPECT_EQ(2, recv(reader_.Get(), consumed, 2, MSG_DONTWAIT));
        EXPECT_EQ(0, memcmp(consumed, "ab", 2));
        EXPECT_EQ(2, send(writer_.Get(), "cd", 2, MSG_NOSIGNAL));
    }
    pollfd done{report[0], POLLIN, 0};
    const int ready = prefix_visible ? poll(&done, 1, 4000) : 0;
    if (ready != 1) kill(child, SIGKILL);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    ssize_t result = -1;
    if (ready == 1) {
        EXPECT_EQ(static_cast<ssize_t>(sizeof(result)),
                  read(report[0], &result, sizeof(result)));
    }
    close(report[0]);
    EXPECT_TRUE(prefix_visible) << "peek must publish its first prefix before waiting";
    EXPECT_EQ(1, ready) << "peek must wake for a moved head and new data";
    if (ready == 1) {
        EXPECT_EQ(4, result);
        EXPECT_EQ(0, memcmp(bytes, "abcd", 4));
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
        ASSERT_NO_FATAL_FAILURE(CheckQueue("cd", 2));
    }
    EXPECT_EQ(0, munmap(mapping, 4096));
}

TEST_P(TcpPeekWaitall, UserFaultAfterStagingWindowReturnsCopiedPrefix) {
    constexpr size_t kPrefix = 64 * 1024;
    int capacity = 256 * 1024;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVBUF,
                           &capacity, sizeof(capacity)));
    void* mapping = mmap(nullptr, kPrefix, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    void* inaccessible = mmap(nullptr, 4096, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    auto* bytes = static_cast<char*>(mapping);
    memset(bytes, '!', kPrefix);
    std::vector<char> expected(kPrefix, 'a');
    ASSERT_NO_FATAL_FAILURE(Queue(expected.data(), expected.size()));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        const auto* visible = static_cast<volatile const char*>(mapping);
        while (visible[kPrefix - 1] != 'a' &&
               std::chrono::steady_clock::now() < deadline) usleep(1000);
        if (visible[kPrefix - 1] != 'a') _exit(1);
        _exit(send(writer_.Get(), "z", 1, MSG_NOSIGNAL) == 1 ? 0 : 2);
    }
    iovec iov[] = {{bytes, kPrefix}, {inaccessible, 1}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    const ssize_t result = recvmsg(reader_.Get(), &message, MSG_WAITALL | MSG_PEEK);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(static_cast<ssize_t>(kPrefix), result) << errno;
    EXPECT_EQ(0, memcmp(bytes, expected.data(), kPrefix));
    // PEEK's successful prefix and the faulting tail remain in the queue.
    std::vector<char> queued(kPrefix + 1);
    size_t offset = 0;
    while (offset < queued.size()) {
        const ssize_t received = recv(reader_.Get(), queued.data() + offset,
                                      queued.size() - offset, MSG_DONTWAIT);
        if (received <= 0) {
            ADD_FAILURE() << "peek lost queued prefix/tail: " << errno;
            break;
        }
        offset += static_cast<size_t>(received);
    }
    EXPECT_EQ(queued.size(), offset);
    EXPECT_EQ(0, memcmp(queued.data(), expected.data(), kPrefix));
    EXPECT_EQ('z', queued.back());
    EXPECT_EQ(0, munmap(inaccessible, 4096));
    EXPECT_EQ(0, munmap(mapping, kPrefix));
}

INSTANTIATE_TEST_SUITE_P(Peek, TcpPeekWaitall,
                        ::testing::Values(MSG_WAITALL | MSG_PEEK));

class TcpWaitallReset : public TcpWaitallFlags {};

TEST_P(TcpWaitallReset, SuccessfulPrefixLeavesResetForNextObserver) {
    ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* bytes = static_cast<char*>(mapping);
    memset(bytes, '!', 4);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        bool progressed = false;
        while (std::chrono::steady_clock::now() < deadline) {
            if (GetParam() & MSG_TRUNC) {
                int queued = -1;
                if (ioctl(reader_.Get(), FIONREAD, &queued) == 0 && queued == 0) {
                    progressed = true;
                    break;
                }
            } else {
                const auto* visible = static_cast<volatile const char*>(mapping);
                if (visible[0] == 'a' && visible[1] == 'b') {
                    progressed = true;
                    break;
                }
            }
            usleep(1000);
        }
        if (!progressed) _exit(1);
        if (GetParam() & MSG_TRUNC) {
            // A second receive must acquire recv_lock after the initial
            // consuming attempt released it. This places RST after that
            // attempt, exercising preservation across WAITALL retries.
            char probe;
            errno = 0;
            if (recv(reader_.Get(), &probe, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN)
                _exit(3);
        }
        linger abort{1, 0};
        if (setsockopt(writer_.Get(), SOL_SOCKET, SO_LINGER, &abort, sizeof(abort)) != 0)
            _exit(2);
        writer_.Reset();
        _exit(0);
    }
    // Only the child retains the peer descriptor, so its abort publishes RST.
    writer_.Reset();
    const ssize_t result = recv(reader_.Get(), bytes, 4, GetParam());
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(2, result) << errno;
    ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
    int error = -1;
    socklen_t length = sizeof(error);
    EXPECT_EQ(0, getsockopt(reader_.Get(), SOL_SOCKET, SO_ERROR, &error, &length));
    EXPECT_EQ(ECONNRESET, error) << "a short successful receive must preserve the error";
    error = -1;
    EXPECT_EQ(0, getsockopt(reader_.Get(), SOL_SOCKET, SO_ERROR, &error, &length));
    EXPECT_EQ(0, error);
    ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", GetParam() & MSG_PEEK ? 2 : 0, true));
    EXPECT_EQ(0, munmap(mapping, 4096));
}

INSTANTIATE_TEST_SUITE_P(Reset, TcpWaitallReset,
                        ::testing::Values(MSG_WAITALL, MSG_WAITALL | MSG_PEEK,
                                          MSG_WAITALL | MSG_TRUNC));

class TcpQueuedPeek : public TcpWaitallFlags {};

TEST_P(TcpQueuedPeek, AlreadyQueuedBytesCrossStagingWindowWithoutExtraWait) {
    constexpr size_t kSize = 96 * 1024;
    int capacity = 256 * 1024;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVBUF,
                           &capacity, sizeof(capacity))) << errno;
    ASSERT_EQ(0, setsockopt(writer_.Get(), SOL_SOCKET, SO_SNDBUF,
                           &capacity, sizeof(capacity))) << errno;
    std::vector<char> expected(kSize);
    for (size_t i = 0; i < expected.size(); ++i) expected[i] = 'a' + (i % 23);
    size_t sent = 0;
    while (sent < expected.size()) {
        const ssize_t result = send(writer_.Get(), expected.data() + sent,
                                    expected.size() - sent, MSG_NOSIGNAL);
        ASSERT_GT(result, 0) << errno;
        sent += static_cast<size_t>(result);
    }
    // Readiness alone may describe only an initial segment. Confirm the whole
    // request is queued before testing the per-attempt staging boundary.
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    int queued = 0;
    do {
        ASSERT_EQ(0, ioctl(reader_.Get(), FIONREAD, &queued)) << errno;
        if (queued == static_cast<int>(kSize)) break;
        usleep(1000);
    } while (std::chrono::steady_clock::now() < deadline);
    ASSERT_EQ(static_cast<int>(kSize), queued);
    std::vector<char> bytes(kSize, '!');
    iovec iov[] = {{bytes.data(), 65533}, {bytes.data() + 65533, kSize - 65533}};
    msghdr message{};
    message.msg_iov = iov;
    message.msg_iovlen = std::size(iov);
    ASSERT_EQ(static_cast<ssize_t>(kSize), recvmsg(reader_.Get(), &message, GetParam()))
        << errno;
    EXPECT_EQ(expected, bytes);
    EXPECT_EQ(0, message.msg_flags);
    ASSERT_EQ(0, ioctl(reader_.Get(), FIONREAD, &queued)) << errno;
    EXPECT_EQ(static_cast<int>(kSize), queued) << "peek must not consume";
    size_t received = 0;
    while (received < bytes.size()) {
        const ssize_t result = recv(reader_.Get(), bytes.data() + received,
                                    bytes.size() - received, MSG_DONTWAIT);
        ASSERT_GT(result, 0) << errno;
        received += static_cast<size_t>(result);
    }
    EXPECT_EQ(expected, bytes);
    ASSERT_NO_FATAL_FAILURE(CheckQueue("", 0));
}

INSTANTIATE_TEST_SUITE_P(Queued, TcpQueuedPeek,
                        ::testing::Values(MSG_PEEK, MSG_PEEK | MSG_DONTWAIT,
                                          MSG_WAITALL | MSG_PEEK | MSG_DONTWAIT));

volatile sig_atomic_t signal_count = 0;
void ReceiveSignal(int) { ++signal_count; }

class TcpWaitallSignal : public TcpWaitallFlags {
 protected:
    void CheckFiniteSignal(bool prefix) {
        timeval timeout{0, 500000};
        ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVTIMEO,
                               &timeout, sizeof(timeout)));
        struct sigaction action{};
        struct sigaction previous{};
        action.sa_handler = ReceiveSignal;
        action.sa_flags = SA_RESTART;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGUSR1, &action, &previous));
        signal_count = 0;
        if (prefix) {
            ASSERT_NO_FATAL_FAILURE(Queue("ab", 2));
        }
        const pid_t receiver = getpid();
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            // Repeat a bounded number of signals so the check does not rely on
            // one signal arriving after the parent enters the receive syscall.
            for (int i = 0; i < 5; ++i) {
                usleep(20000);
                if (kill(receiver, SIGUSR1) != 0) _exit(1);
            }
            _exit(0);
        }
        char bytes[4] = {'!', '!', '!', '!'};
        errno = 0;
        const ssize_t result = recv(reader_.Get(), bytes, 4, GetParam());
        const int receive_error = errno;
        int status = 0;
        pid_t reaped;
        do {
            reaped = waitpid(child, &status, 0);
        } while (reaped == -1 && errno == EINTR);
        EXPECT_EQ(child, reaped);
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
        EXPECT_EQ(0, sigaction(SIGUSR1, &previous, nullptr));
        EXPECT_GT(signal_count, 0);
        if (prefix) {
            EXPECT_EQ(2, result) << receive_error;
            ASSERT_NO_FATAL_FAILURE(CheckPayload(bytes, 2));
            ASSERT_NO_FATAL_FAILURE(CheckQueue("ab", 2));
        } else {
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EINTR, receive_error) << "finite timeout must defeat SA_RESTART";
        }
    }
};

TEST_P(TcpWaitallSignal, FiniteTimeoutInterruptsEvenWithRestartHandler) {
    ASSERT_NO_FATAL_FAILURE(CheckFiniteSignal(false));
}

TEST_P(TcpWaitallSignal, FiniteSignalReturnsAlreadyReceivedPrefix) {
    ASSERT_NO_FATAL_FAILURE(CheckFiniteSignal(true));
}

INSTANTIATE_TEST_SUITE_P(Signal, TcpWaitallSignal,
                        ::testing::Values(MSG_WAITALL | MSG_PEEK,
                                          MSG_WAITALL | MSG_PEEK | MSG_TRUNC));

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
