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
    void ReadOnlyAddress() { ASSERT_EQ(0, mprotect(bytes_, page_, PROT_READ)); }
    void ProtectAddress() { ASSERT_EQ(0, mprotect(bytes_, page_, PROT_NONE)); }
    void ExpectAddressUnchanged() const {
        const auto* bytes = reinterpret_cast<const unsigned char*>(bytes_);
        for (size_t i = 0; i < sizeof(sockaddr_storage); ++i) EXPECT_EQ(0x55, bytes[i]) << i;
    }
 private:
    size_t page_;
    char* bytes_;
};

// Access the numeric-zero mapping only through syscalls. The ordinary
// shared alias provides C++-defined initialization and inspection.
class SharedZeroPage {
 public:
    SharedZeroPage() : page_(static_cast<size_t>(sysconf(_SC_PAGESIZE))) {}
    ~SharedZeroPage() {
        if (zero_mapped_) munmap(nullptr, page_);
        if (alias_ != MAP_FAILED) munmap(alias_, page_);
    }
    void Initialize() {
        backing_.Set(static_cast<int>(syscall(SYS_memfd_create, "socket-name-zero", 0)));
        ASSERT_GE(backing_.Get(), 0) << errno;
        ASSERT_EQ(0, ftruncate(backing_.Get(), page_));
        alias_ = static_cast<char*>(mmap(nullptr, page_, PROT_READ | PROT_WRITE, MAP_SHARED, backing_.Get(), 0));
        ASSERT_NE(MAP_FAILED, alias_) << errno;
        ASSERT_NE(nullptr, alias_);
        // MAP_FIXED_NOREPLACE never destroys a pre-existing mapping. Use the
        // UAPI value because older musl headers do not provide the macro.
        void* mapped = mmap(nullptr, page_, PROT_READ | PROT_WRITE, MAP_SHARED | 0x100000, backing_.Get(), 0);
        ASSERT_NE(MAP_FAILED, mapped) << errno;
        if (mapped != nullptr) {
            munmap(mapped, page_);
            FAIL() << "MAP_FIXED_NOREPLACE did not map numeric address zero";
        }
        zero_mapped_ = true;
    }
    void ProtectZero(int protection) { ASSERT_EQ(0, mprotect(nullptr, page_, protection)); }
    char* Alias() const { return alias_; }
 private:
    Fd backing_;
    size_t page_;
    char* alias_ = static_cast<char*>(MAP_FAILED);
    bool zero_mapped_ = false;
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


// Getname always requests an address result, unlike accept/recvfrom's optional
// output. Exercise both entry points against the same established endpoint.
int GetName(bool peer, int fd, sockaddr* address, socklen_t* length) {
    return static_cast<int>(syscall(peer ? SYS_getpeername : SYS_getsockname, fd, address, length));
}

class SocketAddressOutputName : public testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        const int family = GetParam();
        if (family == AF_UNIX) {
            int pair[2];
            ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
            endpoint_.Set(pair[0]); counterpart_.Set(pair[1]);
            return;
        }
        Fd listener(socket(family, SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0) << errno;
        auto address = Loopback(family);
        socklen_t length = AddressSize(family);
        ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), length));
        ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &length));
        if (family == AF_VSOCK) reinterpret_cast<sockaddr_vm*>(&address)->svm_cid = 1;
        ASSERT_EQ(0, listen(listener.Get(), 2));
        counterpart_.Set(socket(family, SOCK_STREAM, 0));
        ASSERT_GE(counterpart_.Get(), 0);
        ASSERT_EQ(0, connect(counterpart_.Get(), reinterpret_cast<sockaddr*>(&address), length));
        endpoint_.Set(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(endpoint_.Get(), 0);
    }
    Fd endpoint_, counterpart_;
};

enum class NameBoundary {
    Normal, Short, IntMax, NullZero, NullPositive, NullIntMax, NullNegative, NullIntMin,
    NullLength, BadLength, CrossingLength, NullNullLength, NullBadLength, NullCrossingLength, NullReadOnlyLength, Negative, IntMin, ReadOnlyLength,
    ReadOnlyZeroLength, NullReadOnlyZeroLength, BadAddressZero, ProtectedAddressZero,
    ReadOnlyAddress, ReadOnlyAddressZero, BadAddress, ProtectedAddress,
};

