#include <gtest/gtest.h>

#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstddef>
#include <cstring>
#include <vector>

namespace {

class StreamPair {
 public:
  ~StreamPair() {
    if (fds_[0] >= 0) close(fds_[0]);
    if (fds_[1] >= 0) close(fds_[1]);
  }

  bool Open() { return socketpair(AF_UNIX, SOCK_STREAM, 0, fds_) == 0; }
  int receiver() const { return fds_[0]; }
  int sender() const { return fds_[1]; }
  void CloseSender() {
    close(fds_[1]);
    fds_[1] = -1;
  }

 private:
  int fds_[2] = {-1, -1};
};

ssize_t ZeroRecvmsg(int fd, int flags, void* control = nullptr,
                    size_t control_len = 0, msghdr* output = nullptr) {
  iovec iov = {nullptr, 0};
  msghdr msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = control;
  msg.msg_controllen = control_len;
  const ssize_t ret = recvmsg(fd, &msg, flags);
  if (output != nullptr) *output = msg;
  return ret;
}

bool SetTimeout(int fd) {
  timeval timeout = {1, 0};
  return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0;
}

bool SetSendTimeout(int fd) {
  timeval timeout = {3, 0};
  return setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) == 0;
}

bool SendOneByteWithRight(int sender, int right) {
  char data = 'r';
  iovec iov = {&data, 1};
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
  msghdr msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = control;
  msg.msg_controllen = sizeof(control);
  cmsghdr* hdr = CMSG_FIRSTHDR(&msg);
  if (hdr == nullptr) return false;
  hdr->cmsg_level = SOL_SOCKET;
  hdr->cmsg_type = SCM_RIGHTS;
  hdr->cmsg_len = CMSG_LEN(sizeof(int));
  std::memcpy(CMSG_DATA(hdr), &right, sizeof(right));
  return sendmsg(sender, &msg, 0) == 1;
}

int FirstReceivedRight(const msghdr& msg) {
  auto* hdr = CMSG_FIRSTHDR(const_cast<msghdr*>(&msg));
  if (hdr == nullptr || hdr->cmsg_level != SOL_SOCKET ||
      hdr->cmsg_type != SCM_RIGHTS || hdr->cmsg_len < CMSG_LEN(sizeof(int))) {
    return -1;
  }
  int fd = -1;
  std::memcpy(&fd, CMSG_DATA(hdr), sizeof(fd));
  return fd;
}

void ExpectResetThenEof(int flags) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(1, send(pair.receiver(), "x", 1, 0));
  pair.CloseSender();  // The peer closes with unread data.
  errno = 0;
  EXPECT_EQ(-1, recvfrom(pair.receiver(), nullptr, 0, flags | MSG_DONTWAIT,
                         nullptr, nullptr));
  EXPECT_EQ(ECONNRESET, errno);
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                        nullptr, nullptr));
}

}  // namespace

TEST(UnixStreamZeroRecv, EmptyNonblockingIsNotEof) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, recvfrom(pair.receiver(), nullptr, 0,
                         MSG_DONTWAIT | MSG_PEEK | MSG_TRUNC, nullptr, nullptr));
  EXPECT_EQ(EAGAIN, errno);
  errno = 0;
  EXPECT_EQ(-1, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(UnixStreamZeroRecv, WaitsForDataAndDoesNotConsumeIt) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetTimeout(pair.receiver())) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    usleep(100'000);
    _exit(send(pair.sender(), "z", 1, 0) == 1 ? 0 : 2);
  }

  const ssize_t n = recvfrom(pair.receiver(), nullptr, 0, 0, nullptr, nullptr);
  char byte = 0;
  errno = 0;
  const ssize_t data_len = recv(pair.receiver(), &byte, 1, MSG_DONTWAIT);
  const int data_errno = errno;
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_EQ(0, n) << strerror(errno);
  EXPECT_EQ(1, data_len) << "zero-length recv returned before data arrived: "
                         << strerror(data_errno);
  EXPECT_EQ('z', byte);
}

TEST(UnixStreamZeroRecv, QueuedDataIsNotConsumed) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(1, send(pair.sender(), "y", 1, 0));
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0,
                        MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT, nullptr, nullptr));
  EXPECT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT));
  char byte = 0;
  ASSERT_EQ(1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ('y', byte);
}

TEST(UnixStreamZeroRecv, RecvmmsgZeroLengthUsesSameStreamState) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  iovec iov = {nullptr, 0};
  mmsghdr msg = {};
  msg.msg_hdr.msg_iov = &iov;
  msg.msg_hdr.msg_iovlen = 1;
  errno = 0;
  EXPECT_EQ(-1, recvmmsg(pair.receiver(), &msg, 1, MSG_DONTWAIT, nullptr));
  EXPECT_EQ(EAGAIN, errno);

  ASSERT_EQ(1, send(pair.sender(), "m", 1, 0));
  ASSERT_EQ(1, recvmmsg(pair.receiver(), &msg, 1, MSG_DONTWAIT, nullptr)) << strerror(errno);
  EXPECT_EQ(0u, msg.msg_len);
  char byte = 0;
  ASSERT_EQ(1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ('m', byte);
}

