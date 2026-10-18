#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <net/if.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstring>
#include <string>
#include <tuple>

namespace {
volatile sig_atomic_t first_signal = 0;
volatile sig_atomic_t second_signal = 0;
void SignalHandler(int signal) {
  if (signal == SIGUSR1) first_signal = 1;
  if (signal == SIGUSR2) second_signal = 1;
}

class SignalHandlers {
 public:
  bool Install(bool restart) {
    struct sigaction action = {};
    action.sa_handler = SignalHandler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = restart ? SA_RESTART : 0;
    first_installed_ = sigaction(SIGUSR1, &action, &old_first_) == 0;
    if (!first_installed_) return false;
    action.sa_flags = 0;
    second_installed_ = sigaction(SIGUSR2, &action, &old_second_) == 0;
    return second_installed_;
  }
  ~SignalHandlers() {
    if (second_installed_) sigaction(SIGUSR2, &old_second_, nullptr);
    if (first_installed_) sigaction(SIGUSR1, &old_first_, nullptr);
  }
 private:
  struct sigaction old_first_ = {}, old_second_ = {};
  bool first_installed_ = false, second_installed_ = false;
};

class Socket {
 public:
  ~Socket() { if (fd >= 0) close(fd); }
  bool Open(bool packet, int flags = 0) {
    fd = socket(packet ? AF_PACKET : AF_NETLINK, SOCK_RAW | flags,
                packet ? htons(0x88b5) : NETLINK_ROUTE);
    if (fd < 0) return false;
    if (packet) {
      sockaddr_ll address = {};
      address.sll_family = AF_PACKET;
      address.sll_protocol = htons(0x88b5);
      address.sll_ifindex = if_nametoindex("lo");
      if (address.sll_ifindex == 0) return false;
      return bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
    }
    sockaddr_nl address = {};
    address.nl_family = AF_NETLINK;
    return bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
  }
  bool Timeout(long seconds, long microseconds = 0) {
    timeval value = {seconds, microseconds};
    return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &value, sizeof(value)) == 0;
  }
  int fd = -1;
};

// An acknowledgement keeps the sender alive until receive has returned. Repeat
// the non-restarting signal so scheduling before receive cannot consume the
// only wakeup and leave an infinite socket operation stuck.
class SignalDriver {
 public:
  ~SignalDriver() {
    if (pid_ > 0) {
      kill(pid_, SIGKILL);
      while (waitpid(pid_, nullptr, 0) < 0 && errno == EINTR) {}
    }
    for (int fd : ack_) if (fd >= 0) close(fd);
  }
  bool Start() {
    if (pipe(ack_) != 0) return false;
    const pid_t parent = getpid();
    pid_ = fork();
    if (pid_ < 0) return false;
    if (pid_ == 0) {
      close(ack_[1]);
      bool sent = true;
      int signal = SIGUSR1;
      while (true) {
        pollfd descriptor = {ack_[0], POLLIN, 0};
        int ready;
        do { ready = poll(&descriptor, 1, 100); } while (ready < 0 && errno == EINTR);
        if (ready > 0) _exit(sent ? 0 : 2);
        if (ready < 0) _exit(3);
        if (kill(parent, signal) != 0) sent = false;
        signal = SIGUSR2;
      }
    }
    close(ack_[0]);
    ack_[0] = -1;
    return true;
  }
  bool Acknowledge() {
    const char done = 'x';
    ssize_t count;
    do { count = write(ack_[1], &done, 1); } while (count < 0 && errno == EINTR);
    return count == 1;
  }
  bool Join() {
    int status = 0;
    pid_t result;
    do { result = waitpid(pid_, &status, 0); } while (result < 0 && errno == EINTR);
    if (result != pid_) return false;
    pid_ = -1;
    return WIFEXITED(status) && WEXITSTATUS(status) == 0;
  }
 private:
  pid_t pid_ = -1;
  int ack_[2] = {-1, -1};
};

enum class Entry { Read, Readv, Recv, Recvmsg };
ssize_t Receive(int fd, Entry entry, int flags = 0) {
  char bytes[128] = {};
  iovec vector = {bytes, sizeof(bytes)};
  msghdr message = {};
  message.msg_iov = &vector;
  message.msg_iovlen = 1;
  switch (entry) {
    case Entry::Read: return read(fd, bytes, sizeof(bytes));
    case Entry::Readv: return readv(fd, &vector, 1);
    case Entry::Recv: return recv(fd, bytes, sizeof(bytes), flags);
    case Entry::Recvmsg: return recvmsg(fd, &message, flags);
  }
  return -1;
}

// A value beyond u64 microseconds is still a finite Linux timeout. AF_PACKET
// must retain its scheduler-tick representation when classifying interruption.
constexpr long kHugeFiniteSeconds = 20000000000000L;
using Case = std::tuple<bool, int, bool, Entry>;
class SocketReceiveInterrupt : public testing::TestWithParam<Case> {};