void VerifyNameOutput(int fd, bool peer) {
    sockaddr_storage expected{};
    socklen_t actual_length = sizeof(expected);
    ASSERT_EQ(0, GetName(peer, fd, reinterpret_cast<sockaddr*>(&expected), &actual_length));
    ASSERT_GT(actual_length, 0u);
    ASSERT_LE(actual_length, sizeof(expected));
    for (NameBoundary boundary : {NameBoundary::Normal, NameBoundary::Short, NameBoundary::IntMax,
         NameBoundary::NullZero, NameBoundary::NullPositive, NameBoundary::NullIntMax,
         NameBoundary::NullNegative, NameBoundary::NullIntMin, NameBoundary::NullLength,
         NameBoundary::BadLength, NameBoundary::CrossingLength, NameBoundary::NullNullLength,
         NameBoundary::NullBadLength, NameBoundary::NullCrossingLength, NameBoundary::NullReadOnlyLength, NameBoundary::Negative,
         NameBoundary::IntMin, NameBoundary::ReadOnlyLength, NameBoundary::ReadOnlyZeroLength,
         NameBoundary::NullReadOnlyZeroLength, NameBoundary::BadAddressZero,
         NameBoundary::ProtectedAddressZero, NameBoundary::ReadOnlyAddress,
         NameBoundary::ReadOnlyAddressZero, NameBoundary::BadAddress, NameBoundary::ProtectedAddress}) {
        SCOPED_TRACE(testing::Message() << "peer=" << peer << " boundary=" << static_cast<int>(boundary));
        OutputPages pages;
        ASSERT_NO_FATAL_FAILURE(pages.Initialize());
        sockaddr* address = pages.Address();
        socklen_t* length = pages.Length();
        socklen_t capacity = sizeof(sockaddr_storage);
        int error = 0;
        bool readable_address = true, address_copy = true;
        switch (boundary) {
        case NameBoundary::Normal: break;
        case NameBoundary::Short: capacity = 1; break;
        case NameBoundary::IntMax: capacity = 0x7fffffffu; break;
        case NameBoundary::NullZero: address = nullptr; capacity = 0; break;
        case NameBoundary::NullPositive: address = nullptr; error = EFAULT; break;
        case NameBoundary::NullIntMax: address = nullptr; capacity = 0x7fffffffu; error = EFAULT; break;
        case NameBoundary::NullNegative: address = nullptr; capacity = UINT32_MAX; error = EINVAL; break;
        case NameBoundary::NullIntMin: address = nullptr; capacity = 0x80000000u; error = EINVAL; break;
        case NameBoundary::NullLength: length = nullptr; error = EFAULT; break;
        case NameBoundary::BadLength: length = reinterpret_cast<socklen_t*>(1); error = EFAULT; break;
        case NameBoundary::CrossingLength: length = pages.CrossingLength(); error = EFAULT; break;
        case NameBoundary::NullNullLength: address = nullptr; length = nullptr; error = EFAULT; break;
        case NameBoundary::NullBadLength: address = nullptr; length = reinterpret_cast<socklen_t*>(1); error = EFAULT; break;
        case NameBoundary::NullCrossingLength: address = nullptr; length = pages.CrossingLength(); error = EFAULT; break;
        case NameBoundary::NullReadOnlyLength: address = nullptr; error = EFAULT; break;
        case NameBoundary::Negative: capacity = UINT32_MAX; error = EINVAL; break;
        case NameBoundary::IntMin: capacity = 0x80000000u; error = EINVAL; break;
        case NameBoundary::ReadOnlyLength: error = EFAULT; break;
        case NameBoundary::ReadOnlyZeroLength: capacity = 0; error = EFAULT; break;
        case NameBoundary::NullReadOnlyZeroLength: address = nullptr; capacity = 0; error = EFAULT; break;
        case NameBoundary::BadAddressZero: address = reinterpret_cast<sockaddr*>(1); capacity = 0; break;
        case NameBoundary::ProtectedAddressZero: capacity = 0; readable_address = false; break;
        case NameBoundary::ReadOnlyAddress: error = EFAULT; break;
        case NameBoundary::ReadOnlyAddressZero: capacity = 0; break;
        case NameBoundary::BadAddress: address = reinterpret_cast<sockaddr*>(1); error = EFAULT; break;
        case NameBoundary::ProtectedAddress: readable_address = false; error = EFAULT; break;
        }
        *pages.Length() = capacity;
        if (boundary == NameBoundary::ReadOnlyLength || boundary == NameBoundary::NullReadOnlyLength ||
            boundary == NameBoundary::ReadOnlyZeroLength ||
            boundary == NameBoundary::NullReadOnlyZeroLength) {
            ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength());
        }
        if (!readable_address) { ASSERT_NO_FATAL_FAILURE(pages.ProtectAddress()); }
        if (boundary == NameBoundary::ReadOnlyAddress || boundary == NameBoundary::ReadOnlyAddressZero) {
            ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyAddress());
        }
        address_copy = address == pages.Address() && capacity > 0 &&
            (error == 0 || boundary == NameBoundary::ReadOnlyLength);
        errno = 0;
        EXPECT_EQ(error ? -1 : 0, GetName(peer, fd, address, length)) << errno;
        if (error) EXPECT_EQ(error, errno);
        EXPECT_EQ(error ? capacity : actual_length, *pages.Length());
        if (readable_address) {
            if (address_copy) {
                const size_t copied = capacity < actual_length ? capacity : actual_length;
                EXPECT_EQ(0, memcmp(pages.Address(), &expected, copied));
                const auto* bytes = reinterpret_cast<const unsigned char*>(pages.Address());
                for (size_t i = copied; i < sizeof(expected); ++i) EXPECT_EQ(0x55, bytes[i]) << i;
            } else {
                pages.ExpectAddressUnchanged();
            }
        }
    }
    // Output faults must not change the local/peer endpoint or connection.
    sockaddr_storage after{};
    socklen_t after_length = sizeof(after);
    ASSERT_EQ(0, GetName(peer, fd, reinterpret_cast<sockaddr*>(&after), &after_length));
    EXPECT_EQ(actual_length, after_length);
    EXPECT_EQ(0, memcmp(&expected, &after, actual_length));
}

