// af_packet_e2e.cc - AF_PACKET end-to-end send/receive test (dunitest/gtest)
//
// Converted from user/apps/c_unitest/test_af_packet_e2e.c.
// Verify DragonOS AF_PACKET actual Ethernet frame send and receive:
//   Test 1: SOCK_RAW send path -- sendto() / sendmsg() return correct byte count
//   Test 2: SOCK_RAW receive path -- recvfrom() returns data + validate sockaddr_ll
//   Test 3: recvmsg() iovec scatter -- data correctly scattered into multiple buffers
//   Test 4: sendmsg() iovec gather -- multiple buffers correctly assembled then sent
//   Test 5: SOCK_DGRAM send/receive -- kernel constructs Ethernet header, returns L3 payload length
//
// Runtime environment: DragonOS QEMU, eth0(virtio-net) 10.0.2.15/24, gateway 10.0.2.2.
// Use GTEST_SKIP() when the runtime network environment cannot provide packets.

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netinet/if_ether.h>
#include <netpacket/packet.h>
#include <pthread.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>

// ---- Manually define constants (DragonOS musl may lack if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

#ifndef SIOCGIFHWADDR
#define SIOCGIFHWADDR 0x8927
#endif

// PACKET_OUTGOING: loopback packet marker for locally-originated packets (Linux if_packet.h)
#ifndef PACKET_OUTGOING
#define PACKET_OUTGOING 4
#endif
#ifndef PACKET_AUXDATA
#define PACKET_AUXDATA 8
#endif
#ifndef PACKET_FANOUT
#define PACKET_FANOUT 18
#endif
#ifndef PACKET_FANOUT_LB
#define PACKET_FANOUT_LB 1
#endif
#ifndef TP_STATUS_USER
#define TP_STATUS_USER 1
#endif

inline constexpr int kSoAttachFilter = 26;
inline constexpr int kSoDetachFilter = 27;

struct TestSockFilter {
    uint16_t code;
    uint8_t jt;
    uint8_t jf;
    uint32_t k;
};

struct TestSockFprog {
    uint16_t len;
    TestSockFilter* filter;
};

namespace {

inline constexpr int kArpPktLen = 28;
inline constexpr int kEthHdrLen = 14;
inline constexpr int kArpFrameLen = kEthHdrLen + kArpPktLen;  // 42
inline constexpr size_t kVlanFrameLen = 1518;
inline constexpr int kEthFrameLen = 1514;
inline constexpr uint16_t kPrivateEtherType = 0x88b5;
inline constexpr uint16_t kVlanEtherType = 0x8100;

inline constexpr const char* kLocalIp = "10.0.2.15";
inline constexpr const char* kGateway = "10.0.2.2";
inline constexpr int kRecvMaxAttempts = 8;

// Manually define ARP packet (musl lacks struct ether_arp), 28 bytes
struct ArpHdr {
    uint16_t ar_hrd;     // hardware type: ARPHRD_ETHER(1)
    uint16_t ar_pro;     // protocol type: ETH_P_IP(0x0800)
    uint8_t ar_hln;      // hardware address length: 6
    uint8_t ar_pln;      // protocol address length: 4
    uint16_t ar_op;      // operation: ARPOP_REQUEST(1) / ARPOP_REPLY(2)
    uint8_t ar_sha[6];   // sender MAC
    uint8_t ar_spa[4];   // sender IP
    uint8_t ar_tha[6];   // target MAC
    uint8_t ar_tpa[4];   // target IP
};
static_assert(sizeof(ArpHdr) == kArpPktLen, "ArpHdr size must be 28");

struct PacketAuxdata {
    uint32_t tp_status;
    uint32_t tp_len;
    uint32_t tp_snaplen;
    uint16_t tp_mac;
    uint16_t tp_net;
    uint16_t tp_vlan_tci;
    uint16_t tp_vlan_tpid;
};

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

// Brute-force enumerate eth0-eth20, verify existence via ioctl(SIOCGIFINDEX).
// DragonOS lacks /proc/net/dev, and interface names may be unstable.
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

// Get interface index via ioctl; return -1 on failure
int GetIfIndex(int any_fd, const std::string& ifname) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFINDEX, &ifr) < 0) return -1;
    return ifr.ifr_ifindex;
}

// Get local MAC: ioctl(SIOCGIFHWADDR) → sysfs → fallback MAC. Never fails.
void GetIfHwaddr(int any_fd, const std::string& ifname, uint8_t mac[6]) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFHWADDR, &ifr) == 0) {
        std::memcpy(mac, ifr.ifr_hwaddr.sa_data, 6);
        bool allzero = true;
        for (int i = 0; i < 6; ++i) {
            if (mac[i]) {
                allzero = false;
                break;
            }
        }
        if (!allzero) return;
    }
    // fall back to sysfs
    char path[64];
    std::snprintf(path, sizeof(path), "/sys/class/net/%s/address", ifname.c_str());
    FILE* f = std::fopen(path, "r");
    if (f) {
        unsigned int v[6];
        int n = std::fscanf(f, "%x:%x:%x:%x:%x:%x", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]);
        std::fclose(f);
        if (n == 6) {
            for (int i = 0; i < 6; ++i) mac[i] = static_cast<uint8_t>(v[i]);
            return;
        }
    }
    // final fallback: default QEMU virtio-net MAC
    static const uint8_t kDefaultMac[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
    std::memcpy(mac, kDefaultMac, 6);
}

