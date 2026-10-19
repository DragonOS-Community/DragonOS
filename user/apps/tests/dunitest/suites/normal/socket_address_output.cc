#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <poll.h>
#include <linux/vm_sockets.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <tuple>

namespace {
class Fd {
 public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
    void Set(int fd) { if (fd_ >= 0) close(fd_); fd_ = fd; }
 private:
    int fd_;
};

sockaddr_storage Loopback(int family) {
    sockaddr_storage address{};
    if (family == AF_INET) {
        auto* inet = reinterpret_cast<sockaddr_in*>(&address);
        inet->sin_family = AF_INET;
        inet->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    } else if (family == AF_INET6) {
        auto* inet = reinterpret_cast<sockaddr_in6*>(&address);
        inet->sin6_family = AF_INET6;
        inet->sin6_addr = in6addr_loopback;
    } else {
        auto* vsock = reinterpret_cast<sockaddr_vm*>(&address);
        vsock->svm_family = AF_VSOCK;
        vsock->svm_cid = VMADDR_CID_ANY;
        vsock->svm_port = VMADDR_PORT_ANY;
    }
    return address;
}

socklen_t AddressSize(int family) {
    return family == AF_INET ? sizeof(sockaddr_in)
                            : family == AF_INET6 ? sizeof(sockaddr_in6) : sizeof(sockaddr_vm);
}

void WaitReadable(int fd) {
    pollfd ready{fd, POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 3000)) << errno;
    ASSERT_NE(0, ready.revents & POLLIN);
}

// Three adjacent pages make the length load itself cross a protection boundary.
// Keep all mappings alive until after the syscall and its queue checks.
class OutputPages {
 public:
    OutputPages() : page_(static_cast<size_t>(sysconf(_SC_PAGESIZE))),
                    bytes_(static_cast<char*>(mmap(nullptr, 3 * page_, PROT_READ | PROT_WRITE,
                                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0))) {}
    ~OutputPages() { if (Valid()) munmap(bytes_, 3 * page_); }
    bool Valid() const { return bytes_ != MAP_FAILED; }
    void Initialize() {
        ASSERT_TRUE(Valid());
        memset(bytes_, 0x55, 3 * page_);
        *Length() = sizeof(sockaddr_storage);
        ASSERT_EQ(0, mprotect(bytes_ + 2 * page_, page_, PROT_NONE));
    }
    sockaddr* Address() const { return reinterpret_cast<sockaddr*>(bytes_); }
    socklen_t* Length() const { return reinterpret_cast<socklen_t*>(bytes_ + page_); }
    socklen_t* CrossingLength() const { return reinterpret_cast<socklen_t*>(bytes_ + 2 * page_ - 2); }
    void ReadOnlyLength() { ASSERT_EQ(0, mprotect(bytes_ + page_, page_, PROT_READ)); }
    void ProtectAddress() { ASSERT_EQ(0, mprotect(bytes_, page_, PROT_NONE)); }
    void ExpectAddressUnchanged() const {
        const auto* bytes = reinterpret_cast<const unsigned char*>(bytes_);
        for (size_t i = 0; i < sizeof(sockaddr_storage); ++i) EXPECT_EQ(0x55, bytes[i]) << i;
    }
 private:
    size_t page_;
    char* bytes_;
};

enum class Boundary {
    NullAddress, NullAddressBadLength, NullAddressReadOnlyLength,
    NullLength, BadLength, NegativeLength, ReadOnlyLength, BadAddress,
    ProtectedAddress, Normal, ZeroCapacity, IntMinLength, IntMaxLength, CrossingLength,
};
enum class Receive { Consume, Peek, Zero, Empty, Eof };
using TcpCase = std::tuple<int, Boundary, Receive>;

