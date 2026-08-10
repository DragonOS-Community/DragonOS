// AF_PACKET mmap ring lifetime and allocation regression tests.

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netpacket/packet.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>
#include <utility>

#ifndef AF_PACKET
#define AF_PACKET 17
#endif
#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif
#ifndef PACKET_VERSION
#define PACKET_VERSION 10
#endif
#ifndef PACKET_RESERVE
#define PACKET_RESERVE 12
#endif
#ifndef PACKET_RX_RING
#define PACKET_RX_RING 5
#endif
#ifndef TPACKET_V1
#define TPACKET_V1 0
#endif
#ifndef TPACKET_V2
#define TPACKET_V2 1
#endif
#ifndef TP_STATUS_KERNEL
#define TP_STATUS_KERNEL 0
#endif
#ifndef TP_STATUS_USER
#define TP_STATUS_USER 1
#endif
#ifndef PACKET_STATISTICS
#define PACKET_STATISTICS 6
#endif
#ifndef PACKET_OUTGOING
#define PACKET_OUTGOING 4
#endif

namespace {

constexpr int kEthPAll = 0x0003;
constexpr uint16_t kPrivateEtherType = 0x88b5;
constexpr uint16_t kVlanEtherType = 0x8100;
constexpr size_t kTestFrameSize = 96;

struct TpacketHdrV1 {
    uint64_t status;
    uint32_t len;
    uint32_t snaplen;
    uint16_t mac;
    uint16_t net;
    uint32_t sec;
    uint32_t usec;
};

struct TpacketHdrV2 {
    uint32_t status;
    uint32_t len;
    uint32_t snaplen;
    uint16_t mac;
    uint16_t net;
    uint32_t sec;
    uint32_t nsec;
    uint16_t vlan_tci;
    uint16_t vlan_tpid;
    uint8_t padding[4];
};

struct TpacketStats {
    uint32_t packets;
    uint32_t drops;
};

struct TpacketSockaddrLl {
    uint16_t family;
    uint16_t protocol;
    int32_t ifindex;
    uint16_t hatype;
    uint8_t pkttype;
    uint8_t halen;
    uint8_t address[8];
};

static_assert(sizeof(TpacketHdrV1) == 32);
static_assert(sizeof(TpacketHdrV2) == 32);
static_assert(sizeof(TpacketSockaddrLl) == 20);

struct TpacketReq {
    uint32_t block_size;
    uint32_t block_nr;
    uint32_t frame_size;
    uint32_t frame_nr;
};

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

class MappingGuard {
  public:
    MappingGuard(void* address, size_t length) : address_(address), length_(length) {}
    MappingGuard(const MappingGuard&) = delete;
    MappingGuard& operator=(const MappingGuard&) = delete;
    ~MappingGuard() {
        if (address_ != MAP_FAILED) munmap(address_, length_);
    }
    void* Get() const { return address_; }
    int Unmap() {
        if (address_ == MAP_FAILED) return 0;
        int result = munmap(address_, length_);
        if (result == 0) address_ = MAP_FAILED;
        return result;
    }

  private:
    void* address_;
    size_t length_;
};

std::string Error() {
    return std::to_string(errno) + " (" + std::strerror(errno) + ")";
}

int MakeRingSocket() {
    return socket(AF_PACKET, SOCK_RAW, htons(kEthPAll));
}

TpacketReq Request(size_t page_size, uint32_t pages) {
    return TpacketReq{static_cast<uint32_t>(page_size), pages,
                      static_cast<uint32_t>(page_size), pages};
}

int Configure(int fd, const TpacketReq& request) {
    return setsockopt(fd, SOL_PACKET, PACKET_RX_RING, &request, sizeof(request));
}

int Teardown(int fd) {
    const TpacketReq empty{};
    return Configure(fd, empty);
}

void* MapRing(int fd, size_t length, int prot = PROT_READ | PROT_WRITE,
              int flags = MAP_SHARED) {
    return mmap(nullptr, length, prot, flags, fd, 0);
}

int InterfaceIndex(int fd, const char* name) {
    struct ifreq request {};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd, SIOCGIFINDEX, &request) < 0) return -1;
    return request.ifr_ifindex;
}