TEST_P(SocketAddressOutputName, RequiredOutputHonorsCapacityAndFaultOrdering) {
    for (bool peer : {false, true}) {
        socklen_t length = 0;
        EXPECT_EQ(0, GetName(peer, endpoint_.Get(), nullptr, &length)) << errno;
        EXPECT_EQ(GetParam() == AF_UNIX ? sizeof(sa_family_t) : AddressSize(GetParam()), length);
        ASSERT_NO_FATAL_FAILURE(VerifyNameOutput(endpoint_.Get(), peer));
    }
    ASSERT_EQ(1, send(counterpart_.Get(), "x", 1, MSG_NOSIGNAL));
    char payload = 0;
    ASSERT_EQ(1, recv(endpoint_.Get(), &payload, 1, 0));
    EXPECT_EQ('x', payload);
}

TEST_P(SocketAddressOutputName, AddressMayOverlapLength) {
    for (bool peer : {false, true}) {
        SCOPED_TRACE(peer);
        sockaddr_storage expected{};
        socklen_t actual_length = sizeof(expected);
        ASSERT_EQ(0, GetName(peer, endpoint_.Get(), reinterpret_cast<sockaddr*>(&expected), &actual_length));
        sockaddr_storage output{};
        socklen_t capacity = sizeof(output);
        memcpy(&output, &capacity, sizeof(capacity));
        ASSERT_EQ(0, GetName(peer, endpoint_.Get(), reinterpret_cast<sockaddr*>(&output),
                             reinterpret_cast<socklen_t*>(&output)));
        socklen_t resulting_length = 0;
        memcpy(&resulting_length, &output, sizeof(resulting_length));
        EXPECT_EQ(actual_length, resulting_length);
        if (actual_length > sizeof(resulting_length)) {
            EXPECT_EQ(0, memcmp(reinterpret_cast<char*>(&expected) + sizeof(resulting_length),
                                reinterpret_cast<char*>(&output) + sizeof(resulting_length),
                                actual_length - sizeof(resulting_length)));
        }
    }
}

INSTANTIATE_TEST_SUITE_P(InetAndUnix, SocketAddressOutputName, testing::Values(AF_INET, AF_INET6, AF_UNIX));
INSTANTIATE_TEST_SUITE_P(VsockLocal, SocketAddressOutputName, testing::Values(AF_VSOCK));

