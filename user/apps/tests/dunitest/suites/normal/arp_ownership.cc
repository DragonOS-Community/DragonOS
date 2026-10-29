#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/if_addr.h>
#include <linux/if_ether.h>
#include <linux/if_link.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

class Fd {
 public:
  explicit Fd(int fd = -1) : fd_(fd) {}
  ~Fd() { if (fd_ >= 0) close(fd_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
 private:
  int fd_;
};

void AppendAttribute(std::vector<uint8_t>& bytes, uint16_t type,
                     const void* data, size_t size) {
  const size_t offset = bytes.size();
  bytes.resize(offset + RTA_ALIGN(RTA_LENGTH(size)), 0);
  auto* attr = reinterpret_cast<rtattr*>(bytes.data() + offset);
  attr->rta_type = type;
  attr->rta_len = RTA_LENGTH(size);
  if (size) std::memcpy(RTA_DATA(attr), data, size);
}

class ArpOwnership : public ::testing::Test {
 protected:
  Fd route_{socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)};
  int packet_ = -1;
  uint32_t sequence_ = 0;
  unsigned ingress_ = 0;
  unsigned injector_ = 0;
  unsigned holder_ = 0;
  std::array<uint8_t, 6> sender_mac_{};
  std::array<uint8_t, 6> ingress_mac_{};

  int Request(uint16_t type, uint16_t flags, const void* body, size_t size,
              const std::vector<uint8_t>& attributes = {}) {
    std::vector<uint8_t> bytes(NLMSG_SPACE(size), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(bytes.data());
    header->nlmsg_len = bytes.size() + attributes.size();
    header->nlmsg_type = type;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    const uint32_t sequence = ++sequence_;
    header->nlmsg_seq = sequence;
    std::memcpy(NLMSG_DATA(header), body, size);
    bytes.insert(bytes.end(), attributes.begin(), attributes.end());
    sockaddr_nl destination{};
    destination.nl_family = AF_NETLINK;
    if (sendto(route_.get(), bytes.data(), bytes.size(), 0,
               reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) !=
        static_cast<ssize_t>(bytes.size())) return errno;
    for (int attempt = 0; attempt < 32; ++attempt) {
      pollfd ready{route_.get(), POLLIN, 0};
      if (poll(&ready, 1, 2000) <= 0) return ETIMEDOUT;
      alignas(nlmsghdr) std::array<uint8_t, 4096> reply{};
      const ssize_t count = recv(route_.get(), reply.data(), reply.size(), 0);
      if (count < 0) return errno;
      int remaining = count;
      for (auto* h = reinterpret_cast<nlmsghdr*>(reply.data());
           NLMSG_OK(h, remaining); h = NLMSG_NEXT(h, remaining)) {
        if (h->nlmsg_seq != sequence || h->nlmsg_type != NLMSG_ERROR) continue;
        if (h->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) return EPROTO;
        return -reinterpret_cast<nlmsgerr*>(NLMSG_DATA(h))->error;
      }
    }
    return ETIMEDOUT;
  }

  int Pair(const char* first, const char* peer) {
    std::vector<uint8_t> peer_info(sizeof(ifinfomsg), 0);
    AppendAttribute(peer_info, IFLA_IFNAME, peer, std::strlen(peer) + 1);
    std::vector<uint8_t> data;
    AppendAttribute(data, VETH_INFO_PEER | NLA_F_NESTED,
                    peer_info.data(), peer_info.size());
    std::vector<uint8_t> info;
    AppendAttribute(info, IFLA_INFO_KIND, "veth", 5);
    AppendAttribute(info, IFLA_INFO_DATA | NLA_F_NESTED, data.data(), data.size());
    std::vector<uint8_t> attrs;
    AppendAttribute(attrs, IFLA_IFNAME, first, std::strlen(first) + 1);
    AppendAttribute(attrs, IFLA_LINKINFO | NLA_F_NESTED, info.data(), info.size());
    ifinfomsg link{};
    return Request(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &link, sizeof(link), attrs);
  }

  int Up(unsigned index) {
    ifinfomsg link{};
    link.ifi_index = index;
    link.ifi_flags = IFF_UP;
    link.ifi_change = IFF_UP;
    return Request(RTM_NEWLINK, 0, &link, sizeof(link));
  }

  int Address(unsigned index, const char* text, bool add = true) {
    ifaddrmsg address{};
    address.ifa_family = AF_INET;
    address.ifa_prefixlen = 24;
    address.ifa_index = index;
    const in_addr ip{inet_addr(text)};
    std::vector<uint8_t> attrs;
    AppendAttribute(attrs, IFA_LOCAL, &ip, sizeof(ip));
    AppendAttribute(attrs, IFA_ADDRESS, &ip, sizeof(ip));
    return Request(add ? RTM_NEWADDR : RTM_DELADDR,
                   add ? NLM_F_CREATE | NLM_F_EXCL : 0,
                   &address, sizeof(address), attrs);
  }

  // Linux source validation still requires a route to the requester. This is
  // a route, not an ingress address: the weak-host test must leave it unnumbered.
  int SenderRoute() {
    rtmsg route{};
    route.rtm_family = AF_INET;
    route.rtm_dst_len = 24;
    route.rtm_table = RT_TABLE_MAIN;
    route.rtm_protocol = RTPROT_STATIC;
    route.rtm_scope = RT_SCOPE_LINK;
    route.rtm_type = RTN_UNICAST;
    const in_addr network{inet_addr("203.0.113.0")};
    std::vector<uint8_t> attrs;
    AppendAttribute(attrs, RTA_DST, &network, sizeof(network));
    AppendAttribute(attrs, RTA_OIF, &ingress_, sizeof(ingress_));
    return Request(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, &route, sizeof(route), attrs);
  }

  bool Mac(const char* name, std::array<uint8_t, 6>* mac) {
    Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.get() < 0) return false;
    ifreq request{};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd.get(), SIOCGIFHWADDR, &request) != 0) return false;
    std::memcpy(mac->data(), request.ifr_hwaddr.sa_data, mac->size());
    return true;
  }

  void SetUp() override {
    ASSERT_GE(route_.get(), 0);
    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    ASSERT_EQ(0, bind(route_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)));
    ASSERT_EQ(0, Pair("ao_sender", "ao_ingress"));
    ASSERT_EQ(0, Pair("ao_holder", "ao_spare"));
    injector_ = if_nametoindex("ao_sender");
    ingress_ = if_nametoindex("ao_ingress");
    holder_ = if_nametoindex("ao_holder");
    ASSERT_NE(0u, injector_);
    ASSERT_NE(0u, ingress_);
    ASSERT_NE(0u, holder_);
    for (const char* name : {"ao_sender", "ao_ingress", "ao_holder", "ao_spare"}) {
      ASSERT_EQ(0, Up(if_nametoindex(name)));
    }
    ASSERT_EQ(0, SenderRoute());
    ASSERT_TRUE(Mac("ao_sender", &sender_mac_));
    ASSERT_TRUE(Mac("ao_ingress", &ingress_mac_));
    packet_ = socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_ARP));
    ASSERT_GE(packet_, 0);
    sockaddr_ll bind_address{};
    bind_address.sll_family = AF_PACKET;
    bind_address.sll_protocol = htons(ETH_P_ARP);
    bind_address.sll_ifindex = injector_;
    ASSERT_EQ(0, bind(packet_, reinterpret_cast<sockaddr*>(&bind_address),
                      sizeof(bind_address)));
  }

  void TearDown() override {
    if (packet_ >= 0) close(packet_);
    for (const char* name : {"ao_sender", "ao_holder"}) {
      const unsigned index = if_nametoindex(name);
      if (index) {
        ifinfomsg link{};
        link.ifi_index = index;
        EXPECT_EQ(0, Request(RTM_DELLINK, 0, &link, sizeof(link)));
      }
    }
  }

  // ARP has no transaction ID. Match the complete reply tuple and both MACs,
  // so gratuitous traffic and the outgoing request cannot satisfy this check.
  void Probe(const char* target_text, const char* source_text, bool expect_reply) {
    const uint32_t target = inet_addr(target_text);
    const uint32_t source = inet_addr(source_text);
    std::array<uint8_t, 2048> received{};
    while (recv(packet_, received.data(), received.size(), MSG_DONTWAIT) >= 0) {}
    ASSERT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK);
    std::array<uint8_t, 42> frame{};
    std::memset(frame.data(), 0xff, 6);
    std::memcpy(frame.data() + 6, sender_mac_.data(), 6);
    auto put16 = [&](size_t offset, uint16_t value) {
      value = htons(value);
      std::memcpy(frame.data() + offset, &value, sizeof(value));
    };
    put16(12, ETH_P_ARP);
    put16(14, 1);  // Ethernet hardware type.
    put16(16, ETH_P_IP);
    frame[18] = 6;
    frame[19] = 4;
    put16(20, 1);  // ARP request.
    std::memcpy(frame.data() + 22, sender_mac_.data(), 6);
    std::memcpy(frame.data() + 28, &source, 4);
    std::memcpy(frame.data() + 38, &target, 4);
    sockaddr_ll destination{};
    destination.sll_family = AF_PACKET;
    destination.sll_protocol = htons(ETH_P_ARP);
    destination.sll_ifindex = injector_;
    destination.sll_halen = 6;
    std::memset(destination.sll_addr, 0xff, 6);
    ASSERT_EQ(static_cast<ssize_t>(frame.size()),
              sendto(packet_, frame.data(), frame.size(), 0,
                     reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::milliseconds(expect_reply ? 2000 : 300);
    bool saw_reply = false;
    while (std::chrono::steady_clock::now() < deadline) {
      const auto left = std::chrono::duration_cast<std::chrono::milliseconds>(
          deadline - std::chrono::steady_clock::now()).count();
      pollfd ready{packet_, POLLIN, 0};
      const int result = poll(&ready, 1, static_cast<int>(left + 1));
      if (result < 0 && errno == EINTR) continue;
      ASSERT_GE(result, 0);
      if (!result) break;
      const ssize_t size = recv(packet_, received.data(), received.size(), MSG_DONTWAIT);
      if (size < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) continue;
      ASSERT_GE(size, 0);
      if (size < 42 || received[12] != 8 || received[13] != 6 ||
          received[14] != 0 || received[15] != 1 || received[16] != 8 ||
          received[17] != 0 || received[18] != 6 || received[19] != 4 ||
          received[20] != 0 || received[21] != 2) continue;
      if (std::memcmp(received.data(), sender_mac_.data(), 6) ||
          std::memcmp(received.data() + 6, ingress_mac_.data(), 6) ||
          std::memcmp(received.data() + 22, ingress_mac_.data(), 6) ||
          std::memcmp(received.data() + 28, &target, 4) ||
          std::memcmp(received.data() + 32, sender_mac_.data(), 6) ||
          std::memcmp(received.data() + 38, &source, 4)) continue;
      saw_reply = true;
      break;
    }
    EXPECT_EQ(expect_reply, saw_reply)
        << "target=" << target_text << " source=" << source_text;
  }
};

