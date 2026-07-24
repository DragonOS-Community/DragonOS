#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/if_link.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

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

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
