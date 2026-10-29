#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <net/if.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <initializer_list>
#include <string>
#include <tuple>

namespace {
using Clock = std::chrono::steady_clock;
constexpr uint16_t kProtocol = 0x88ba;

enum class Family { Route, Netfilter, Uevent, PacketRaw, PacketDgram };
enum class Entry { Read, Readv, Recv, Recvmsg };

class Fd {
 public:
  explicit Fd(int value = -1) : value_(value) {}
  ~Fd() { if (value_ >= 0) close(value_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return value_; }
  void reset(int value) { if (value_ >= 0) close(value_); value_ = value; }
 private:
  int value_;
};

bool IsPacket(Family family) {
  return family == Family::PacketRaw || family == Family::PacketDgram;
}

bool Open(Fd* fd, Family family, int flags = 0, bool receive_packets = false) {
  int type = family == Family::PacketDgram ? SOCK_DGRAM : SOCK_RAW;
  int protocol = family == Family::Route ? NETLINK_ROUTE :
      family == Family::Netfilter ? NETLINK_NETFILTER : NETLINK_KOBJECT_UEVENT;
  if (IsPacket(family)) protocol = receive_packets ? htons(kProtocol) : 0;
  fd->reset(socket(IsPacket(family) ? AF_PACKET : AF_NETLINK, type | flags, protocol));
  if (fd->get() < 0) return false;
  if (IsPacket(family)) {
    if (!receive_packets) return true;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(kProtocol);
    address.sll_ifindex = if_nametoindex("lo");
    return address.sll_ifindex > 0 &&
        bind(fd->get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
  }
  sockaddr_nl address{};
  address.nl_family = AF_NETLINK;
  return bind(fd->get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
}

bool Timeout(int fd, long microseconds) {
  timeval timeout{microseconds / 1000000, microseconds % 1000000};
  return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0;
}

ssize_t Receive(int fd, Entry entry, int flags = 0, void* buffer = nullptr,
                size_t size = 32) {
  std::array<char, 32> local{};
  if (buffer == nullptr) buffer = local.data();
  iovec iov{buffer, size};
  switch (entry) {
    case Entry::Read: return read(fd, buffer, size);
    case Entry::Readv: return readv(fd, &iov, 1);
    case Entry::Recv: return recv(fd, buffer, size, flags);
    case Entry::Recvmsg: {
      msghdr message{};
      message.msg_iov = &iov;
      message.msg_iovlen = 1;
      return recvmsg(fd, &message, flags);
    }
  }
  return -1;
}

void ExpectEmpty(int fd, Entry entry, bool nonblocking, int flags = 0) {
  // Distinct finite budgets make a stale protocol-mode cache fail without
  // signals, helper threads, or an unbounded hang. Blocking is checked too:
  // merely making every receive nonblocking cannot satisfy these controls.
  ASSERT_TRUE(Timeout(fd, nonblocking ? 1000000 : 120000)) << strerror(errno);
  const auto start = Clock::now();
  errno = 0;
  const ssize_t result = Receive(fd, entry, flags);
  const int error = errno;
  const auto elapsed = Clock::now() - start;
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EAGAIN, error);
  if (nonblocking) {
    EXPECT_LT(elapsed, std::chrono::milliseconds(500));
  } else {
    EXPECT_GE(elapsed, std::chrono::milliseconds(60));
    EXPECT_LT(elapsed, std::chrono::seconds(3));
  }
}

using ModeParam = std::tuple<Family, Entry>;
class SocketReceiveMode : public testing::TestWithParam<ModeParam> {};

TEST_P(SocketReceiveMode, RuntimeFcntlAndIoctlSynchronizeSharedDescription) {
  const auto [family, entry] = GetParam();
  for (bool use_ioctl : {false, true}) {
    SCOPED_TRACE(use_ioctl ? "FIONBIO" : "F_SETFL");
    Fd original;
    ASSERT_TRUE(Open(&original, family)) << strerror(errno);
    Fd alias(dup(original.get()));
    ASSERT_GE(alias.get(), 0) << strerror(errno);
    const int initial_flags = fcntl(original.get(), F_GETFL);
    ASSERT_GE(initial_flags, 0);
    ASSERT_EQ(0, initial_flags & O_NONBLOCK);
    if (use_ioctl) {
      int enabled = 1;
      ASSERT_EQ(0, ioctl(alias.get(), FIONBIO, &enabled)) << strerror(errno);
    } else {
      ASSERT_EQ(0, fcntl(alias.get(), F_SETFL, initial_flags | O_NONBLOCK));
    }
    EXPECT_NE(0, fcntl(original.get(), F_GETFL) & O_NONBLOCK);
    ExpectEmpty(original.get(), entry, true);
    if (use_ioctl) {
      int disabled = 0;
      ASSERT_EQ(0, ioctl(original.get(), FIONBIO, &disabled));
    } else {
      ASSERT_EQ(0, fcntl(original.get(), F_SETFL, initial_flags));
    }
    EXPECT_EQ(0, fcntl(alias.get(), F_GETFL) & O_NONBLOCK);
    ExpectEmpty(alias.get(), entry, false);
  }
}

TEST_P(SocketReceiveMode, CreationModeCanBeClearedAndDoesNotLeakAcrossSockets) {
  const auto [family, entry] = GetParam();
  Fd nonblocking;
  Fd blocking;
  ASSERT_TRUE(Open(&nonblocking, family, SOCK_NONBLOCK)) << strerror(errno);
  ASSERT_TRUE(Open(&blocking, family)) << strerror(errno);
  EXPECT_NE(0, fcntl(nonblocking.get(), F_GETFL) & O_NONBLOCK);
  EXPECT_EQ(0, fcntl(blocking.get(), F_GETFL) & O_NONBLOCK);
  ExpectEmpty(nonblocking.get(), entry, true);
  ExpectEmpty(blocking.get(), entry, false);
  int disabled = 0;
  ASSERT_EQ(0, ioctl(nonblocking.get(), FIONBIO, &disabled));
  EXPECT_EQ(0, fcntl(nonblocking.get(), F_GETFL) & O_NONBLOCK);
  ExpectEmpty(nonblocking.get(), entry, false);
}

INSTANTIATE_TEST_SUITE_P(AllReceiveOwners, SocketReceiveMode,
    testing::Combine(testing::Values(Family::Route, Family::Netfilter,
                                     Family::Uevent, Family::PacketRaw,
                                     Family::PacketDgram),
                     testing::Values(Entry::Read, Entry::Readv,
                                     Entry::Recv, Entry::Recvmsg)));

class PacketReceiveAdmission : public testing::TestWithParam<std::tuple<Family, Entry>> {};

TEST_P(PacketReceiveAdmission, UnsupportedFlagsPrecedeErrorQueueAndOrdinaryQueue) {
  const auto [family, entry] = GetParam();
  Fd socket;
  ASSERT_TRUE(Open(&socket, family)) << strerror(errno);
  ASSERT_TRUE(Timeout(socket.get(), 1000000));
  // Linux packet_recvmsg() admits only PEEK, DONTWAIT, TRUNC and ERRQUEUE
  // for native callers. Unknown raw bits must survive syscall decoding too;
  // ERRQUEUE must not mask either recognized unsupported or unknown flags.
  for (int unsupported : std::initializer_list<int>{MSG_OOB, MSG_WAITALL, MSG_DONTROUTE, MSG_CTRUNC,
                          MSG_EOR, MSG_CONFIRM, MSG_NOSIGNAL, MSG_MORE,
                          MSG_CMSG_CLOEXEC, MSG_WAITFORONE, 0x01000000, 0x10000000}) {
    for (int queue : std::initializer_list<int>{0, MSG_ERRQUEUE}) {
      SCOPED_TRACE(unsupported | queue);
      errno = 0;
      const auto start = Clock::now();
      EXPECT_EQ(-1, Receive(socket.get(), entry, unsupported | queue));
      EXPECT_EQ(EINVAL, errno);
      EXPECT_LT(Clock::now() - start, std::chrono::milliseconds(500));
    }
  }
}

TEST_P(PacketReceiveAdmission, EmptyErrorQueueNeverWaitsEvenOnBlockingSocket) {
  const auto [family, entry] = GetParam();
  Fd socket;
  ASSERT_TRUE(Open(&socket, family)) << strerror(errno);
  for (int extra : std::initializer_list<int>{0, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC,
                    MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT}) {
    SCOPED_TRACE(extra);
    ExpectEmpty(socket.get(), entry, true, MSG_ERRQUEUE | extra);
  }
  // Normal receive still uses the ordinary queue and the configured timeout.
  ExpectEmpty(socket.get(), entry, false);
}

using Frame = std::array<unsigned char, 60>;

bool QueueFrame(int receiver, Frame* output) {
  Fd sender;
  if (!Open(&sender, Family::PacketRaw)) return false;
  Frame frame{};
  std::memset(frame.data(), 0xff, 6);
  frame[6] = 0x02;
  frame[11] = 0x01;
  frame[12] = kProtocol >> 8;
  frame[13] = kProtocol & 0xff;
  for (size_t i = 14; i < frame.size(); ++i) frame[i] = static_cast<unsigned char>(i);
  sockaddr_ll destination{};
  destination.sll_family = AF_PACKET;
  destination.sll_protocol = htons(kProtocol);
  destination.sll_ifindex = if_nametoindex("lo");
  if (destination.sll_ifindex <= 0 ||
      sendto(sender.get(), frame.data(), frame.size(), 0,
             reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) !=
          static_cast<ssize_t>(frame.size())) return false;
  pollfd ready{receiver, POLLIN, 0};
  if (poll(&ready, 1, 1000) != 1 || (ready.revents & POLLIN) == 0) return false;
  *output = frame;
  return true;
}

TEST_P(PacketReceiveAdmission, ErrorQueueAndRejectedFlagsDoNotConsumeNormalData) {
  const auto [family, entry] = GetParam();
  Fd receiver;
  ASSERT_TRUE(Open(&receiver, family, 0, true)) << strerror(errno);
  ASSERT_TRUE(Timeout(receiver.get(), 1000000));
  Frame frame;
  ASSERT_TRUE(QueueFrame(receiver.get(), &frame)) << strerror(errno);
  for (int flags : std::initializer_list<int>{MSG_ERRQUEUE, MSG_ERRQUEUE | MSG_PEEK,
                    MSG_ERRQUEUE | MSG_TRUNC | MSG_DONTWAIT,
                    MSG_OOB, MSG_OOB | MSG_ERRQUEUE, MSG_WAITALL}) {
    SCOPED_TRACE(flags);
    std::array<unsigned char, 64> output;
    output.fill(0xa5);
    errno = 0;
    EXPECT_EQ(-1, Receive(receiver.get(), entry, flags, output.data(), output.size()));
    EXPECT_EQ((flags & (MSG_OOB | MSG_WAITALL)) ? EINVAL : EAGAIN, errno);
    for (unsigned char value : output) EXPECT_EQ(0xa5, value);
  }
  const size_t offset = family == Family::PacketDgram ? 14 : 0;
  std::array<unsigned char, 64> output{};
  const size_t length = frame.size() - offset;
  ASSERT_EQ(static_cast<ssize_t>(length),
      Receive(receiver.get(), entry, MSG_PEEK | MSG_DONTWAIT, output.data(), output.size()));
  EXPECT_EQ(0, std::memcmp(output.data(), frame.data() + offset, length));
  output.fill(0);
  ASSERT_EQ(static_cast<ssize_t>(length),
      Receive(receiver.get(), entry, MSG_DONTWAIT, output.data(), output.size()));
  EXPECT_EQ(0, std::memcmp(output.data(), frame.data() + offset, length));
}

TEST_P(PacketReceiveAdmission, AdmissionPrecedesPayloadAccess) {
  const auto [family, entry] = GetParam();
  Fd socket;
  ASSERT_TRUE(Open(&socket, family)) << strerror(errno);
  ASSERT_TRUE(Timeout(socket.get(), 1000000));
  for (int flags : std::initializer_list<int>{MSG_ERRQUEUE, MSG_OOB,
                    MSG_OOB | MSG_ERRQUEUE, 0x01000000,
                    0x10000000 | MSG_ERRQUEUE}) {
    SCOPED_TRACE(flags);
    iovec iov{nullptr, 1};
    msghdr message{};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    errno = 0;
    const ssize_t result = entry == Entry::Recv
        ? recv(socket.get(), nullptr, 1, flags)
        : recvmsg(socket.get(), &message, flags);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(flags == MSG_ERRQUEUE ? EAGAIN : EINVAL, errno);
  }
}

class PacketBatchAdmission : public testing::TestWithParam<Family> {};

TEST_P(PacketBatchAdmission, WaitForOneBelongsToBatchAndDoesNotReachProtocol) {
  const Family family = GetParam();
  Fd receiver;
  ASSERT_TRUE(Open(&receiver, family, 0, true)) << strerror(errno);
  ASSERT_TRUE(Timeout(receiver.get(), 1000000));
  Frame frame;
  ASSERT_TRUE(QueueFrame(receiver.get(), &frame)) << strerror(errno);
  // Loopback can provide outgoing and incoming copies. Three slots leave an
  // empty suffix, whose receive must use DONTWAIT after the first record.
  std::array<std::array<unsigned char, 64>, 3> output{};
  std::array<iovec, 3> iov{};
  std::array<mmsghdr, 3> messages{};
  for (size_t i = 0; i < messages.size(); ++i) {
    iov[i] = {output[i].data(), output[i].size()};
    messages[i].msg_hdr.msg_iov = &iov[i];
    messages[i].msg_hdr.msg_iovlen = 1;
  }
  const auto start = Clock::now();
  const int count = recvmmsg(receiver.get(), messages.data(), messages.size(),
                            MSG_WAITFORONE, nullptr);
  ASSERT_GE(count, 1) << strerror(errno);
  EXPECT_LT(count, static_cast<int>(messages.size()));
  EXPECT_LT(Clock::now() - start, std::chrono::milliseconds(500));
  const size_t offset = family == Family::PacketDgram ? 14 : 0;
  for (int i = 0; i < count; ++i) {
    ASSERT_EQ(frame.size() - offset, messages[i].msg_len);
    EXPECT_EQ(0, std::memcmp(output[i].data(), frame.data() + offset, messages[i].msg_len));
  }
}

TEST_P(PacketBatchAdmission, AdmissionBypassesOrdinaryReadabilityTimeout) {
  Fd socket;
  ASSERT_TRUE(Open(&socket, GetParam())) << strerror(errno);
  ASSERT_TRUE(Timeout(socket.get(), 1000000));
  for (int flags : std::initializer_list<int>{MSG_ERRQUEUE, MSG_ERRQUEUE | MSG_WAITFORONE,
                    MSG_OOB, MSG_OOB | MSG_ERRQUEUE, 0x01000000}) {
    SCOPED_TRACE(flags);
    char output[32]{};
    iovec iov{output, sizeof(output)};
    mmsghdr message{};
    message.msg_hdr.msg_iov = &iov;
    message.msg_hdr.msg_iovlen = 1;
    timespec timeout{1, 0};
    const auto start = Clock::now();
    errno = 0;
    EXPECT_EQ(-1, recvmmsg(socket.get(), &message, 1, flags, &timeout));
    EXPECT_EQ((flags & ~MSG_WAITFORONE) == MSG_ERRQUEUE ? EAGAIN : EINVAL, errno);
    EXPECT_LT(Clock::now() - start, std::chrono::milliseconds(500));
  }
}

TEST_P(PacketBatchAdmission, InvalidIovStillPrecedesProtocolAdmission) {
  Fd socket;
  ASSERT_TRUE(Open(&socket, GetParam())) << strerror(errno);
  for (int flags : {MSG_ERRQUEUE, MSG_OOB}) {
    mmsghdr message{};
    message.msg_hdr.msg_iov = reinterpret_cast<iovec*>(1);
    message.msg_hdr.msg_iovlen = 1;
    timespec timeout{1, 0};
    errno = 0;
    EXPECT_EQ(-1, recvmmsg(socket.get(), &message, 1, flags, &timeout));
    EXPECT_EQ(EFAULT, errno);
  }
}

INSTANTIATE_TEST_SUITE_P(PacketKinds, PacketBatchAdmission,
    testing::Values(Family::PacketRaw, Family::PacketDgram));

INSTANTIATE_TEST_SUITE_P(PacketKindsAndReceiveEntries, PacketReceiveAdmission,
    testing::Combine(testing::Values(Family::PacketRaw, Family::PacketDgram),
                     testing::Values(Entry::Recv, Entry::Recvmsg)));
}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