// Build a broadcast ARP request frame (ether_header + ArpHdr), length = kArpFrameLen
void BuildArpRequest(uint8_t* frame, const uint8_t local_mac[6]) {
    std::memset(frame, 0, kArpFrameLen);
    // Ethernet header
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    std::memcpy(frame + 0, bcast, 6);       // dst = broadcast
    std::memcpy(frame + 6, local_mac, 6);   // src = local
    frame[12] = (ETH_P_ARP >> 8) & 0xff;    // ethertype = 0x0806 (network byte order)
    frame[13] = ETH_P_ARP & 0xff;
    // ARP payload
    ArpHdr* a = reinterpret_cast<ArpHdr*>(frame + kEthHdrLen);
    a->ar_hrd = htons(1);                    // ARPHRD_ETHER
    a->ar_pro = htons(ETH_P_IP);             // IPv4
    a->ar_hln = 6;
    a->ar_pln = 4;
    a->ar_op = htons(ARPOP_REQUEST);
    std::memcpy(a->ar_sha, local_mac, 6);
    struct in_addr spa;
    spa.s_addr = inet_addr(kLocalIp);
    std::memcpy(a->ar_spa, &spa.s_addr, 4);
    // ar_tha stays 0
    struct in_addr tpa;
    tpa.s_addr = inet_addr(kGateway);
    std::memcpy(a->ar_tpa, &tpa.s_addr, 4);
}

// Build destination address for sockaddr_ll
void MakeDstLL(struct sockaddr_ll* sa, int ifindex, const uint8_t dst_mac[6]) {
    std::memset(sa, 0, sizeof(*sa));
    sa->sll_family = AF_PACKET;
    sa->sll_protocol = htons(ETH_P_ARP);
    sa->sll_ifindex = ifindex;
    sa->sll_hatype = ARPHRD_ETHER;
    sa->sll_pkttype = 0;
    sa->sll_halen = ETH_ALEN;
    std::memcpy(sa->sll_addr, dst_mac, 6);
}

// Validate received sockaddr_ll fields against expectations
::testing::AssertionResult ValidateSll(const struct sockaddr_ll* sll,
                                       const uint8_t* frame, ssize_t n) {
    if (sll->sll_family != AF_PACKET) {
        return ::testing::AssertionFailure()
               << "sll_family=" << sll->sll_family << " (expected " << AF_PACKET << ")";
    }
    unsigned short proto = ntohs(sll->sll_protocol);
    if (proto == 0) {
        return ::testing::AssertionFailure() << "sll_protocol=0 (invalid)";
    }
    if (n >= kEthHdrLen) {
        unsigned short eth_proto =
            (static_cast<unsigned short>(frame[12]) << 8) | frame[13];
        if (proto != eth_proto) {
            return ::testing::AssertionFailure()
                   << "sll_protocol=0x" << std::hex << proto
                   << " does not match frame ethertype=0x" << eth_proto;
        }
    }
    if (sll->sll_hatype != ARPHRD_ETHER) {
        return ::testing::AssertionFailure()
               << "sll_hatype=" << sll->sll_hatype << " (expected ARPHRD_ETHER)";
    }
    if (sll->sll_halen != ETH_ALEN) {
        return ::testing::AssertionFailure()
               << "sll_halen=" << static_cast<int>(sll->sll_halen) << " (expected ETH_ALEN)";
    }
    bool allzero = true;
    for (int i = 0; i < ETH_ALEN; ++i) {
        if (sll->sll_addr[i]) {
            allzero = false;
            break;
        }
    }
    if (allzero) {
        return ::testing::AssertionFailure() << "sll_addr is all zeros (expected valid MAC)";
    }
    return ::testing::AssertionSuccess();
}

// Send several ARP requests on a bound SOCK_RAW socket to elicit gateway responses
void Stimulate(int tx_fd, int ifindex, const uint8_t local_mac[6]) {
    uint8_t frame[kArpFrameLen];
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);
    BuildArpRequest(frame, local_mac);
    for (int i = 0; i < 3; ++i) {
        sendto(tx_fd, frame, kArpFrameLen, 0,
               reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
        usleep(20 * 1000);  // 20ms interval
    }
}

// Create a SOCK_RAW socket bound to the specified interface, return fd or -1.
int MakeBoundRaw(const std::string& ifname, int ifindex, uint8_t mac[6]) {
    int fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (fd < 0) return -1;
    struct sockaddr_ll sa;
    std::memset(&sa, 0, sizeof(sa));
    sa.sll_family = AF_PACKET;
    sa.sll_protocol = htons(ETH_P_ALL);
    sa.sll_ifindex = ifindex;
    if (bind(fd, reinterpret_cast<struct sockaddr*>(&sa), sizeof(sa)) < 0) {
        close(fd);
        return -1;
    }
    GetIfHwaddr(fd, ifname, mac);
    struct timeval tv;
    tv.tv_sec = 1;
    tv.tv_usec = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

// Probe NIC ifindex; return -1 on failure
int ProbeIfindex(const std::string& ifname) {
    int ctrl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctrl < 0) return -1;
    int idx = GetIfIndex(ctrl, ifname);
    close(ctrl);
    return idx;
}

