// af_packet_mcast.cc - AF_PACKET multicast membership tests (dunitest/gtest)
//
// Converted from user/apps/c_unitest/test_af_packet_mcast.c.
// Verify DragonOS AF_PACKET PACKET_ADD_MEMBERSHIP / PACKET_DROP_MEMBERSHIP
// behavior, covering the three mr_type values PROMISC / ALLMULTI / MULTICAST
// as well as error-code semantics.
//
// Requires a network interface: uses discover_ifname() to brute-force
// enumerate eth0-eth20.

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
#include <cstdint>
#include <optional>
#include <string>

// ---- Manually define constants (DragonOS musl may lack if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

#ifndef SIOCGIFINDEX
#define SIOCGIFINDEX 0x8933
#endif

#ifndef IFNAMSIZ
#define IFNAMSIZ 16
#endif

#ifndef IFLA_PROMISCUITY
#define IFLA_PROMISCUITY 30
#endif
#ifndef IFLA_ALLMULTI
#define IFLA_ALLMULTI 61
#endif

inline constexpr int kEthPAll = 0x0003;

#ifndef PACKET_ADD_MEMBERSHIP
#define PACKET_ADD_MEMBERSHIP 1
#endif
#ifndef PACKET_DROP_MEMBERSHIP
#define PACKET_DROP_MEMBERSHIP 2
#endif
#ifndef PACKET_FANOUT
#define PACKET_FANOUT 18
#endif
#ifndef PACKET_FANOUT_LB
#define PACKET_FANOUT_LB 1
#endif

// mr_type constants for packet_mreq (corresponding to Linux if_packet.h)
#ifndef PACKET_MR_PROMISC
#define PACKET_MR_PROMISC 1
#endif
#ifndef PACKET_MR_MULTICAST
#define PACKET_MR_MULTICAST 0
#endif
#ifndef PACKET_MR_ALLMULTI
#define PACKET_MR_ALLMULTI 2
#endif
#ifndef PACKET_MR_UNICAST
#define PACKET_MR_UNICAST 3
#endif

namespace {

// Manually define struct packet_mreq (corresponding to Linux struct packet_mreq).
// Layout: mr_ifindex(i32) + mr_type(u16) + mr_alen(u16) + mr_address[8].
struct PacketMreq {
    int mr_ifindex;
    unsigned short mr_type;
    unsigned short mr_alen;
    unsigned char mr_address[8];
};
static_assert(sizeof(PacketMreq) == 16, "packet_mreq must match the Linux UAPI layout");

// RAII fd guard
class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

// Brute-force enumerate eth0-eth20, verifying existence via ioctl(SIOCGIFINDEX).
// DragonOS does not have /proc/net/dev, and interface names may be unstable.
std::string DiscoverIfname() {
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) return "eth0";
    for (int i = 0; i <= 20; ++i) {
        std::string name = "eth" + std::to_string(i);
        struct ifreq ifr;
        std::memset(&ifr, 0, sizeof(ifr));
        std::strncpy(ifr.ifr_name, name.c_str(), IFNAMSIZ - 1);
        if (ioctl(sock, SIOCGIFINDEX, &ifr) == 0) {
            close(sock);
            return name;
        }
    }
    close(sock);
    return "eth0";
}

// Get the NIC ifindex: use a temporary AF_INET/DGRAM control socket to send ioctl.
// Returns ifindex (>=1) on success, -1 on failure.
int GetIfIndex(const std::string& ifname) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    ifr.ifr_name[IFNAMSIZ - 1] = '\0';

    int ctrl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctrl < 0) return -1;
    int rc = ioctl(ctrl, SIOCGIFINDEX, &ifr);
    close(ctrl);
    if (rc < 0) return -1;
    return ifr.ifr_ifindex;
}

