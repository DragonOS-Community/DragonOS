#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/netfilter/nfnetlink.h>
#include <linux/netlink.h>
#include <poll.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <functional>
#include <vector>

namespace {
// Unknown subsystem avoids depending on whether host nftables modules are loaded.
constexpr uint16_t kUnknown = 255 << 8;

class Fd {
 public:
  explicit Fd(int fd) : fd_(fd) {}
  ~Fd() { if (fd_ >= 0) close(fd_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
 private:
  int fd_;
};

int Open(int type = SOCK_RAW, int protocol = NETLINK_NETFILTER) {
  return socket(AF_NETLINK, type | SOCK_NONBLOCK | SOCK_CLOEXEC, protocol);
}

int Bind(int fd, uint32_t port = 0) {
  sockaddr_nl addr{};
  addr.nl_family = AF_NETLINK;
  addr.nl_pid = port;
  return bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
}

uint32_t Port(int fd) {
  sockaddr_nl addr{};
  socklen_t size = sizeof(addr);
  if (getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &size) < 0 ||
      size != sizeof(addr) || addr.nl_family != AF_NETLINK) return 0;
  return addr.nl_pid;
}

std::vector<uint8_t> Request(uint16_t type, uint32_t seq, size_t payload = sizeof(nfgenmsg)) {
  std::vector<uint8_t> bytes(NLMSG_SPACE(payload), 0);
  nlmsghdr h{};
  h.nlmsg_len = NLMSG_LENGTH(payload);
  h.nlmsg_type = type;
  h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  h.nlmsg_seq = seq;
  std::memcpy(bytes.data(), &h, sizeof(h));
  return bytes;
}

ssize_t Send(int fd, const std::vector<uint8_t>& bytes) {
  sockaddr_nl kernel{};
  kernel.nl_family = AF_NETLINK;
  return sendto(fd, bytes.data(), bytes.size(), 0,
                reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel));
}

bool NetAdmin() {
  __user_cap_header_struct header{};
  header.version = _LINUX_CAPABILITY_VERSION_3;
  __user_cap_data_struct data[2]{};
  return syscall(SYS_capget, &header, data) == 0 &&
         (data[0].effective & (uint32_t{1} << CAP_NET_ADMIN));
}

void Ack(int fd, uint32_t seq, int error, uint16_t type) {
  pollfd p{fd, POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  ASSERT_TRUE(p.revents & POLLIN);
  std::array<uint8_t, 512> bytes{};
  sockaddr_nl sender{};
  socklen_t size = sizeof(sender);
  const ssize_t n = recvfrom(fd, bytes.data(), bytes.size(), 0,
                             reinterpret_cast<sockaddr*>(&sender), &size);
  ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
  EXPECT_EQ(0u, sender.nl_pid);
  nlmsghdr h{};
  nlmsgerr e{};
  std::memcpy(&h, bytes.data(), sizeof(h));
  std::memcpy(&e, bytes.data() + NLMSG_HDRLEN, sizeof(e));
  EXPECT_EQ(NLMSG_ERROR, h.nlmsg_type);
  EXPECT_EQ(seq, h.nlmsg_seq);
  EXPECT_EQ(-error, e.error);
  EXPECT_EQ(seq, e.msg.nlmsg_seq);
  EXPECT_EQ(type, e.msg.nlmsg_type);
}

void Child(const std::function<int()>& fn) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) _exit(fn());
  int status = 0;
  ASSERT_EQ(pid, waitpid(pid, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(NetlinkNetfilter, SocketTypesFlagsAndEmptyReceive) {
  for (int type : {SOCK_RAW, SOCK_DGRAM}) {
    Fd fd(Open(type));
    ASSERT_GE(fd.get(), 0) << std::strerror(errno);
    EXPECT_NE(0, fcntl(fd.get(), F_GETFL) & O_NONBLOCK);
    EXPECT_NE(0, fcntl(fd.get(), F_GETFD) & FD_CLOEXEC);
    ASSERT_EQ(0, Bind(fd.get()));
    EXPECT_NE(0u, Port(fd.get()));
    char byte;
    EXPECT_EQ(-1, recv(fd.get(), &byte, 1, 0));
    EXPECT_EQ(EAGAIN, errno);
    pollfd p{fd.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&p, 1, 0));
  }
}

TEST(NetlinkNetfilter, IndependentPortsAndCloseReleasesBinding) {
  uint32_t port;
  Fd duplicate(Open());
  ASSERT_GE(duplicate.get(), 0);
  {
    Fd first(Open());
    Fd automatic(Open());
    Fd route(Open(SOCK_RAW, NETLINK_ROUTE));
    ASSERT_GE(first.get(), 0);
    ASSERT_GE(automatic.get(), 0);
    ASSERT_GE(route.get(), 0);
    ASSERT_EQ(0, Bind(first.get()));
    port = Port(first.get());
    ASSERT_NE(0u, port);
    ASSERT_EQ(0, Bind(automatic.get()));
    EXPECT_NE(port, Port(automatic.get()));
    EXPECT_EQ(-1, Bind(duplicate.get(), port));
    EXPECT_EQ(EADDRINUSE, errno);
    EXPECT_EQ(0, Bind(route.get(), port));
  }
  EXPECT_EQ(0, Bind(duplicate.get(), port));
}

TEST(NetlinkNetfilter, NamespacePortIsolation) {
  Fd parent(Open());
  ASSERT_GE(parent.get(), 0);
  ASSERT_EQ(0, Bind(parent.get()));
  const uint32_t port = Port(parent.get());
  ASSERT_NE(0u, port);
  Child([port]() {
    if (unshare(CLONE_NEWNET) < 0) return 1;
    Fd child(Open());
    if (child.get() < 0 || Bind(child.get(), port) < 0) return 2;
    return Port(child.get()) == port ? 0 : 3;
  });
}

TEST(NetlinkNetfilter, UnknownSubsystemAndMultipleMessages) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto first = Request(kUnknown, 41);
  auto second = Request(kUnknown | 1, 42);
  first.insert(first.end(), second.begin(), second.end());
  ASSERT_EQ(static_cast<ssize_t>(first.size()), Send(fd.get(), first));
  Ack(fd.get(), 41, EINVAL, kUnknown);
  Ack(fd.get(), 42, EINVAL, kUnknown | 1);
}

TEST(NetlinkNetfilter, ErrorAckAlignsPayloadAndPreservesOriginalHeader) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  const uint32_t actual_port = Port(fd.get());
  ASSERT_NE(0u, actual_port);
  const uint32_t forged_port = actual_port ^ 0xffffffffu;
  auto request = Request(kUnknown, 49, 5);
  nlmsghdr original{};
  std::memcpy(&original, request.data(), sizeof(original));
  original.nlmsg_pid = forged_port;
  std::memcpy(request.data(), &original, sizeof(original));
  request[NLMSG_HDRLEN + 4] = 0xa5;
  request.resize(original.nlmsg_len);
  ASSERT_EQ(21u, request.size());
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  ASSERT_TRUE(p.revents & POLLIN);
  std::array<uint8_t, 128> bytes;
  bytes.fill(0xcc);
  sockaddr_nl sender{};
  socklen_t size = sizeof(sender);
  ASSERT_EQ(44, recvfrom(fd.get(), bytes.data(), bytes.size(), 0,
                         reinterpret_cast<sockaddr*>(&sender), &size));
  EXPECT_EQ(0u, sender.nl_pid);
  nlmsghdr reply{};
  nlmsgerr error{};
  std::memcpy(&reply, bytes.data(), sizeof(reply));
  std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
  EXPECT_EQ(44u, reply.nlmsg_len);
  EXPECT_EQ(NLMSG_ERROR, reply.nlmsg_type);
  EXPECT_EQ(49u, reply.nlmsg_seq);
  EXPECT_EQ(actual_port, reply.nlmsg_pid);
  EXPECT_EQ(-EINVAL, error.error);
  EXPECT_EQ(21u, error.msg.nlmsg_len);
  EXPECT_EQ(forged_port, error.msg.nlmsg_pid);
  EXPECT_EQ(0, std::memcmp(bytes.data() + NLMSG_HDRLEN + sizeof(error.error),
                           request.data(), request.size()));
  for (size_t i = 41; i < 44; ++i) EXPECT_EQ(0, bytes[i]);
}