int AttachFilter(int fd, TestSockFilter* filter, uint16_t len) {
    TestSockFprog program{len, filter};
    return setsockopt(fd, SOL_SOCKET, kSoAttachFilter, &program, sizeof(program));
}

struct FilterSwapStress {
    int fd;
    int error{0};
};

void* ReplaceAndDetachFilters(void* raw) {
    auto* stress = static_cast<FilterSwapStress*>(raw);
    TestSockFilter accept_all[] = {{0x06, 0, 0, 0xffffffffU}};
    TestSockFilter drop_all[] = {{0x06, 0, 0, 0}};
    int ignored = 0;
    constexpr unsigned int kControlCycles = 256;
    for (unsigned int cycle = 0; cycle < kControlCycles; ++cycle) {
        if (AttachFilter(stress->fd, accept_all, 1) != 0 ||
            AttachFilter(stress->fd, drop_all, 1) != 0 ||
            setsockopt(stress->fd, SOL_SOCKET, kSoDetachFilter, &ignored,
                       sizeof(ignored)) != 0) {
            stress->error = errno == 0 ? EIO : errno;
            break;
        }
    }

    return nullptr;
}

}  // namespace

// ===== Test 1: SOCK_RAW send (sendto / sendmsg) =====
TEST(AfPacketE2E, RawSendtoAndSendmsg) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found (" << ifname << "), skipping e2e send test";
    }

    FdGuard raw_fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(raw_fd.Get(), ifname, local_mac);
    uint8_t frame[kArpFrameLen];
    BuildArpRequest(frame, local_mac);
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    // 1a: sendto
    ssize_t n = sendto(raw_fd.Get(), frame, kArpFrameLen, 0,
                       reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
    ASSERT_EQ(n, kArpFrameLen) << "sendto byte count error: " << ErrnoString(errno);

    // 1b: sendmsg single iovec
    struct iovec iov;
    iov.iov_base = frame;
    iov.iov_len = kArpFrameLen;
    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &dst;
    msg.msg_namelen = sizeof(dst);
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    ssize_t m = sendmsg(raw_fd.Get(), &msg, 0);
    ASSERT_EQ(m, kArpFrameLen) << "sendmsg byte count error: " << ErrnoString(errno);
}

// ===== Test 4: sendmsg iovec gather =====
// (Placed before receive tests so Test 2/3 can observe traffic)
TEST(AfPacketE2E, SendmsgIovecGather) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping sendmsg gather test";
    }

    FdGuard raw_fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(raw_fd.Get(), ifname, local_mac);
    uint8_t frame[kArpFrameLen];
    BuildArpRequest(frame, local_mac);

    // Split frame into 2 segments: eth header(14) + arp payload(28)
    uint8_t part1[kEthHdrLen];
    uint8_t part2[kArpPktLen];
    std::memcpy(part1, frame, kEthHdrLen);
    std::memcpy(part2, frame + kEthHdrLen, kArpPktLen);

    struct iovec iov[2];
    iov[0].iov_base = part1;
    iov[0].iov_len = kEthHdrLen;
    iov[1].iov_base = part2;
    iov[1].iov_len = kArpPktLen;

    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &dst;
    msg.msg_namelen = sizeof(dst);
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;

    ssize_t m = sendmsg(raw_fd.Get(), &msg, 0);
    ASSERT_EQ(m, kArpFrameLen) << "sendmsg gather byte count error: " << ErrnoString(errno);
}

// ===== Test 2: SOCK_RAW receive (recvfrom + sockaddr_ll) =====
TEST(AfPacketE2E, RecvfromReturnsDataAndSockaddrLl) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping recvfrom receive test";
    }

    uint8_t local_mac[6];
    FdGuard raw_fd(MakeBoundRaw(ifname, ifindex, local_mac));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    Stimulate(raw_fd.Get(), ifindex, local_mac);

    uint8_t rbuf[kEthFrameLen + 64];
    struct sockaddr_ll from;
    bool got_inbound = false;
    ssize_t n = -1;
    // Poll in bounded nonblocking steps while waiting for the stimulated traffic.
    for (int attempt = 0; attempt < kRecvMaxAttempts; ++attempt) {
        std::memset(&from, 0, sizeof(from));
        socklen_t fromlen = sizeof(from);
        n = recvfrom(raw_fd.Get(), rbuf, sizeof(rbuf), MSG_DONTWAIT,
                     reinterpret_cast<struct sockaddr*>(&from), &fromlen);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                usleep(50 * 1000);  // retry after 50ms
                continue;
            }
            break;  // other error
        }
        if (from.sll_pkttype == PACKET_OUTGOING) {
            continue;  // skip locally-originated loopback packets
        }
        got_inbound = true;
        break;
    }

    if (!got_inbound || n < 0) {
        GTEST_SKIP() << "Timed out waiting for any inbound frame (network environment has no loopback/response)";
    }
    EXPECT_GT(n, 0);
    EXPECT_TRUE(ValidateSll(&from, rbuf, n));
}

