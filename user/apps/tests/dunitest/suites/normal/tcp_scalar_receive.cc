#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <chrono>
#include <cstring>
#include <thread>
#include <vector>

namespace {

class Fd {
 public:
    ~Fd() { if (fd_ >= 0) close(fd_); }
    void Set(int fd) { fd_ = fd; }
    int Get() const { return fd_; }
 private:
    int fd_ = -1;
};

class Mapping {
 public:
    explicit Mapping(size_t length, int protection = PROT_READ | PROT_WRITE,
                     int sharing = MAP_PRIVATE)
        : length_(length), address_(mmap(nullptr, length, protection,
                                        sharing | MAP_ANONYMOUS, -1, 0)) {}
    ~Mapping() { if (address_ != MAP_FAILED) munmap(address_, length_); }
    char* Get() const { return static_cast<char*>(address_); }
 private:
    size_t length_;
    void* address_;
};

class TcpScalarReceive : public ::testing::Test {
 protected:
    void SetUp() override {
        Fd listener;
        listener.Set(socket(AF_INET, SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
        socklen_t length = sizeof(address);
        ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &length));
        ASSERT_EQ(0, listen(listener.Get(), 1));
        reader_.Set(socket(AF_INET, SOCK_STREAM, 0));
        ASSERT_GE(reader_.Get(), 0);
        ASSERT_EQ(0, connect(reader_.Get(), reinterpret_cast<sockaddr*>(&address), length));
        writer_.Set(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(writer_.Get(), 0);
        timeval timeout{3, 0};
        ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
        ASSERT_EQ(0, setsockopt(writer_.Get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)));
        page_ = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    }

    void Queue(const char* bytes, size_t size) {
        size_t sent = 0;
        while (sent < size) {
            const ssize_t n = send(writer_.Get(), bytes + sent, size - sent, MSG_NOSIGNAL);
            ASSERT_GT(n, 0) << errno;
            sent += n;
        }
        pollfd ready{reader_.Get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 3000));
    }

    void CheckQueued(const char* bytes, size_t size) {
        std::vector<char> actual(size);
        ASSERT_EQ(static_cast<ssize_t>(size), recv(reader_.Get(), actual.data(), size, MSG_WAITALL));
        EXPECT_EQ(0, memcmp(bytes, actual.data(), size));
    }

    Fd reader_, writer_;
    size_t page_ = 0;
};

TEST_F(TcpScalarReceive, FaultDoesNotConsumeQueuedData) {
    Mapping inaccessible(page_, PROT_NONE);
    ASSERT_NE(MAP_FAILED, inaccessible.Get());
    for (int flags : std::array<int, 2>{0, MSG_PEEK}) {
        SCOPED_TRACE(flags);
        ASSERT_NO_FATAL_FAILURE(Queue("abcd", 4));
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), inaccessible.Get(), 4, flags));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(CheckQueued("abcd", 4));
    }
}

TEST_F(TcpScalarReceive, FaultWithinOneCopyDoesNotCommitReadablePrefix) {
    Mapping output(2 * page_);
    ASSERT_NE(MAP_FAILED, output.Get());
    ASSERT_EQ(0, mprotect(output.Get() + page_, page_, PROT_NONE));
    // One tiny send avoids depending on a specific MSS or large-buffer split.
    for (int flags : std::array<int, 2>{0, MSG_PEEK}) {
        SCOPED_TRACE(flags);
        ASSERT_NO_FATAL_FAILURE(Queue("abcdefgh", 8));
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), output.Get() + page_ - 4, 8, flags));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(CheckQueued("abcdefgh", 8));
    }
}