TEST(NetlinkNetfilter, BatchMissingSubsystemErrors) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  for (uint16_t subsystem : {uint16_t{255}, uint16_t{NFNL_SUBSYS_NONE}}) {
    auto request = Request(NFNL_MSG_BATCH_BEGIN, 50 + subsystem);
    nfgenmsg gen{};
    gen.res_id = htons(subsystem);
    std::memcpy(request.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    Ack(fd.get(), 50 + subsystem, subsystem == 255 ? EINVAL : EOPNOTSUPP,
        NFNL_MSG_BATCH_BEGIN);
  }
}

TEST(NetlinkNetfilter, ShortPayloadAckAndMalformedHeaderSilence) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(kUnknown, 60, 0);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  // Linux nfnetlink_rcv_msg returns zero before subsystem dispatch here.
  Ack(fd.get(), 60, 0, kUnknown);
  request.resize(NLMSG_HDRLEN - 1);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&p, 1, 50));
}

TEST(NetlinkNetfilter, PeekTruncAndQueueConsumption) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  const auto request = Request(kUnknown, 70);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  char byte;
  const ssize_t full = recv(fd.get(), &byte, 1, MSG_PEEK | MSG_TRUNC);
  ASSERT_GE(full, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
  EXPECT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(full, recv(fd.get(), &byte, 1, MSG_TRUNC));
  EXPECT_EQ(0, poll(&p, 1, 0));
  EXPECT_EQ(-1, recv(fd.get(), &byte, 1, 0));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(NetlinkNetfilter, SendSizeEmptyAndOutOfBandBoundaries) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(kUnknown, 75);
  sockaddr_nl kernel{};
  kernel.nl_family = AF_NETLINK;
  EXPECT_EQ(-1, sendto(fd.get(), request.data(), request.size(), MSG_OOB,
                      reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)));
  EXPECT_EQ(EOPNOTSUPP, errno);
  char byte;
  EXPECT_EQ(-1, recv(fd.get(), &byte, 1, MSG_OOB));
  EXPECT_EQ(EOPNOTSUPP, errno);
  EXPECT_EQ(-1, sendto(fd.get(), request.data(), 0, 0,
                      reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)));
  EXPECT_EQ(ENODATA, errno);
  // Well above the default socket send budget on both reference and guest;
  // this tests rejection, not a Linux promise about a fixed budget value.
  request.resize(16 * 1024 * 1024);
  EXPECT_EQ(-1, Send(fd.get(), request));
  EXPECT_EQ(EMSGSIZE, errno);
  pollfd p{fd.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&p, 1, 0));
}