// ===== Test 3: recvmsg iovec scatter =====
TEST(AfPacketE2E, RecvmsgIovecScatter) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping recvmsg scatter test";
    }

    uint8_t local_mac[6];
    FdGuard raw_fd(MakeBoundRaw(ifname, ifindex, local_mac));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    Stimulate(raw_fd.Get(), ifindex, local_mac);

    // Scatter into 2 buffers: first 16 bytes + remainder
    uint8_t seg0[16];
    uint8_t seg1[kEthFrameLen];
    struct iovec iov[2];
    iov[0].iov_base = seg0;
    iov[1].iov_base = seg1;

    struct sockaddr_ll from;
    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &from;
    msg.msg_namelen = sizeof(from);
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;

    bool got_inbound = false;
    ssize_t n = -1;
    for (int attempt = 0; attempt < kRecvMaxAttempts; ++attempt) {
        std::memset(&from, 0, sizeof(from));
        msg.msg_namelen = sizeof(from);
        // reset iov lengths (must restore after being modified by recvmsg)
        iov[0].iov_len = sizeof(seg0);
        iov[1].iov_len = sizeof(seg1);
        n = recvmsg(raw_fd.Get(), &msg, MSG_DONTWAIT);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                usleep(50 * 1000);
                continue;
            }
            break;
        }
        if (from.sll_pkttype == PACKET_OUTGOING) {
            continue;
        }
        got_inbound = true;
        break;
    }

    if (!got_inbound || n < 0) {
        GTEST_SKIP() << "Timed out waiting for any inbound frame";
    }

    // Assemble the actual contents of the two iovecs
    size_t take0 = static_cast<size_t>(n) < sizeof(seg0)
                       ? static_cast<size_t>(n)
                       : sizeof(seg0);
    size_t take1 = static_cast<size_t>(n) > sizeof(seg0)
                       ? static_cast<size_t>(n) - sizeof(seg0)
                       : 0;
    EXPECT_EQ(static_cast<ssize_t>(take0 + take1), n);

    uint8_t joined[kEthFrameLen + 64];
    std::memcpy(joined, seg0, take0);
    std::memcpy(joined + take0, seg1, take1);

    // Validate assembled frame: ethertype matches sll_protocol
    if (static_cast<size_t>(n) >= static_cast<size_t>(kEthHdrLen)) {
        unsigned short eth_proto =
            (static_cast<unsigned short>(joined[12]) << 8) | joined[13];
        unsigned short sll_proto = ntohs(from.sll_protocol);
        EXPECT_EQ(eth_proto, sll_proto)
            << "Assembled frame ethertype does not match sll_protocol";
    } else {
        ADD_FAILURE() << "Received frame too short (" << n << " < " << kEthHdrLen << ")";
    }

    EXPECT_TRUE(ValidateSll(&from, joined, n));
}

// ===== Test 5: SOCK_DGRAM send/receive (kernel constructs Ethernet header) =====
TEST(AfPacketE2E, DgramSendReturnsLayer3PayloadLen) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping SOCK_DGRAM test";
    }

    FdGuard dgram_fd(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_ALL)));
    ASSERT_GE(dgram_fd.Get(), 0) << ErrnoString(errno);

    // bind to interface
    struct sockaddr_ll sa;
    std::memset(&sa, 0, sizeof(sa));
    sa.sll_family = AF_PACKET;
    sa.sll_protocol = htons(ETH_P_ALL);
    sa.sll_ifindex = ifindex;
    ASSERT_EQ(bind(dgram_fd.Get(), reinterpret_cast<struct sockaddr*>(&sa), sizeof(sa)), 0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(dgram_fd.Get(), ifname, local_mac);

    // DGRAM sends only L3 payload (ARP 28 bytes), kernel adds Ethernet header
    ArpHdr arp;
    std::memset(&arp, 0, sizeof(arp));
    arp.ar_hrd = htons(1);
    arp.ar_pro = htons(ETH_P_IP);
    arp.ar_hln = 6;
    arp.ar_pln = 4;
    arp.ar_op = htons(ARPOP_REQUEST);
    std::memcpy(arp.ar_sha, local_mac, 6);
    struct in_addr spa;
    spa.s_addr = inet_addr(kLocalIp);
    std::memcpy(arp.ar_spa, &spa.s_addr, 4);
    struct in_addr tpa;
    tpa.s_addr = inet_addr(kGateway);
    std::memcpy(arp.ar_tpa, &tpa.s_addr, 4);

    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    ssize_t n = sendto(dgram_fd.Get(), &arp, sizeof(arp), 0,
                       reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
    ASSERT_EQ(n, static_cast<ssize_t>(sizeof(arp)))
        << "DGRAM sendto should return L3 payload length 28: " << ErrnoString(errno);
}

TEST(AfPacketE2E, DgramReceivesHeaderOnlyFrameWithoutFilter) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic zero-length receive testing";

    FdGuard receiver(socket(AF_PACKET, SOCK_DGRAM, htons(kPrivateEtherType)));
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    struct sockaddr_ll bind_addr{};
    bind_addr.sll_family = AF_PACKET;
    bind_addr.sll_protocol = htons(kPrivateEtherType);
    bind_addr.sll_ifindex = ifindex;
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)), 0)
        << ErrnoString(errno);
    struct timeval timeout{1, 0};
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout,
                         sizeof(timeout)),
              0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(sender.Get(), ifname, local_mac);
    uint8_t frame[kEthHdrLen]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kPrivateEtherType);

    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kPrivateEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);
    ASSERT_EQ(sendto(sender.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<sockaddr*>(&dst), sizeof(dst)),
              static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);

    uint8_t byte = 0xaa;
    struct sockaddr_ll from{};
    socklen_t from_len = sizeof(from);
    ASSERT_EQ(recvfrom(receiver.Get(), &byte, sizeof(byte), 0,
                       reinterpret_cast<sockaddr*>(&from), &from_len),
              0)
        << "header-only SOCK_DGRAM packet must be queued: " << ErrnoString(errno);
    EXPECT_EQ(byte, 0xaa);
    EXPECT_EQ(from.sll_pkttype, PACKET_OUTGOING);
    EXPECT_EQ(ntohs(from.sll_protocol), kPrivateEtherType);
}