// Probe for a NIC and create a SOCK_RAW socket.
// GTEST_SKIP if no NIC or insufficient permissions; the returned FdGuard holds a valid fd.
// Returns ifindex, outputs socket fd via out_fd.
int SetupMcastEnv(FdGuard* out_fd) {
    std::string ifname = DiscoverIfname();
    int ifindex = GetIfIndex(ifname);
    if (ifindex < 0) {
        return -1;
    }
    out_fd->Reset(socket(AF_PACKET, SOCK_RAW, htons(kEthPAll)));
    if (out_fd->Get() < 0) {
        return -2;
    }
    return ifindex;
}

struct LinkSnapshot {
    unsigned int configured_flags = 0;
    uint32_t promiscuity = 0;
    uint32_t allmulti = 0;
};

int RecvAck(int fd, uint32_t seq);

int OpenRouteSocket(uint32_t groups = 0) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    sockaddr_nl addr{};
    addr.nl_family = AF_NETLINK;
    addr.nl_groups = groups;
    if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

int SetLinkAllmultiAttr(int fd, int ifindex, uint32_t seq) {
    alignas(nlmsghdr) char buf[NLMSG_SPACE(sizeof(ifinfomsg)) + RTA_SPACE(sizeof(uint32_t))]{};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    nlh->nlmsg_type = RTM_SETLINK;
    nlh->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    nlh->nlmsg_seq = seq;
    auto* ifi = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(nlh));
    ifi->ifi_family = AF_UNSPEC;
    ifi->ifi_index = ifindex;

    auto* attr = reinterpret_cast<rtattr*>(buf + NLMSG_ALIGN(nlh->nlmsg_len));
    attr->rta_type = IFLA_ALLMULTI;
    attr->rta_len = RTA_LENGTH(sizeof(uint32_t));
    uint32_t value = 1;
    std::memcpy(RTA_DATA(attr), &value, sizeof(value));
    nlh->nlmsg_len = NLMSG_ALIGN(nlh->nlmsg_len) + RTA_LENGTH(sizeof(uint32_t));

    if (send(fd, buf, nlh->nlmsg_len, 0) < 0) return errno;
    return RecvAck(fd, seq);
}

std::optional<LinkSnapshot> ParseLinkSnapshot(const nlmsghdr* nlh, int ifindex) {
    if (nlh->nlmsg_type != RTM_NEWLINK) return std::nullopt;
    const auto* ifi = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(nlh));
    if (ifi->ifi_index != ifindex) return std::nullopt;

    LinkSnapshot snapshot{};
    snapshot.configured_flags = ifi->ifi_flags;
    bool have_promiscuity = false;
    bool have_allmulti = false;
    int attr_len = IFLA_PAYLOAD(nlh);
    for (auto* attr = IFLA_RTA(ifi); RTA_OK(attr, attr_len);
         attr = RTA_NEXT(attr, attr_len)) {
        if (RTA_PAYLOAD(attr) != sizeof(uint32_t)) continue;
        uint32_t value = 0;
        std::memcpy(&value, RTA_DATA(attr), sizeof(value));
        if (attr->rta_type == IFLA_PROMISCUITY) {
            snapshot.promiscuity = value;
            have_promiscuity = true;
        } else if (attr->rta_type == IFLA_ALLMULTI) {
            snapshot.allmulti = value;
            have_allmulti = true;
        }
    }
    if (!have_promiscuity || !have_allmulti) return std::nullopt;
    return snapshot;
}

std::optional<LinkSnapshot> RecvLinkNotification(int fd, int ifindex) {
    char buf[4096]{};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) return std::nullopt;
        for (auto* nlh = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(nlh, len);
             nlh = NLMSG_NEXT(nlh, len)) {
            if (nlh->nlmsg_seq != 0) continue;
            if (auto snapshot = ParseLinkSnapshot(nlh, ifindex); snapshot.has_value()) {
                return snapshot;
            }
        }
    }
}