TEST(NetlinkNetfilter, QueueOverrunReportsAndClearsErrorThenRecovers) {
  for (bool clear_with_sockopt : {false, true}) {
    Fd fd(Open());
    ASSERT_GE(fd.get(), 0);
    const auto request = Request(kUnknown, 76);
    pollfd p{fd.get(), POLLIN, 0};
    bool overrun = false;
    // Discover saturation instead of assuming Linux skb accounting matches
    // DragonOS's bounded queue. No rules or multicast subscribers are touched.
    for (size_t sent = 0; sent < 65536; ++sent) {
      ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
      ASSERT_EQ(1, poll(&p, 1, 0));
      if (p.revents & POLLERR) {
        overrun = true;
        break;
      }
    }
    ASSERT_TRUE(overrun) << "receive queue did not saturate within test budget";
    std::array<uint8_t, 512> bytes{};
    if (clear_with_sockopt) {
      int error = 0;
      socklen_t size = sizeof(error);
      ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
      EXPECT_EQ(ENOBUFS, error);
      ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
      EXPECT_EQ(0, error);
    } else {
      EXPECT_EQ(-1, recv(fd.get(), bytes.data(), bytes.size(), 0));
      EXPECT_EQ(ENOBUFS, errno);
    }
    ASSERT_EQ(1, poll(&p, 1, 0));
    EXPECT_EQ(0, p.revents & POLLERR);
    EXPECT_NE(0, p.revents & POLLIN);
    size_t drained = 0;
    for (; drained < 65536; ++drained) {
      const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
      if (n < 0) {
        ASSERT_EQ(EAGAIN, errno);
        break;
      }
      ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
    }
    ASSERT_GT(drained, 0u);
    ASSERT_LT(drained, 65536u);
    EXPECT_EQ(0, poll(&p, 1, 0));
    const auto recovery = Request(kUnknown, 77);
    ASSERT_EQ(static_cast<ssize_t>(recovery.size()), Send(fd.get(), recovery));
    Ack(fd.get(), 77, NetAdmin() ? EINVAL : EPERM, kUnknown);
  }
}

