#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>
#include <array>
#include <cstring>

namespace {
class Fd {
 public:
  explicit Fd(int fd) : fd_(fd) {}
  ~Fd() { if (fd_ >= 0) close(fd_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
  int release() { const int fd = fd_; fd_ = -1; return fd; }
 private:
  int fd_;
};

sockaddr_in Local4(int fd) {
  sockaddr_in local {};
  socklen_t length = sizeof(local);
  EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&local), &length), 0);
  EXPECT_EQ(length, sizeof(local));
  EXPECT_EQ(local.sin_family, AF_INET);
  return local;
}

TEST(UdpUnspecBind, IPv4AllocatesRealPortAndReceives) {
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address {};
  address.sin_family = AF_UNSPEC;
  ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  auto local = Local4(receiver.get());
  ASSERT_NE(local.sin_port, 0);
  EXPECT_EQ(local.sin_addr.s_addr, htonl(INADDR_ANY));
  local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(sendto(sender.get(), "U", 1, 0, reinterpret_cast<sockaddr*>(&local), sizeof(local)), 1);
  char value = 0;
  for (int i = 0; i < 100 && recv(receiver.get(), &value, 1, MSG_DONTWAIT) < 0; ++i) usleep(1000);
  EXPECT_EQ(value, 'U');
}

TEST(UdpUnspecBind, PortConflictAndRetryUseOriginalPayload) {
  Fd owner(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(owner.get(), 0);
  sockaddr_in address {};
  address.sin_family = AF_INET;
  ASSERT_EQ(bind(owner.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  address = Local4(owner.get());
  Fd receiver(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(receiver.get(), 0);
  address.sin_family = AF_UNSPEC;
  EXPECT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
  EXPECT_EQ(errno, EADDRINUSE);
  EXPECT_EQ(Local4(receiver.get()).sin_port, 0);
  ASSERT_EQ(close(owner.release()), 0);
  ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  EXPECT_EQ(Local4(receiver.get()).sin_port, address.sin_port);
}

TEST(UdpUnspecBind, RejectsNonzeroAddressWithoutChangingState) {
  Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(fd.get(), 0);
  sockaddr_in address {};
  address.sin_family = AF_UNSPEC;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
  EXPECT_EQ(errno, EAFNOSUPPORT);
  EXPECT_EQ(Local4(fd.get()).sin_port, 0);
  address.sin_family = AF_INET;
  ASSERT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  auto before = Local4(fd.get());
  address.sin_family = AF_UNSPEC;
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
  EXPECT_EQ(errno, EAFNOSUPPORT);
  EXPECT_EQ(Local4(fd.get()).sin_port, before.sin_port);
}

TEST(UdpUnspecBind, RepeatBindIsNotNoop) {
  Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
  sockaddr_in address {};
  address.sin_family = AF_UNSPEC;
  ASSERT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  auto before = Local4(fd.get());
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
  EXPECT_EQ(errno, EINVAL);
  EXPECT_EQ(Local4(fd.get()).sin_port, before.sin_port);
}

TEST(UdpUnspecBind, ProtocolMinimumLengthPrecedesFamily) {
  for (int family : {AF_INET, AF_INET6}) {
    for (socklen_t length : {0u, 1u, 2u, 15u, 16u, 23u, 24u, 27u, 28u, 128u}) {
      SCOPED_TRACE(testing::Message() << family << " length=" << length);
      Fd fd(socket(family, SOCK_DGRAM, 0));
      ASSERT_GE(fd.get(), 0);
      sockaddr_storage address {};
      address.ss_family = AF_UNSPEC;
      int result = bind(fd.get(), reinterpret_cast<sockaddr*>(&address), length);
      if (family == AF_INET && length >= sizeof(sockaddr_in)) {
        EXPECT_EQ(result, 0);
        EXPECT_NE(Local4(fd.get()).sin_port, 0);
      } else {
        EXPECT_EQ(result, -1);
        EXPECT_EQ(errno, length < (family == AF_INET ? 16u : 24u) ? EINVAL : EAFNOSUPPORT);
      }
    }
  }
}

TEST(UdpUnspecBind, NativeFamiliesAndMappedIPv6RemainDistinct) {
  for (int family : {AF_INET, AF_INET6}) {
    for (int input_family : {AF_INET, AF_INET6}) {
      Fd fd(socket(family, SOCK_DGRAM, 0));
      sockaddr_storage storage {};
      storage.ss_family = input_family;
      int result = bind(fd.get(), reinterpret_cast<sockaddr*>(&storage), sizeof(storage));
      EXPECT_EQ(result, family == input_family ? 0 : -1);
      if (family != input_family) EXPECT_EQ(errno, EAFNOSUPPORT);
    }
  }
  Fd mapped(socket(AF_INET6, SOCK_DGRAM, 0));
  sockaddr_in6 address {};
  address.sin6_family = AF_INET6;
  ASSERT_EQ(inet_pton(AF_INET6, "::ffff:127.0.0.1", &address.sin6_addr), 1);
  ASSERT_EQ(bind(mapped.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
  Fd old_layout(socket(AF_INET6, SOCK_DGRAM, 0));
  address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(bind(old_layout.get(), reinterpret_cast<sockaddr*>(&address), 24), 0);
}

TEST(UdpUnspecBind, ProtectedCopyAndLengthErrors) {
  Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
  long page_size = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page_size, 0);
  void* page = mmap(nullptr, page_size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(page, MAP_FAILED);
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(page), 16), -1);
  EXPECT_EQ(errno, EFAULT);
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(page), 1), -1);
  EXPECT_EQ(errno, EFAULT);
  EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(page), 129), -1);
  EXPECT_EQ(errno, EINVAL);
  EXPECT_EQ(bind(-1, reinterpret_cast<sockaddr*>(page), 16), -1);
  EXPECT_EQ(errno, EBADF);
  ASSERT_EQ(munmap(page, page_size), 0);
  EXPECT_EQ(Local4(fd.get()).sin_port, 0);
}

