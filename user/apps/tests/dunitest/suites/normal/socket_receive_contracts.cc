#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <sys/epoll.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cstring>

namespace {
class Pair {
 public:
  ~Pair() { for (int fd : fds) if (fd >= 0) close(fd); }
  bool Open(int type = SOCK_SEQPACKET) {
    return socketpair(AF_UNIX, type, 0, fds) == 0;
  }
  bool OpenTcp() {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    if (listener < 0) return false;
    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t length = sizeof(address);
    bool ok = bind(listener, reinterpret_cast<sockaddr*>(&address), length) == 0 &&
              listen(listener, 1) == 0 &&
              getsockname(listener, reinterpret_cast<sockaddr*>(&address), &length) == 0;
    if (ok) {
      fds[1] = socket(AF_INET, SOCK_STREAM, 0);
      ok = fds[1] >= 0 &&
           connect(fds[1], reinterpret_cast<sockaddr*>(&address), length) == 0;
    }
    if (ok) { fds[0] = accept(listener, nullptr, nullptr); ok = fds[0] >= 0; }
    close(listener);
    return ok;
  }
  bool OpenBound(int family, int type, int protocol = 0) {
    fds[0] = socket(family, type, protocol);
    if (fds[0] < 0) return false;
    if (family == AF_INET) {
      sockaddr_in address = {};
      address.sin_family = AF_INET;
      address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      return bind(fds[0], reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
    }
    sockaddr_in6 address = {};
    address.sin6_family = AF_INET6;
    address.sin6_addr = in6addr_loopback;
    return bind(fds[0], reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
  }
  void CloseSender() { close(fds[1]); fds[1] = -1; }
  int fds[2] = {-1, -1};
};

// Every child has bounded work. Failed parent assertions cannot leave a sender
// behind or keep a socket alive for subsequent cases.
class Child {
 public:
  explicit Child(pid_t pid) : pid_(pid) {}
  ~Child() { if (pid_ > 0) { kill(pid_, SIGKILL); waitpid(pid_, nullptr, 0); } }
  bool Join() {
    for (int i = 0; i < 600; ++i) {
      int status = 0;
      pid_t result = waitpid(pid_, &status, WNOHANG);
      if (result == pid_) { pid_ = -1; return WIFEXITED(status) && WEXITSTATUS(status) == 0; }
      if (result < 0 && errno != EINTR) return false;
      usleep(10000);
    }
    return false;
  }
 private:
  pid_t pid_;
};

bool Timeout(int fd, long milliseconds) {
  timeval value = {milliseconds / 1000, (milliseconds % 1000) * 1000};
  return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &value, sizeof(value)) == 0;
}
int SocketError(int fd) {
  int error = -1;
  socklen_t length = sizeof(error);
  if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) != 0) return -1;
  return error;
}
enum class Entry { Read, Readv, Recv, Recvfrom, Recvmsg, Peek, Trunc };
ssize_t Receive(int fd, Entry entry, void* bytes, size_t size) {
  iovec vector = {bytes, size};
  msghdr message = {};
  message.msg_iov = &vector;
  message.msg_iovlen = 1;
  switch (entry) {
    case Entry::Read: return read(fd, bytes, size);
    case Entry::Readv: return readv(fd, &vector, 1);
    case Entry::Recv: return recv(fd, bytes, size, 0);
    case Entry::Recvfrom: return recvfrom(fd, bytes, size, 0, nullptr, nullptr);
    default:
      return recvmsg(fd, &message, entry == Entry::Peek ? MSG_PEEK :
                                      entry == Entry::Trunc ? MSG_TRUNC : 0);
  }
}
constexpr Entry kEntries[] = {Entry::Read, Entry::Readv, Entry::Recv,
                             Entry::Recvfrom, Entry::Recvmsg, Entry::Peek, Entry::Trunc};

void ResetPair(Pair& pair, int record_size) {
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(Timeout(pair.fds[0], 1000));
  ASSERT_EQ(1, send(pair.fds[0], "x", 1, 0));
  if (record_size >= 0) {
    ASSERT_EQ(record_size, send(pair.fds[1], "ok", record_size, 0));
  }
  pair.CloseSender();  // Unread "x" causes a real peer reset.
}