class SocketAddressOutputStream : public testing::TestWithParam<TcpCase> {
 protected:
    void SetUp() override {
        family_ = std::get<0>(GetParam());
        Fd listener(socket(family_, SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0) << errno;
        auto address = Loopback(family_);
        socklen_t size = AddressSize(family_);
        ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), size)) << errno;
        ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &size));
        if (family_ == AF_VSOCK) {
            // VMADDR_CID_LOCAL (1) is absent from the older musl sysroot headers.
            reinterpret_cast<sockaddr_vm*>(&address)->svm_cid = 1;
        }
        ASSERT_EQ(0, listen(listener.Get(), 2));
        writer_.Set(socket(family_, SOCK_STREAM, 0));
        ASSERT_GE(writer_.Get(), 0);
        ASSERT_EQ(0, connect(writer_.Get(), reinterpret_cast<sockaddr*>(&address), size));
        reader_.Set(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(reader_.Get(), 0);
    }
    Fd reader_, writer_;
    int family_;
};

TEST_P(SocketAddressOutputStream, OutputLengthAndReceiveOrdering) {
    const Boundary boundary = std::get<1>(GetParam());
    const Receive receive = std::get<2>(GetParam());
    if (receive == Receive::Eof) {
        ASSERT_EQ(0, shutdown(writer_.Get(), SHUT_WR));
        ASSERT_NO_FATAL_FAILURE(WaitReadable(reader_.Get()));
    } else if (receive != Receive::Empty) {
        ASSERT_EQ(3, send(writer_.Get(), "abc", 3, MSG_NOSIGNAL));
        ASSERT_NO_FATAL_FAILURE(WaitReadable(reader_.Get()));
        std::array<char, 3> queued{};
        ASSERT_EQ(3, recv(reader_.Get(), queued.data(), queued.size(), MSG_PEEK | MSG_DONTWAIT));
        ASSERT_EQ(0, memcmp(queued.data(), "abc", queued.size()));
    }

    OutputPages pages;
    ASSERT_NO_FATAL_FAILURE(pages.Initialize());
    sockaddr* address = pages.Address();
    socklen_t* length = pages.Length();
    socklen_t original_length = sizeof(sockaddr_storage);
    int output_error = 0;
    bool ignored = false;
    bool protected_address = false;
    switch (boundary) {
    case Boundary::NullAddress: address = nullptr; ignored = true; break;
    case Boundary::NullAddressBadLength:
        address = nullptr; length = reinterpret_cast<socklen_t*>(1); ignored = true; break;
    case Boundary::NullAddressReadOnlyLength:
        address = nullptr; ignored = true; ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength()); break;
    case Boundary::NullLength: length = nullptr; output_error = EFAULT; break;
    case Boundary::BadLength: length = reinterpret_cast<socklen_t*>(1); output_error = EFAULT; break;
    case Boundary::NegativeLength:
        original_length = UINT32_MAX; *length = original_length; output_error = EINVAL; break;
    case Boundary::ReadOnlyLength:
        ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength()); output_error = EFAULT; break;
    case Boundary::BadAddress: address = reinterpret_cast<sockaddr*>(1); break;
    case Boundary::ProtectedAddress:
        ASSERT_NO_FATAL_FAILURE(pages.ProtectAddress()); protected_address = true; break;
    case Boundary::Normal: break;
    case Boundary::ZeroCapacity: original_length = 0; *length = 0; break;
    case Boundary::IntMinLength:
        original_length = 0x80000000u; *length = original_length; output_error = EINVAL; break;
    case Boundary::IntMaxLength:
        original_length = 0x7fffffffu; *length = original_length; break;
    case Boundary::CrossingLength: length = pages.CrossingLength(); output_error = EFAULT; break;
    }

    std::array<char, 3> payload{};
    errno = 0;
    const ssize_t got = recvfrom(reader_.Get(), payload.data(), receive == Receive::Zero ? 0 : payload.size(),
                                 MSG_DONTWAIT | (receive == Receive::Peek ? MSG_PEEK : 0), address, length);
    const int error = errno;
    if (receive == Receive::Empty) {
        EXPECT_EQ(-1, got);
        EXPECT_EQ(EAGAIN, error);
    } else if (output_error) {
        EXPECT_EQ(-1, got);
        EXPECT_EQ(output_error, error);
    } else {
        EXPECT_EQ(receive == Receive::Zero || receive == Receive::Eof ? 0 : 3, got) << error;
    }
    if (receive != Receive::Zero && receive != Receive::Empty && receive != Receive::Eof) {
        EXPECT_EQ(0, memcmp(payload.data(), "abc", payload.size()));
    }
    EXPECT_EQ(receive == Receive::Empty || output_error || ignored ? original_length : 0, *pages.Length());
    if (!protected_address) pages.ExpectAddressUnchanged();

    std::array<char, 3> remaining{};
    errno = 0;
    const ssize_t next = recv(reader_.Get(), remaining.data(), remaining.size(), MSG_DONTWAIT);
    if (receive == Receive::Peek || receive == Receive::Zero) {
        ASSERT_EQ(3, next) << errno;
        EXPECT_EQ(0, memcmp(remaining.data(), "abc", remaining.size()));
    } else if (receive == Receive::Eof) {
        EXPECT_EQ(0, next);
    } else {
        EXPECT_EQ(-1, next);
        EXPECT_EQ(EAGAIN, errno);
    }
}

