#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <linux/if_ether.h>
#include <linux/if_link.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <vector>

namespace {

class Fd {
  public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return value_; }
  private:
    int value_;
};

void AppendAttr(std::vector<uint8_t>* bytes, uint16_t type, const void* data, size_t length) {
    const size_t offset = bytes->size();
    bytes->resize(offset + RTA_ALIGN(RTA_LENGTH(length)), 0);
    auto* attr = reinterpret_cast<rtattr*>(bytes->data() + offset);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(length);
    if (length != 0) std::memcpy(RTA_DATA(attr), data, length);
}

std::vector<uint8_t> LinkRequest(uint16_t type, uint16_t flags, uint32_t sequence) {
    std::vector<uint8_t> request(NLMSG_LENGTH(sizeof(ifinfomsg)), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = request.size();
    header->nlmsg_type = type;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    header->nlmsg_seq = sequence;
    return request;
}

void AddRequestAttr(std::vector<uint8_t>* request, uint16_t type, const void* data,
                    size_t length) {
    auto* header = reinterpret_cast<nlmsghdr*>(request->data());
    request->resize(NLMSG_ALIGN(header->nlmsg_len), 0);
    AppendAttr(request, type, data, length);
    reinterpret_cast<nlmsghdr*>(request->data())->nlmsg_len = request->size();
}

int SendAck(int fd, const std::vector<uint8_t>& request) {
    const uint32_t sequence = reinterpret_cast<const nlmsghdr*>(request.data())->nlmsg_seq;
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }
    std::array<uint8_t, 4096> reply{};
    for (;;) {
        const ssize_t count = recv(fd, reply.data(), reply.size(), 0);
        if (count < 0) return errno;
        int remaining = static_cast<int>(count);
        for (auto* message = reinterpret_cast<nlmsghdr*>(reply.data());
             NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
            if (message->nlmsg_seq != sequence || message->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(message));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int SetUp(int fd, uint32_t ifindex, uint32_t sequence) {
    auto request = LinkRequest(RTM_SETLINK, 0, sequence);
    auto* body = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    body->ifi_index = ifindex;
    body->ifi_flags = IFF_UP;
    body->ifi_change = IFF_UP;
    return SendAck(fd, request);
}

int AddIpv4(int fd, uint32_t ifindex, uint32_t sequence, const char* address,
            const char* broadcast_address) {
    std::vector<uint8_t> request(NLMSG_LENGTH(sizeof(ifaddrmsg)), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = request.size();
    header->nlmsg_type = RTM_NEWADDR;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    header->nlmsg_seq = sequence;
    auto* body = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    body->ifa_family = AF_INET;
    body->ifa_prefixlen = 24;
    body->ifa_index = ifindex;
    in_addr local{};
    in_addr broadcast{};
    inet_pton(AF_INET, address, &local);
    inet_pton(AF_INET, broadcast_address, &broadcast);
    AddRequestAttr(&request, IFA_LOCAL, &local, sizeof(local));
    AddRequestAttr(&request, IFA_ADDRESS, &local, sizeof(local));
    AddRequestAttr(&request, IFA_BROADCAST, &broadcast, sizeof(broadcast));
    return SendAck(fd, request);
}

uint16_t Checksum(const uint8_t* data, size_t length) {
    uint32_t sum = 0;
    for (size_t i = 0; i < length; i += 2) {
        sum += (static_cast<uint16_t>(data[i]) << 8) |
               (i + 1 < length ? data[i + 1] : 0);
    }
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return static_cast<uint16_t>(~sum);
}

int InjectUdp(int fd, uint32_t ifindex, const char* source, const char* destination,
              uint16_t port, uint8_t payload) {
    std::array<uint8_t, 29> packet{};
    packet[0] = 0x45;
    packet[3] = packet.size();
    packet[8] = 64;
    packet[9] = IPPROTO_UDP;
    inet_pton(AF_INET, source, &packet[12]);
    inet_pton(AF_INET, destination, &packet[16]);
    const uint16_t checksum = Checksum(packet.data(), 20);
    packet[10] = checksum >> 8;
    packet[11] = checksum;
    packet[20] = 0xa0;
    packet[21] = 0x47;
    packet[22] = port >> 8;
    packet[23] = port;
    packet[25] = 9;
    packet[28] = payload;
    sockaddr_ll link_destination{};
    link_destination.sll_family = AF_PACKET;
    link_destination.sll_protocol = htons(ETH_P_IP);
    link_destination.sll_ifindex = ifindex;
    link_destination.sll_halen = ETH_ALEN;
    std::memset(link_destination.sll_addr, 0xff, ETH_ALEN);
    return sendto(fd, packet.data(), packet.size(), 0,
                  reinterpret_cast<sockaddr*>(&link_destination), sizeof(link_destination)) ==
                   static_cast<ssize_t>(packet.size())
               ? 0
               : errno;
}

int RunIngressCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return -1;
    Fd route(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    if (route.Get() < 0) return 100 + errno;
    sockaddr_nl nl{};
    nl.nl_family = AF_NETLINK;
    if (bind(route.Get(), reinterpret_cast<sockaddr*>(&nl), sizeof(nl)) != 0) return 200 + errno;
    timeval timeout{2, 0};
    if (setsockopt(route.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        return 300 + errno;
    }
    const char first[] = "dkc47a";
    const char peer[] = "dkc47b";
    std::vector<uint8_t> peer_body(sizeof(ifinfomsg), 0);
    AppendAttr(&peer_body, IFLA_IFNAME, peer, sizeof(peer));
    std::vector<uint8_t> data;
    AppendAttr(&data, VETH_INFO_PEER, peer_body.data(), peer_body.size());
    std::vector<uint8_t> info;
    const char kind[] = "veth";
    AppendAttr(&info, IFLA_INFO_KIND, kind, sizeof(kind));
    AppendAttr(&info, IFLA_INFO_DATA, data.data(), data.size());
    auto create = LinkRequest(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 1);
    AddRequestAttr(&create, IFLA_IFNAME, first, sizeof(first));
    AddRequestAttr(&create, IFLA_LINKINFO, info.data(), info.size());
    if (const int error = SendAck(route.Get(), create); error != 0) return 400 + error;
    const uint32_t first_index = if_nametoindex(first);
    const uint32_t peer_index = if_nametoindex(peer);
    if (first_index == 0 || peer_index == 0) return 500;
    if (SetUp(route.Get(), first_index, 2) != 0 || SetUp(route.Get(), peer_index, 3) != 0 ||
        AddIpv4(route.Get(), first_index, 4, "10.47.0.1", "10.47.0.99") != 0 ||
        AddIpv4(route.Get(), peer_index, 5, "10.48.0.1", "10.48.0.99") != 0) {
        return 600;
    }
    Fd receiver(socket(AF_INET, SOCK_DGRAM, 0));
    if (receiver.Get() < 0) return 700 + errno;
    timeval receive_timeout{0, 500000};
    setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &receive_timeout,
               sizeof(receive_timeout));
    sockaddr_in local{};
    local.sin_family = AF_INET;
    local.sin_port = htons(48547);
    if (bind(receiver.Get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) {
        return 800 + errno;
    }
    Fd packet(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
    if (packet.Get() < 0) return 900 + errno;
    if (const int error = InjectUdp(packet.Get(), peer_index, "10.47.0.2",
                                    "10.47.0.99", 48547, 'A'); error != 0) {
        return 1000 + error;
    }
    char received = 0;
    if (recv(receiver.Get(), &received, 1, 0) != 1 || received != 'A') return 1100 + errno;
    if (const int error = InjectUdp(packet.Get(), peer_index, "10.47.0.99",
                                    "10.47.0.99", 48547, 'B'); error != 0) {
        return 1200 + error;
    }
    if (recv(receiver.Get(), &received, 1, 0) >= 0 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        return 1300;
    }
    // The explicit broadcast is owned by the peer stack, but this frame
    // enters on the first interface. It must survive the pre-routed handoff.
    Fd peer_receiver(socket(AF_INET, SOCK_DGRAM, 0));
    if (peer_receiver.Get() < 0) return 1400 + errno;
    setsockopt(peer_receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &receive_timeout,
               sizeof(receive_timeout));
    sockaddr_in peer_local{};
    peer_local.sin_family = AF_INET;
    peer_local.sin_port = htons(48548);
    inet_pton(AF_INET, "10.48.0.99", &peer_local.sin_addr);
    if (bind(peer_receiver.Get(), reinterpret_cast<sockaddr*>(&peer_local),
             sizeof(peer_local)) != 0) return 1500 + errno;
    if (const int error = InjectUdp(packet.Get(), peer_index, "10.47.0.2",
                                    "10.48.0.99", 48548, 'C'); error != 0) {
        return 1600 + error;
    }
    if (recv(peer_receiver.Get(), &received, 1, 0) != 1 || received != 'C') {
        return 1700 + errno;
    }
    // A locally sent broadcast is reinjected into the receiving stack after
    // routing. It must keep the same broadcast classification as link ingress.
    Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
    if (sender.Get() < 0) return 1800 + errno;
    const int enabled = 1;
    if (setsockopt(sender.Get(), SOL_SOCKET, SO_BROADCAST, &enabled, sizeof(enabled)) != 0) {
        return 1900 + errno;
    }
    if (sendto(sender.Get(), "D", 1, 0, reinterpret_cast<sockaddr*>(&peer_local),
               sizeof(peer_local)) != 1) return 2000 + errno;
    if (recv(peer_receiver.Get(), &received, 1, 0) != 1 || received != 'D') {
        return 2100 + errno;
    }
    return 0;
}

}  // namespace

TEST(RtnetlinkIpv4BroadcastIngress, VethCrossIfaceAndLocalOutputPreserveClassification) {
    int pipe_fds[2];
    ASSERT_EQ(pipe(pipe_fds), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(pipe_fds[0]);
        const int result = RunIngressCase();
        const ssize_t written = write(pipe_fds[1], &result, sizeof(result));
        (void)written;
        _exit(result == 0 || result == -1 ? 0 : 1);
    }
    close(pipe_fds[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    while (waitpid(child, &status, WNOHANG) == 0) {
        if (std::chrono::steady_clock::now() > deadline) {
            kill(child, SIGKILL);
            waitpid(child, &status, 0);
            close(pipe_fds[0]);
            FAIL() << "broadcast ingress child timed out";
        }
        usleep(10000);
    }
    int result = 0;
    ASSERT_EQ(read(pipe_fds[0], &result, sizeof(result)),
              static_cast<ssize_t>(sizeof(result)));
    close(pipe_fds[0]);
    if (result == -1) {
        utsname name{};
        const bool dragonos = uname(&name) == 0 &&
                                std::strstr(name.release, "dragonos") != nullptr;
        if (!dragonos) GTEST_SKIP() << "network namespace unavailable on host";
        FAIL() << "DragonOS failed to create the required network namespace";
    }
    EXPECT_EQ(result, 0) << "stage/error=" << result;
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