TEST_F(ArpOwnership, ForeignTargetMustNotUseIpAnyIpAsProxyArp) {
  ASSERT_EQ(0, Address(ingress_, "198.18.40.1"));
  Probe("198.18.40.99", "198.18.40.7", false);
  Probe("198.18.40.1", "198.18.40.8", true);
}

TEST_F(ArpOwnership, UnnumberedIngressAnswersOtherInterfaceWithOffSubnetSender) {
  ASSERT_EQ(0, Address(holder_, "198.18.41.1"));
  Probe("198.18.41.1", "203.0.113.7", true);
}

TEST_F(ArpOwnership, DuplicateAddressDetectionOnlyAnswersOwnedTarget) {
  ASSERT_EQ(0, Address(ingress_, "198.18.40.1"));
  Probe("198.18.40.1", "0.0.0.0", true);
  Probe("198.18.40.99", "0.0.0.0", false);
}

TEST_F(ArpOwnership, LoopbackAndMulticastAreNotEthernetArpTargets) {
  ASSERT_EQ(0, Up(if_nametoindex("lo")));
  ASSERT_EQ(0, Address(ingress_, "198.18.40.1"));
  Probe("127.0.0.1", "203.0.113.8", false);
  Probe("224.0.0.1", "203.0.113.9", false);
}