int RecvAck(int fd, uint32_t seq) {
    char buf[4096]{};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) return errno;

        for (auto* nlh = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(nlh, len);
             nlh = NLMSG_NEXT(nlh, len)) {
            if (nlh->nlmsg_seq != seq || nlh->nlmsg_type != NLMSG_ERROR) continue;
            const auto* err = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(nlh));
            return err->error == 0 ? 0 : -err->error;
        }
    }
}

int SetLinkFlags(int fd, int ifindex, unsigned int flags, unsigned int change, uint32_t seq) {
    struct {
        nlmsghdr nlh;
        ifinfomsg ifi;
    } req{};
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;
    req.ifi.ifi_flags = flags;
    req.ifi.ifi_change = change;

    if (send(fd, &req, req.nlh.nlmsg_len, 0) < 0) return errno;
    return RecvAck(fd, seq);
}

std::optional<LinkSnapshot> GetLinkSnapshot(int fd, int ifindex, uint32_t seq) {
    struct {
        nlmsghdr nlh;
        ifinfomsg ifi;
    } req{};
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    req.nlh.nlmsg_type = RTM_GETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;

    if (send(fd, &req, req.nlh.nlmsg_len, 0) < 0) return std::nullopt;

    char buf[4096]{};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) return std::nullopt;

        for (auto* nlh = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(nlh, len);
             nlh = NLMSG_NEXT(nlh, len)) {
            if (nlh->nlmsg_seq != seq) continue;
            if (nlh->nlmsg_type == NLMSG_ERROR) return std::nullopt;
            if (auto snapshot = ParseLinkSnapshot(nlh, ifindex); snapshot.has_value()) {
                return snapshot;
            }
        }
    }
}

class LinkFlagsRestore {
  public:
    LinkFlagsRestore(int fd, int ifindex, unsigned int original, uint32_t* seq)
        : fd_(fd), ifindex_(ifindex), original_(original), seq_(seq) {}
    LinkFlagsRestore(const LinkFlagsRestore&) = delete;
    LinkFlagsRestore& operator=(const LinkFlagsRestore&) = delete;
    ~LinkFlagsRestore() {
        constexpr unsigned int kMask = IFF_PROMISC | IFF_ALLMULTI;
        (void)SetLinkFlags(fd_, ifindex_, original_ & kMask, kMask, ++(*seq_));
    }

  private:
    int fd_;
    int ifindex_;
    unsigned int original_;
    uint32_t* seq_;
};

}  // namespace

// ===== Test 1: ADD_MEMBERSHIP (PACKET_MR_PROMISC) =====
TEST(AfPacketMcast, AddMembershipPromisc) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno)
                           << " (requires CAP_NET_RAW)";

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

// ===== Test 2: DROP_MEMBERSHIP (PACKET_MR_PROMISC) =====
TEST(AfPacketMcast, DropMembershipPromisc) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno);

    // First ADD then DROP (reusing the same mreq)
    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_PROMISC;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno) << " (should restore interface flags)";
}

// ===== Test 3: ADD/DROP MEMBERSHIP (PACKET_MR_ALLMULTI) =====
TEST(AfPacketMcast, AddDropMembershipAllmulti) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_ALLMULTI;

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    // DROP cleanup
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno) << " (should restore)";
}

TEST(AfPacketMcast, UnknownMrTypeMatchesLinuxNoop) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = 999;  // invalid type
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

TEST(AfPacketMcast, UnknownIfindexReturnsENODEV) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = 99999;  // non-existent interface
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
    EXPECT_EQ(errno, ENODEV);
}