const auto kBoundaries = testing::Values(Boundary::NullAddress, Boundary::NullAddressBadLength,
    Boundary::NullAddressReadOnlyLength, Boundary::NullLength, Boundary::BadLength,
    Boundary::NegativeLength, Boundary::ReadOnlyLength, Boundary::BadAddress,
    Boundary::ProtectedAddress, Boundary::Normal, Boundary::ZeroCapacity,
    Boundary::IntMinLength, Boundary::IntMaxLength, Boundary::CrossingLength);
const auto kReceives = testing::Values(Receive::Consume, Receive::Peek, Receive::Zero, Receive::Empty, Receive::Eof);
INSTANTIATE_TEST_SUITE_P(Ipv4AndIpv6, SocketAddressOutputStream,
    testing::Combine(testing::Values(AF_INET, AF_INET6), kBoundaries, kReceives));
INSTANTIATE_TEST_SUITE_P(VsockLocal, SocketAddressOutputStream,
    testing::Combine(testing::Values(AF_VSOCK), kBoundaries, kReceives));

class SocketAddressOutputWrite : public testing::TestWithParam<int> {};

TEST_P(SocketAddressOutputWrite, DatagramErrorsFollowPayloadAndAddressCopy) {
    const int family = GetParam();
    Fd receiver(socket(family, SOCK_DGRAM, 0)), sender(socket(family, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0); ASSERT_GE(sender.Get(), 0);
    auto address = Loopback(family);
    socklen_t size = AddressSize(family);
    ASSERT_EQ(0, bind(receiver.Get(), reinterpret_cast<sockaddr*>(&address), size));
    ASSERT_EQ(0, getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&address), &size));
    ASSERT_EQ(0, connect(sender.Get(), reinterpret_cast<sockaddr*>(&address), size));
    sockaddr_storage source{};
    socklen_t source_size = sizeof(source);
    ASSERT_EQ(0, getsockname(sender.Get(), reinterpret_cast<sockaddr*>(&source), &source_size));
    for (int boundary = 0; boundary < 5; ++boundary) {
        SCOPED_TRACE(boundary);
        ASSERT_EQ(3, send(sender.Get(), "abc", 3, 0));
        ASSERT_NO_FATAL_FAILURE(WaitReadable(receiver.Get()));
        OutputPages pages;
        ASSERT_NO_FATAL_FAILURE(pages.Initialize());
        socklen_t* length = pages.Length();
        if (boundary == 0) length = nullptr;
        if (boundary == 1) *length = UINT32_MAX;
        if (boundary == 2) { ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength()); }
        if (boundary == 3) *length = 0;
        if (boundary == 4) *length = 2;
        std::array<char, 3> payload{};
        errno = 0;
        const ssize_t result = recvfrom(receiver.Get(), payload.data(), payload.size(), MSG_DONTWAIT,
                                        pages.Address(), length);
        const int error = errno;
        EXPECT_EQ(0, memcmp(payload.data(), "abc", 3));
        if (boundary < 3) {
            EXPECT_EQ(-1, result);
            EXPECT_EQ(boundary == 1 ? EINVAL : EFAULT, error);
        } else {
            EXPECT_EQ(3, result) << error;
            EXPECT_EQ(source_size, *length);
        }
        if (boundary == 2) {
            // Linux copies the actual sockaddr before failing the final put_user.
            EXPECT_EQ(0, memcmp(pages.Address(), &source, source_size));
            EXPECT_EQ(sizeof(sockaddr_storage), *length);
        } else if (boundary == 4) {
            EXPECT_EQ(0, memcmp(pages.Address(), &source, 2));
            const auto* bytes = reinterpret_cast<const unsigned char*>(pages.Address());
            EXPECT_EQ(0x55, bytes[2]);
        } else {
            pages.ExpectAddressUnchanged();
        }
        errno = 0;
        EXPECT_EQ(-1, recv(receiver.Get(), payload.data(), payload.size(), MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
}

TEST_P(SocketAddressOutputWrite, DatagramAddressMayOverlapLength) {
    const int family = GetParam();
    Fd receiver(socket(family, SOCK_DGRAM, 0)), sender(socket(family, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0); ASSERT_GE(sender.Get(), 0);
    auto destination = Loopback(family);
    socklen_t size = AddressSize(family);
    ASSERT_EQ(0, bind(receiver.Get(), reinterpret_cast<sockaddr*>(&destination), size));
    ASSERT_EQ(0, getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&destination), &size));
    ASSERT_EQ(0, connect(sender.Get(), reinterpret_cast<sockaddr*>(&destination), size));
    sockaddr_storage source{};
    socklen_t source_size = sizeof(source);
    ASSERT_EQ(0, getsockname(sender.Get(), reinterpret_cast<sockaddr*>(&source), &source_size));
    ASSERT_EQ(3, send(sender.Get(), "abc", 3, 0));
    ASSERT_NO_FATAL_FAILURE(WaitReadable(receiver.Get()));
    OutputPages pages;
    ASSERT_NO_FATAL_FAILURE(pages.Initialize());
    auto* length = reinterpret_cast<socklen_t*>(pages.Address());
    *length = sizeof(sockaddr_storage);
    std::array<char, 3> payload{};
    ASSERT_EQ(3, recvfrom(receiver.Get(), payload.data(), payload.size(), MSG_DONTWAIT, pages.Address(), length));
    EXPECT_EQ(0, memcmp(payload.data(), "abc", payload.size()));
    EXPECT_EQ(source_size, *length);
    // The capacity is read once: the final actual length overwrites the first
    // four address bytes, while the remaining bytes retain the source address.
    const auto* actual = reinterpret_cast<const char*>(pages.Address());
    const auto* expected = reinterpret_cast<const char*>(&source);
    EXPECT_EQ(0, memcmp(actual + sizeof(*length), expected + sizeof(*length), source_size - sizeof(*length)));
}