TEST_F(TcpScalarReceive, TruncNeverTouchesInaccessiblePayload) {
    Mapping inaccessible(page_, PROT_NONE);
    ASSERT_NE(MAP_FAILED, inaccessible.Get());
    ASSERT_NO_FATAL_FAILURE(Queue("abcd", 4));
    EXPECT_EQ(4, recv(reader_.Get(), inaccessible.Get(), 4, MSG_TRUNC | MSG_PEEK));
    EXPECT_EQ(4, recv(reader_.Get(), inaccessible.Get(), 4, MSG_TRUNC));
    char byte;
    EXPECT_EQ(-1, recv(reader_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_F(TcpScalarReceive, EmptyQueueChecksReadinessBeforePayload) {
    Mapping inaccessible(page_, PROT_NONE);
    ASSERT_NE(MAP_FAILED, inaccessible.Get());
    for (int flags : std::array<int, 4>{0, MSG_PEEK, MSG_TRUNC, MSG_PEEK | MSG_TRUNC}) {
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), inaccessible.Get(), 1, flags | MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
    ASSERT_EQ(0, shutdown(writer_.Get(), SHUT_WR));
    EXPECT_EQ(0, recv(reader_.Get(), inaccessible.Get(), 1, 0));
}

TEST_F(TcpScalarReceive, NullPayloadUsesQueueAndCopySemantics) {
    for (int flags : std::array<int, 4>{0, MSG_PEEK, MSG_TRUNC, MSG_PEEK | MSG_TRUNC}) {
        SCOPED_TRACE(flags);
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), nullptr, 1, flags | MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
    for (int flags : std::array<int, 2>{0, MSG_PEEK}) {
        SCOPED_TRACE(flags);
        ASSERT_NO_FATAL_FAILURE(Queue("abcd", 4));
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), nullptr, 4, flags));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(CheckQueued("abcd", 4));
    }
    ASSERT_NO_FATAL_FAILURE(Queue("abcd", 4));
    EXPECT_EQ(4, recv(reader_.Get(), nullptr, 4, MSG_TRUNC | MSG_PEEK));
    EXPECT_EQ(4, recv(reader_.Get(), nullptr, 4, MSG_TRUNC));
    char byte;
    errno = 0;
    EXPECT_EQ(-1, recv(reader_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    ASSERT_EQ(0, shutdown(writer_.Get(), SHUT_WR));
    for (int flags : std::array<int, 4>{0, MSG_PEEK, MSG_TRUNC, MSG_PEEK | MSG_TRUNC}) {
        SCOPED_TRACE(flags);
        EXPECT_EQ(0, recv(reader_.Get(), nullptr, 1, flags));
    }
}

TEST_F(TcpScalarReceive, ZeroCapacityStillObservesEmptyAndQueuedState) {
    char byte;
    errno = 0;
    EXPECT_EQ(-1, recv(reader_.Get(), &byte, 0, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    ASSERT_NO_FATAL_FAILURE(Queue("z", 1));
    EXPECT_EQ(0, recv(reader_.Get(), &byte, 0, MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(CheckQueued("z", 1));
    ASSERT_EQ(0, shutdown(writer_.Get(), SHUT_WR));
    EXPECT_EQ(0, recv(reader_.Get(), &byte, 0, 0));
}

TEST_F(TcpScalarReceive, EmptyReadAndMessageReceiveKeepDistinctSemantics) {
    char byte;
    iovec vector{&byte, 0};
    msghdr message{};
    message.msg_iov = &vector;
    message.msg_iovlen = 1;
    EXPECT_EQ(0, read(reader_.Get(), &byte, 0));
    EXPECT_EQ(0, readv(reader_.Get(), &vector, 1));
    errno = 0;
    EXPECT_EQ(-1, recvmsg(reader_.Get(), &message, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    ASSERT_NO_FATAL_FAILURE(Queue("z", 1));
    EXPECT_EQ(0, recvmsg(reader_.Get(), &message, MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(CheckQueued("z", 1));
    ASSERT_EQ(0, shutdown(writer_.Get(), SHUT_WR));
    EXPECT_EQ(0, recvmsg(reader_.Get(), &message, 0));
}

TEST_F(TcpScalarReceive, FreshAnonymousPagesCanFaultDuringCopy) {
    Mapping output(2 * page_);
    ASSERT_NE(MAP_FAILED, output.Get());
    std::vector<char> expected(2 * page_, 'p');
    ASSERT_NO_FATAL_FAILURE(Queue(expected.data(), expected.size()));
    ASSERT_EQ(static_cast<ssize_t>(expected.size()),
              recv(reader_.Get(), output.Get(), expected.size(), MSG_WAITALL));
    EXPECT_EQ(0, memcmp(output.Get(), expected.data(), expected.size()));
}

TEST_F(TcpScalarReceive, PrivateForkPagesKeepParentsContents) {
    Mapping output(page_);
    ASSERT_NE(MAP_FAILED, output.Get());
    memset(output.Get(), '!', page_);
    ASSERT_NO_FATAL_FAILURE(Queue("abcd", 4));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const ssize_t n = recv(reader_.Get(), output.Get(), 4, MSG_WAITALL);
        _exit(n == 4 && memcmp(output.Get(), "abcd", 4) == 0 ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, memcmp(output.Get(), "!!!!", 4));
}

TEST_F(TcpScalarReceive, ProtectionChangeWhileWaitingDoesNotConsumeData) {
    Mapping output(page_);
    ASSERT_NE(MAP_FAILED, output.Get());
    std::atomic<bool> entering{false};
    ssize_t received = 0;
    int receive_error = 0;
    std::thread receiver([&] {
        entering.store(true, std::memory_order_release);
        received = recv(reader_.Get(), output.Get(), 4, 0);
        receive_error = errno;
    });
    while (!entering.load(std::memory_order_acquire)) std::this_thread::yield();
    // No data is available before protection changes. Regardless of whether
    // recv has entered its wait, the later payload must use current mappings.
    const int protected_result = mprotect(output.Get(), page_, PROT_NONE);
    EXPECT_EQ(0, protected_result);
    EXPECT_EQ(4, send(writer_.Get(), "abcd", 4, MSG_NOSIGNAL));
    receiver.join();
    EXPECT_EQ(-1, received);
    EXPECT_EQ(EFAULT, receive_error);
    ASSERT_NO_FATAL_FAILURE(CheckQueued("abcd", 4));
}

// A large userspace capacity is not evidence that the stream has that much
// data. Exercise every entry sharing the protected copy/consume engine.
TEST_F(TcpScalarReceive, ShortQueueWithLargeCapacityAcrossReceiveEntries) {
    constexpr size_t kCapacity = 256 * 1024;
    Mapping output(kCapacity);
    ASSERT_NE(MAP_FAILED, output.Get());
    constexpr char payload[] = "short-payload";
    constexpr size_t kLength = sizeof(payload) - 1;
    for (int entry = 0; entry < 4; ++entry) {
        SCOPED_TRACE(entry);
        memset(output.Get(), '!', kCapacity);
        ASSERT_NO_FATAL_FAILURE(Queue(payload, kLength));
        iovec vectors[] = {{output.Get(), 3}, {output.Get() + 3, kCapacity - 3}};
        msghdr message{};
        message.msg_iov = vectors;
        message.msg_iovlen = 2;
        ssize_t count;
        switch (entry) {
            case 0: count = recv(reader_.Get(), output.Get(), kCapacity, MSG_DONTWAIT); break;
            case 1: count = read(reader_.Get(), output.Get(), kCapacity); break;
            case 2: count = readv(reader_.Get(), vectors, 2); break;
            default: count = recvmsg(reader_.Get(), &message, MSG_DONTWAIT); break;
        }
        ASSERT_EQ(static_cast<ssize_t>(kLength), count) << strerror(errno);
        EXPECT_EQ(0, memcmp(output.Get(), payload, kLength));
        EXPECT_EQ('!', output.Get()[kLength]);
        char byte;
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), &byte, 1, MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
}

TEST_F(TcpScalarReceive, WaitallPreservesPrefixAcrossLaterStorageGrowth) {
    constexpr size_t kSuffix = 128 * 1024;
    Mapping output(kSuffix + 1, PROT_READ | PROT_WRITE, MAP_SHARED);
    ASSERT_NE(MAP_FAILED, output.Get());
    memset(output.Get(), '!', kSuffix + 1);
    ASSERT_NO_FATAL_FAILURE(Queue("a", 1));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        while (__atomic_load_n(output.Get(), __ATOMIC_RELAXED) != 'a' &&
               std::chrono::steady_clock::now() < deadline) usleep(1000);
        if (__atomic_load_n(output.Get(), __ATOMIC_RELAXED) != 'a') _exit(1);
        std::vector<char> suffix(kSuffix, 'b');
        size_t total = 0;
        while (total < suffix.size()) {
            const ssize_t count = send(writer_.Get(), suffix.data() + total,
                                       suffix.size() - total, MSG_NOSIGNAL);
            if (count <= 0) _exit(2);
            total += static_cast<size_t>(count);
        }
        _exit(0);
    }
    const ssize_t count = recv(reader_.Get(), output.Get(), kSuffix + 1, MSG_WAITALL);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(static_cast<ssize_t>(kSuffix + 1), count) << strerror(errno);
    EXPECT_EQ('a', output.Get()[0]);
    for (size_t i = 1; i <= kSuffix; ++i) ASSERT_EQ('b', output.Get()[i]) << i;
}

class TcpScalarPartial : public TcpScalarReceive,
                         public ::testing::WithParamInterface<int> {};

TEST_P(TcpScalarPartial, FaultAfterCompletedWindowReturnsOnlyCompletedBytes) {
    constexpr size_t kPrefix = 64 * 1024;
    Mapping output(kPrefix + page_, PROT_READ | PROT_WRITE, MAP_SHARED);
    ASSERT_NE(MAP_FAILED, output.Get());
    ASSERT_EQ(0, mprotect(output.Get() + kPrefix, page_, PROT_NONE));
    memset(output.Get(), '!', kPrefix);
    int capacity = 256 * 1024;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVBUF, &capacity, sizeof(capacity)));
    std::vector<char> expected(kPrefix, 'a');
    ASSERT_NO_FATAL_FAILURE(Queue(expected.data(), expected.size()));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        while (__atomic_load_n(output.Get() + kPrefix - 1, __ATOMIC_RELAXED) != 'a' &&
               std::chrono::steady_clock::now() < deadline) usleep(1000);
        if (__atomic_load_n(output.Get() + kPrefix - 1, __ATOMIC_RELAXED) != 'a') _exit(1);
        _exit(send(writer_.Get(), "z", 1, MSG_NOSIGNAL) == 1 ? 0 : 2);
    }
    const ssize_t received = recv(reader_.Get(), output.Get(), kPrefix + 1,
                                  MSG_WAITALL | GetParam());
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(static_cast<ssize_t>(kPrefix), received) << errno;
    EXPECT_EQ(0, memcmp(output.Get(), expected.data(), kPrefix));
    if (GetParam() & MSG_PEEK) {
        expected.push_back('z');
        ASSERT_NO_FATAL_FAILURE(CheckQueued(expected.data(), expected.size()));
    } else {
        ASSERT_NO_FATAL_FAILURE(CheckQueued("z", 1));
    }
}

INSTANTIATE_TEST_SUITE_P(Flags, TcpScalarPartial, ::testing::Values(0, MSG_PEEK));

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
