#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <poll.h>
#include <fcntl.h>
#include <atomic>
#include <thread>
#include <vector>
#include <sys/mman.h>
#include <sys/socket.h>
#include <chrono>
#include <unistd.h>

#include <cerrno>
#include <cstring>

namespace {
// These mappings reserve address space only. A tiny queued record must not
// cause the kernel to allocate payload storage for the advertised capacity.
constexpr size_t kLargeCapacity = size_t{2} * 1024 * 1024 * 1024;
class RecvmsgRecordBuffer : public testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        const int kind = GetParam();
        if (kind < 2) {
            ASSERT_EQ(0, socketpair(AF_UNIX, kind == 0 ? SOCK_DGRAM : SOCK_SEQPACKET,
                                   0, descriptors_));
        } else {
            const int family = kind == 2 ? AF_INET : AF_INET6;
            descriptors_[0] = socket(family, SOCK_DGRAM, 0);
            descriptors_[1] = socket(family, SOCK_DGRAM, 0);
            ASSERT_GE(descriptors_[0], 0);
            ASSERT_GE(descriptors_[1], 0);
            sockaddr_storage address{};
            socklen_t length;
            if (family == AF_INET) {
                auto* endpoint = reinterpret_cast<sockaddr_in*>(&address);
                endpoint->sin_family = AF_INET;
                endpoint->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
                length = sizeof(*endpoint);
            } else {
                auto* endpoint = reinterpret_cast<sockaddr_in6*>(&address);
                endpoint->sin6_family = AF_INET6;
                endpoint->sin6_addr = in6addr_loopback;
                length = sizeof(*endpoint);
            }
            ASSERT_EQ(0, bind(descriptors_[1], reinterpret_cast<sockaddr*>(&address), length))
                << strerror(errno);
            ASSERT_EQ(0, getsockname(descriptors_[1], reinterpret_cast<sockaddr*>(&address), &length));
            ASSERT_EQ(0, connect(descriptors_[0], reinterpret_cast<sockaddr*>(&address), length));
        }
        memory_ = mmap(nullptr, kLargeCapacity, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        ASSERT_NE(MAP_FAILED, memory_) << strerror(errno);
    }
    void TearDown() override {
        for (int fd : descriptors_) if (fd >= 0) close(fd);
        if (memory_ != MAP_FAILED) munmap(memory_, kLargeCapacity);
    }
    ssize_t Receive(size_t capacity, int flags = MSG_DONTWAIT) {
        iovec vector{memory_, capacity};
        msghdr message{};
        message.msg_iov = &vector;
        message.msg_iovlen = 1;
        return recvmsg(descriptors_[1], &message, flags);
    }
    int descriptors_[2] = {-1, -1};
    void* memory_ = MAP_FAILED;
};
TEST_P(RecvmsgRecordBuffer, SmallRecordDoesNotAllocateAdvertisedCapacity) {
    constexpr char payload[] = "record";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(payload)),
              send(descriptors_[0], payload, sizeof(payload), 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(payload)), Receive(kLargeCapacity)) << strerror(errno);
    EXPECT_EQ(0, memcmp(memory_, payload, sizeof(payload)));
    errno = 0;
    EXPECT_EQ(-1, Receive(1));
    EXPECT_EQ(EAGAIN, errno);
}
TEST_P(RecvmsgRecordBuffer, EmptyNonblockingQueueDoesNotAllocateAdvertisedCapacity) {
    errno = 0;
    EXPECT_EQ(-1, Receive(kLargeCapacity));
    EXPECT_EQ(EAGAIN, errno);
}


