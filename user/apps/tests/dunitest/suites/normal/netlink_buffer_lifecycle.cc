#include <gtest/gtest.h>

#include <linux/netlink.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <thread>
#include <vector>

namespace {
class Fd {
 public:
  explicit Fd(int fd) : fd_(fd) {}
  ~Fd() { reset(); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
  void reset() { if (fd_ >= 0) close(fd_); fd_ = -1; }
 private:
  int fd_;
};

bool Bind(int fd, sockaddr_nl* address) {
  address->nl_family = AF_NETLINK;
  if (bind(fd, reinterpret_cast<sockaddr*>(address), sizeof(*address)) != 0)
    return false;
  socklen_t length = sizeof(*address);
  return getsockname(fd, reinterpret_cast<sockaddr*>(address), &length) == 0;
}

ssize_t Send(int fd, const sockaddr_nl& destination,
             const std::vector<char>& bytes, int flags = 0) {
  return sendto(fd, bytes.data(), bytes.size(), flags,
                reinterpret_cast<const sockaddr*>(&destination), sizeof(destination));
}

class NetlinkReceiveTimeout : public testing::TestWithParam<int> {};

TEST_P(NetlinkReceiveTimeout, BoundEmptyQueueHonorsOneReceiveBudget) {
  Fd socket_fd(socket(AF_NETLINK, SOCK_RAW, GetParam()));
  ASSERT_GE(socket_fd.get(), 0) << strerror(errno);
  sockaddr_nl address{};
  ASSERT_TRUE(Bind(socket_fd.get(), &address)) << strerror(errno);
  timeval timeout{0, 120000};
  ASSERT_EQ(setsockopt(socket_fd.get(), SOL_SOCKET, SO_RCVTIMEO,
                       &timeout, sizeof(timeout)), 0);
  std::array<char, 64> bytes{};
  auto start = std::chrono::steady_clock::now();
  EXPECT_EQ(recv(socket_fd.get(), bytes.data(), bytes.size(), MSG_DONTWAIT), -1);
  EXPECT_EQ(errno, EAGAIN);
  EXPECT_LT(std::chrono::steady_clock::now() - start, std::chrono::seconds(1));
  start = std::chrono::steady_clock::now();
  EXPECT_EQ(recv(socket_fd.get(), bytes.data(), bytes.size(), 0), -1);
  EXPECT_EQ(errno, EAGAIN);
  auto elapsed = std::chrono::steady_clock::now() - start;
  EXPECT_GE(elapsed, std::chrono::milliseconds(60));
  EXPECT_LT(elapsed, std::chrono::seconds(2));
}

INSTANTIATE_TEST_SUITE_P(ExistingProtocols, NetlinkReceiveTimeout,
    testing::Values(NETLINK_ROUTE, NETLINK_NETFILTER, NETLINK_KOBJECT_UEVENT));

class UserQueue {
 public:
  Fd sender{socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT)};
  Fd receiver{socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT)};
  sockaddr_nl destination{};
  std::vector<char> bytes = std::vector<char>(1024, 'q');
  bool Init(int capacity = 0) {
    if (sender.get() < 0 || receiver.get() < 0) return false;
    timeval timeout{2, 0};
    return setsockopt(sender.get(), SOL_SOCKET, SO_SNDTIMEO,
                       &timeout, sizeof(timeout)) == 0 &&
        setsockopt(receiver.get(), SOL_SOCKET, SO_RCVBUF,
                   &capacity, sizeof(capacity)) == 0 &&
        Bind(receiver.get(), &destination);
  }
  int Fill() {
    for (int n = 0; n < 4096; ++n) {
      memcpy(bytes.data(), &n, sizeof(n));
      if (Send(sender.get(), destination, bytes, MSG_DONTWAIT) < 0)
        return errno == EAGAIN ? n : -1;
    }
    return -1;
  }
};