void BudgetCase(bool tcp, int replacement) {
  Pair pair;
  ASSERT_TRUE(tcp ? pair.OpenTcp() : pair.Open(SOCK_STREAM));
  ASSERT_TRUE(Timeout(pair.fds[0], 2000));
  pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    // Three partial arrivals force three waits. The final byte arrives after
    // the original 2s budget, but before a renewed 2s wait would expire.
    for (int i = 0; i < 3; ++i) {
      usleep(500000);
      if (i == 0 && replacement >= 0 && !Timeout(pair.fds[0], replacement)) _exit(2);
      if (send(pair.fds[1], "abc" + i, 1, MSG_NOSIGNAL) != 1) _exit(3);
    }
    usleep(1200000);
    _exit(send(pair.fds[1], "d", 1, MSG_NOSIGNAL) == 1 ? 0 : 4);
  }
  Child child(pid);
  char buffer[4] = {};
  EXPECT_EQ(3, recv(pair.fds[0], buffer, sizeof(buffer), MSG_WAITALL)) << strerror(errno);
  EXPECT_EQ(0, std::memcmp(buffer, "abc", 3));
  ASSERT_TRUE(child.Join());
  // The final byte must remain available to the next operation.
  EXPECT_EQ(1, recv(pair.fds[0], buffer, 1, MSG_DONTWAIT));
  EXPECT_EQ('d', buffer[0]);
}

void ExpectReadiness(int fd, bool pending_error) {
  pollfd descriptor = {fd, 0, 0};
  ASSERT_EQ(1, poll(&descriptor, 1, 0));
  EXPECT_NE(0, descriptor.revents & POLLHUP);
  EXPECT_EQ(pending_error, (descriptor.revents & POLLERR) != 0);

  // ERR and HUP are reported even when the caller requests no events.
  int epoll_fd = epoll_create1(0);
  ASSERT_GE(epoll_fd, 0);
  epoll_event interest = {};
  const int added = epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fd, &interest);
  epoll_event ready = {};
  const int count = added == 0 ? epoll_wait(epoll_fd, &ready, 1, 0) : -1;
  close(epoll_fd);
  ASSERT_EQ(0, added);
  ASSERT_EQ(1, count);
  EXPECT_NE(0u, ready.events & EPOLLHUP);
  EXPECT_EQ(pending_error, (ready.events & EPOLLERR) != 0);
}

volatile sig_atomic_t signals_seen = 0;
void SignalHandler(int) { ++signals_seen; }
class RestartHandler {
 public:
  bool Install() {
    struct sigaction action = {};
    action.sa_handler = SignalHandler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_RESTART;
    installed_ = sigaction(SIGUSR1, &action, &old_) == 0;
    return installed_;
  }
  ~RestartHandler() { if (installed_) sigaction(SIGUSR1, &old_, nullptr); }
 private:
  struct sigaction old_ = {};
  bool installed_ = false;
};
void ExpectFiniteReceiveInterrupted(Pair& pair, Entry entry) {
  ASSERT_TRUE(Timeout(pair.fds[0], 1000));
  signals_seen = 0;
  pid_t parent = getpid();
  pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) { usleep(200000); _exit(kill(parent, SIGUSR1) == 0 ? 0 : 2); }
  Child child(pid);
  char byte = 0;
  errno = 0;
  const ssize_t result = Receive(pair.fds[0], entry, &byte, 1);
  const int error = errno;
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EINTR, error);
  EXPECT_GT(signals_seen, 0);
  EXPECT_TRUE(child.Join());
}
}  // namespace