TEST_F(ArpOwnership, MartianSenderCannotUseWeakHostReplyPermission) {
  ASSERT_EQ(0, Address(ingress_, "198.18.40.1"));
  Probe("198.18.40.1", "127.0.0.2", false);
  Probe("198.18.40.1", "0.1.2.3", false);
  Probe("198.18.40.1", "198.18.40.10", true);
}

TEST_F(ArpOwnership, RemovedAddressLosesOwnershipImmediately) {
  ASSERT_EQ(0, Address(holder_, "198.18.41.1"));
  Probe("198.18.41.1", "203.0.113.10", true);
  ASSERT_EQ(0, Address(holder_, "198.18.41.1", false));
  Probe("198.18.41.1", "203.0.113.11", false);
  ASSERT_EQ(0, Address(holder_, "198.18.41.1"));
  Probe("198.18.41.1", "203.0.113.12", true);
}

TEST_F(ArpOwnership, GratuitousForeignRequestDoesNotGrantReplyOwnership) {
  ASSERT_EQ(0, Address(ingress_, "198.18.40.1"));
  Probe("198.18.40.99", "198.18.40.99", false);
  Probe("198.18.40.99", "198.18.40.9", false);
}

TEST_F(ArpOwnership, MovingAddressToAnotherNamespaceRevokesOldNamespaceOwnership) {
  ASSERT_EQ(0, Address(holder_, "198.18.41.1"));
  Probe("198.18.41.1", "203.0.113.13", true);
  int ready_pipe[2];
  int stop_pipe[2];
  ASSERT_EQ(0, pipe(ready_pipe));
  if (pipe(stop_pipe) != 0) {
    close(ready_pipe[0]);
    close(ready_pipe[1]);
    FAIL() << "pipe: " << errno;
  }
  const pid_t child = fork();
  if (child == 0) {
    close(ready_pipe[0]);
    close(stop_pipe[1]);
    close(route_.get());
    close(packet_);
    const char ready = unshare(CLONE_NEWNET) == 0 ? 'R' : 'E';
    if (write(ready_pipe[1], &ready, 1) != 1 || ready != 'R') _exit(1);
    close(ready_pipe[1]);
    char stop;
    const ssize_t count = read(stop_pipe[0], &stop, 1);
    _exit(count == 0 ? 0 : 2);
  }
  close(ready_pipe[1]);
  close(stop_pipe[0]);
  if (child < 0) {
    close(ready_pipe[0]);
    close(stop_pipe[1]);
    FAIL() << "fork: " << errno;
  }
  // Cleanup also runs after an ASSERT failure, never leaving a child netns alive.
  struct Child {
    pid_t pid;
    int stop;
    ~Child() {
      close(stop);
      int status = 0;
      pid_t result;
      do { result = waitpid(pid, &status, 0); } while (result < 0 && errno == EINTR);
      EXPECT_EQ(pid, result);
      EXPECT_TRUE(WIFEXITED(status));
      if (WIFEXITED(status)) {
        EXPECT_EQ(0, WEXITSTATUS(status));
      }
    }
  } cleanup{child, stop_pipe[1]};
  Fd ready(ready_pipe[0]);
  pollfd poll_ready{ready.get(), POLLIN, 0};
  ASSERT_GT(poll(&poll_ready, 1, 2000), 0);
  char value = 0;
  ASSERT_EQ(1, read(ready.get(), &value, 1));
  ASSERT_EQ('R', value);
  ifinfomsg link{};
  link.ifi_index = holder_;
  const uint32_t target_pid = child;
  std::vector<uint8_t> attrs;
  AppendAttribute(attrs, IFLA_NET_NS_PID, &target_pid, sizeof(target_pid));
  ASSERT_EQ(0, Request(RTM_NEWLINK, 0, &link, sizeof(link), attrs));
  EXPECT_EQ(0u, if_nametoindex("ao_holder"));
  Probe("198.18.41.1", "203.0.113.14", false);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  // One private namespace for the whole suite: no host addresses, nftables,
  // conntrack or pre-existing neighbors may influence these ownership tests.
  if (unshare(CLONE_NEWNET) != 0) {
    std::perror("ARP ownership requires a private network namespace");
    return 1;
  }
  return RUN_ALL_TESTS();
}