TEST_P(SocketAddressOutputWrite, SocketNamesValidateLengthBeforeCopy) {
    const int family = GetParam();
    Fd listener(socket(family, SOCK_STREAM, 0)), client(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(listener.Get(), 0); ASSERT_GE(client.Get(), 0);
    auto address = Loopback(family);
    socklen_t size = AddressSize(family);
    ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), size));
    ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &size));
    ASSERT_EQ(0, listen(listener.Get(), 2));
    ASSERT_EQ(0, connect(client.Get(), reinterpret_cast<sockaddr*>(&address), size));
    Fd accepted(accept(listener.Get(), nullptr, nullptr));
    ASSERT_GE(accepted.Get(), 0);
    for (bool peer : {false, true}) {
        for (bool negative : {false, true}) {
            SCOPED_TRACE(testing::Message() << "peer=" << peer << " negative=" << negative);
            OutputPages pages;
            ASSERT_NO_FATAL_FAILURE(pages.Initialize());
            socklen_t* length = negative ? pages.Length() : nullptr;
            if (negative) *length = UINT32_MAX;
            errno = 0;
            const int result = peer ? getpeername(accepted.Get(), pages.Address(), length)
                                    : getsockname(accepted.Get(), pages.Address(), length);
            EXPECT_EQ(-1, result);
            EXPECT_EQ(negative ? EINVAL : EFAULT, errno);
            pages.ExpectAddressUnchanged();
        }
    }
}