TEST(SocketReceiveContracts, SeqpacketResetPrecedesQueuedRecordAcrossEntries) {
  for (Entry entry : kEntries) {
    SCOPED_TRACE(static_cast<int>(entry));
    Pair pair;
    ASSERT_NO_FATAL_FAILURE(ResetPair(pair, 2));
    char bytes[4] = {};
    errno = 0;
    EXPECT_EQ(-1, Receive(pair.fds[0], entry, bytes, sizeof(bytes)));
    EXPECT_EQ(ECONNRESET, errno);
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    EXPECT_EQ(2, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
    EXPECT_EQ(0, std::memcmp(bytes, "ok", 2));
    EXPECT_EQ(0, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, SeqpacketResetPrecedesEmptyRecordAndEof) {
  for (Entry entry : kEntries) for (int size : {-1, 0}) {
    SCOPED_TRACE(static_cast<int>(entry));
    SCOPED_TRACE(size);
    Pair pair;
    ASSERT_NO_FATAL_FAILURE(ResetPair(pair, size));
    char byte = 0;
    errno = 0;
    EXPECT_EQ(-1, Receive(pair.fds[0], entry, &byte, 1));
    EXPECT_EQ(ECONNRESET, errno);
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    EXPECT_EQ(0, recv(pair.fds[0], &byte, 1, MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, SocketErrorConsumesResetOnceAndPreservesRecord) {
  for (Entry entry : kEntries) {
    Pair pair;
    ASSERT_NO_FATAL_FAILURE(ResetPair(pair, 2));
    EXPECT_EQ(ECONNRESET, SocketError(pair.fds[0]));
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    char bytes[4] = {};
    EXPECT_EQ(2, Receive(pair.fds[0], entry, bytes, sizeof(bytes)));
    EXPECT_EQ(0, std::memcmp(bytes, "ok", 2));
    if (entry == Entry::Peek) {
      EXPECT_EQ(2, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
    }
    EXPECT_EQ(0, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, ZeroReadPreservesResetButZeroRecvConsumesIt) {
  for (Entry entry : kEntries) {
    Pair pair;
    ASSERT_NO_FATAL_FAILURE(ResetPair(pair, 2));
    errno = 0;
    const ssize_t result = Receive(pair.fds[0], entry, nullptr, 0);
    if (entry == Entry::Read || entry == Entry::Readv) {
      EXPECT_EQ(0, result);
      EXPECT_EQ(ECONNRESET, SocketError(pair.fds[0]));
    } else {
      EXPECT_EQ(-1, result);
      EXPECT_EQ(ECONNRESET, errno);
      EXPECT_EQ(0, SocketError(pair.fds[0]));
    }
    char bytes[4] = {};
    EXPECT_EQ(2, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, PendingResetPrecedesPayloadFault) {
  void* inaccessible = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(MAP_FAILED, inaccessible);
  for (Entry entry : kEntries) {
    Pair pair;
    ASSERT_NO_FATAL_FAILURE(ResetPair(pair, 2));
    errno = 0;
    EXPECT_EQ(-1, Receive(pair.fds[0], entry, inaccessible, 1));
    EXPECT_EQ(ECONNRESET, errno);
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    char bytes[4] = {};
    EXPECT_EQ(2, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
  }
  EXPECT_EQ(0, munmap(inaccessible, 4096));
}

TEST(SocketReceiveContracts, PollAndEpollObserveResetWithoutConsumingIt) {
  for (int type : {SOCK_STREAM, SOCK_SEQPACKET}) {
    Pair pair;
    ASSERT_TRUE(pair.Open(type));
    ASSERT_EQ(1, send(pair.fds[0], "x", 1, 0));
    pair.CloseSender();
    for (int i = 0; i < 3; ++i) {
      ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], true));
    }
    EXPECT_EQ(ECONNRESET, SocketError(pair.fds[0]));
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], false));
  }
}

TEST(SocketReceiveContracts, ConsumingResetClearsErrorReadinessAndPreservesHangup) {
  for (int type : {SOCK_STREAM, SOCK_SEQPACKET}) for (bool use_socket_error : {false, true}) {
    Pair pair;
    ASSERT_TRUE(pair.Open(type));
    ASSERT_TRUE(Timeout(pair.fds[0], 1000));
    ASSERT_EQ(1, send(pair.fds[0], "x", 1, 0));
    pair.CloseSender();
    ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], true));
    if (use_socket_error) {
      EXPECT_EQ(ECONNRESET, SocketError(pair.fds[0]));
    } else {
      char byte = 0;
      errno = 0;
      EXPECT_EQ(-1, recv(pair.fds[0], &byte, 1, 0));
      EXPECT_EQ(ECONNRESET, errno);
    }
    EXPECT_EQ(0, SocketError(pair.fds[0]));
    ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], false));
    char byte = 0;
    EXPECT_EQ(0, recv(pair.fds[0], &byte, 1, MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, StreamOrdinaryReadReturnsDataBeforePendingReset) {
  for (Entry entry : {Entry::Read, Entry::Readv, Entry::Recv, Entry::Recvfrom, Entry::Recvmsg}) {
    Pair pair;
    ASSERT_TRUE(pair.Open(SOCK_STREAM));
    ASSERT_TRUE(Timeout(pair.fds[0], 1000));
    ASSERT_EQ(1, send(pair.fds[0], "x", 1, 0));
    ASSERT_EQ(2, send(pair.fds[1], "ok", 2, 0));
    pair.CloseSender();
    char bytes[4] = {};
    EXPECT_EQ(2, Receive(pair.fds[0], entry, bytes, sizeof(bytes)));
    EXPECT_EQ(0, std::memcmp(bytes, "ok", 2));
    ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], true));
    EXPECT_EQ(ECONNRESET, SocketError(pair.fds[0]));
    ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], false));
    EXPECT_EQ(0, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
  }
}