TEST_P(SocketReceiveInterrupt, UsesOriginalTimeoutRestartPolicy) {
  const auto [packet, timeout_kind, restart, entry] = GetParam();
  Socket socket;
  ASSERT_TRUE(socket.Open(packet)) << strerror(errno);
  // Installing zero must clear a previously finite timeout.
  if (timeout_kind == 0) {
    ASSERT_TRUE(socket.Timeout(2));
  }
  ASSERT_TRUE(socket.Timeout(timeout_kind == 2 ? kHugeFiniteSeconds :
                             timeout_kind == 1 ? 2 : 0)) << strerror(errno);
  SignalHandlers handlers;
  ASSERT_TRUE(handlers.Install(restart)) << strerror(errno);
  first_signal = second_signal = 0;
  SignalDriver driver;
  ASSERT_TRUE(driver.Start()) << strerror(errno);
  errno = 0;
  const ssize_t result = Receive(socket.fd, entry);
  const int error = errno;
  const bool saw_first = first_signal != 0;
  const bool saw_second = second_signal != 0;
  ASSERT_TRUE(driver.Acknowledge());
  // Reap before restoring handlers; no sender can signal a later case.
  ASSERT_TRUE(driver.Join());
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EINTR, error);
  EXPECT_TRUE(saw_first);
  EXPECT_EQ(timeout_kind == 0 && restart, saw_second);
}

std::string CaseName(const testing::TestParamInfo<Case>& info) {
  const auto [packet, timeout, restart, entry] = info.param;
  const char* names[] = {"Read", "Readv", "Recv", "Recvmsg"};
  return std::string(packet ? "Packet" : "Netlink") +
         (timeout == 0 ? "Infinite" : timeout == 1 ? "Finite" : "HugeFinite") +
         (restart ? "Restart" : "Interrupt") + names[static_cast<int>(entry)];
}
INSTANTIATE_TEST_SUITE_P(
    FamiliesAndEntrypoints, SocketReceiveInterrupt,
    testing::Combine(testing::Bool(), testing::Values(0, 1, 2), testing::Bool(),
                     testing::Values(Entry::Read, Entry::Readv, Entry::Recv, Entry::Recvmsg)),
    CaseName);

TEST(SocketReceiveInterruptControls, FiniteTimeoutExpiresWithoutSignal) {
  for (bool packet : {false, true}) {
    SCOPED_TRACE(packet ? "Packet" : "Netlink");
    Socket socket;
    ASSERT_TRUE(socket.Open(packet)) << strerror(errno);
    ASSERT_TRUE(socket.Timeout(0, 30000));
    errno = 0;
    EXPECT_EQ(-1, Receive(socket.fd, Entry::Recv));
    EXPECT_EQ(EAGAIN, errno);
  }
}

TEST(SocketReceiveInterruptControls, NegativeTimeoutIsImmediatelyFinite) {
  for (bool packet : {false, true}) {
    SCOPED_TRACE(packet ? "Packet" : "Netlink");
    Socket socket;
    ASSERT_TRUE(socket.Open(packet)) << strerror(errno);
    ASSERT_TRUE(socket.Timeout(-1));
    errno = 0;
    EXPECT_EQ(-1, Receive(socket.fd, Entry::Recv));
    EXPECT_EQ(EAGAIN, errno);
  }
}

TEST(SocketReceiveInterruptControls, NonblockingDoesNotWaitForConfiguredTimeout) {
  for (bool packet : {false, true}) {
    for (bool creation_flag : {false, true}) {
      SCOPED_TRACE(packet ? "Packet" : "Netlink");
      SCOPED_TRACE(creation_flag ? "SOCK_NONBLOCK" : "MSG_DONTWAIT");
      Socket socket;
      ASSERT_TRUE(socket.Open(packet, creation_flag ? SOCK_NONBLOCK : 0)) << strerror(errno);
      ASSERT_TRUE(socket.Timeout(5));
      SignalHandlers handlers;
      ASSERT_TRUE(handlers.Install(false));
      first_signal = second_signal = 0;
      SignalDriver driver;
      ASSERT_TRUE(driver.Start());
      errno = 0;
      const ssize_t result = creation_flag
          ? Receive(socket.fd, Entry::Readv)
          : Receive(socket.fd, Entry::Recvmsg, MSG_DONTWAIT);
      const int error = errno;
      const bool interrupted = first_signal != 0;
      ASSERT_TRUE(driver.Acknowledge());
      ASSERT_TRUE(driver.Join());
      // If either nonblocking path sleeps, the non-restarting signal returns
      // EINTR instead of EAGAIN, so a full socket timeout cannot fake success.
      EXPECT_EQ(-1, result);
      EXPECT_EQ(EAGAIN, error);
      EXPECT_FALSE(interrupted);
    }
  }
}

TEST(SocketReceiveInterruptControls, HugeFiniteTimeoutRoundTripsOldAndNewOptions) {
  constexpr int kReceiveTimeoutOld = 20;
  constexpr int kReceiveTimeoutNew = 66;
  for (bool packet : {false, true}) {
    for (int set_option : {kReceiveTimeoutOld, kReceiveTimeoutNew}) {
      SCOPED_TRACE(packet ? "Packet" : "Netlink");
      SCOPED_TRACE(set_option);
      Socket socket;
      ASSERT_TRUE(socket.Open(packet)) << strerror(errno);
      const timeval configured = {kHugeFiniteSeconds, 0};
      ASSERT_EQ(0, setsockopt(socket.fd, SOL_SOCKET, set_option,
                             &configured, sizeof(configured))) << strerror(errno);
      for (int get_option : {kReceiveTimeoutOld, kReceiveTimeoutNew}) {
        SCOPED_TRACE(get_option);
        timeval actual = {};
        socklen_t length = sizeof(actual);
        ASSERT_EQ(0, getsockopt(socket.fd, SOL_SOCKET, get_option,
                               &actual, &length)) << strerror(errno);
        EXPECT_EQ(sizeof(actual), length);
        EXPECT_EQ(configured.tv_sec, actual.tv_sec);
        EXPECT_EQ(configured.tv_usec, actual.tv_usec);
      }
    }
  }
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
