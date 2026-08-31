#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/if_link.h>
#include <linux/if.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <optional>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

bool IsDragonOS() {
    struct utsname uts {};
    return uname(&uts) == 0 && std::strstr(uts.release, "dragonos") != nullptr;
}

int OpenRouteSocket() {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout {};
    timeout.tv_sec = 2;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    sockaddr_nl address {};
    address.nl_family = AF_NETLINK;
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

int RecvAck(int fd, uint32_t seq) {
    std::array<unsigned char, 4096> buffer {};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq || header->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int SetLinkUp(int fd, int ifindex, bool up, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_SETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;
    request.link.ifi_flags = up ? IFF_UP : 0;
    request.link.ifi_change = IFF_UP;

    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return errno;
    }
    return RecvAck(fd, seq);
}

std::optional<short> QueryIoctlFlags(int fd, const char* name) {
    ifreq request {};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd, SIOCGIFFLAGS, &request) < 0) return std::nullopt;
    return request.ifr_flags;
}

std::optional<uint32_t> QueryLinkFlags(int fd, int ifindex, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;

    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return std::nullopt;
    }

    std::array<unsigned char, 4096> buffer {};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return std::nullopt;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR) return std::nullopt;
            if (header->nlmsg_type != RTM_NEWLINK) continue;
            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index == ifindex) return link->ifi_flags;
        }
    }
}

std::optional<int> FindEthernetIfindex() {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return std::nullopt;

    for (int i = 0; i <= 20; ++i) {
        const std::string name = "eth" + std::to_string(i);
        ifreq ifr{};
        std::strncpy(ifr.ifr_name, name.c_str(), IFNAMSIZ - 1);
        if (ioctl(fd.Get(), SIOCGIFINDEX, &ifr) == 0) return ifr.ifr_ifindex;
    }
    return std::nullopt;
}

std::optional<uint32_t> QueryLinkMtu(int ifindex) {
    FdGuard fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    if (fd.Get() < 0) return std::nullopt;

    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    if (bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        return std::nullopt;
    }

    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request{};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = 1;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;

    if (send(fd.Get(), &request, request.header.nlmsg_len, 0) < 0) return std::nullopt;

    char buffer[4096]{};
    for (;;) {
        ssize_t length = recv(fd.Get(), buffer, sizeof(buffer), 0);
        if (length < 0) return std::nullopt;

        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer); NLMSG_OK(header, length);
             header = NLMSG_NEXT(header, length)) {
            if (header->nlmsg_seq != request.header.nlmsg_seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR || header->nlmsg_type == NLMSG_DONE) {
                return std::nullopt;
            }
            if (header->nlmsg_type != RTM_NEWLINK) continue;

            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index != ifindex) continue;

            int attr_length = IFLA_PAYLOAD(header);
            for (auto* attr = IFLA_RTA(link); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type != IFLA_MTU || RTA_PAYLOAD(attr) != sizeof(uint32_t)) {
                    continue;
                }
                uint32_t mtu = 0;
                std::memcpy(&mtu, RTA_DATA(attr), sizeof(mtu));
                return mtu;
            }
            return std::nullopt;
        }
    }
}

TEST(RtnetlinkLinkSemantics, VirtioEthernetReportsStandardIpMtu) {
    const auto ifindex = FindEthernetIfindex();
    if (!ifindex.has_value()) GTEST_SKIP() << "No eth0-eth20 interface found";

    const auto mtu = QueryLinkMtu(*ifindex);
    ASSERT_TRUE(mtu.has_value()) << "RTM_GETLINK did not return IFLA_MTU: errno=" << errno
                                << " (" << std::strerror(errno) << ")";
    EXPECT_EQ(*mtu, 1500U);
}

int RunVisibleFlagsCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;

    FdGuard ioctl_fd(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard route_fd(OpenRouteSocket());
    if (ioctl_fd.Get() < 0 || route_fd.Get() < 0) return 10;

    ifreq index_request {};
    std::strncpy(index_request.ifr_name, "lo", IFNAMSIZ - 1);
    if (ioctl(ioctl_fd.Get(), SIOCGIFINDEX, &index_request) < 0) return 11;
    const int ifindex = index_request.ifr_ifindex;

    if (SetLinkUp(route_fd.Get(), ifindex, true, 1) != 0) return 12;
    const auto ioctl_up = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_up = QueryLinkFlags(route_fd.Get(), ifindex, 2);
    if (!ioctl_up.has_value() || !rtnl_up.has_value()) return 13;
    constexpr uint32_t kRuntimeFlags = IFF_RUNNING | IFF_LOWER_UP | IFF_DORMANT;
    constexpr uint32_t kExpectedUp = IFF_UP | IFF_RUNNING;
    if ((static_cast<uint16_t>(*ioctl_up) & kExpectedUp) != kExpectedUp) return 14;
    if ((*rtnl_up & kExpectedUp) != kExpectedUp) return 15;
    if ((static_cast<uint16_t>(*ioctl_up) & IFF_LOOPBACK) == 0 ||
        (*rtnl_up & IFF_LOOPBACK) == 0) {
        return 16;
    }

    if (SetLinkUp(route_fd.Get(), ifindex, false, 3) != 0) return 17;
    const auto ioctl_down = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_down = QueryLinkFlags(route_fd.Get(), ifindex, 4);
    if (!ioctl_down.has_value() || !rtnl_down.has_value()) return 18;
    if ((static_cast<uint16_t>(*ioctl_down) & (IFF_UP | kRuntimeFlags)) != 0) return 19;
    if ((*rtnl_down & (IFF_UP | kRuntimeFlags)) != 0) return 20;
    if ((static_cast<uint16_t>(*ioctl_down) & IFF_LOOPBACK) == 0 ||
        (*rtnl_down & IFF_LOOPBACK) == 0) {
        return 21;
    }
    if ((static_cast<uint16_t>(*ioctl_down) & 0xffffU) != (*rtnl_down & 0xffffU)) return 22;

    if (SetLinkUp(route_fd.Get(), ifindex, true, 5) != 0) return 23;
    const auto ioctl_restored = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_restored = QueryLinkFlags(route_fd.Get(), ifindex, 6);
    if (!ioctl_restored.has_value() || !rtnl_restored.has_value()) return 24;
    if ((static_cast<uint16_t>(*ioctl_restored) & kExpectedUp) != kExpectedUp) return 25;
    if ((*rtnl_restored & kExpectedUp) != kExpectedUp) return 26;
    return 0;
}

TEST(RtnetlinkLinkSemantics, VisibleFlagsFollowAdministrativeAndRuntimeState) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) _exit(RunVisibleFlagsCase());

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    const int result = WEXITSTATUS(status);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