// A socket created with a non-zero protocol is wildcard-bound until an explicit
// bind. Exercise the netns registry, short-iovec MSG_TRUNC, full name length and
// the minimum truthful PACKET_AUXDATA fields together.
TEST(AfPacketE2E, WildcardRecvmsgTruncAndAuxdata) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping wildcard recvmsg test";
    }

    FdGuard fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    int enabled = 1;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_AUXDATA, &enabled, sizeof(enabled)), 0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(fd.Get(), ifname, local_mac);
    Stimulate(fd.Get(), ifindex, local_mac);

    uint8_t short_buf[8]{};
    struct iovec iov{short_buf, sizeof(short_buf)};
    struct sockaddr_ll from{};
    alignas(struct cmsghdr) uint8_t control[CMSG_SPACE(sizeof(PacketAuxdata))]{};
    struct msghdr msg{};
    msg.msg_name = &from;
    msg.msg_namelen = 1;  // force address truncation; kernel must still report full size
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof(control);

    ssize_t n = -1;
    for (int attempt = 0; attempt < kRecvMaxAttempts; ++attempt) {
        msg.msg_namelen = 1;
        msg.msg_controllen = sizeof(control);
        n = recvmsg(fd.Get(), &msg, MSG_DONTWAIT | MSG_TRUNC);
        if (n >= 0) break;
        if (errno != EAGAIN && errno != EWOULDBLOCK) break;
        usleep(50 * 1000);
    }
    if (n < 0) {
        GTEST_SKIP() << "Network environment returned no frames usable for wildcard recvmsg";
    }

    EXPECT_GT(n, static_cast<ssize_t>(sizeof(short_buf)));
    EXPECT_NE(msg.msg_flags & MSG_TRUNC, 0);
    EXPECT_EQ(msg.msg_namelen, sizeof(struct sockaddr_ll));

    struct cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
    ASSERT_NE(cmsg, nullptr);
    EXPECT_EQ(cmsg->cmsg_level, SOL_PACKET);
    EXPECT_EQ(cmsg->cmsg_type, PACKET_AUXDATA);
    ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(PacketAuxdata)));
    const auto* aux = reinterpret_cast<const PacketAuxdata*>(CMSG_DATA(cmsg));
    EXPECT_NE(aux->tp_status & TP_STATUS_USER, 0u);
    EXPECT_EQ(aux->tp_len, static_cast<uint32_t>(n));
    EXPECT_GE(aux->tp_snaplen, sizeof(short_buf));
    EXPECT_GE(aux->tp_net, static_cast<uint16_t>(kEthHdrLen));
}

TEST(AfPacketE2E, PeekDoesNotConsumeFrame) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "No usable NIC found, skipping MSG_PEEK test";
    }

    uint8_t local_mac[6];
    FdGuard fd(MakeBoundRaw(ifname, ifindex, local_mac));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);

    uint8_t frame[kArpFrameLen];
    BuildArpRequest(frame, local_mac);
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);
    ASSERT_EQ(sendto(fd.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst)),
              static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);

    uint8_t peek_buf[kEthFrameLen + 64]{};
    uint8_t recv_buf[kEthFrameLen + 64]{};
    struct sockaddr_ll peek_from{};
    struct sockaddr_ll recv_from{};
    socklen_t peek_len = sizeof(peek_from);
    socklen_t recv_len = sizeof(recv_from);
    ssize_t peeked = recvfrom(fd.Get(), peek_buf, sizeof(peek_buf), MSG_PEEK | MSG_DONTWAIT,
                              reinterpret_cast<struct sockaddr*>(&peek_from), &peek_len);
    ASSERT_GT(peeked, 0) << ErrnoString(errno);
    ssize_t received = recvfrom(fd.Get(), recv_buf, sizeof(recv_buf), MSG_DONTWAIT,
                                reinterpret_cast<struct sockaddr*>(&recv_from), &recv_len);
    ASSERT_EQ(received, peeked) << ErrnoString(errno);
    EXPECT_EQ(std::memcmp(peek_buf, recv_buf, static_cast<size_t>(received)), 0);
    EXPECT_EQ(peek_from.sll_protocol, recv_from.sll_protocol);
    EXPECT_EQ(peek_from.sll_pkttype, recv_from.sll_pkttype);
}