TEST(UnixStreamZeroRecv, UnconnectedAndListenerReturnEinval) {
  const int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  ASSERT_GE(fd, 0) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, recvfrom(fd, nullptr, 0, MSG_DONTWAIT, nullptr, nullptr));
  EXPECT_EQ(EINVAL, errno);
  errno = 0;
  EXPECT_EQ(-1, ZeroRecvmsg(fd, MSG_DONTWAIT));
  EXPECT_EQ(EINVAL, errno);
  ASSERT_EQ(0, close(fd));

  const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
  ASSERT_GE(listener, 0) << strerror(errno);
  sockaddr_un addr = {};
  addr.sun_family = AF_UNIX;
  std::memcpy(addr.sun_path + 1, "dkc012-listener", 15);
  ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&addr),
                    offsetof(sockaddr_un, sun_path) + 1 + 15)) << strerror(errno);
  ASSERT_EQ(0, listen(listener, 1)) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, recvfrom(listener, nullptr, 0, MSG_DONTWAIT, nullptr, nullptr));
  EXPECT_EQ(EINVAL, errno);
  ASSERT_EQ(0, close(listener));
}

TEST(UnixStreamZeroRecv, PeerFinIsEof) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(0, shutdown(pair.sender(), SHUT_WR));
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                        nullptr, nullptr));
}

TEST(UnixStreamZeroRecv, LocalReadShutdownIsEof) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(0, shutdown(pair.receiver(), SHUT_RD));
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                        nullptr, nullptr));
}

TEST(UnixStreamZeroRecv, ResetBeatsEofWithAndWithoutPeek) {
  ExpectResetThenEof(0);
  ExpectResetThenEof(MSG_PEEK);
}

TEST(UnixStreamZeroRecv, RecvmsgResetBeatsEof) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(1, send(pair.receiver(), "x", 1, 0));
  pair.CloseSender();
  errno = 0;
  EXPECT_EQ(-1, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT | MSG_PEEK));
  EXPECT_EQ(ECONNRESET, errno);
  EXPECT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT));
}

TEST(UnixStreamZeroRecv, QueuedDataBeatsPendingReset) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(1, send(pair.sender(), "y", 1, 0));
  ASSERT_EQ(1, send(pair.receiver(), "x", 1, 0));
  pair.CloseSender();
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                        nullptr, nullptr));
  char byte = 0;
  ASSERT_EQ(1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ('y', byte);
  errno = 0;
  EXPECT_EQ(-1, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                         nullptr, nullptr));
  EXPECT_EQ(ECONNRESET, errno);
}

TEST(UnixStreamZeroRecv, ReadZeroStillReturnsImmediately) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(0, read(pair.receiver(), nullptr, 0));
  EXPECT_EQ(0, read(pair.sender(), nullptr, 0));
}

TEST(UnixStreamZeroRecv, ZeroRecvmsgPeekAndConsumeRightsWithoutData) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int pipe_fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
  ASSERT_TRUE(SendOneByteWithRight(pair.sender(), pipe_fds[0])) << strerror(errno);

  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
  msghdr msg = {};
  ASSERT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_PEEK | MSG_DONTWAIT,
                           control, sizeof(control), &msg)) << strerror(errno);
  int peeked_fd = FirstReceivedRight(msg);
  ASSERT_GE(peeked_fd, 0);
  close(peeked_fd);

  std::memset(control, 0, sizeof(control));
  ASSERT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT,
                           control, sizeof(control), &msg)) << strerror(errno);
  int received_fd = FirstReceivedRight(msg);
  ASSERT_GE(received_fd, 0);
  close(received_fd);

  char data = 0;
  iovec iov = {&data, 1};
  std::memset(control, 0, sizeof(control));
  msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = control;
  msg.msg_controllen = sizeof(control);
  ASSERT_EQ(1, recvmsg(pair.receiver(), &msg, MSG_DONTWAIT)) << strerror(errno);
  EXPECT_EQ('r', data);
  EXPECT_EQ(nullptr, CMSG_FIRSTHDR(&msg));

  close(pipe_fds[0]);
  close(pipe_fds[1]);
}

TEST(UnixStreamZeroRecv, ZeroRecvmsgWithoutControlReportsTruncation) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int pipe_fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
  ASSERT_TRUE(SendOneByteWithRight(pair.sender(), pipe_fds[0])) << strerror(errno);
  msghdr msg = {};
  ASSERT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT,
                           nullptr, 0, &msg)) << strerror(errno);
  EXPECT_NE(0, msg.msg_flags & MSG_CTRUNC);

  char data = 0;
  iovec iov = {&data, 1};
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
  msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = control;
  msg.msg_controllen = sizeof(control);
  ASSERT_EQ(1, recvmsg(pair.receiver(), &msg, MSG_DONTWAIT)) << strerror(errno);
  EXPECT_EQ('r', data);
  EXPECT_EQ(nullptr, CMSG_FIRSTHDR(&msg));
  close(pipe_fds[0]);
  close(pipe_fds[1]);
}