void RunReceiveDataPath(int version, const char* interface, uint16_t hardware_type,
                        int receiver_type = SOCK_RAW,
                        uint16_t outer_protocol = kPrivateEtherType,
                        uint16_t inner_protocol = kPrivateEtherType) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    ASSERT_GE(page, 4096U);
    const uint32_t frame_size = static_cast<uint32_t>(page / 2);
    const TpacketReq request{static_cast<uint32_t>(page), 1, frame_size, 2};

    FdGuard receiver(socket(AF_PACKET, receiver_type, htons(outer_protocol)));
    FdGuard sender(socket(AF_PACKET, SOCK_RAW, htons(outer_protocol)));
    ASSERT_GE(receiver.Get(), 0) << Error();
    ASSERT_GE(sender.Get(), 0) << Error();

    const int ifindex = InterfaceIndex(receiver.Get(), interface);
    ASSERT_GE(ifindex, 0) << interface << " must exist for deterministic TPACKET RX testing";
    struct sockaddr_ll bind_address {};
    bind_address.sll_family = AF_PACKET;
    bind_address.sll_protocol = htons(outer_protocol);
    bind_address.sll_ifindex = ifindex;
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&bind_address),
                   sizeof(bind_address)),
              0)
        << Error();

    ASSERT_EQ(setsockopt(receiver.Get(), SOL_PACKET, PACKET_VERSION, &version,
                         sizeof(version)),
              0)
        << Error();
    ASSERT_EQ(Configure(receiver.Get(), request), 0) << Error();
    MappingGuard mapping(MapRing(receiver.Get(), page), page);
    ASSERT_NE(mapping.Get(), MAP_FAILED) << Error();

    uint8_t sent[kTestFrameSize]{};
    std::memset(sent, 0xff, 6);
    const uint8_t source[6] = {0x02, 0x00, 0x00, 0x20, 0x96, 0x01};
    std::memcpy(sent + 6, source, sizeof(source));
    sent[12] = static_cast<uint8_t>(outer_protocol >> 8);
    sent[13] = static_cast<uint8_t>(outer_protocol);
    const bool vlan = outer_protocol != inner_protocol;
    const size_t network_offset = vlan ? 18 : 14;
    if (vlan) {
        sent[14] = 0;
        sent[15] = 7;
        sent[16] = static_cast<uint8_t>(inner_protocol >> 8);
        sent[17] = static_cast<uint8_t>(inner_protocol);
    }
    std::memcpy(sent + network_offset, "DragonOS TPACKET dunitest", 25);

    struct sockaddr_ll destination {};
    destination.sll_family = AF_PACKET;
    destination.sll_protocol = htons(outer_protocol);
    destination.sll_ifindex = ifindex;
    // This is intentionally unrelated to the expected receive metadata. The
    // ring must source sll_hatype from the ingress interface, not from sendto.
    destination.sll_hatype = 0;
    destination.sll_halen = 6;
    std::memset(destination.sll_addr, 0xff, 6);
    ASSERT_EQ(sendto(sender.Get(), sent, sizeof(sent), 0,
                     reinterpret_cast<sockaddr*>(&destination), sizeof(destination)),
              static_cast<ssize_t>(sizeof(sent)))
        << Error();

    struct pollfd poll_fd { receiver.Get(), POLLIN, 0 };
    ASSERT_EQ(poll(&poll_fd, 1, 3000), 1) << Error();
    ASSERT_NE(poll_fd.revents & POLLIN, 0);

    bool matched = false;
    auto* ring = static_cast<uint8_t*>(mapping.Get());
    for (uint32_t frame_index = 0; frame_index < request.frame_nr; ++frame_index) {
        uint8_t* frame = ring + frame_index * frame_size;
        uint64_t status;
        uint32_t packet_len;
        uint32_t snaplen;
        uint16_t mac;
        uint16_t net;
        if (version == TPACKET_V1) {
            auto* header = reinterpret_cast<TpacketHdrV1*>(frame);
            status = __atomic_load_n(&header->status, __ATOMIC_ACQUIRE);
            packet_len = header->len;
            snaplen = header->snaplen;
            mac = header->mac;
            net = header->net;
        } else {
            auto* header = reinterpret_cast<TpacketHdrV2*>(frame);
            status = __atomic_load_n(&header->status, __ATOMIC_ACQUIRE);
            packet_len = header->len;
            snaplen = header->snaplen;
            mac = header->mac;
            net = header->net;
        }
        if ((status & TP_STATUS_USER) == 0) continue;

        const bool bounds_valid = mac <= frame_size && snaplen <= frame_size - mac;
        EXPECT_TRUE(bounds_valid) << "version=" << version << " mac=" << mac
                                  << " snaplen=" << snaplen;
        const size_t visible_offset = receiver_type == SOCK_DGRAM ? network_offset : 0;
        const size_t visible_len = sizeof(sent) - visible_offset;
        EXPECT_EQ(packet_len, visible_len);
        EXPECT_EQ(snaplen, visible_len);
        if (receiver_type == SOCK_DGRAM) {
            EXPECT_EQ(net, mac);
        } else {
            EXPECT_GE(net, static_cast<uint16_t>(mac + network_offset));
        }
        auto* link_address = reinterpret_cast<TpacketSockaddrLl*>(frame + 32);
        EXPECT_EQ(link_address->family, AF_PACKET);
        EXPECT_EQ(link_address->protocol, htons(inner_protocol));
        EXPECT_EQ(link_address->ifindex, ifindex);
        EXPECT_EQ(link_address->hatype, hardware_type);
        if (vlan && receiver_type == SOCK_DGRAM) {
            EXPECT_EQ(link_address->pkttype, PACKET_OUTGOING);
        }
        EXPECT_EQ(link_address->halen, 6);
        if (bounds_valid && snaplen >= visible_len &&
            std::memcmp(frame + mac, sent + visible_offset, visible_len) == 0) {
            matched = true;
        }

        if (version == TPACKET_V1) {
            __atomic_store_n(&reinterpret_cast<TpacketHdrV1*>(frame)->status,
                             static_cast<uint64_t>(TP_STATUS_KERNEL), __ATOMIC_RELEASE);
        } else {
            __atomic_store_n(&reinterpret_cast<TpacketHdrV2*>(frame)->status,
                             static_cast<uint32_t>(TP_STATUS_KERNEL), __ATOMIC_RELEASE);
        }
    }
    EXPECT_TRUE(matched) << "TPACKET version " << version << " did not contain the sent frame";

    TpacketStats statistics{};
    socklen_t statistics_len = sizeof(statistics);
    ASSERT_EQ(getsockopt(receiver.Get(), SOL_PACKET, PACKET_STATISTICS, &statistics,
                         &statistics_len),
              0)
        << Error();
    EXPECT_EQ(statistics_len, sizeof(statistics));
    EXPECT_GE(statistics.packets, 1U);

    ASSERT_EQ(mapping.Unmap(), 0) << Error();
    EXPECT_EQ(Teardown(receiver.Get()), 0) << Error();
}

}  // namespace