TEST_P(RecvmsgRecordBuffer, LargeRecordCrossesIovecsAndPeekDoesNotConsume) {
    constexpr size_t size = 32 * 1024;
    std::vector<unsigned char> sent(size);
    for (size_t n = 0; n < size; ++n) sent[n] = static_cast<unsigned char>(n * 17);
    ASSERT_EQ(static_cast<ssize_t>(size), send(descriptors_[0], sent.data(), size, 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    auto* out = static_cast<unsigned char*>(memory_);
    iovec vectors[] = {{out, 8193}, {nullptr, 0}, {out + 8193, kLargeCapacity - 8193}};
    for (int flags : {static_cast<int>(MSG_PEEK), 0}) {
        msghdr message{};
        message.msg_iov = vectors;
        message.msg_iovlen = 3;
        ASSERT_EQ(static_cast<ssize_t>(size), recvmsg(descriptors_[1], &message, flags | MSG_DONTWAIT));
        EXPECT_EQ(0, message.msg_flags & MSG_TRUNC);
        EXPECT_EQ(0, memcmp(out, sent.data(), size));
    }
    errno = 0;
    EXPECT_EQ(-1, Receive(1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(RecvmsgRecordBuffer, TruncatedPeekAndConsumeReportOriginalLength) {
    constexpr char sent[] = "a complete record";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), send(descriptors_[0], sent, sizeof(sent), 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    for (int flags : {static_cast<int>(MSG_PEEK | MSG_TRUNC), static_cast<int>(MSG_TRUNC)}) {
        iovec vector{memory_, 3};
        msghdr message{};
        message.msg_iov = &vector; message.msg_iovlen = 1;
        ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), recvmsg(descriptors_[1], &message, flags | MSG_DONTWAIT));
        EXPECT_NE(0, message.msg_flags & MSG_TRUNC);
        EXPECT_EQ(0, memcmp(memory_, sent, 3));
    }
    errno = 0;
    EXPECT_EQ(-1, Receive(1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(RecvmsgRecordBuffer, ZeroIovConsumesOneRecordWithoutTouchingPayload) {
    constexpr char sent[] = "not copied";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), send(descriptors_[0], sent, sizeof(sent), 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    msghdr message{};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), recvmsg(descriptors_[1], &message, MSG_DONTWAIT | MSG_PEEK | MSG_TRUNC));
    EXPECT_NE(0, message.msg_flags & MSG_TRUNC);
    message = {};
    ASSERT_EQ(0, recvmsg(descriptors_[1], &message, MSG_DONTWAIT));
    EXPECT_NE(0, message.msg_flags & MSG_TRUNC);
    errno = 0;
    EXPECT_EQ(-1, Receive(1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(RecvmsgRecordBuffer, FaultAfterCopiedPrefixConsumesOnlyNonPeekRecord) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    auto* out = static_cast<unsigned char*>(memory_);
    ASSERT_EQ(0, mprotect(out + page, page, PROT_NONE));
    constexpr char sent[] = "fault-prefix";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), send(descriptors_[0], sent, sizeof(sent), 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    for (int flags : {static_cast<int>(MSG_PEEK), 0}) {
        iovec vectors[] = {{out, 3}, {out + page, sizeof(sent) - 3}};
        msghdr message{};
        message.msg_iov = vectors; message.msg_iovlen = 2;
        errno = 0;
        EXPECT_EQ(-1, recvmsg(descriptors_[1], &message, flags | MSG_DONTWAIT));
        EXPECT_EQ(EFAULT, errno);
        EXPECT_EQ(0, memcmp(out, sent, 3));
    }
    errno = 0;
    EXPECT_EQ(-1, Receive(1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(RecvmsgRecordBuffer, RecvmmsgKeepsDistinctRecordsWithLargeCapacities) {
    const timeval timeout{2, 0};
    ASSERT_EQ(0, setsockopt(descriptors_[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
    const char first[] = "first", second[] = "second record";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(first)), send(descriptors_[0], first, sizeof(first), 0));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(second)), send(descriptors_[0], second, sizeof(second), 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    auto* out = static_cast<char*>(memory_);
    iovec vectors[] = {{out, kLargeCapacity / 2}, {out + kLargeCapacity / 2, kLargeCapacity / 2}};
    mmsghdr messages[2]{};
    for (size_t n = 0; n < 2; ++n) {
        messages[n].msg_hdr.msg_iov = &vectors[n];
        messages[n].msg_hdr.msg_iovlen = 1;
    }
    ASSERT_EQ(2, recvmmsg(descriptors_[1], messages, 2, 0, nullptr));
    EXPECT_EQ(sizeof(first), messages[0].msg_len);
    EXPECT_EQ(sizeof(second), messages[1].msg_len);
    EXPECT_EQ(0, memcmp(out, first, sizeof(first)));
    EXPECT_EQ(0, memcmp(out + kLargeCapacity / 2, second, sizeof(second)));
}

TEST_P(RecvmsgRecordBuffer, BlockingWakeAndTimeoutDoNotAllocateRequestCapacity) {
    const timeval timeout{2, 0};
    ASSERT_EQ(0, setsockopt(descriptors_[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
    ssize_t received = -1;
    int receive_error = 0;
    std::atomic<bool> started{false};
    std::thread reader([&] {
        started.store(true);
        received = Receive(kLargeCapacity, 0);
        receive_error = errno;
    });
    while (!started.load()) std::this_thread::yield();
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
    const char sent[] = "wake";
    const ssize_t sent_count = send(descriptors_[0], sent, sizeof(sent), MSG_DONTWAIT);
    reader.join();
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), sent_count);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), received) << strerror(receive_error);
    EXPECT_EQ(0, memcmp(memory_, sent, sizeof(sent)));
    const timeval short_timeout{0, 30000};
    ASSERT_EQ(0, setsockopt(descriptors_[1], SOL_SOCKET, SO_RCVTIMEO, &short_timeout, sizeof(short_timeout)));
    errno = 0;
    EXPECT_EQ(-1, Receive(kLargeCapacity, 0));
    EXPECT_EQ(EAGAIN, errno);
}


TEST_P(RecvmsgRecordBuffer, ZeroLengthRecordIsNotAnEmptyQueue) {
    ASSERT_EQ(0, send(descriptors_[0], nullptr, 0, 0));
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    EXPECT_EQ(0, Receive(kLargeCapacity, MSG_DONTWAIT | MSG_PEEK));
    EXPECT_EQ(0, Receive(kLargeCapacity));
    errno = 0;
    EXPECT_EQ(-1, Receive(kLargeCapacity));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(RecvmsgRecordBuffer, ConcurrentReadersKeepDifferentSizeRecordsIntact) {
    const timeval timeout{2, 0};
    ASSERT_EQ(0, setsockopt(descriptors_[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
    constexpr size_t records = 8;
    for (size_t n = 0; n < records; ++n) {
        const size_t length = n % 2 ? 12000 : 7;
        std::vector<unsigned char> record(length, static_cast<unsigned char>(n));
        ASSERT_EQ(static_cast<ssize_t>(length), send(descriptors_[0], record.data(), length, 0));
    }
    pollfd ready{descriptors_[1], POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::atomic<size_t> next{0};
    std::atomic<unsigned int> seen{0};
    std::atomic<size_t> errors{0};
    std::vector<std::thread> readers;
    for (size_t n = 0; n < 2; ++n) {
        readers.emplace_back([&, n] {
            auto* out = static_cast<unsigned char*>(memory_) + n * (kLargeCapacity / 2);
            while (next.fetch_add(1) < records) {
                iovec vector{out, kLargeCapacity / 2};
                msghdr message{};
                message.msg_iov = &vector; message.msg_iovlen = 1;
                const auto received = recvmsg(descriptors_[1], &message, 0);
                if (received <= 0 || out[0] >= records) {
                    ++errors;
                    continue;
                }
                const unsigned char tag = out[0];
                const size_t expected = tag % 2 ? 12000 : 7;
                if (received != static_cast<ssize_t>(expected)) {
                    ++errors;
                    continue;
                }
                for (size_t byte = 0; byte < expected; ++byte) {
                    if (out[byte] != tag) { ++errors; break; }
                }
                if (seen.fetch_or(1U << tag) & (1U << tag)) ++errors;
            }
        });
    }
    for (auto& reader : readers) reader.join();
    EXPECT_EQ(0U, errors.load());
    EXPECT_EQ((1U << records) - 1, seen.load());
}

TEST(RecvmsgRecordScm, ConcurrentReceiversKeepRightsWithTheirRecord) {
    int sockets[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets));
    const int sources[] = {open("/dev/null", O_RDONLY), open("/dev/null", O_WRONLY)};
    ASSERT_GE(sources[0], 0);
    ASSERT_GE(sources[1], 0);
    constexpr size_t records = 192;
    for (size_t n = 0; n < records; ++n) {
        char tag = n % 2;
        iovec data{&tag, 1};
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
        msghdr message{};
        message.msg_iov = &data;
        message.msg_iovlen = 1;
        message.msg_control = control;
        message.msg_controllen = sizeof(control);
        auto* header = CMSG_FIRSTHDR(&message);
        header->cmsg_level = SOL_SOCKET;
        header->cmsg_type = SCM_RIGHTS;
        header->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(header), &sources[n % 2], sizeof(int));
        ASSERT_EQ(1, sendmsg(sockets[0], &message, MSG_DONTWAIT)) << strerror(errno);
    }
    std::atomic<size_t> next{0};
    std::atomic<size_t> mismatches{0};
    std::vector<std::thread> readers;
    for (int n = 0; n < 4; ++n) {
        readers.emplace_back([&] {
            while (next.fetch_add(1) < records) {
                char tag = -1;
                iovec data{&tag, 1};
                alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
                msghdr message{};
                message.msg_iov = &data;
                message.msg_iovlen = 1;
                message.msg_control = control;
                message.msg_controllen = sizeof(control);
                const auto count = recvmsg(sockets[1], &message, MSG_DONTWAIT);
                auto* header = CMSG_FIRSTHDR(&message);
                int received = -1;
                if (count != 1 || !header || header->cmsg_level != SOL_SOCKET ||
                    header->cmsg_type != SCM_RIGHTS || header->cmsg_len != CMSG_LEN(sizeof(int))) {
                    ++mismatches;
                    continue;
                }
                memcpy(&received, CMSG_DATA(header), sizeof(int));
                const int mode = fcntl(received, F_GETFL);
                if (mode < 0 || (mode & O_ACCMODE) != tag) ++mismatches;
                close(received);
            }
        });
    }
    for (auto& reader : readers) reader.join();
    EXPECT_EQ(0U, mismatches.load());
    close(sources[0]); close(sources[1]);
    close(sockets[0]); close(sockets[1]);
}


TEST(RecvmsgRecordScm, EmptySeqpacketRecordRetainsAncillaryIdentity) {
    int sockets[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets));
    const int enabled = 1;
    ASSERT_EQ(0, setsockopt(sockets[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof(enabled)));
    const int source = open("/dev/null", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(source, 0);
    alignas(cmsghdr) char sent_control[CMSG_SPACE(sizeof(int))] = {};
    msghdr sent{};
    sent.msg_control = sent_control;
    sent.msg_controllen = sizeof(sent_control);
    auto* sent_header = CMSG_FIRSTHDR(&sent);
    sent_header->cmsg_level = SOL_SOCKET;
    sent_header->cmsg_type = SCM_RIGHTS;
    sent_header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(sent_header), &source, sizeof(source));
    ASSERT_EQ(0, sendmsg(sockets[0], &sent, 0));
    for (int flags : {static_cast<int>(MSG_PEEK), 0}) {
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int)) + CMSG_SPACE(sizeof(ucred))] = {};
        msghdr message{};
        message.msg_control = control;
        message.msg_controllen = sizeof(control);
        ASSERT_EQ(0, recvmsg(sockets[1], &message, flags | MSG_DONTWAIT | MSG_CMSG_CLOEXEC));
        int rights = 0, credentials = 0;
        for (auto* header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
            if (header->cmsg_level != SOL_SOCKET) continue;
            if (header->cmsg_type == SCM_RIGHTS) {
                int fd = -1;
                memcpy(&fd, CMSG_DATA(header), sizeof(fd));
                EXPECT_GE(fd, 0);
                if (fd >= 0) {
                    EXPECT_EQ(FD_CLOEXEC, fcntl(fd, F_GETFD));
                    close(fd);
                }
                ++rights;
            } else if (header->cmsg_type == SCM_CREDENTIALS) {
                ucred cred{};
                memcpy(&cred, CMSG_DATA(header), sizeof(cred));
                EXPECT_EQ(getpid(), cred.pid);
                EXPECT_EQ(getuid(), cred.uid);
                ++credentials;
            }
        }
        EXPECT_EQ(1, rights);
        EXPECT_EQ(1, credentials);
    }
    msghdr empty{};
    errno = 0;
    EXPECT_EQ(-1, recvmsg(sockets[1], &empty, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    close(source); close(sockets[0]); close(sockets[1]);
}

class RecvmsgRawRecord : public testing::Test {
 protected:
    void SetUp() override {
        raw_ = socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP);
        ASSERT_GE(raw_, 0) << strerror(errno);
        receiver_ = socket(AF_INET, SOCK_DGRAM, 0);
        sender_ = socket(AF_INET, SOCK_DGRAM, 0);
        ASSERT_GE(receiver_, 0); ASSERT_GE(sender_, 0);
        sockaddr_in target{};
        target.sin_family = AF_INET;
        target.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(0, bind(receiver_, reinterpret_cast<sockaddr*>(&target), sizeof(target)));
        socklen_t length = sizeof(target);
        ASSERT_EQ(0, getsockname(receiver_, reinterpret_cast<sockaddr*>(&target), &length));
        constexpr char bytes[] = "raw";
        ASSERT_EQ(static_cast<ssize_t>(sizeof(bytes)),
                  sendto(sender_, bytes, sizeof(bytes), 0, reinterpret_cast<sockaddr*>(&target), length));
        pollfd ready{raw_, POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 2000));
    }
    void TearDown() override {
        for (int fd : {raw_, receiver_, sender_}) if (fd >= 0) close(fd);
    }
    ssize_t Receive(size_t capacity, int flags, int* output_flags = nullptr) {
        unsigned char data[4096] = {};
        iovec vector{data, capacity};
        msghdr message{};
        message.msg_iov = &vector; message.msg_iovlen = 1;
        auto n = recvmsg(raw_, &message, flags | MSG_DONTWAIT);
        if (output_flags) *output_flags = message.msg_flags;
        return n;
    }
    int raw_ = -1, receiver_ = -1, sender_ = -1;
};
TEST_F(RecvmsgRawRecord, PeekPreservesWholePacket) {
    EXPECT_EQ(32, Receive(4096, MSG_PEEK));
    EXPECT_EQ(32, Receive(4096, 0)) << strerror(errno);
}
TEST_F(RecvmsgRawRecord, TruncReportsOriginalPacketLength) {
    int output_flags = 0;
    EXPECT_EQ(32, Receive(2, MSG_TRUNC, &output_flags));
    EXPECT_NE(0, output_flags & MSG_TRUNC);
}


TEST_F(RecvmsgRawRecord, LargeAdvertisedCapacityUsesActualPacketLength) {
    void* memory = mmap(nullptr, kLargeCapacity, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    ASSERT_NE(MAP_FAILED, memory);
    iovec vector{memory, kLargeCapacity};
    for (int flags : {static_cast<int>(MSG_PEEK), 0}) {
        msghdr message{};
        message.msg_iov = &vector; message.msg_iovlen = 1;
        EXPECT_EQ(32, recvmsg(raw_, &message, flags | MSG_DONTWAIT)) << strerror(errno);
        EXPECT_EQ(0, message.msg_flags & MSG_TRUNC);
    }
    msghdr empty{};
    empty.msg_iov = &vector; empty.msg_iovlen = 1;
    errno = 0;
    EXPECT_EQ(-1, recvmsg(raw_, &empty, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, munmap(memory, kLargeCapacity));
}


class RecvmsgRawIpv6Record : public testing::Test {
 protected:
    void SetUp() override {
        raw_ = socket(AF_INET6, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP);
        ASSERT_GE(raw_, 0) << strerror(errno);
        // Linux defaults UDP raw checksum processing to disabled. Enable the
        // UDP checksum field explicitly so short iovecs still validate the
        // actual complete datagram, including bytes beyond their capacity.
        const int checksum_offset = 6;
        ASSERT_EQ(0, setsockopt(raw_, IPPROTO_IPV6, IPV6_CHECKSUM,
                               &checksum_offset, sizeof(checksum_offset)));
        receiver_ = socket(AF_INET6, SOCK_DGRAM, 0);
        sender_ = socket(AF_INET6, SOCK_DGRAM, 0);
        ASSERT_GE(receiver_, 0); ASSERT_GE(sender_, 0);
        sockaddr_in6 target{};
        target.sin6_family = AF_INET6;
        target.sin6_addr = in6addr_loopback;
        ASSERT_EQ(0, bind(raw_, reinterpret_cast<sockaddr*>(&target), sizeof(target)));
        ASSERT_EQ(0, bind(receiver_, reinterpret_cast<sockaddr*>(&target), sizeof(target)));
        socklen_t length = sizeof(target);
        ASSERT_EQ(0, getsockname(receiver_, reinterpret_cast<sockaddr*>(&target), &length));
        destination_port_ = target.sin6_port;
        ASSERT_EQ(static_cast<ssize_t>(sizeof(kPayload)),
                  sendto(sender_, kPayload, sizeof(kPayload), 0,
                         reinterpret_cast<sockaddr*>(&target), length));
        pollfd ready{raw_, POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 2000));
        memory_ = mmap(nullptr, kLargeCapacity, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        ASSERT_NE(MAP_FAILED, memory_);
    }
    void TearDown() override {
        if (memory_ != MAP_FAILED) munmap(memory_, kLargeCapacity);
        for (int fd : {raw_, receiver_, sender_}) if (fd >= 0) close(fd);
    }
    static constexpr char kPayload[] = "raw6";
    static constexpr size_t kDatagramLength = 8 + sizeof(kPayload);
    int raw_ = -1, receiver_ = -1, sender_ = -1;
    uint16_t destination_port_ = 0;
    void* memory_ = MAP_FAILED;
};

TEST_F(RecvmsgRawIpv6Record, PeekTruncationAndZeroIovExcludeIpv6Header) {
    iovec vector{memory_, kLargeCapacity};
    sockaddr_in6 source{};
    msghdr message{};
    message.msg_iov = &vector; message.msg_iovlen = 1;
    message.msg_name = &source; message.msg_namelen = sizeof(source);
    ASSERT_EQ(static_cast<ssize_t>(kDatagramLength),
              recvmsg(raw_, &message, MSG_PEEK | MSG_DONTWAIT)) << strerror(errno);
    EXPECT_EQ(0, message.msg_flags & MSG_TRUNC);
    EXPECT_EQ(AF_INET6, source.sin6_family);
    EXPECT_EQ(0, source.sin6_port);
    EXPECT_EQ(0, memcmp(&source.sin6_addr, &in6addr_loopback, sizeof(in6addr_loopback)));
    auto* bytes = static_cast<unsigned char*>(memory_);
    uint16_t port = 0, length = 0;
    memcpy(&port, bytes + 2, sizeof(port));
    memcpy(&length, bytes + 4, sizeof(length));
    EXPECT_EQ(destination_port_, port);
    EXPECT_EQ(kDatagramLength, ntohs(length));
    EXPECT_EQ(0, memcmp(bytes + 8, kPayload, sizeof(kPayload)));
    vector.iov_len = 3;
    message = {};
    message.msg_iov = &vector; message.msg_iovlen = 1;
    ASSERT_EQ(static_cast<ssize_t>(kDatagramLength),
              recvmsg(raw_, &message, MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT));
    EXPECT_NE(0, message.msg_flags & MSG_TRUNC);
    message = {};
    ASSERT_EQ(0, recvmsg(raw_, &message, MSG_DONTWAIT));
    EXPECT_NE(0, message.msg_flags & MSG_TRUNC);
    message = {};
    errno = 0;
    EXPECT_EQ(-1, recvmsg(raw_, &message, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_F(RecvmsgRawIpv6Record, PrefixFaultRetainsPeekAndConsumesNormalRecord) {
    auto* out = static_cast<unsigned char*>(memory_);
    iovec vector{out, kLargeCapacity};
    msghdr peek{};
    peek.msg_iov = &vector; peek.msg_iovlen = 1;
    ASSERT_EQ(static_cast<ssize_t>(kDatagramLength), recvmsg(raw_, &peek, MSG_DONTWAIT | MSG_PEEK));
    unsigned char prefix[3];
    memcpy(prefix, out, sizeof(prefix));
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    ASSERT_EQ(0, mprotect(out + page, page, PROT_NONE));
    for (int flags : {static_cast<int>(MSG_PEEK), 0}) {
        memset(out, 0, sizeof(prefix));
        iovec vectors[] = {{out, sizeof(prefix)}, {out + page, kDatagramLength - sizeof(prefix)}};
        msghdr message{};
        message.msg_iov = vectors; message.msg_iovlen = 2;
        errno = 0;
        EXPECT_EQ(-1, recvmsg(raw_, &message, flags | MSG_DONTWAIT));
        EXPECT_EQ(EFAULT, errno);
        EXPECT_EQ(0, memcmp(out, prefix, sizeof(prefix)));
    }
    msghdr empty{};
    empty.msg_iov = &vector; empty.msg_iovlen = 1;
    errno = 0;
    EXPECT_EQ(-1, recvmsg(raw_, &empty, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(RecvmsgTcpBuffer, PeekAndDiscardDoNotAllocateAdvertisedCapacity) {
    const int listener = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0);
    sockaddr_in target{};
    target.sin_family = AF_INET;
    target.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&target), sizeof(target)));
    ASSERT_EQ(0, listen(listener, 1));
    socklen_t address_length = sizeof(target);
    ASSERT_EQ(0, getsockname(listener, reinterpret_cast<sockaddr*>(&target), &address_length));
    const int sender = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(sender, 0);
    ASSERT_EQ(0, connect(sender, reinterpret_cast<sockaddr*>(&target), sizeof(target)));
    const int receiver = accept(listener, nullptr, nullptr);
    ASSERT_GE(receiver, 0);
    void* memory = mmap(nullptr, kLargeCapacity, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    ASSERT_NE(MAP_FAILED, memory);
    for (int flags : {static_cast<int>(MSG_PEEK), static_cast<int>(MSG_TRUNC),
                      static_cast<int>(MSG_PEEK | MSG_TRUNC)}) {
        msghdr zero{};
        errno = 0;
        EXPECT_EQ(-1, recvmsg(receiver, &zero, flags | MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
        errno = 0;
        EXPECT_EQ(-1, recv(receiver, nullptr, 0, flags | MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
    const char sent[] = "tcp-record";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), send(sender, sent, sizeof(sent), 0));
    pollfd ready{receiver, POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    for (int flags : {static_cast<int>(MSG_PEEK), static_cast<int>(MSG_TRUNC),
                      static_cast<int>(MSG_PEEK | MSG_TRUNC)}) {
        msghdr zero{};
        EXPECT_EQ(0, recvmsg(receiver, &zero, flags | MSG_DONTWAIT));
        EXPECT_EQ(0, recv(receiver, nullptr, 0, flags | MSG_DONTWAIT));
    }
    // Every zero-capacity operation above leaves the queued data available.
    iovec vector{memory, kLargeCapacity};
    for (int flags : {static_cast<int>(MSG_PEEK), static_cast<int>(MSG_PEEK | MSG_TRUNC), static_cast<int>(MSG_TRUNC)}) {
        msghdr message{};
        message.msg_iov = &vector; message.msg_iovlen = 1;
        memset(memory, 0x5a, sizeof(sent));
        ASSERT_EQ(static_cast<ssize_t>(sizeof(sent)), recvmsg(receiver, &message, flags | MSG_DONTWAIT));
        if (flags & MSG_TRUNC) {
            EXPECT_EQ(0x5a, *static_cast<unsigned char*>(memory));
        } else {
            EXPECT_EQ(0, memcmp(memory, sent, sizeof(sent)));
        }
    }
    msghdr empty{};
    empty.msg_iov = &vector; empty.msg_iovlen = 1;
    errno = 0;
    EXPECT_EQ(-1, recvmsg(receiver, &empty, MSG_DONTWAIT | MSG_PEEK));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, munmap(memory, kLargeCapacity));
    close(receiver); close(sender); close(listener);
}

INSTANTIATE_TEST_SUITE_P(Owners, RecvmsgRecordBuffer, testing::Values(0, 1, 2, 3));
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