TEST(SocketReceiveContracts, StreamWaitallConsumesPendingResetAfterDataProgress) {
  Pair pair;
  ASSERT_TRUE(pair.Open(SOCK_STREAM));
  ASSERT_TRUE(Timeout(pair.fds[0], 1000));
  ASSERT_EQ(1, send(pair.fds[0], "x", 1, 0));
  ASSERT_EQ(2, send(pair.fds[1], "ok", 2, 0));
  pair.CloseSender();
  ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], true));
  char bytes[4] = {};
  EXPECT_EQ(2, recv(pair.fds[0], bytes, sizeof(bytes), MSG_WAITALL));
  EXPECT_EQ(0, std::memcmp(bytes, "ok", 2));
  // The error terminates WAITALL after its prefix; it is consumed even though
  // the positive progress takes precedence over returning ECONNRESET.
  EXPECT_EQ(0, SocketError(pair.fds[0]));
  ASSERT_NO_FATAL_FAILURE(ExpectReadiness(pair.fds[0], false));
  EXPECT_EQ(0, recv(pair.fds[0], bytes, sizeof(bytes), MSG_DONTWAIT));
}

TEST(SocketReceiveContracts, UnixWaitallUsesOneTimeoutBudget) { BudgetCase(false, -1); }
TEST(SocketReceiveContracts, TcpWaitallUsesOneTimeoutBudget) { BudgetCase(true, -1); }
TEST(SocketReceiveContracts, UnixWaitallIgnoresConcurrentTimeoutExtension) { BudgetCase(false, 5000); }
TEST(SocketReceiveContracts, TcpWaitallIgnoresConcurrentTimeoutExtension) { BudgetCase(true, 5000); }
TEST(SocketReceiveContracts, UnixWaitallIgnoresConcurrentTimeoutReduction) { BudgetCase(false, 100); }
TEST(SocketReceiveContracts, TcpWaitallIgnoresConcurrentTimeoutReduction) { BudgetCase(true, 100); }

TEST(SocketReceiveContracts, FiniteReceiveTimeoutDisablesSignalRestart) {
  RestartHandler handler;
  ASSERT_TRUE(handler.Install());
  for (bool tcp : {false, true}) {
    Pair pair;
    ASSERT_TRUE(tcp ? pair.OpenTcp() : pair.Open(SOCK_STREAM));
    ASSERT_TRUE(Timeout(pair.fds[0], 2000));
    signals_seen = 0;
    pid_t parent = getpid();
    pid_t pid = fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) { usleep(200000); _exit(kill(parent, SIGUSR1) == 0 ? 0 : 2); }
    Child child(pid);
    char byte = 0;
    errno = 0;
    EXPECT_EQ(-1, recv(pair.fds[0], &byte, 1, 0));
    EXPECT_EQ(EINTR, errno);
    EXPECT_GT(signals_seen, 0);
    EXPECT_TRUE(child.Join());
  }
}