TEST(UnixStreamZeroRecv, ZeroRecvmsgShortControlReportsTruncation) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int pipe_fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
  ASSERT_TRUE(SendOneByteWithRight(pair.sender(), pipe_fds[0])) << strerror(errno);
  char short_control[1] = {};
  msghdr msg = {};
  ASSERT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT,
                           short_control, sizeof(short_control), &msg)) << strerror(errno);
  EXPECT_NE(0, msg.msg_flags & MSG_CTRUNC);
  close(pipe_fds[0]);
  close(pipe_fds[1]);
}

TEST(UnixStreamZeroRecv, ZeroRecvfromDiscardsRightsButNotData) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int pipe_fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
  ASSERT_TRUE(SendOneByteWithRight(pair.sender(), pipe_fds[0])) << strerror(errno);
  ASSERT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_DONTWAIT,
                        nullptr, nullptr)) << strerror(errno);

  char data = 0;
  iovec iov = {&data, 1};
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
  msghdr msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = control;
  msg.msg_controllen = sizeof(control);
  ASSERT_EQ(1, recvmsg(pair.receiver(), &msg, MSG_DONTWAIT)) << strerror(errno);
  EXPECT_EQ('r', data);
  EXPECT_EQ(nullptr, CMSG_FIRSTHDR(&msg));
  close(pipe_fds[0]);
  close(pipe_fds[1]);
}

TEST(UnixStreamZeroRecv, BlockingLargeSendPublishesRightsBeforeFirstChunk) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetTimeout(pair.receiver()));
  ASSERT_TRUE(SetSendTimeout(pair.sender()));
  int pipe_fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
  constexpr size_t kPayloadSize = 128 * 1024;
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    alarm(10);
    std::vector<char> payload(kPayloadSize, 'x');
    iovec iov = {payload.data(), payload.size()};
    alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
    msghdr msg = {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof(control);
    cmsghdr* hdr = CMSG_FIRSTHDR(&msg);
    hdr->cmsg_level = SOL_SOCKET;
    hdr->cmsg_type = SCM_RIGHTS;
    hdr->cmsg_len = CMSG_LEN(sizeof(int));
    std::memcpy(CMSG_DATA(hdr), &pipe_fds[0], sizeof(int));
    _exit(sendmsg(pair.sender(), &msg, 0) == static_cast<ssize_t>(kPayloadSize) ? 0 : 2);
  }

  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
  msghdr msg = {};
  const ssize_t zero_result = ZeroRecvmsg(pair.receiver(), 0, control, sizeof(control), &msg);
  const int received_fd = zero_result == 0 ? FirstReceivedRight(msg) : -1;

  size_t drained = 0;
  char buffer[4096];
  while (drained < kPayloadSize) {
    const ssize_t n = recv(pair.receiver(), buffer, sizeof(buffer), 0);
    if (n <= 0) break;
    drained += static_cast<size_t>(n);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  EXPECT_EQ(0, zero_result) << strerror(errno);
  EXPECT_GE(received_fd, 0) << "first visible chunk omitted SCM_RIGHTS";
  EXPECT_EQ(kPayloadSize, drained);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  if (received_fd >= 0) close(received_fd);
  close(pipe_fds[0]);
  close(pipe_fds[1]);
}

TEST(UnixStreamZeroRecv, PasscredAtEofReturnsZeroCredential) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int on = 1;
  ASSERT_EQ(0, setsockopt(pair.receiver(), SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)));
  ASSERT_EQ(0, shutdown(pair.sender(), SHUT_WR));
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(ucred))] = {};
  msghdr msg = {};
  ASSERT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT,
                           control, sizeof(control), &msg)) << strerror(errno);
  auto* hdr = CMSG_FIRSTHDR(&msg);
  ASSERT_NE(nullptr, hdr);
  ASSERT_EQ(SOL_SOCKET, hdr->cmsg_level);
  ASSERT_EQ(SCM_CREDENTIALS, hdr->cmsg_type);
  ucred cred = {};
  std::memcpy(&cred, CMSG_DATA(hdr), sizeof(cred));
  EXPECT_EQ(0, cred.pid);
  EXPECT_EQ(0u, cred.uid);
  EXPECT_EQ(0u, cred.gid);
}

TEST(UnixStreamZeroRecv, InvalidCredentialControlDoesNotUndoRead) {
  StreamPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  int on = 1;
  ASSERT_EQ(0, setsockopt(pair.receiver(), SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)));
  ASSERT_EQ(1, send(pair.sender(), "z", 1, 0));
  EXPECT_EQ(0, ZeroRecvmsg(pair.receiver(), MSG_DONTWAIT,
                           reinterpret_cast<void*>(1), CMSG_SPACE(sizeof(ucred))))
      << strerror(errno);
  char byte = 0;
  ASSERT_EQ(1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ('z', byte);
}

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
