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
#include <net/if.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
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

inline constexpr int kEthPAll = 0x0003;

#ifndef PACKET_ADD_MEMBERSHIP
#define PACKET_ADD_MEMBERSHIP 1
#endif
#ifndef PACKET_DROP_MEMBERSHIP
#define PACKET_DROP_MEMBERSHIP 2
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

// DragonOS rejects invalid mr_type with EINVAL (stricter than Linux 6.6 which accepts it).
TEST(AfPacketMcast, InvalidMrTypeReturnsEINVAL) {
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
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), -1);
    EXPECT_EQ(errno, EINVAL);
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