TEST(SocketReceiveContracts, RecordOwnersWithFiniteTimeoutDoNotRestartAfterSignal) {
  RestartHandler handler;
  ASSERT_TRUE(handler.Install());
  for (Entry entry : {Entry::Recv, Entry::Recvfrom, Entry::Recvmsg}) {
    SCOPED_TRACE(static_cast<int>(entry));
    for (int type : {SOCK_DGRAM, SOCK_SEQPACKET}) {
      SCOPED_TRACE(type);
      Pair pair;
      ASSERT_TRUE(pair.Open(type));
      ASSERT_NO_FATAL_FAILURE(ExpectFiniteReceiveInterrupted(pair, entry));
    }
    for (int family : {AF_INET, AF_INET6}) {
      SCOPED_TRACE(family);
      Pair pair;
      ASSERT_TRUE(pair.OpenBound(family, SOCK_DGRAM)) << strerror(errno);
      ASSERT_NO_FATAL_FAILURE(ExpectFiniteReceiveInterrupted(pair, entry));
    }
  }
}

TEST(SocketReceiveContracts, RawOwnersWithFiniteTimeoutDoNotRestartAfterSignal) {
  RestartHandler handler;
  ASSERT_TRUE(handler.Install());
  for (Entry entry : {Entry::Recv, Entry::Recvfrom, Entry::Recvmsg}) {
    SCOPED_TRACE(static_cast<int>(entry));
    for (int family : {AF_INET, AF_INET6}) {
      SCOPED_TRACE(family);
      Pair pair;
      // Protocol 253 is reserved for experimentation; this bound empty socket
      // receives no traffic generated by the suite. The guest runner is root.
      ASSERT_TRUE(pair.OpenBound(family, SOCK_RAW, 253)) << strerror(errno);
      ASSERT_NO_FATAL_FAILURE(ExpectFiniteReceiveInterrupted(pair, entry));
    }
  }
}

TEST(SocketReceiveContracts, InfiniteReceiveRetainsSignalRestart) {
  RestartHandler handler;
  ASSERT_TRUE(handler.Install());
  for (bool tcp : {false, true}) {
    Pair pair;
    ASSERT_TRUE(tcp ? pair.OpenTcp() : pair.Open(SOCK_STREAM));
    signals_seen = 0;
    pid_t parent = getpid();
    pid_t pid = fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) {
      usleep(200000);
      if (kill(parent, SIGUSR1) != 0) _exit(2);
      usleep(200000);
      int sent = send(pair.fds[1], "x", 1, MSG_NOSIGNAL);
      shutdown(pair.fds[1], SHUT_WR);
      _exit(sent == 1 ? 0 : 3);
    }
    Child child(pid);
    char byte = 0;
    EXPECT_EQ(1, recv(pair.fds[0], &byte, 1, 0));
    EXPECT_EQ('x', byte);
    EXPECT_GT(signals_seen, 0);
    EXPECT_TRUE(child.Join());
  }
}

TEST(SocketReceiveContracts, SignalAfterWaitallProgressReturnsPrefix) {
  RestartHandler handler;
  ASSERT_TRUE(handler.Install());
  for (bool tcp : {false, true}) {
    Pair pair;
    ASSERT_TRUE(tcp ? pair.OpenTcp() : pair.Open(SOCK_STREAM));
    ASSERT_TRUE(Timeout(pair.fds[0], 2000));
    ASSERT_EQ(1, send(pair.fds[1], "x", 1, MSG_NOSIGNAL));
    signals_seen = 0;
    pid_t parent = getpid();
    pid_t pid = fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) { usleep(200000); _exit(kill(parent, SIGUSR1) == 0 ? 0 : 2); }
    Child child(pid);
    char bytes[2] = {};
    EXPECT_EQ(1, recv(pair.fds[0], bytes, sizeof(bytes), MSG_WAITALL));
    EXPECT_EQ('x', bytes[0]);
    EXPECT_GT(signals_seen, 0);
    EXPECT_TRUE(child.Join());
  }
}

TEST(SocketReceiveContracts, DontwaitDoesNotSpendReceiveTimeout) {
  for (bool tcp : {false, true}) {
    Pair pair;
    ASSERT_TRUE(tcp ? pair.OpenTcp() : pair.Open(SOCK_STREAM));
    ASSERT_TRUE(Timeout(pair.fds[0], 5000));
    char byte = 0;
    auto start = std::chrono::steady_clock::now();
    errno = 0;
    EXPECT_EQ(-1, recv(pair.fds[0], &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_LT(std::chrono::steady_clock::now() - start, std::chrono::seconds(2));
  }
}

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