TEST(AfPacketE2E, VethAcceptsFullMtuVlanFrame) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "veth1 is unavailable";
    }

    FdGuard fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint8_t local_mac[6];
    GetIfHwaddr(fd.Get(), ifname, local_mac);

    uint8_t frame[kVlanFrameLen]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = 0x08;
    frame[13] = 0x00;

    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(0x8100);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);

    errno = 0;
    EXPECT_EQ(sendto(fd.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst)),
              -1);
    EXPECT_EQ(errno, EMSGSIZE);

    frame[12] = 0x81;
    frame[13] = 0x00;
    frame[14] = 0x00;
    frame[15] = 0x01;
    frame[16] = 0x08;
    frame[17] = 0x00;

    EXPECT_EQ(sendto(fd.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst)),
              static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);
}

TEST(AfPacketE2E, OutgoingRawFilterPreservesInlineVlanHeader) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic outgoing VLAN testing";

    uint8_t local_mac[6];
    FdGuard receiver(MakeBoundRaw(ifname, ifindex, local_mac));
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    TestSockFilter outgoing_vlan[] = {
        {0x20, 0, 0, 0xfffff004U},  // LD W ABS SKF_AD_OFF + SKF_AD_PKTTYPE
        {0x15, 0, 3, PACKET_OUTGOING},
        {0x28, 0, 0, 12},  // LD H ABS Ethernet EtherType
        {0x15, 0, 1, kVlanEtherType},
        {0x06, 0, 0, 0xffffffffU},
        {0x06, 0, 0, 0},
    };
    ASSERT_EQ(AttachFilter(receiver.Get(), outgoing_vlan, 6), 0) << ErrnoString(errno);

    uint8_t frame[96]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kVlanEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kVlanEtherType);
    frame[14] = 0x00;
    frame[15] = 0x2a;
    frame[16] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[17] = static_cast<uint8_t>(kPrivateEtherType);
    std::memcpy(frame + 18, "outgoing-vlan", 13);

    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kVlanEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);
    ASSERT_EQ(sendto(sender.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<sockaddr*>(&dst), sizeof(dst)),
              static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);

    uint8_t received[sizeof(frame)]{};
    struct sockaddr_ll from{};
    socklen_t from_len = sizeof(from);
    ssize_t n = recvfrom(receiver.Get(), received, sizeof(received), 0,
                         reinterpret_cast<sockaddr*>(&from), &from_len);
    ASSERT_EQ(n, static_cast<ssize_t>(sizeof(frame))) << ErrnoString(errno);
    EXPECT_EQ(from.sll_pkttype, PACKET_OUTGOING);
    EXPECT_EQ(ntohs(from.sll_protocol), kVlanEtherType);
    EXPECT_EQ(std::memcmp(received, frame, sizeof(frame)), 0);
}

TEST(AfPacketE2E, ReceiveBufferSizeControlsQueuedBytes) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic AF_PACKET queue testing";

    auto make_receiver = [ifindex]() {
        int fd = socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType));
        if (fd < 0) return fd;
        struct sockaddr_ll bind_addr{};
        bind_addr.sll_family = AF_PACKET;
        bind_addr.sll_protocol = htons(kPrivateEtherType);
        bind_addr.sll_ifindex = ifindex;
        if (bind(fd, reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)) < 0) {
            close(fd);
            return -1;
        }
        return fd;
    };

    FdGuard small(make_receiver());
    FdGuard large(make_receiver());
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    ASSERT_GE(small.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(large.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    int small_request = 0;
    int large_request = 100000;
    ASSERT_EQ(setsockopt(small.Get(), SOL_SOCKET, SO_RCVBUF, &small_request,
                         sizeof(small_request)),
              0);
    ASSERT_EQ(setsockopt(large.Get(), SOL_SOCKET, SO_RCVBUF, &large_request,
                         sizeof(large_request)),
              0);

    uint8_t local_mac[6];
    GetIfHwaddr(sender.Get(), ifname, local_mac);
    uint8_t frame[512]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kPrivateEtherType);

    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kPrivateEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);

    int sent = 0;
    for (int i = 0; i < 128; ++i) {
        if (sendto(sender.Get(), frame, sizeof(frame), 0,
                   reinterpret_cast<sockaddr*>(&dst), sizeof(dst)) !=
            static_cast<ssize_t>(sizeof(frame))) {
            break;
        }
        ++sent;
    }
    ASSERT_GT(sent, 16) << ErrnoString(errno);

    auto drain = [](int fd) {
        uint8_t buffer[512];
        int count = 0;
        while (recv(fd, buffer, sizeof(buffer), MSG_DONTWAIT) > 0) ++count;
        return count;
    };
    int small_count = drain(small.Get());
    int large_count = drain(large.Get());
    EXPECT_GT(small_count, 0);
    EXPECT_GT(large_count, small_count)
        << "sent=" << sent << " small=" << small_count << " large=" << large_count;
}