TEST(NetlinkNetfilter, CongestionPersistsUntilReceiveQueueDrained) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  const auto request = Request(kUnknown, 78);
  pollfd p{fd.get(), POLLIN, 0};
  bool overrun = false;
  for (size_t sent = 0; sent < 65536; ++sent) {
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    ASSERT_EQ(1, poll(&p, 1, 0));
    if (p.revents & POLLERR) {
      overrun = true;
      break;
    }
  }
  ASSERT_TRUE(overrun);
  int error = 0;
  socklen_t size = sizeof(error);
  ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
  ASSERT_EQ(ENOBUFS, error);
  const auto discarded = Request(kUnknown, 79);
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  ASSERT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(0, p.revents & POLLERR);
  // Freeing just one slot must not clear the congestion state.
  Ack(fd.get(), 78, NetAdmin() ? EINVAL : EPERM, kUnknown);
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  ASSERT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(0, p.revents & POLLERR);
  std::array<uint8_t, 512> bytes{};
  size_t drained = 0;
  for (; drained < 65536; ++drained) {
    const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
    if (n < 0) {
      ASSERT_EQ(EAGAIN, errno);
      break;
    }
    ASSERT_GE(n, static_cast<ssize_t>(sizeof(nlmsghdr)));
    nlmsghdr header{};
    std::memcpy(&header, bytes.data(), sizeof(header));
    EXPECT_EQ(78u, header.nlmsg_seq);
  }
  ASSERT_GT(drained, 0u);
  ASSERT_LT(drained, 65536u);
  EXPECT_EQ(0, poll(&p, 1, 0));
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  Ack(fd.get(), 79, NetAdmin() ? EINVAL : EPERM, kUnknown);
}

TEST(NetlinkNetfilter, MembershipCapacityAndUnprivilegedRejection) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  // Linux allocates at least 32 groups even when nfnetlink defines fewer.
  for (int option : {NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP}) {
    int group = 32;
    EXPECT_EQ(0, setsockopt(fd.get(), SOL_NETLINK, option, &group, sizeof(group)));
    group = 33;
    EXPECT_EQ(-1, setsockopt(fd.get(), SOL_NETLINK, option, &group, sizeof(group)));
    EXPECT_EQ(EINVAL, errno);
  }
  Child([]() {
    Fd candidate(Open());
    if (candidate.get() < 0) return 1;
    // Reserve a currently free port, then release it before the failed bind.
    uint32_t port;
    {
      Fd reserve(Open());
      if (reserve.get() < 0 || Bind(reserve.get()) < 0) return 2;
      port = Port(reserve.get());
      if (!port) return 3;
    }
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct data[2]{};
    if (syscall(SYS_capset, &header, data) < 0) return 4;
    for (int option : {NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP}) {
      int group = 32;
      if (setsockopt(candidate.get(), SOL_NETLINK, option, &group, sizeof(group)) != -1 ||
          errno != EPERM) return 5;
    }
    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    address.nl_pid = port;
    address.nl_groups = uint32_t{1} << 31;
    if (bind(candidate.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) != -1 ||
        errno != EPERM) return 6;
    if (Port(candidate.get()) != 0) return 7;
    Fd replacement(Open());
    if (replacement.get() < 0 || Bind(replacement.get(), port) < 0) return 8;
    return 0;
  });
}