TEST(UdpUnspecBind, ConnectUnspecStillDisconnectsBothFamilies) {
  for (int family : {AF_INET, AF_INET6}) {
    Fd fd(socket(family, SOCK_DGRAM, 0));
    sockaddr_storage address {};
    socklen_t length;
    if (family == AF_INET) {
      auto* ip = reinterpret_cast<sockaddr_in*>(&address);
      ip->sin_family = family;
      ip->sin_port = htons(12345);
      ip->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      length = sizeof(*ip);
    } else {
      auto* ip = reinterpret_cast<sockaddr_in6*>(&address);
      ip->sin6_family = family;
      ip->sin6_port = htons(12345);
      ip->sin6_addr = in6addr_loopback;
      length = sizeof(*ip);
    }
    ASSERT_EQ(connect(fd.get(), reinterpret_cast<sockaddr*>(&address), length), 0);
    sockaddr unspecified {};
    ASSERT_EQ(connect(fd.get(), &unspecified, sizeof(unspecified.sa_family)), 0);
    length = sizeof(address);
    EXPECT_EQ(getpeername(fd.get(), reinterpret_cast<sockaddr*>(&address), &length), -1);
    EXPECT_EQ(errno, ENOTCONN);
  }
}

TEST(UdpUnspecBind, EntireDeclaredAddressIsCopiedBeforeProtocolValidation) {
  const long page_size = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page_size, 0);
  auto* pages = static_cast<char*>(mmap(nullptr, page_size * 2, PROT_READ | PROT_WRITE,
                                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  ASSERT_NE(pages, MAP_FAILED);
  ASSERT_EQ(mprotect(pages + page_size, page_size, PROT_NONE), 0);
  for (int family : {AF_INET, AF_INET6}) {
    const socklen_t prefix = family == AF_INET ? 16 : 24;
    auto* address = pages + page_size - prefix;
    std::memset(address, 0, prefix);
    const uint16_t input_family = family == AF_INET ? AF_UNSPEC : AF_INET6;
    std::memcpy(address, &input_family, sizeof(input_family));
    for (socklen_t length : {prefix, prefix + 1, socklen_t(128)}) {
      SCOPED_TRACE(testing::Message() << family << " length=" << length);
      Fd fd(socket(family, SOCK_DGRAM, 0));
      const int result = bind(fd.get(), reinterpret_cast<sockaddr*>(address), length);
      EXPECT_EQ(result, length == prefix ? 0 : -1);
      if (length != prefix) EXPECT_EQ(errno, EFAULT);
    }
  }
  ASSERT_EQ(munmap(pages, page_size * 2), 0);
}

TEST(UdpUnspecBind, UnknownFamilyAndFdErrorsHaveLinuxPriority) {
  sockaddr_storage address {};
  address.ss_family = 0xffff;
  for (int family : {AF_INET, AF_INET6}) {
    Fd fd(socket(family, SOCK_DGRAM, 0));
    EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
    EXPECT_EQ(errno, EAFNOSUPPORT);
  }
  EXPECT_EQ(bind(-1, nullptr, 129), -1);
  EXPECT_EQ(errno, EBADF);
  int pipe_fds[2];
  ASSERT_EQ(pipe(pipe_fds), 0);
  Fd read_end(pipe_fds[0]);
  Fd write_end(pipe_fds[1]);
  EXPECT_EQ(bind(read_end.get(), nullptr, 16), -1);
  EXPECT_EQ(errno, ENOTSOCK);
}
}  // namespace
int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