TEST(NetlinkBufferLifecycle, ShrinkingRetainsQueuedMessagesAndTheirOrder) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init(65536)) << strerror(errno);
  int count = queue.Fill();
  ASSERT_GT(count, 1);
  int capacity = 0;
  ASSERT_EQ(setsockopt(queue.receiver.get(), SOL_SOCKET, SO_RCVBUF,
                       &capacity, sizeof(capacity)), 0);
  EXPECT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes, MSG_DONTWAIT), -1);
  EXPECT_EQ(errno, EAGAIN);
  std::array<char, 1024> received{};
  for (int n = 0; n < count; ++n) {
    ASSERT_EQ(recv(queue.receiver.get(), received.data(), received.size(), MSG_DONTWAIT),
                static_cast<ssize_t>(received.size()));
    int ordinal;
    memcpy(&ordinal, received.data(), sizeof(ordinal));
    EXPECT_EQ(ordinal, n);
  }
  EXPECT_EQ(recv(queue.receiver.get(), received.data(), received.size(), MSG_DONTWAIT), -1);
  EXPECT_EQ(errno, EAGAIN);
  EXPECT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes, MSG_DONTWAIT),
              static_cast<ssize_t>(queue.bytes.size()));
}

TEST(NetlinkBufferLifecycle, ClosingReceiverAndRebindingPortKeepsDatagramOwnership) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init());
  ASSERT_GT(queue.Fill(), 0);
  Fd replacement(socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK, NETLINK_KOBJECT_UEVENT));
  ASSERT_GE(replacement.get(), 0);
  std::atomic<bool> entered{false}, completed{false};
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
  queue.receiver.reset();
  sockaddr_nl reused = queue.destination;
  int bind_result = bind(replacement.get(), reinterpret_cast<sockaddr*>(&reused), sizeof(reused));
  int bind_error = errno;
  sender.join();
  ASSERT_EQ(bind_result, 0) << bind_error;
  char byte;
  // Close wakes the sender concurrently with rebind. Linux retries lookup:
  // it can either observe no receiver or deliver once to the replacement.
  if (result < 0) {
    EXPECT_EQ(error, ECONNREFUSED);
    EXPECT_EQ(recv(replacement.get(), &byte, 1, MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
  } else {
    EXPECT_EQ(result, static_cast<ssize_t>(queue.bytes.size()));
    EXPECT_EQ(recv(replacement.get(), &byte, 1, MSG_TRUNC | MSG_DONTWAIT),
                static_cast<ssize_t>(queue.bytes.size()));
    EXPECT_EQ(recv(replacement.get(), &byte, 1, MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
  }
  // A new send uses the replacement regardless of that earlier race.
  EXPECT_EQ(Send(queue.sender.get(), reused, queue.bytes, MSG_DONTWAIT),
              static_cast<ssize_t>(queue.bytes.size()));
  EXPECT_EQ(recv(replacement.get(), &byte, 1, MSG_TRUNC | MSG_DONTWAIT),
              static_cast<ssize_t>(queue.bytes.size()));
}

TEST(NetlinkBufferLifecycle, ClosingReceiverReleasesAllBlockedSenders) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init());
  ASSERT_GT(queue.Fill(), 0);
  constexpr size_t kSenders = 3;
  std::array<std::thread, kSenders> senders;
  std::array<ssize_t, kSenders> results{};
  std::array<int, kSenders> errors{};
  std::atomic<unsigned> entered{0};
  for (size_t n = 0; n < kSenders; ++n) {
    senders[n] = std::thread([&, n] {
      entered.fetch_add(1);
      results[n] = Send(queue.sender.get(), queue.destination, queue.bytes);
      errors[n] = errno;
    });
  }
  while (entered.load() != kSenders) std::this_thread::yield();
  std::this_thread::sleep_for(std::chrono::milliseconds(80));
  auto closed = std::chrono::steady_clock::now();
  queue.receiver.reset();
  for (auto& sender : senders) sender.join();
  // Every waiter must be released by close, not just the first sender while
  // the others remain asleep until their two-second send timeout expires.
  EXPECT_LT(std::chrono::steady_clock::now() - closed, std::chrono::seconds(1));
  for (size_t n = 0; n < kSenders; ++n) {
    EXPECT_EQ(results[n], -1);
    EXPECT_EQ(errors[n], ECONNREFUSED);
  }
}

TEST(NetlinkBufferLifecycle, IncreasingCapacityAllowsAllBlockedSenders) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init());
  ASSERT_GT(queue.Fill(), 0);
  constexpr size_t kSenders = 3;
  std::array<std::thread, kSenders> senders;
  std::array<ssize_t, kSenders> results{};
  std::atomic<unsigned> entered{0}, completed{0};
  for (size_t n = 0; n < kSenders; ++n) {
    senders[n] = std::thread([&, n] {
      entered.fetch_add(1);
      results[n] = Send(queue.sender.get(), queue.destination, queue.bytes);
      completed.fetch_add(1);
    });
  }
  while (entered.load() != kSenders) std::this_thread::yield();
  std::this_thread::sleep_for(std::chrono::milliseconds(80));
  EXPECT_EQ(completed.load(), 0u);
  int capacity = 65536;
  EXPECT_EQ(setsockopt(queue.receiver.get(), SOL_SOCKET, SO_RCVBUF,
                       &capacity, sizeof(capacity)), 0);
  for (auto& sender : senders) sender.join();
  for (ssize_t result : results)
    EXPECT_EQ(result, static_cast<ssize_t>(queue.bytes.size()));
}
TEST_P(NetlinkReceiveTimeout, HugeReceiveCapacityCopiesOnlyActualDatagram) {
  Fd fd(socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK, GetParam()));
  ASSERT_GE(fd.get(), 0);
  sockaddr_nl address{};
  ASSERT_TRUE(Bind(fd.get(), &address));
  constexpr size_t capacity = 1ULL << 30;
  void* mapping = mmap(nullptr, capacity, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
  ASSERT_NE(mapping, MAP_FAILED);
  iovec iov{mapping, capacity};
  msghdr message{};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  EXPECT_EQ(recvmsg(fd.get(), &message, MSG_DONTWAIT), -1);
  EXPECT_EQ(errno, EAGAIN);
  struct Request { nlmsghdr header; unsigned char nfgen[4]; } request{};
  request.header.nlmsg_len = sizeof(request);
  request.header.nlmsg_type = GetParam() == NETLINK_NETFILTER ? (10 << 8) | 16 : NLMSG_NOOP;
  request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  request.header.nlmsg_seq = 599;
  sockaddr_nl destination{};
  destination.nl_family = AF_NETLINK;
  if (GetParam() == NETLINK_KOBJECT_UEVENT) destination = address;
  EXPECT_EQ(sendto(fd.get(), &request, sizeof(request), 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)),
             static_cast<ssize_t>(sizeof(request)));
  ssize_t actual = recvmsg(fd.get(), &message, MSG_DONTWAIT | MSG_PEEK);
  EXPECT_GT(actual, 0);
  EXPECT_LT(actual, 4096);
  if (actual > 0) {
    char first = *static_cast<char*>(mapping);
    iov.iov_len = 1;
    EXPECT_EQ(recvmsg(fd.get(), &message, MSG_DONTWAIT | MSG_TRUNC), actual);
    EXPECT_TRUE(message.msg_flags & MSG_TRUNC);
    EXPECT_EQ(*static_cast<char*>(mapping), first);
  }
  EXPECT_EQ(munmap(mapping, capacity), 0);
}