TEST(AfPacketRing, V1ReceiveDataPath) {
    RunReceiveDataPath(TPACKET_V1, "veth1", ARPHRD_ETHER);
}

TEST(AfPacketRing, V2ReceiveDataPath) {
    RunReceiveDataPath(TPACKET_V2, "veth1", ARPHRD_ETHER);
}

TEST(AfPacketRing, LoopbackReportsNativeHardwareType) {
    RunReceiveDataPath(TPACKET_V2, "lo", ARPHRD_LOOPBACK);
}

TEST(AfPacketRing, DgramOutgoingVlanReportsInnerProtocol) {
    RunReceiveDataPath(TPACKET_V2, "veth1", ARPHRD_ETHER, SOCK_DGRAM, kVlanEtherType,
                       kPrivateEtherType);
}

TEST(AfPacketRing, RingRequestValidatesLengthBeforePointer) {
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    for (socklen_t len = 0; len < sizeof(TpacketReq); ++len) {
        errno = 0;
        EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_RX_RING,
                             reinterpret_cast<const void*>(1), len),
                  -1);
        EXPECT_EQ(errno, EINVAL) << "len=" << len;
    }

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_RX_RING,
                         reinterpret_cast<const void*>(1), sizeof(TpacketReq)),
              -1);
    EXPECT_EQ(errno, EINVAL) << "Linux maps a tpacket_req copy fault to EINVAL";
}