TEST_P(SocketAddressOutputWrite, FailedAcceptDoesNotInstallDescriptor) {
    const int family = GetParam();
    Fd listener(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(listener.Get(), 0);
    auto address = Loopback(family);
    socklen_t size = AddressSize(family);
    ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), size));
    ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &size));
    ASSERT_EQ(0, listen(listener.Get(), 2));
    // Reserving a duplicate exposes the next free descriptor without relying on
    // procfs enumeration or an arbitrary upper fd limit.
    for (bool accept_four : {false, true}) {
        for (bool negative : {false, true}) {
            SCOPED_TRACE(testing::Message() << "accept4=" << accept_four << " negative=" << negative);
            Fd client(socket(family, SOCK_STREAM, 0));
            ASSERT_GE(client.Get(), 0);
            ASSERT_EQ(0, connect(client.Get(), reinterpret_cast<sockaddr*>(&address), size));
            ASSERT_NO_FATAL_FAILURE(WaitReadable(listener.Get()));
            OutputPages pages;
            ASSERT_NO_FATAL_FAILURE(pages.Initialize());
            socklen_t* length = negative ? pages.Length() : nullptr;
            if (negative) *length = UINT32_MAX;
            const int expected_fd = dup(listener.Get());
            ASSERT_GE(expected_fd, 0);
            ASSERT_EQ(0, close(expected_fd));
            errno = 0;
            // Older musl treats any EINVAL as missing accept4 support and
            // retries accept, consuming a second connection. Test the syscall
            // directly so a negative output length observes one admission.
            Fd result(accept_four ? static_cast<int>(syscall(SYS_accept4, listener.Get(), pages.Address(),
                                                            length, SOCK_NONBLOCK | SOCK_CLOEXEC))
                                  : accept(listener.Get(), pages.Address(), length));
            EXPECT_EQ(-1, result.Get());
            EXPECT_EQ(negative ? EINVAL : EFAULT, errno);
            pages.ExpectAddressUnchanged();
            Fd next(dup(listener.Get()));
            ASSERT_GE(next.Get(), 0);
            EXPECT_EQ(expected_fd, next.Get());
        }
        // An omitted address suppresses even an invalid length pointer.
        Fd client(socket(family, SOCK_STREAM, 0));
        ASSERT_GE(client.Get(), 0);
        ASSERT_EQ(0, connect(client.Get(), reinterpret_cast<sockaddr*>(&address), size));
        ASSERT_NO_FATAL_FAILURE(WaitReadable(listener.Get()));
        auto* invalid_length = reinterpret_cast<socklen_t*>(1);
        Fd accepted(accept_four ? static_cast<int>(syscall(SYS_accept4, listener.Get(), nullptr,
                                                          invalid_length, SOCK_NONBLOCK | SOCK_CLOEXEC))
                                : accept(listener.Get(), nullptr, invalid_length));
        ASSERT_GE(accepted.Get(), 0) << errno;
    }
}

INSTANTIATE_TEST_SUITE_P(Ipv4AndIpv6, SocketAddressOutputWrite, testing::Values(AF_INET, AF_INET6));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
