#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/capability.h>
#include <linux/netlink.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <climits>
#include <cstring>
#include <thread>
#include <vector>

#include "rtnetlink_route_test_support.h"

namespace {
class Fd {
 public:
  explicit Fd(int value) : value_(value) {}
  ~Fd() { if (value_ >= 0) close(value_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return value_; }
  void reset() { if (value_ >= 0) close(value_); value_ = -1; }
 private:
  int value_;
};

int GetInt(int fd, int option) {
  int value = -1;
  socklen_t length = sizeof(value);
  if (getsockopt(fd, SOL_SOCKET, option, &value, &length) != 0 ||
      length != sizeof(value)) return -1;
  return value;
}

bool Bind(int fd, sockaddr_nl* address) {
  *address = {};
  address->nl_family = AF_NETLINK;
  if (bind(fd, reinterpret_cast<sockaddr*>(address), sizeof(*address)) != 0)
    return false;
  socklen_t length = sizeof(*address);
  return getsockname(fd, reinterpret_cast<sockaddr*>(address), &length) == 0;
}

ssize_t Send(int fd, const sockaddr_nl& destination,
             const std::vector<char>& bytes, int flags = 0) {
  return sendto(fd, bytes.data(), bytes.size(), flags,
                reinterpret_cast<const sockaddr*>(&destination),
                sizeof(destination));
}

std::vector<char> Noop(size_t length, bool ack = false) {
  std::vector<char> bytes(length);
  nlmsghdr header{};
  header.nlmsg_len = length;
  header.nlmsg_type = NLMSG_NOOP;
  header.nlmsg_flags = ack ? NLM_F_ACK : 0;
  header.nlmsg_seq = 175;
  if (length >= sizeof(header)) memcpy(bytes.data(), &header, sizeof(header));
  return bytes;
}

class NetlinkBuffer : public ::testing::TestWithParam<int> {};

TEST_P(NetlinkBuffer, DefaultsNormalAndBindPreservesConfiguration) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW, GetParam()));
  ASSERT_GE(fd.get(), 0) << strerror(errno);
  EXPECT_GT(GetInt(fd.get(), SO_SNDBUF), 32);
  EXPECT_GT(GetInt(fd.get(), SO_RCVBUF), 0);
  for (int option : {SO_SNDBUF, SO_RCVBUF}) {
    int value = 4096;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    EXPECT_EQ(GetInt(fd.get(), option), value * 2);
  }
  sockaddr_nl address{};
  ASSERT_TRUE(Bind(fd.get(), &address)) << strerror(errno);
  EXPECT_EQ(GetInt(fd.get(), SO_SNDBUF), 8192);
  EXPECT_EQ(GetInt(fd.get(), SO_RCVBUF), 8192);
  for (int option : {SO_SNDBUF, SO_RCVBUF}) {
    int value = 16384;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    EXPECT_EQ(GetInt(fd.get(), option), value * 2);
  }
}

TEST_P(NetlinkBuffer, ShortNegativeAndClampedHints) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW, GetParam()));
  ASSERT_GE(fd.get(), 0);
  for (int option : {SO_SNDBUF, SO_RCVBUF}) {
    int value = 4096;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    EXPECT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, 3), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(GetInt(fd.get(), option), 8192);
    value = INT_MAX;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    int maximum = GetInt(fd.get(), option);
    EXPECT_GE(maximum, 8192);
    value = -1;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    EXPECT_EQ(GetInt(fd.get(), option), maximum);
    value = 0;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)), 0);
    int minimum = GetInt(fd.get(), option);
    EXPECT_GT(minimum, 32);
    EXPECT_LE(minimum, 8192);
  }
}

