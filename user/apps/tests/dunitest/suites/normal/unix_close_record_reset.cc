#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cstddef>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

class Connection {
 public:
  ~Connection() {
    for (int fd : fds_) {
      if (fd >= 0) close(fd);
    }
    if (listener_ >= 0) close(listener_);
  }

  bool Open(bool bound, int type = SOCK_SEQPACKET) {
    if (!bound) return socketpair(AF_UNIX, type, 0, fds_) == 0;
    listener_ = socket(AF_UNIX, type, 0);
    if (listener_ < 0) return false;
    sockaddr_un address = {};
    address.sun_family = AF_UNIX;
    static unsigned serial = 0;
    const int length = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                "dunitest-close-record-%ld-%u", static_cast<long>(getpid()),
                                serial++);
    if (length < 0 || static_cast<size_t>(length) >= sizeof(address.sun_path) - 1) {
      return false;
    }
    const socklen_t address_length = offsetof(sockaddr_un, sun_path) + 1 + length;
    if (bind(listener_, reinterpret_cast<sockaddr*>(&address), address_length) != 0 ||
        listen(listener_, 1) != 0) return false;
    fds_[0] = socket(AF_UNIX, type, 0);
    if (fds_[0] < 0 ||
        connect(fds_[0], reinterpret_cast<sockaddr*>(&address), address_length) != 0) {
      return false;
    }
    fds_[1] = accept(listener_, nullptr, nullptr);
    return fds_[1] >= 0;
  }

  int survivor() const { return fds_[0]; }
  int closing() const { return fds_[1]; }
  int CloseReceiver() {
    const int fd = fds_[1];
    fds_[1] = -1;
    return close(fd);
  }

 private:
  int fds_[2] = {-1, -1};
  int listener_ = -1;
};

int SocketError(int fd) {
  int error = -1;
  socklen_t size = sizeof(error);
  if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size) != 0) return -1;
  return error;
}

// Linux 6.6's default CONFIG_AF_UNIX_OOB=y skips exactly one empty head skb
// during close, for seqpacket as well as stream. Ancillary rights do not alter
// that payload-length test. These are the default target semantics, without
// requiring this suite to send unsupported Unix OOB data.
struct CloseCase {
  bool bound;
  std::vector<size_t> records;
  bool consume_error;
  bool first_rights;
  bool consume_first;
};

std::vector<CloseCase> Cases() {
  std::vector<CloseCase> cases;
  for (bool bound : {false, true}) {
    for (bool consume_error : {false, true}) {
      for (const auto& records : std::vector<std::vector<size_t>>{
               {}, {0}, {1}, {0, 1}, {0, 0}, {1, 0}}) {
        cases.push_back({bound, records, consume_error, false, false});
      }
      for (const auto& records : std::vector<std::vector<size_t>>{{0}, {0, 0}, {0, 1}}) {
        cases.push_back({bound, records, consume_error, true, false});
        cases.push_back({bound, records, consume_error, false, true});
      }
    }
  }
  return cases;
}

class UnixCloseRecordReset : public testing::TestWithParam<CloseCase> {};