TEST(SocketAddressOutputNames, MappedNumericZeroUsesNormalOutputProtocol) {
    SharedZeroPage mapped;
    ASSERT_NO_FATAL_FAILURE(mapped.Initialize());
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    Fd endpoint(pair[0]), counterpart(pair[1]);
    for (bool peer : {false, true}) {
        sockaddr_storage expected{};
        socklen_t actual_length = sizeof(expected);
        ASSERT_EQ(0, GetName(peer, endpoint.Get(), reinterpret_cast<sockaddr*>(&expected), &actual_length));
        ASSERT_EQ(sizeof(sa_family_t), actual_length);
        for (int boundary = 0; boundary < 4; ++boundary) {
            SCOPED_TRACE(testing::Message() << "peer=" << peer << " boundary=" << boundary);
            memset(mapped.Alias(), 0x55, sizeof(sockaddr_storage));
            sockaddr_storage address;
            memset(&address, 0x55, sizeof(address));
            socklen_t capacity = boundary == 2 ? 0 : sizeof(sockaddr_storage);
            socklen_t ordinary_length = capacity;
            if (boundary != 0) memcpy(mapped.Alias(), &capacity, sizeof(capacity));
            sockaddr* output = boundary == 1 ? reinterpret_cast<sockaddr*>(&address) : nullptr;
            socklen_t* length = boundary == 0 ? &ordinary_length : nullptr;
            ASSERT_EQ(0, GetName(peer, endpoint.Get(), output, length)) << errno;
            socklen_t resulting_length = ordinary_length;
            if (boundary != 0) memcpy(&resulting_length, mapped.Alias(), sizeof(resulting_length));
            EXPECT_EQ(actual_length, resulting_length);
            if (boundary < 2) {
                const void* actual = boundary == 0 ? static_cast<void*>(mapped.Alias()) : &address;
                EXPECT_EQ(0, memcmp(actual, &expected, actual_length));
            }
            // Aliased output writes its final length after any address copy.
            const size_t written = boundary == 0 ? actual_length : sizeof(socklen_t);
            for (size_t i = written; i < sizeof(sockaddr_storage); ++i) {
                EXPECT_EQ(0x55, static_cast<unsigned char>(mapped.Alias()[i])) << i;
            }
        }
    }
}

TEST(SocketAddressOutputNames, MappedNumericZeroIovecsRespectMappingPermissions) {
    SharedZeroPage mapped;
    ASSERT_NO_FATAL_FAILURE(mapped.Initialize());
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, pair));
    Fd receiver(pair[0]), sender(pair[1]);
    char prefix = 'a';
    memcpy(mapped.Alias(), "bc", 2);
    iovec input[] = {{&prefix, 1}, {nullptr, 2}};
    msghdr outgoing{};
    outgoing.msg_iov = input;
    outgoing.msg_iovlen = 2;
    ASSERT_EQ(3, sendmsg(sender.Get(), &outgoing, MSG_NOSIGNAL)) << errno;
    char received_prefix = 0;
    iovec output[] = {{&received_prefix, 1}, {nullptr, 2}};
    msghdr incoming{};
    incoming.msg_iov = output;
    incoming.msg_iovlen = 2;
    memset(mapped.Alias(), 0x55, sizeof(sockaddr_storage));
    ASSERT_EQ(3, recvmsg(receiver.Get(), &incoming, MSG_DONTWAIT)) << errno;
    EXPECT_EQ('a', received_prefix);
    EXPECT_EQ(0, memcmp(mapped.Alias(), "bc", 2));
    for (size_t i = 2; i < sizeof(sockaddr_storage); ++i) {
        EXPECT_EQ(0x55, static_cast<unsigned char>(mapped.Alias()[i])) << i;
    }
    for (int protection : {PROT_READ, PROT_NONE}) {
        SCOPED_TRACE(protection);
        memset(mapped.Alias(), 0x55, sizeof(sockaddr_storage));
        received_prefix = 0;
        ASSERT_NO_FATAL_FAILURE(mapped.ProtectZero(protection));
        ASSERT_EQ(3, send(sender.Get(), "def", 3, MSG_NOSIGNAL));
        errno = 0;
        EXPECT_EQ(-1, recvmsg(receiver.Get(), &incoming, MSG_DONTWAIT));
        EXPECT_EQ(EFAULT, errno);
        // Sequential copying may expose the valid first iovec before the
        // protected second iovec fails; the latter must remain untouched.
        EXPECT_EQ('d', received_prefix);
        for (size_t i = 0; i < sizeof(sockaddr_storage); ++i) {
            EXPECT_EQ(0x55, static_cast<unsigned char>(mapped.Alias()[i])) << i;
        }
    }
}