TEST_P(NetlinkBuffer, ForceHintsAndCurrentCallerPermission) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW, GetParam()));
  ASSERT_GE(fd.get(), 0);
  for (auto pair : {std::pair<int, int>{SO_SNDBUFFORCE, SO_SNDBUF},
                    {SO_RCVBUFFORCE, SO_RCVBUF}}) {
    int value = -1;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, pair.first, &value, sizeof(value)), 0)
        << strerror(errno);
    int minimum = GetInt(fd.get(), pair.second);
    EXPECT_GT(minimum, 32);
    value = 1024 * 1024;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, pair.first, &value, sizeof(value)), 0);
    EXPECT_EQ(GetInt(fd.get(), pair.second), value * 2);
    EXPECT_EQ(setsockopt(fd.get(), SOL_SOCKET, pair.first, &value, 3), -1);
    EXPECT_EQ(errno, EINVAL);
  }
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    std::array<__user_cap_data_struct, 2> data{};
    if (syscall(SYS_capset, &header, data.data()) != 0) _exit(1);
    int value = 4096;
    for (int option : {SO_SNDBUFFORCE, SO_RCVBUFFORCE}) {
      if (setsockopt(fd.get(), SOL_SOCKET, option, &value, sizeof(value)) != -1 ||
          errno != EPERM) _exit(2);
    }
    _exit(0);
  }
  int status = 0;
  ASSERT_EQ(waitpid(child, &status, 0), child);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST_P(NetlinkBuffer, AllSendEntrypointsRespectSndbufMinus32) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW, GetParam()));
  ASSERT_GE(fd.get(), 0);
  int requested = 4096;
  ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_SNDBUF,
                       &requested, sizeof(requested)), 0);
  sockaddr_nl destination{};
  destination.nl_family = AF_NETLINK;
  ASSERT_EQ(connect(fd.get(), reinterpret_cast<sockaddr*>(&destination),
                    sizeof(destination)), 0);
  size_t maximum = GetInt(fd.get(), SO_SNDBUF) - 32;
  for (bool too_large : {false, true}) {
    auto bytes = Noop(maximum + (too_large ? 1 : 0));
    for (int entrypoint = 0; entrypoint != 4; ++entrypoint) {
      SCOPED_TRACE(entrypoint);
      ssize_t result;
      if (entrypoint == 0) result = Send(fd.get(), destination, bytes);
      else if (entrypoint == 1) result = write(fd.get(), bytes.data(), bytes.size());
      else if (entrypoint == 2) result = send(fd.get(), bytes.data(), bytes.size(), 0);
      else {
        iovec vectors[2] = {{bytes.data(), bytes.size() / 2},
                           {bytes.data() + bytes.size() / 2,
                            bytes.size() - bytes.size() / 2}};
        msghdr message{};
        message.msg_iov = vectors;
        message.msg_iovlen = 2;
        result = sendmsg(fd.get(), &message, 0);
      }
      if (too_large) {
        EXPECT_EQ(result, -1);
        EXPECT_EQ(errno, EMSGSIZE);
      } else {
        EXPECT_EQ(result, static_cast<ssize_t>(bytes.size())) << strerror(errno);
      }
    }
  }
}

INSTANTIATE_TEST_SUITE_P(ExistingProtocols, NetlinkBuffer,
    ::testing::Values(NETLINK_ROUTE, NETLINK_NETFILTER, NETLINK_KOBJECT_UEVENT));

TEST(NetlinkBufferQueue, RouteRepliesAreChargedAndRecoverAfterDrain) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK, NETLINK_ROUTE));
  ASSERT_GE(fd.get(), 0);
  int capacity = 0;
  ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_RCVBUF, &capacity, sizeof(capacity)), 0);
  sockaddr_nl kernel{};
  kernel.nl_family = AF_NETLINK;
  auto request = Noop(NLMSG_HDRLEN, true);
  bool overflow = false;
  // Do not assume an skb size or fixed number of ACKs per buffer.
  for (int n = 0; n < 4096; ++n) {
    ASSERT_EQ(Send(fd.get(), kernel, request), static_cast<ssize_t>(request.size()));
    pollfd event{fd.get(), POLLIN | POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 0), 1);
    if (event.revents & POLLERR) { overflow = true; break; }
  }
  ASSERT_TRUE(overflow) << "Kernel ACKs did not consume receive capacity";
  EXPECT_EQ(GetInt(fd.get(), SO_ERROR), ENOBUFS);
  EXPECT_EQ(GetInt(fd.get(), SO_ERROR), 0);
  std::array<char, 4096> bytes{};
  ASSERT_GT(recv(fd.get(), bytes.data(), bytes.size(), MSG_PEEK | MSG_DONTWAIT), 0);
  ASSERT_EQ(Send(fd.get(), kernel, request), static_cast<ssize_t>(request.size()));
  // Reading SO_ERROR or peeking must not end the congestion episode.
  EXPECT_EQ(GetInt(fd.get(), SO_ERROR), 0);
  int drained = 0;
  while (recv(fd.get(), bytes.data(), 1, MSG_TRUNC | MSG_DONTWAIT) > 0) ++drained;
  ASSERT_GT(drained, 0);
  ASSERT_EQ(errno, EAGAIN);
  ASSERT_EQ(Send(fd.get(), kernel, request), static_cast<ssize_t>(request.size()));
  pollfd event{fd.get(), POLLIN | POLLERR, 0};
  ASSERT_EQ(poll(&event, 1, 1000), 1);
  EXPECT_EQ(event.revents & POLLERR, 0);
  EXPECT_GT(recv(fd.get(), bytes.data(), bytes.size(), MSG_DONTWAIT), 0);
}