TEST(AfPacketRing, PartialMiddleMunmapKeepsBackingBusy) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 3)), 0) << Error();
    auto* mapping = static_cast<uint8_t*>(MapRing(fd.Get(), page * 3));
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    ASSERT_EQ(munmap(mapping + page, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();

    ASSERT_EQ(munmap(mapping, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(mapping + page * 2, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, MetadataSplitsKeepAccountingBalanced) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 5)), 0) << Error();
    auto* mapping = static_cast<uint8_t*>(MapRing(fd.Get(), page * 5));
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    ASSERT_EQ(mprotect(mapping + page, page, PROT_READ), 0) << Error();
#ifdef MADV_DONTFORK
    // mprotect leaves a three-page tail; advising its middle page forces an
    // independent before/middle/after split instead of touching a whole VMA.
    ASSERT_EQ(madvise(mapping + page * 3, page, MADV_DONTFORK), 0) << Error();
#endif
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(mapping, page * 5), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, ForkedMappingKeepsBackingBusy) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* mapping = MapRing(fd.Get(), page);
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    int release_pipe[2];
    int done_pipe[2];
    ASSERT_EQ(pipe(release_pipe), 0);
    ASSERT_EQ(pipe(done_pipe), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << Error();
    if (child == 0) {
        close(release_pipe[1]);
        close(done_pipe[0]);
        char byte;
        if (read(release_pipe[0], &byte, 1) != 1 || munmap(mapping, page) != 0) _exit(2);
        if (write(done_pipe[1], "x", 1) != 1) _exit(3);
        _exit(0);
    }
    close(release_pipe[0]);
    close(done_pipe[1]);

    ASSERT_EQ(munmap(mapping, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(write(release_pipe[1], "x", 1), 1);
    char byte;
    ASSERT_EQ(read(done_pipe[0], &byte, 1), 1);
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    close(release_pipe[1]);
    close(done_pipe[0]);
}

TEST(AfPacketRing, MremapMoveTransfersOneVmaReference) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* source = MapRing(fd.Get(), page);
    ASSERT_NE(source, MAP_FAILED) << Error();
    void* target = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(target, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(target, page), 0);

    void* moved = mremap(source, page, page, MREMAP_MAYMOVE | MREMAP_FIXED, target);
    ASSERT_EQ(moved, target) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(moved, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

#ifdef MREMAP_DONTUNMAP
TEST(AfPacketRing, MremapDontunmapKeepsBothVmaReferences) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* source = MapRing(fd.Get(), page);
    ASSERT_NE(source, MAP_FAILED) << Error();
    void* target = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(target, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(target, page), 0);

    void* duplicate = mremap(source, page, page,
                             MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP, target);
    ASSERT_EQ(duplicate, target) << Error();
    ASSERT_EQ(munmap(source, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(duplicate, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}
#endif

TEST(AfPacketRing, ReadOnlyAndPrivateMappingsCloseExactlyOnce) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    for (auto [prot, flags] : {
             std::pair{PROT_READ, MAP_SHARED},
             std::pair{PROT_READ | PROT_WRITE, MAP_PRIVATE},
         }) {
        FdGuard fd(MakeRingSocket());
        ASSERT_GE(fd.Get(), 0) << Error();
        ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
        void* mapping = MapRing(fd.Get(), page, prot, flags);
        ASSERT_NE(mapping, MAP_FAILED) << "flags=" << flags << ": " << Error();
        ASSERT_EQ(munmap(mapping, page), 0) << Error();
        EXPECT_EQ(Teardown(fd.Get()), 0) << "flags=" << flags << ": " << Error();
    }
}

TEST(AfPacketRing, ReserveCannotChangeAfterRingCreation) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    uint32_t reserve = 16;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_RESERVE, &reserve, sizeof(reserve)), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, NonPowerOfTwoBlockExercisesBuddySurplusRelease) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    const TpacketReq request{static_cast<uint32_t>(page * 3), 1,
                             static_cast<uint32_t>(page), 3};
    ASSERT_EQ(Configure(fd.Get(), request), 0) << Error();
    void* mapping = MapRing(fd.Get(), page * 3);
    ASSERT_NE(mapping, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(mapping, page * 3), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, HugeBlockVectorReturnsEnomemWithoutPanic) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    const TpacketReq huge{static_cast<uint32_t>(page), UINT32_MAX,
                          static_cast<uint32_t>(page), UINT32_MAX};
    errno = 0;
    EXPECT_EQ(Configure(fd.Get(), huge), -1);
    EXPECT_EQ(errno, ENOMEM) << Error();
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