TEST(SocketAddressOutputNames, DescriptorErrorsPrecedeOutputAccess) {
    Fd regular(open("/dev/null", O_RDONLY));
    ASSERT_GE(regular.Get(), 0);
    for (bool peer : {false, true}) {
        for (int fd : {-1, regular.Get()}) {
            for (int boundary = 0; boundary < 4; ++boundary) {
                SCOPED_TRACE(testing::Message() << "peer=" << peer << " fd=" << fd << " boundary=" << boundary);
                OutputPages pages;
                ASSERT_NO_FATAL_FAILURE(pages.Initialize());
                *pages.Length() = UINT32_MAX;
                if (boundary == 3) { ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength()); }
                sockaddr* address = boundary == 0 ? nullptr : pages.Address();
                socklen_t* length = boundary == 0 ? nullptr : boundary == 1 ? reinterpret_cast<socklen_t*>(1) : pages.Length();
                errno = 0;
                EXPECT_EQ(-1, GetName(peer, fd, address, length));
                EXPECT_EQ(fd == -1 ? EBADF : ENOTSOCK, errno);
                EXPECT_EQ(UINT32_MAX, *pages.Length());
                pages.ExpectAddressUnchanged();
            }
        }
    }
}

class SocketAddressOutputUnconnectedName : public testing::TestWithParam<int> {};
TEST_P(SocketAddressOutputUnconnectedName, ProtocolErrorsPrecedeOutputAndLocalZeroCapacitySucceeds) {
    const int family = GetParam();
    Fd socket_fd(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(socket_fd.Get(), 0) << errno;
    for (int boundary = 0; boundary < 4; ++boundary) {
        SCOPED_TRACE(boundary);
        OutputPages pages;
        ASSERT_NO_FATAL_FAILURE(pages.Initialize());
        *pages.Length() = UINT32_MAX;
        if (boundary == 3) { ASSERT_NO_FATAL_FAILURE(pages.ReadOnlyLength()); }
        sockaddr* address = boundary == 0 ? nullptr : pages.Address();
        socklen_t* length = boundary == 0 ? nullptr : boundary == 1 ? reinterpret_cast<socklen_t*>(1) : pages.Length();
        errno = 0;
        EXPECT_EQ(-1, GetName(true, socket_fd.Get(), address, length));
        EXPECT_EQ(ENOTCONN, errno);
        EXPECT_EQ(UINT32_MAX, *pages.Length());
        pages.ExpectAddressUnchanged();
    }
    socklen_t length = 0;
    ASSERT_EQ(0, GetName(false, socket_fd.Get(), nullptr, &length)) << errno;
    EXPECT_EQ(family == AF_UNIX ? sizeof(sa_family_t) : AddressSize(family), length);
}
INSTANTIATE_TEST_SUITE_P(InetAndUnix, SocketAddressOutputUnconnectedName, testing::Values(AF_INET, AF_INET6, AF_UNIX));
TEST(SocketAddressOutputVsockName, LocalRequiredOutputHonorsCapacityAndFaultOrdering) {
    Fd socket_fd(socket(AF_VSOCK, SOCK_STREAM, 0));
    ASSERT_GE(socket_fd.Get(), 0) << errno;
    ASSERT_NO_FATAL_FAILURE(VerifyNameOutput(socket_fd.Get(), false));
}

// Linux hosts without an accessible vsock device run the InetAndUnix cases;
// this mandatory guest case is selected explicitly rather than silently skipped.
INSTANTIATE_TEST_SUITE_P(VsockLocal, SocketAddressOutputUnconnectedName, testing::Values(AF_VSOCK));

INSTANTIATE_TEST_SUITE_P(Ipv4AndIpv6, SocketAddressOutputWrite, testing::Values(AF_INET, AF_INET6));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