// ===== Test 6: ADD/DROP MEMBERSHIP (PACKET_MR_MULTICAST, specific multicast MAC) =====
TEST(AfPacketMcast, AddDropMembershipMulticast) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "No usable NIC found, skipping multicast test";
    }
    ASSERT_NE(ifindex, -2) << "Failed to create AF_PACKET socket: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_MULTICAST;
    mreq.mr_alen = 6;  // Ethernet MAC length
    // Multicast MAC: 01:00:5e:00:00:01 (IGMP/multicast group mapping)
    mreq.mr_address[0] = 0x01;
    mreq.mr_address[1] = 0x00;
    mreq.mr_address[2] = 0x5e;
    mreq.mr_address[3] = 0x00;
    mreq.mr_address[4] = 0x00;
    mreq.mr_address[5] = 0x01;

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    // DROP cleanup
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

TEST(AfPacketMcast, MembershipPayloadValidationMatchesLinux) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = ifindex;
    mreq.mr_type = PACKET_MR_MULTICAST;
    mreq.mr_alen = 6;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq,
                         sizeof(mreq) - 1),
              -1);
    EXPECT_EQ(errno, EINVAL);

    mreq.mr_alen = 9;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
    EXPECT_EQ(errno, EINVAL);
}

TEST(AfPacketMcast, AddressMembershipRequiresDeviceAddressLength) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);

    for (unsigned short type : {PACKET_MR_MULTICAST, PACKET_MR_UNICAST}) {
        PacketMreq mreq{};
        mreq.mr_ifindex = ifindex;
        mreq.mr_type = type;
        mreq.mr_alen = 5;
        EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
        EXPECT_EQ(errno, EINVAL);

        mreq.mr_alen = 7;
        EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
        EXPECT_EQ(errno, EINVAL);

        mreq.mr_alen = 6;
        EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
            << ErrnoString(errno);
        EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
            << ErrnoString(errno);
    }
}

TEST(AfPacketMcast, DropMissingMembershipIsIdempotent) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = ifindex;
    mreq.mr_type = PACKET_MR_PROMISC;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

TEST(AfPacketMcast, DuplicateMembershipAndCloseUseOneDeviceReference) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);
    FdGuard route_fd(OpenRouteSocket());
    ASSERT_GE(route_fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 100;
    auto baseline = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(baseline.has_value());

    PacketMreq mreq{};
    mreq.mr_ifindex = ifindex;
    mreq.mr_type = PACKET_MR_PROMISC;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0);
    auto first = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(first.has_value());
    EXPECT_EQ(first->promiscuity, baseline->promiscuity + 1);

    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0);
    auto duplicate = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(duplicate.has_value());
    EXPECT_EQ(duplicate->promiscuity, first->promiscuity);

    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0);
    auto first_drop = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(first_drop.has_value());
    EXPECT_EQ(first_drop->promiscuity, first->promiscuity);

    fd.Reset();
    auto closed = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(closed.has_value());
    EXPECT_EQ(closed->promiscuity, baseline->promiscuity);
}

TEST(AfPacketMcast, FanoutCloseReleasesPromiscMembership) {
    FdGuard packet_fd;
    int ifindex = SetupMcastEnv(&packet_fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);
    FdGuard route_fd(OpenRouteSocket());
    ASSERT_GE(route_fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 150;
    auto baseline = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(baseline.has_value());

    PacketMreq mreq{};
    mreq.mr_ifindex = ifindex;
    mreq.mr_type = PACKET_MR_PROMISC;
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq,
                         sizeof(mreq)),
              0)
        << ErrnoString(errno);

    constexpr int kFanoutGroup = 0x5a30;
    int fanout = (PACKET_FANOUT_LB << 16) | kFanoutGroup;
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_FANOUT, &fanout, sizeof(fanout)), 0)
        << ErrnoString(errno);
    auto active = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(active.has_value());
    EXPECT_EQ(active->promiscuity, baseline->promiscuity + 1);

    packet_fd.Reset();
    auto closed = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(closed.has_value());
    EXPECT_EQ(closed->promiscuity, baseline->promiscuity);
}

