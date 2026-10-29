#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstring>

namespace {

class SocketPair {
 public:
  ~SocketPair() {
    for (int fd : fds_) {
      if (fd >= 0) close(fd);
    }
  }

  bool Open() { return socketpair(AF_UNIX, SOCK_SEQPACKET, 0, fds_) == 0; }
  int receiver() const { return fds_[0]; }
  int sender() const { return fds_[1]; }
  void CloseSender() {
    close(fds_[1]);
    fds_[1] = -1;
  }

 private:
  int fds_[2] = {-1, -1};
};

bool SetReceiveTimeout(int fd) {
  timeval timeout = {1, 0};
  return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0;
}

}  // namespace

TEST(UnixSeqpacketZeroRecv, EmptyNonblockingPeekIsNotEof) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, recvfrom(pair.receiver(), nullptr, 0,
                         MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT, nullptr, nullptr));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(UnixSeqpacketZeroRecv, PeekWaitsForPacketLengthWithoutConsuming) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);

  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    usleep(100'000);
    _exit(send(pair.sender(), "hello", 5, 0) == 5 ? 0 : 2);
  }

  const ssize_t length = recvfrom(pair.receiver(), nullptr, 0, MSG_PEEK | MSG_TRUNC, nullptr,
                                  nullptr);
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_EQ(5, length) << strerror(errno);

  char buffer[8] = {};
  ASSERT_EQ(5, recv(pair.receiver(), buffer, sizeof(buffer), 0)) << strerror(errno);
  EXPECT_STREQ("hello", buffer);
}

TEST(UnixSeqpacketZeroRecv, ZeroLengthNonpeekConsumesRecord) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  ASSERT_EQ(5, send(pair.sender(), "hello", 5, 0)) << strerror(errno);
  EXPECT_EQ(5, recvfrom(pair.receiver(), nullptr, 0, MSG_TRUNC, nullptr, nullptr));
  char byte = 0;
  errno = 0;
  EXPECT_EQ(-1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(UnixSeqpacketZeroRecv, EmptyRecordIsReadableButHasNoPayload) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  int socket_type = 0;
  socklen_t socket_type_len = sizeof(socket_type);
  ASSERT_EQ(0, getsockopt(pair.sender(), SOL_SOCKET, SO_TYPE, &socket_type, &socket_type_len));
  ASSERT_EQ(SOCK_SEQPACKET, socket_type);
  char empty_payload = 0;
  ASSERT_EQ(0, send(pair.sender(), &empty_payload, 0, 0)) << strerror(errno);

  pollfd pfd = {pair.receiver(), POLLIN, 0};
  EXPECT_EQ(1, poll(&pfd, 1, 0)) << strerror(errno);
  EXPECT_NE(0, pfd.revents & POLLIN);
  int available = -1;
  ASSERT_EQ(0, ioctl(pair.receiver(), FIONREAD, &available)) << strerror(errno);
  EXPECT_EQ(0, available);
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_PEEK | MSG_TRUNC, nullptr, nullptr));

  char byte = 0;
  EXPECT_EQ(0, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  errno = 0;
  EXPECT_EQ(-1, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(UnixSeqpacketZeroRecv, NullBufferZeroLengthSendQueuesRecord) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_EQ(0, send(pair.sender(), nullptr, 0, 0)) << strerror(errno);
  pollfd pfd = {pair.receiver(), POLLIN, 0};
  EXPECT_EQ(1, poll(&pfd, 1, 0));
  char byte = 0;
  EXPECT_EQ(0, recv(pair.receiver(), &byte, 1, MSG_DONTWAIT));
}

TEST(UnixSeqpacketZeroRecv, EmptyRecordWithPeerReset) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  ASSERT_EQ(1, send(pair.receiver(), "x", 1, 0));
  char empty_payload = 0;
  ASSERT_EQ(0, send(pair.sender(), &empty_payload, 0, 0));
  pair.CloseSender();
  char byte = 0;
  errno = 0;
  const ssize_t received = recv(pair.receiver(), &byte, 1, 0);
  EXPECT_EQ(-1, received);
  EXPECT_EQ(ECONNRESET, errno);
}

TEST(UnixSeqpacketZeroRecv, PeerResetPrecedesPeekedRecord) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  ASSERT_EQ(1, send(pair.receiver(), "x", 1, 0));
  ASSERT_EQ(2, send(pair.sender(), "ok", 2, 0));
  pair.CloseSender();
  char buffer[4] = {};
  errno = 0;
  EXPECT_EQ(-1, recv(pair.receiver(), buffer, sizeof(buffer), MSG_PEEK));
  EXPECT_EQ(ECONNRESET, errno);
  EXPECT_EQ(2, recv(pair.receiver(), buffer, sizeof(buffer), 0));
  EXPECT_STREQ("ok", buffer);
}

TEST(UnixSeqpacketZeroRecv, EmptyRecordWakesBlockingReceiver) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  int ready[2] = {-1, -1};
  ASSERT_EQ(0, pipe(ready)) << strerror(errno);

  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(ready[0]);
    char marker = 'x';
    if (write(ready[1], &marker, 1) != 1) _exit(2);
    char byte = 0;
    const ssize_t result = recv(pair.receiver(), &byte, 1, 0);
    _exit(result == 0 ? 0 : 3);
  }

  close(ready[1]);
  char marker = 0;
  ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
  close(ready[0]);
  usleep(100'000);
  char empty_payload = 0;
  ASSERT_EQ(0, send(pair.sender(), &empty_payload, 0, 0)) << strerror(errno);
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(UnixSeqpacketZeroRecv, PeerCloseIsActualEof) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  pair.CloseSender();
  EXPECT_EQ(0, recvfrom(pair.receiver(), nullptr, 0, MSG_PEEK | MSG_TRUNC, nullptr, nullptr));
}

TEST(UnixSeqpacketZeroRecv, ReadZeroLengthStillReturnsImmediately) {
  SocketPair pair;
  ASSERT_TRUE(pair.Open()) << strerror(errno);
  ASSERT_TRUE(SetReceiveTimeout(pair.receiver())) << strerror(errno);
  EXPECT_EQ(0, read(pair.receiver(), nullptr, 0));
}

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