TEST(NetlinkNetfilter, IgnoredMessagesHaveCappedSuccessfulAcknowledgements) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  for (bool control : {false, true}) {
    auto request = Request(control ? NLMSG_NOOP : kUnknown, 81);
    nlmsghdr header{};
    std::memcpy(&header, request.data(), sizeof(header));
    if (!control) header.nlmsg_flags = NLM_F_ACK;
    std::memcpy(request.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    pollfd p{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&p, 1, 2000));
    std::array<uint8_t, 256> bytes{};
    ASSERT_EQ(static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))),
              recv(fd.get(), bytes.data(), bytes.size(), 0));
    nlmsghdr reply{};
    nlmsgerr error{};
    std::memcpy(&reply, bytes.data(), sizeof(reply));
    std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
    EXPECT_EQ(NLMSG_ERROR, reply.nlmsg_type);
    EXPECT_EQ(81u, reply.nlmsg_seq);
    EXPECT_NE(0, reply.nlmsg_flags & NLM_F_CAPPED);
    EXPECT_EQ(0, error.error);
    EXPECT_EQ(header.nlmsg_type, error.msg.nlmsg_type);
  }
}

TEST(NetlinkNetfilter, BatchShortGenerationIdIsRejectedBeforeDispatch) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(NFNL_MSG_BATCH_BEGIN, 82,
                         sizeof(nfgenmsg) + NLA_ALIGN(NLA_HDRLEN + 1));
  nfgenmsg gen{};
  gen.res_id = htons(255);
  std::memcpy(request.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  nlattr attr{};
  attr.nla_type = NFNL_BATCH_GENID;
  attr.nla_len = NLA_HDRLEN + 1;
  std::memcpy(request.data() + NLMSG_HDRLEN + sizeof(gen), &attr, sizeof(attr));
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  Ack(fd.get(), 82, ERANGE, NFNL_MSG_BATCH_BEGIN);
}

TEST(NetlinkNetfilter, DroppedSenderCapabilitiesRejectedOnExistingSocket) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  Child([&fd]() {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct data[2]{};
    if (syscall(SYS_capset, &header, data) < 0) return 1;
    const auto request = Request(kUnknown, 80);
    if (Send(fd.get(), request) != static_cast<ssize_t>(request.size())) return 2;
    pollfd p{fd.get(), POLLIN, 0};
    if (poll(&p, 1, 2000) != 1) return 3;
    std::array<uint8_t, 256> bytes{};
    if (recv(fd.get(), bytes.data(), bytes.size(), 0) <
        static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr)))) return 4;
    nlmsgerr error{};
    std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
    return error.error == -EPERM ? 0 : 5;
  });
}

TEST(NetlinkNetfilter, OpenerCredentialsAndExplicitDestinationException) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Child([]() {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct original[2]{};
    if (syscall(SYS_capget, &header, original) < 0) return 1;
    __user_cap_data_struct reduced[2]{};
    std::memcpy(reduced, original, sizeof(reduced));
    reduced[0].effective &= ~(uint32_t{1} << CAP_NET_ADMIN);
    if (syscall(SYS_capset, &header, reduced) < 0) return 2;
    Fd fd(Open());
    if (fd.get() < 0) return 3;
    if (syscall(SYS_capset, &header, original) < 0) return 4;
    const auto request = Request(kUnknown, 90);
    // An implicit destination checks both opener and sender credentials.
    // sendto's explicit kernel address sets NETLINK_SKB_DST in Linux,
    // bypassing only the opener check, never the sender check.
    for (bool explicit_destination : {false, true}) {
      const ssize_t sent = explicit_destination
          ? Send(fd.get(), request)
          : send(fd.get(), request.data(), request.size(), 0);
      if (sent != static_cast<ssize_t>(request.size())) return 5;
      pollfd p{fd.get(), POLLIN, 0};
      if (poll(&p, 1, 2000) != 1) return 6;
      std::array<uint8_t, 256> bytes{};
      if (recv(fd.get(), bytes.data(), bytes.size(), 0) <
          static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr)))) return 7;
      nlmsgerr error{};
      std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
      if (error.error != -(explicit_destination ? EINVAL : EPERM)) return 8;
    }
    return 0;
  });
}
}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