TEST(AfPacketMcast, AdministrativeReceiveModesSurviveMembershipDrop) {
    FdGuard packet_fd;
    int ifindex = SetupMcastEnv(&packet_fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);
    FdGuard route_fd(OpenRouteSocket());
    ASSERT_GE(route_fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 200;
    auto baseline = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(baseline.has_value());

    LinkFlagsRestore restore(route_fd.Get(), ifindex, baseline->configured_flags, &seq);
    constexpr unsigned int kModes = IFF_PROMISC | IFF_ALLMULTI;
    ASSERT_EQ(SetLinkFlags(route_fd.Get(), ifindex, kModes, kModes, ++seq), 0);
    auto configured = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(configured.has_value());
    EXPECT_EQ(configured->configured_flags & kModes, kModes);
    EXPECT_EQ(configured->promiscuity,
              baseline->promiscuity + ((baseline->configured_flags & IFF_PROMISC) ? 0 : 1));
    EXPECT_EQ(configured->allmulti,
              baseline->allmulti + ((baseline->configured_flags & IFF_ALLMULTI) ? 0 : 1));

    PacketMreq promisc{};
    promisc.mr_ifindex = ifindex;
    promisc.mr_type = PACKET_MR_PROMISC;
    PacketMreq allmulti = promisc;
    allmulti.mr_type = PACKET_MR_ALLMULTI;
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &promisc,
                         sizeof(promisc)),
              0);
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &allmulti,
                         sizeof(allmulti)),
              0);
    auto added = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(added.has_value());
    EXPECT_EQ(added->promiscuity, configured->promiscuity + 1);
    EXPECT_EQ(added->allmulti, configured->allmulti + 1);

    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &promisc,
                         sizeof(promisc)),
              0);
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &allmulti,
                         sizeof(allmulti)),
              0);
    auto dropped = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(dropped.has_value());
    EXPECT_EQ(dropped->configured_flags & kModes, kModes);
    EXPECT_EQ(dropped->promiscuity, configured->promiscuity);
    EXPECT_EQ(dropped->allmulti, configured->allmulti);
}

TEST(AfPacketMcast, MembershipChangesNotifyLinkSubscribers) {
    FdGuard packet_fd;
    int ifindex = SetupMcastEnv(&packet_fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);
    FdGuard route_fd(OpenRouteSocket(RTMGRP_LINK));
    ASSERT_GE(route_fd.Get(), 0) << ErrnoString(errno);
    timeval timeout{1, 0};
    ASSERT_EQ(setsockopt(route_fd.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
    uint32_t seq = 300;
    auto baseline = GetLinkSnapshot(route_fd.Get(), ifindex, ++seq);
    ASSERT_TRUE(baseline.has_value());

    PacketMreq mreq{};
    mreq.mr_ifindex = ifindex;
    mreq.mr_type = PACKET_MR_PROMISC;
    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)),
              0);
    auto added = RecvLinkNotification(route_fd.Get(), ifindex);
    ASSERT_TRUE(added.has_value());
    EXPECT_EQ(added->promiscuity, baseline->promiscuity + 1);

    ASSERT_EQ(setsockopt(packet_fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)),
              0);
    auto dropped = RecvLinkNotification(route_fd.Get(), ifindex);
    ASSERT_TRUE(dropped.has_value());
    EXPECT_EQ(dropped->promiscuity, baseline->promiscuity);
}

TEST(AfPacketMcast, SetLinkRejectsAllmultiCountAttribute) {
    FdGuard packet_fd;
    int ifindex = SetupMcastEnv(&packet_fd);
    if (ifindex == -1) GTEST_SKIP() << "No usable NIC found";
    ASSERT_NE(ifindex, -2) << ErrnoString(errno);
    FdGuard route_fd(OpenRouteSocket());
    ASSERT_GE(route_fd.Get(), 0) << ErrnoString(errno);
    EXPECT_EQ(SetLinkAllmultiAttr(route_fd.Get(), ifindex, 400), EINVAL);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