TEST(AfPacketE2E, ClassicFilterSnaplenAndRuntimeErrorsAreFailClosed) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic cBPF testing";

    auto make_receiver = [ifindex]() {
        int fd = socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType));
        if (fd < 0) return fd;
        struct sockaddr_ll bind_addr{};
        bind_addr.sll_family = AF_PACKET;
        bind_addr.sll_protocol = htons(kPrivateEtherType);
        bind_addr.sll_ifindex = ifindex;
        if (bind(fd, reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)) < 0) {
            close(fd);
            return -1;
        }
        struct timeval tv{1, 0};
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
        return fd;
    };

    FdGuard receiver(make_receiver());
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(sender.Get(), ifname, local_mac);
    uint8_t frame[96]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kPrivateEtherType);
    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kPrivateEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);

    auto send_one = [&]() {
        ASSERT_EQ(sendto(sender.Get(), frame, sizeof(frame), 0,
                         reinterpret_cast<sockaddr*>(&dst), sizeof(dst)),
                  static_cast<ssize_t>(sizeof(frame)))
            << ErrnoString(errno);
    };

    TestSockFilter snap_to_eight[] = {{0x06, 0, 0, 8}};  // RET #8
    ASSERT_EQ(AttachFilter(receiver.Get(), snap_to_eight, 1), 0) << ErrnoString(errno);
    send_one();
    uint8_t buffer[128]{};
    EXPECT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), MSG_TRUNC), 8)
        << "MSG_TRUNC must report filtered snaplen";

    TestSockFilter oob_then_accept[] = {
        {0x20, 0, 0, 0x7fffffffU},  // LD W ABS, runtime OOB
        {0x06, 0, 0, 0xffffffffU},
    };
    ASSERT_EQ(AttachFilter(receiver.Get(), oob_then_accept, 2), 0) << ErrnoString(errno);
    send_one();
    errno = 0;
    EXPECT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), 0), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << ErrnoString(errno);

    TestSockFilter divide_by_zero_then_accept[] = {
        {0x01, 0, 0, 0},           // LDX IMM #0
        {0x3c, 0, 0, 0},           // DIV X
        {0x06, 0, 0, 0xffffffffU},
    };
    ASSERT_EQ(AttachFilter(receiver.Get(), divide_by_zero_then_accept, 3), 0)
        << ErrnoString(errno);
    send_one();
    errno = 0;
    EXPECT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), 0), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << ErrnoString(errno);

    FdGuard arp_receiver(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ARP)));
    ASSERT_GE(arp_receiver.Get(), 0) << ErrnoString(errno);
    struct sockaddr_ll arp_bind{};
    arp_bind.sll_family = AF_PACKET;
    arp_bind.sll_protocol = htons(ETH_P_ARP);
    arp_bind.sll_ifindex = ifindex;
    ASSERT_EQ(bind(arp_receiver.Get(), reinterpret_cast<sockaddr*>(&arp_bind), sizeof(arp_bind)), 0)
        << ErrnoString(errno);
    struct timeval timeout{1, 0};
    ASSERT_EQ(setsockopt(arp_receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout,
                         sizeof(timeout)),
              0);
    TestSockFilter pay_offset[] = {
        {0x20, 0, 0, 0xfffff034U},  // LD W ABS SKF_AD_OFF + SKF_AD_PAY_OFFSET
        {0x16, 0, 0, 0},            // RET A
    };
    ASSERT_EQ(AttachFilter(arp_receiver.Get(), pay_offset, 2), 0) << ErrnoString(errno);
    frame[12] = static_cast<uint8_t>(ETH_P_ARP >> 8);
    frame[13] = static_cast<uint8_t>(ETH_P_ARP);
    dst.sll_protocol = htons(ETH_P_ARP);
    ASSERT_EQ(sendto(sender.Get(), frame, sizeof(frame), 0,
                     reinterpret_cast<sockaddr*>(&dst), sizeof(dst)),
              static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);
    EXPECT_EQ(recv(arp_receiver.Get(), buffer, sizeof(buffer), MSG_TRUNC), kEthHdrLen)
        << "Linux skb_get_poff returns the network offset for ARP";
}