TEST(NetlinkBufferLifecycle, ReadAndReadvDoNotImposeScratchLengthLimit) {
  UserQueue queue;
  ASSERT_TRUE(queue.Init());
  int size = 128 * 1024;
  ASSERT_EQ(setsockopt(queue.sender.get(), SOL_SOCKET, SO_SNDBUF, &size, sizeof(size)), 0);
  queue.bytes.resize(96 * 1024);
  for (size_t n = 0; n < queue.bytes.size(); ++n) queue.bytes[n] = static_cast<char>(n);
  std::vector<char> output(queue.bytes.size());
  for (bool vectored : {false, true}) {
    ASSERT_EQ(Send(queue.sender.get(), queue.destination, queue.bytes),
               static_cast<ssize_t>(queue.bytes.size()));
    std::fill(output.begin(), output.end(), 0);
    ssize_t count;
    if (vectored) {
      iovec iovs[2]{{output.data(), 17000}, {output.data() + 17000, output.size() - 17000}};
      count = readv(queue.receiver.get(), iovs, 2);
    } else {
      count = read(queue.receiver.get(), output.data(), output.size());
    }
    EXPECT_EQ(count, static_cast<ssize_t>(output.size()));
    EXPECT_EQ(output, queue.bytes);
  }
}
}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