class UserQueue {
 public:
  Fd sender{socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT)};
  Fd receiver{socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT)};
  sockaddr_nl destination{};
  std::vector<char> bytes = Noop(1024);
  bool Init() {
    if (sender.get() < 0 || receiver.get() < 0) return false;
    int capacity = 0;
    return setsockopt(receiver.get(), SOL_SOCKET, SO_RCVBUF, &capacity,
                       sizeof(capacity)) == 0 && Bind(receiver.get(), &destination);
  }
  bool Fill() {
    for (int n = 0; n < 4096; ++n) {
      if (Send(sender.get(), destination, bytes, MSG_DONTWAIT) < 0)
        return errno == EAGAIN;
    }
    return false;
  }
};

TEST(NetlinkBufferQueue, UserUnicastBackpressurePeekAndTruncate) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init()) << strerror(errno);
  ASSERT_TRUE(queue.Fill()) << strerror(errno);
  EXPECT_EQ(GetInt(queue.receiver.get(), SO_ERROR), 0);
  pollfd writable{queue.sender.get(), POLLOUT, 0};
  ASSERT_EQ(poll(&writable, 1, 0), 1);
  EXPECT_NE(writable.revents & POLLOUT, 0);
  char byte;
  ASSERT_EQ(recv(queue.receiver.get(), &byte, 1, MSG_PEEK | MSG_TRUNC),
              static_cast<ssize_t>(queue.bytes.size()));
  EXPECT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes, MSG_DONTWAIT), -1);
  EXPECT_EQ(errno, EAGAIN);
  ASSERT_EQ(recv(queue.receiver.get(), &byte, 1, MSG_TRUNC),
              static_cast<ssize_t>(queue.bytes.size()));
  EXPECT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes, MSG_DONTWAIT),
              static_cast<ssize_t>(queue.bytes.size()));
}

TEST(NetlinkBufferQueue, UserUnicastHonorsSendTimeout) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init());
  ASSERT_TRUE(queue.Fill());
  timeval timeout{0, 150000};
  ASSERT_EQ(setsockopt(queue.sender.get(), SOL_SOCKET, SO_SNDTIMEO,
                       &timeout, sizeof(timeout)), 0);
  auto start = std::chrono::steady_clock::now();
  EXPECT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes), -1);
  EXPECT_EQ(errno, EAGAIN);
  EXPECT_GE(std::chrono::steady_clock::now() - start, std::chrono::milliseconds(75));
  EXPECT_EQ(GetInt(queue.receiver.get(), SO_ERROR), 0);
}

TEST(NetlinkBufferQueue, UserUnicastWakesAfterReceiveOrClose) {
  for (bool close_receiver : {false, true}) {
    SCOPED_TRACE(close_receiver);
    UserQueue queue;
    ASSERT_TRUE(queue.Init());
    ASSERT_TRUE(queue.Fill());
    timeval timeout{2, 0};
    ASSERT_EQ(setsockopt(queue.sender.get(), SOL_SOCKET, SO_SNDTIMEO,
                         &timeout, sizeof(timeout)), 0);
    std::atomic<bool> entered{false};
    std::atomic<bool> completed{false};
    ssize_t result = 0;
    int error = 0;
    std::thread sender([&] {
      entered.store(true);
      result = Send(queue.sender.get(), queue.destination, queue.bytes);
      error = errno;
      completed.store(true);
    });
    while (!entered.load()) std::this_thread::yield();
    std::this_thread::sleep_for(std::chrono::milliseconds(80));
    EXPECT_FALSE(completed.load());
    if (close_receiver) queue.receiver.reset();
    else {
      char byte;
      EXPECT_EQ(recv(queue.receiver.get(), &byte, 1, MSG_TRUNC),
                  static_cast<ssize_t>(queue.bytes.size()));
    }
    sender.join();
    if (close_receiver) { EXPECT_EQ(result, -1); EXPECT_EQ(error, ECONNREFUSED); }
    else EXPECT_EQ(result, static_cast<ssize_t>(queue.bytes.size()));
  }
}