TEST(AfPacketE2E, FilterReplacementPreservesReceiveState) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic cBPF testing";

    FdGuard receiver(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    struct sockaddr_ll bind_addr{};
    bind_addr.sll_family = AF_PACKET;
    bind_addr.sll_protocol = htons(kPrivateEtherType);
    bind_addr.sll_ifindex = ifindex;
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)), 0)
        << ErrnoString(errno);
    struct timeval timeout{1, 0};
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(sender.Get(), ifname, local_mac);
    uint8_t frame[96]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kPrivateEtherType);
    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kPrivateEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);

    auto send_one = [&]() {
        return sendto(sender.Get(), frame, sizeof(frame), 0,
                      reinterpret_cast<sockaddr*>(&dst), sizeof(dst));
    };
    TestSockFilter accept_all[] = {{0x06, 0, 0, 0xffffffffU}};
    ASSERT_EQ(AttachFilter(receiver.Get(), accept_all, 1), 0) << ErrnoString(errno);
    ASSERT_EQ(send_one(), static_cast<ssize_t>(sizeof(frame))) << ErrnoString(errno);
    uint8_t buffer[128]{};
    ASSERT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), 0), static_cast<ssize_t>(sizeof(frame)))
        << ErrnoString(errno);

    FilterSwapStress stress{receiver.Get()};
    pthread_t control{};
    ASSERT_EQ(pthread_create(&control, nullptr, ReplaceAndDetachFilters, &stress), 0);
    ASSERT_EQ(pthread_join(control, nullptr), 0);
    EXPECT_EQ(stress.error, 0) << ErrnoString(stress.error);

    ASSERT_EQ(AttachFilter(receiver.Get(), accept_all, 1), 0) << ErrnoString(errno);
    ASSERT_EQ(send_one(), static_cast<ssize_t>(sizeof(frame))) << ErrnoString(errno);
    ASSERT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), 0), static_cast<ssize_t>(sizeof(frame)))
        << "filter control path must remain usable after the stress phase: "
        << ErrnoString(errno);

    int ignored = 0;
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, kSoDetachFilter, &ignored, sizeof(ignored)), 0)
        << ErrnoString(errno);
    ASSERT_EQ(send_one(), static_cast<ssize_t>(sizeof(frame))) << ErrnoString(errno);
    EXPECT_EQ(recv(receiver.Get(), buffer, sizeof(buffer), 0), static_cast<ssize_t>(sizeof(frame)))
        << "detached socket must continue receiving after the stress phase: "
        << ErrnoString(errno);
}

TEST(AfPacketE2E, FanoutLbDeliversExactlyOneCopyPerFrame) {
    const std::string ifname = "veth1";
    int ifindex = ProbeIfindex(ifname);
    ASSERT_GE(ifindex, 0) << "veth1 must exist for deterministic fanout testing";

    auto make_receiver = [ifindex]() {
        int fd = socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType));
        if (fd < 0) return fd;
        struct sockaddr_ll addr{};
        addr.sll_family = AF_PACKET;
        addr.sll_protocol = htons(kPrivateEtherType);
        addr.sll_ifindex = ifindex;
        if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
            close(fd);
            return -1;
        }
        return fd;
    };

    FdGuard first(make_receiver());
    FdGuard second(make_receiver());
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType)));
    ASSERT_GE(first.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(second.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);

    int fanout = (PACKET_FANOUT_LB << 16) | 0x5a31;
    ASSERT_EQ(setsockopt(first.Get(), SOL_PACKET, PACKET_FANOUT, &fanout, sizeof(fanout)), 0)
        << ErrnoString(errno);
    ASSERT_EQ(setsockopt(second.Get(), SOL_PACKET, PACKET_FANOUT, &fanout, sizeof(fanout)), 0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(sender.Get(), ifname, local_mac);
    uint8_t frame[96]{};
    std::memset(frame, 0xff, 6);
    std::memcpy(frame + 6, local_mac, 6);
    frame[12] = static_cast<uint8_t>(kPrivateEtherType >> 8);
    frame[13] = static_cast<uint8_t>(kPrivateEtherType);

    struct sockaddr_ll dst{};
    dst.sll_family = AF_PACKET;
    dst.sll_protocol = htons(kPrivateEtherType);
    dst.sll_ifindex = ifindex;
    dst.sll_hatype = ARPHRD_ETHER;
    dst.sll_halen = ETH_ALEN;
    std::memset(dst.sll_addr, 0xff, ETH_ALEN);

    constexpr int kFrames = 16;
    for (int sequence = 0; sequence < kFrames; ++sequence) {
        std::memcpy(frame + kEthHdrLen, &sequence, sizeof(sequence));
        ASSERT_EQ(sendto(sender.Get(), frame, sizeof(frame), 0,
                         reinterpret_cast<sockaddr*>(&dst), sizeof(dst)),
                  static_cast<ssize_t>(sizeof(frame)))
            << ErrnoString(errno);
    }

    auto drain = [](int fd) {
        uint8_t buffer[128];
        int count = 0;
        while (recv(fd, buffer, sizeof(buffer), MSG_DONTWAIT) > 0) ++count;
        return count;
    };
    const int first_count = drain(first.Get());
    const int second_count = drain(second.Get());
    EXPECT_EQ(first_count + second_count, kFrames)
        << "fanout must not broadcast or drop: first=" << first_count
        << " second=" << second_count;
    EXPECT_EQ(first_count, kFrames / 2);
    EXPECT_EQ(second_count, kFrames / 2);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