TEST_P(UnixCloseRecordReset, QueueTopologyDeterminesOnePendingError) {
  const CloseCase& test = GetParam();
  Connection connection;
  ASSERT_TRUE(connection.Open(test.bound)) << strerror(errno);
  for (size_t index = 0; index < test.records.size(); ++index) {
    const size_t length = test.records[index];
    if (test.first_rights && index == 0) {
      int descriptors[2];
      ASSERT_EQ(0, pipe(descriptors)) << strerror(errno);
      alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
      char byte = 'x';
      iovec vector = {&byte, length};
      msghdr message = {};
      message.msg_iov = &vector;
      message.msg_iovlen = 1;
      message.msg_control = control;
      message.msg_controllen = sizeof(control);
      cmsghdr* header = CMSG_FIRSTHDR(&message);
      header->cmsg_level = SOL_SOCKET;
      header->cmsg_type = SCM_RIGHTS;
      header->cmsg_len = CMSG_LEN(sizeof(int));
      memcpy(CMSG_DATA(header), &descriptors[0], sizeof(int));
      const ssize_t sent = sendmsg(connection.survivor(), &message, MSG_NOSIGNAL);
      const int saved_errno = errno;
      close(descriptors[0]);
      close(descriptors[1]);
      ASSERT_EQ(static_cast<ssize_t>(length), sent) << strerror(saved_errno);
    } else {
      ASSERT_EQ(static_cast<ssize_t>(length),
                send(connection.survivor(), "x", length, MSG_NOSIGNAL)) << strerror(errno);
    }
  }

  std::vector<size_t> unread = test.records;
  if (test.consume_first) {
    char byte = 0;
    ASSERT_EQ(0, recv(connection.closing(), &byte, 1, MSG_DONTWAIT)) << strerror(errno);
    unread.erase(unread.begin());
  }
  const bool reset = !unread.empty() && (unread.front() != 0 || unread.size() > 1);
  ASSERT_EQ(0, connection.CloseReceiver()) << strerror(errno);

  pollfd state = {connection.survivor(), POLLIN, 0};
  ASSERT_EQ(1, poll(&state, 1, 0)) << strerror(errno);
  EXPECT_EQ(reset, (state.revents & POLLERR) != 0);
  char byte = 0;
  if (test.consume_error) {
    EXPECT_EQ(reset ? ECONNRESET : 0, SocketError(connection.survivor()));
    EXPECT_EQ(0, SocketError(connection.survivor()));
  } else if (reset) {
    errno = 0;
    EXPECT_EQ(-1, recv(connection.survivor(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(ECONNRESET, errno);
  }
  // Either observation consumes the same error. EOF and poll remain stable;
  // no endpoint has sent payload in the survivor's receive direction.
  EXPECT_EQ(0, recv(connection.survivor(), &byte, 1, MSG_DONTWAIT)) << strerror(errno);
  EXPECT_EQ(0, recv(connection.survivor(), &byte, 1, MSG_DONTWAIT)) << strerror(errno);
  EXPECT_EQ(0, SocketError(connection.survivor()));
  state.revents = 0;
  ASSERT_EQ(1, poll(&state, 1, 0)) << strerror(errno);
  EXPECT_EQ(0, state.revents & POLLERR);
}

std::string CaseName(const testing::TestParamInfo<CloseCase>& info) {
  const CloseCase& test = info.param;
  std::string name = test.bound ? "Bound" : "Pair";
  name += test.consume_error ? "SoError" : "Recv";
  name += test.first_rights ? "Rights" : "Plain";
  name += test.consume_first ? "ConsumedHead" : "Unread";
  name += "Records";
  for (size_t length : test.records) name += "L" + std::to_string(length);
  if (test.records.empty()) name += "None";
  return name;
}

INSTANTIATE_TEST_SUITE_P(DefaultLinuxConfiguration, UnixCloseRecordReset,
                         testing::ValuesIn(Cases()), CaseName);

TEST(UnixCloseRecordResetStream, OrdinaryStreamQueueControl) {
  for (bool bound : {false, true}) {
    for (bool unread : {false, true}) {
      SCOPED_TRACE(testing::Message() << "bound=" << bound << " unread=" << unread);
      Connection connection;
      ASSERT_TRUE(connection.Open(bound, SOCK_STREAM)) << strerror(errno);
      if (unread) {
        ASSERT_EQ(1, send(connection.survivor(), "x", 1, MSG_NOSIGNAL));
      }
      ASSERT_EQ(0, connection.CloseReceiver());
      EXPECT_EQ(unread ? ECONNRESET : 0, SocketError(connection.survivor()));
      EXPECT_EQ(0, SocketError(connection.survivor()));
      char byte = 0;
      EXPECT_EQ(0, recv(connection.survivor(), &byte, 1, MSG_DONTWAIT));
    }
  }
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