void InNetworkNamespace(const std::function<void()>& test) {
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    if (unshare(CLONE_NEWNET) != 0) _exit(99);
    test();
    _exit(::testing::Test::HasFailure() ? 1 : 0);
  }
  int status = 0;
  ASSERT_EQ(waitpid(child, &status, 0), child);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(WEXITSTATUS(status), 0);
}

void PopulateRoutes(int count) {
  Fd fd(OpenRouteSocket());
  ASSERT_GE(fd.get(), 0);
  unsigned loopback = if_nametoindex("lo");
  ASSERT_NE(loopback, 0u);
  ASSERT_EQ(SetLinkUp(fd.get(), loopback, true, 1), 0);
  for (int n = 0; n < count; ++n) {
    RouteSpec route;
    route.dst = htonl(0xc6130000u + n);
    route.prefix_len = 32;
    route.table = 100;
    route.oif = loopback;
    ASSERT_EQ(SendRouteRequest(fd.get(), RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        route, n + 2), 0) << n;
  }
}

bool StartRouteDump(int fd, uint32_t sequence, uint8_t family = AF_INET) {
  struct { nlmsghdr header; rtmsg route; } request{};
  request.header.nlmsg_len = sizeof(request);
  request.header.nlmsg_type = RTM_GETROUTE;
  request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
  request.header.nlmsg_seq = sequence;
  request.route.rtm_family = family;
  return send(fd, &request, sizeof(request), 0) == sizeof(request);
}

void ReadIpv6DumpToDone(int fd, uint32_t sequence) {
  bool done = false;
  for (int n = 0; n < 1024 && !done; ++n) {
    pollfd event{fd, POLLIN | POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 1000), 1);
    ASSERT_EQ(event.revents & POLLERR, 0);
    std::array<char, 65536> bytes{};
    ssize_t length = recv(fd, bytes.data(), bytes.size(), MSG_DONTWAIT);
    ASSERT_GT(length, 0);
    for (auto* header = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(header, length); header = NLMSG_NEXT(header, length)) {
      ASSERT_EQ(header->nlmsg_seq, sequence);
      if (header->nlmsg_type == NLMSG_DONE) { done = true; continue; }
      ASSERT_EQ(header->nlmsg_type, RTM_NEWROUTE);
      ASSERT_GE(header->nlmsg_len, NLMSG_LENGTH(sizeof(rtmsg)));
      EXPECT_EQ(reinterpret_cast<rtmsg*>(NLMSG_DATA(header))->rtm_family, AF_INET6);
    }
  }
  EXPECT_TRUE(done);
}

TEST(NetlinkBufferQueue, RouteDumpExceedsSmallBufferWithoutLossOrPeekProgress) {
  InNetworkNamespace([] {
    constexpr int kRoutes = 256;
    PopulateRoutes(kRoutes);
    ASSERT_FALSE(::testing::Test::HasFailure());
    Fd fd(OpenRouteSocket());
    ASSERT_GE(fd.get(), 0);
    int capacity = 0;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_RCVBUF, &capacity, sizeof(capacity)), 0);
    ASSERT_TRUE(StartRouteDump(fd.get(), 951));
    std::array<char, 65536> bytes{}, peek{};
    pollfd event{fd.get(), POLLIN | POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 1000), 1);
    ASSERT_EQ(event.revents & POLLERR, 0);
    ssize_t peek_size = recv(fd.get(), peek.data(), peek.size(), MSG_PEEK);
    ASSERT_GT(peek_size, 0);
    std::vector<bool> seen(kRoutes, false);
    int count = 0, datagrams = 0;
    bool done = false;
    while (!done && datagrams < 1024) {
      event.revents = 0;
      ASSERT_EQ(poll(&event, 1, 1000), 1);
      ASSERT_EQ(event.revents & POLLERR, 0);
      ssize_t length = recv(fd.get(), bytes.data(), bytes.size(), MSG_DONTWAIT);
      ASSERT_GT(length, 0) << strerror(errno);
      if (datagrams == 0) {
        ASSERT_EQ(length, peek_size);
        EXPECT_EQ(memcmp(bytes.data(), peek.data(), length), 0);
      }
      ++datagrams;
      for (auto* header = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(header, length); header = NLMSG_NEXT(header, length)) {
        ASSERT_EQ(header->nlmsg_seq, 951u);
        ASSERT_NE(header->nlmsg_type, NLMSG_ERROR);
        ASSERT_NE(header->nlmsg_flags & NLM_F_MULTI, 0);
        if (header->nlmsg_type == NLMSG_DONE) { done = true; continue; }
        ASSERT_EQ(header->nlmsg_type, RTM_NEWROUTE);
        auto* route = reinterpret_cast<rtmsg*>(NLMSG_DATA(header));
        if (route->rtm_dst_len != 32 || route->rtm_table != 100) continue;
        int remaining = header->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
        for (auto* attribute = RTM_RTA(route); RTA_OK(attribute, remaining);
             attribute = RTA_NEXT(attribute, remaining)) {
          if (attribute->rta_type != RTA_DST || RTA_PAYLOAD(attribute) != 4) continue;
          uint32_t destination;
          memcpy(&destination, RTA_DATA(attribute), sizeof(destination));
          uint32_t ordinal = ntohl(destination) - 0xc6130000u;
          ASSERT_LT(ordinal, static_cast<uint32_t>(kRoutes));
          EXPECT_FALSE(seen[ordinal]) << "duplicate route " << ordinal;
          seen[ordinal] = true;
          ++count;
        }
      }
    }
    EXPECT_TRUE(done);
    EXPECT_GE(datagrams, 2);
    EXPECT_EQ(count, kRoutes);
    EXPECT_EQ(GetInt(fd.get(), SO_ERROR), 0);
    // More than one scan budget is rejected by this family filter. The
    // empty intermediate chunks must not strand the dump without POLLIN.
    ASSERT_TRUE(StartRouteDump(fd.get(), 952, AF_INET6));
    ReadIpv6DumpToDone(fd.get(), 952);
  });
}

TEST(NetlinkBufferQueue, OnlyOneDumpRunsPerSocketAndCompletionReleasesIt) {
  InNetworkNamespace([] {
    PopulateRoutes(256);
    ASSERT_FALSE(::testing::Test::HasFailure());
    Fd fd(OpenRouteSocket());
    ASSERT_GE(fd.get(), 0);
    int capacity = 65536;
    ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_RCVBUF, &capacity, sizeof(capacity)), 0);
    ASSERT_TRUE(StartRouteDump(fd.get(), 961));
    ASSERT_TRUE(StartRouteDump(fd.get(), 962));
    bool done = false, busy = false;
    for (int n = 0; n < 1024 && (!done || !busy); ++n) {
      pollfd event{fd.get(), POLLIN | POLLERR, 0};
      ASSERT_EQ(poll(&event, 1, 1000), 1);
      ASSERT_EQ(event.revents & POLLERR, 0);
      std::array<char, 65536> bytes{};
      ssize_t length = recv(fd.get(), bytes.data(), bytes.size(), MSG_DONTWAIT);
      ASSERT_GT(length, 0);
      for (auto* header = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(header, length); header = NLMSG_NEXT(header, length)) {
        if (header->nlmsg_seq == 961 && header->nlmsg_type == NLMSG_DONE) done = true;
        if (header->nlmsg_seq == 962 && header->nlmsg_type == NLMSG_ERROR) {
          ASSERT_GE(header->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
          auto* error = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(header));
          EXPECT_EQ(error->error, -EBUSY);
          busy = true;
        }
      }
    }
    ASSERT_TRUE(done);
    ASSERT_TRUE(busy);
    EXPECT_EQ(GetInt(fd.get(), SO_ERROR), 0);
    ASSERT_TRUE(StartRouteDump(fd.get(), 963, AF_INET6));
    ReadIpv6DumpToDone(fd.get(), 963);
  });
}
}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
