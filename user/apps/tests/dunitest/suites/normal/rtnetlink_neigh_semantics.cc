#include "rtnetlink_route_test_support.h"

#include <linux/neighbour.h>

#include <algorithm>
#include <array>
#include <fstream>
#include <limits>
#include <sstream>

namespace {

constexpr std::array<uint8_t, ETH_ALEN> kMacA = {0x02, 0x22, 0x33, 0x44, 0x55, 0x61};
constexpr std::array<uint8_t, ETH_ALEN> kMacB = {0x02, 0x22, 0x33, 0x44, 0x55, 0x62};

struct NeighborAddress {
    int family = AF_UNSPEC;
    std::array<uint8_t, sizeof(in6_addr)> bytes{};
    size_t length = 0;
};

struct NeighborSpec {
    NeighborAddress destination;
    uint32_t ifindex = 0;
    uint16_t state = NUD_PERMANENT;
    uint8_t flags = 0;
    uint8_t kind = RTN_UNICAST;
    std::optional<std::array<uint8_t, ETH_ALEN>> lladdr;
};

struct DumpedNeighbor {
    NeighborSpec spec;
    uint16_t message_flags = 0;
    bool has_cacheinfo = false;
    bool has_probes = false;
};

struct NeighborRequestOptions {
    bool include_destination = true;
    std::optional<uint32_t> ifindex_attr;
    const NeighborAddress* first_destination = nullptr;
    std::optional<size_t> destination_length;
    std::optional<size_t> lladdr_length;
    std::optional<uint32_t> flags_ext;
    std::optional<size_t> flags_ext_length;
    std::optional<uint8_t> protocol;
    bool include_unnested_fdb_ext = false;
    bool include_state_mask = false;
};

NeighborAddress ParseNeighborAddress(int family, const char* text) {
    NeighborAddress address{};
    address.family = family;
    address.length = family == AF_INET ? sizeof(in_addr) : sizeof(in6_addr);
    EXPECT_EQ(inet_pton(family, text, address.bytes.data()), 1) << text;
    return address;
}

bool SameAddress(const NeighborAddress& left, const NeighborAddress& right) {
    return left.family == right.family && left.length == right.length &&
           std::memcmp(left.bytes.data(), right.bytes.data(), left.length) == 0;
}

NeighborSpec MakeNeighbor(int family, const char* destination, uint32_t ifindex,
                          const std::array<uint8_t, ETH_ALEN>& mac = kMacA) {
    NeighborSpec spec{};
    spec.destination = ParseNeighborAddress(family, destination);
    spec.ifindex = ifindex;
    spec.lladdr = mac;
    return spec;
}

int SendNeighborRequest(int fd, uint16_t type, uint16_t flags, const NeighborSpec& spec,
                        uint32_t seq, const NeighborRequestOptions& options = {}) {
    alignas(nlmsghdr) char buffer[512] = {};
    auto* header = reinterpret_cast<nlmsghdr*>(buffer);
    auto* message = reinterpret_cast<ndmsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(ndmsg));
    header->nlmsg_type = type;
    header->nlmsg_flags = flags;
    header->nlmsg_seq = seq;
    message->ndm_family = static_cast<uint8_t>(spec.destination.family);
    message->ndm_ifindex = static_cast<int32_t>(spec.ifindex);
    message->ndm_state = spec.state;
    message->ndm_flags = spec.flags;
    message->ndm_type = spec.kind;

    if (options.first_destination != nullptr) {
        AddAttr(header, sizeof(buffer), NDA_DST, options.first_destination->bytes.data(),
                options.first_destination->length);
    }
    if (options.include_destination) {
        AddAttr(header, sizeof(buffer), NDA_DST, spec.destination.bytes.data(),
                options.destination_length.value_or(spec.destination.length));
    }
    if (spec.lladdr.has_value()) {
        AddAttr(header, sizeof(buffer), NDA_LLADDR, spec.lladdr->data(),
                options.lladdr_length.value_or(spec.lladdr->size()));
    }
    if (options.ifindex_attr.has_value()) {
        AddAttr(header, sizeof(buffer), NDA_IFINDEX, &*options.ifindex_attr,
                sizeof(*options.ifindex_attr));
    }
    if (options.flags_ext.has_value()) {
        const std::array<uint32_t, 2> values = {*options.flags_ext, 0};
        AddAttr(header, sizeof(buffer), NDA_FLAGS_EXT, values.data(),
                options.flags_ext_length.value_or(sizeof(*options.flags_ext)));
    }
    if (options.protocol.has_value()) {
        AddAttr(header, sizeof(buffer), NDA_PROTOCOL, &*options.protocol,
                sizeof(*options.protocol));
    }
    const uint8_t empty_payload = 0;
    if (options.include_unnested_fdb_ext) {
        AddAttr(header, sizeof(buffer), NDA_FDB_EXT_ATTRS, &empty_payload, 0);
    }
    if (options.include_state_mask) {
        const uint16_t state_mask = NUD_PERMANENT;
        AddAttr(header, sizeof(buffer), NDA_NDM_STATE_MASK, &state_mask, sizeof(state_mask));
    }
    if (send(fd, header, header->nlmsg_len, 0) != static_cast<ssize_t>(header->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

std::vector<DumpedNeighbor> DumpNeighbors(int fd, int family, uint32_t seq,
                                          int body_ifindex = 0,
                                          std::optional<uint32_t> filter_ifindex = std::nullopt,
                                          int* dump_error = nullptr,
                                          uint8_t body_flags = 0,
                                          size_t filter_length = sizeof(uint32_t)) {
    alignas(nlmsghdr) char request[256] = {};
    auto* header = reinterpret_cast<nlmsghdr*>(request);
    auto* message = reinterpret_cast<ndmsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(ndmsg));
    header->nlmsg_type = RTM_GETNEIGH;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    header->nlmsg_seq = seq;
    message->ndm_family = static_cast<uint8_t>(family);
    message->ndm_ifindex = body_ifindex;
    message->ndm_flags = body_flags;
    if (filter_ifindex.has_value()) {
        AddAttr(header, sizeof(request), NDA_IFINDEX, &*filter_ifindex, filter_length);
    }
    if (send(fd, header, header->nlmsg_len, 0) != static_cast<ssize_t>(header->nlmsg_len)) {
        if (dump_error != nullptr) *dump_error = errno;
        return {};
    }

    std::vector<DumpedNeighbor> neighbors;
    bool done = false;
    while (!done) {
        std::array<uint8_t, 8192> response{};
        ssize_t length = recv(fd, response.data(), response.size(), 0);
        if (length < 0) {
            if (dump_error != nullptr) *dump_error = errno;
            return neighbors;
        }
        int remaining = static_cast<int>(length);
        for (auto* reply = reinterpret_cast<nlmsghdr*>(response.data());
             NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
            if (reply->nlmsg_seq != seq) continue;
            if (reply->nlmsg_type == NLMSG_DONE) {
                done = true;
                break;
            }
            if (reply->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(reply));
                if (dump_error != nullptr) *dump_error = error->error == 0 ? 0 : -error->error;
                return neighbors;
            }
            if (reply->nlmsg_type != RTM_NEWNEIGH ||
                reply->nlmsg_len < NLMSG_LENGTH(sizeof(ndmsg))) {
                continue;
            }

            const auto* ndm = reinterpret_cast<const ndmsg*>(NLMSG_DATA(reply));
            DumpedNeighbor neighbor{};
            neighbor.spec.destination.family = ndm->ndm_family;
            neighbor.spec.destination.length =
                    ndm->ndm_family == AF_INET ? sizeof(in_addr) : sizeof(in6_addr);
            neighbor.spec.ifindex = static_cast<uint32_t>(ndm->ndm_ifindex);
            neighbor.spec.state = ndm->ndm_state;
            neighbor.spec.flags = ndm->ndm_flags;
            neighbor.spec.kind = ndm->ndm_type;
            neighbor.message_flags = reply->nlmsg_flags;
            int attr_length = static_cast<int>(reply->nlmsg_len - NLMSG_LENGTH(sizeof(ndmsg)));
            auto* attr = reinterpret_cast<const rtattr*>(
                    reinterpret_cast<const uint8_t*>(ndm) + NLMSG_ALIGN(sizeof(ndmsg)));
            for (; RTA_OK(attr, attr_length); attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type == NDA_DST &&
                    RTA_PAYLOAD(attr) >= neighbor.spec.destination.length) {
                    std::memcpy(neighbor.spec.destination.bytes.data(), RTA_DATA(attr),
                                neighbor.spec.destination.length);
                } else if (attr->rta_type == NDA_LLADDR && RTA_PAYLOAD(attr) >= ETH_ALEN) {
                    std::array<uint8_t, ETH_ALEN> mac{};
                    std::memcpy(mac.data(), RTA_DATA(attr), mac.size());
                    neighbor.spec.lladdr = mac;
                } else if (attr->rta_type == NDA_CACHEINFO &&
                           RTA_PAYLOAD(attr) == sizeof(nda_cacheinfo)) {
                    neighbor.has_cacheinfo = true;
                } else if (attr->rta_type == NDA_PROBES &&
                           RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    neighbor.has_probes = true;
                }
            }
            neighbors.push_back(neighbor);
        }
    }
    if (dump_error != nullptr) *dump_error = 0;
    return neighbors;
}

std::optional<DumpedNeighbor> FindNeighbor(int fd, const NeighborSpec& expected, uint32_t seq) {
    for (const auto& neighbor : DumpNeighbors(fd, expected.destination.family, seq)) {
        if (neighbor.spec.ifindex == expected.ifindex &&
            SameAddress(neighbor.spec.destination, expected.destination)) {
            return neighbor;
        }
    }
    return std::nullopt;
}

void DeleteNeighborIfPresent(int fd, const NeighborSpec& spec, uint32_t* seq) {
    (void)SendNeighborRequest(fd, RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec, ++*seq);
}

class NeighborCleanup {
  public:
    NeighborCleanup(int fd, uint32_t* sequence) : fd_(fd), sequence_(sequence) {}
    void Add(const NeighborSpec& spec) { specs_.push_back(spec); }
    ~NeighborCleanup() {
        for (const auto& spec : specs_) DeleteNeighborIfPresent(fd_, spec, sequence_);
    }

  private:
    int fd_;
    uint32_t* sequence_;
    std::vector<NeighborSpec> specs_;
};

class RtnetlinkNeighborSemantics : public testing::Test {
  protected:
    void SetUp() override {
        fd_.Reset(OpenRouteSocket());
        ASSERT_GE(fd_.Get(), 0) << ErrnoString(errno);
        veth1_ = if_nametoindex("veth1");
        veth2_ = if_nametoindex("veth2");
        loopback_ = if_nametoindex("lo");
        ASSERT_NE(veth1_, 0u) << "required network fixture veth1 is unavailable";
        ASSERT_NE(veth2_, 0u) << "required network fixture veth2 is unavailable";
        ASSERT_NE(loopback_, 0u) << "loopback interface is unavailable";
    }

    FdGuard fd_;
    uint32_t seq_ = 10000;
    uint32_t veth1_ = 0;
    uint32_t veth2_ = 0;
    uint32_t loopback_ = 0;
};

TEST_F(RtnetlinkNeighborSemantics, HeaderlessDeviceKeepsControlPlaneEntryWithoutEthernetMac) {
    NeighborSpec spec = MakeNeighbor(AF_INET, "198.18.230.10", loopback_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(spec);
    DeleteNeighborIfPresent(fd_.Get(), spec, &seq_);

    NeighborRequestOptions short_opaque_lladdr;
    short_opaque_lladdr.lladdr_length = 1;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  spec, ++seq_, short_opaque_lladdr),
              0);
    const auto dumped = FindNeighbor(fd_.Get(), spec, ++seq_);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_FALSE(dumped->spec.lladdr.has_value());
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  spec, ++seq_),
              0);
    EXPECT_FALSE(FindNeighbor(fd_.Get(), spec, ++seq_).has_value());
}

TEST_F(RtnetlinkNeighborSemantics, Ipv4AndIpv6CreateDumpDelete) {
    NeighborSpec ipv4 = MakeNeighbor(AF_INET, "198.18.230.11", veth1_);
    NeighborSpec ipv6 = MakeNeighbor(AF_INET6, "2001:db8:2233::11", veth1_, kMacB);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(ipv4);
    cleanup.Add(ipv6);
    DeleteNeighborIfPresent(fd_.Get(), ipv4, &seq_);
    DeleteNeighborIfPresent(fd_.Get(), ipv6, &seq_);

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  ipv4, ++seq_),
              0);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  ipv6, ++seq_),
              0);
    auto dumped4 = FindNeighbor(fd_.Get(), ipv4, ++seq_);
    auto dumped6 = FindNeighbor(fd_.Get(), ipv6, ++seq_);
    ASSERT_TRUE(dumped4.has_value());
    ASSERT_TRUE(dumped6.has_value());
    EXPECT_EQ(dumped4->spec.lladdr, ipv4.lladdr);
    EXPECT_EQ(dumped6->spec.lladdr, ipv6.lladdr);
    EXPECT_EQ(dumped4->spec.state, NUD_PERMANENT);
    EXPECT_EQ(dumped6->spec.kind, RTN_UNICAST);
    EXPECT_TRUE(dumped4->has_cacheinfo);
    EXPECT_TRUE(dumped4->has_probes);
    EXPECT_TRUE(dumped6->has_cacheinfo);
    EXPECT_TRUE(dumped6->has_probes);

    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, ipv4,
                                  ++seq_),
              0);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, ipv6,
                                  ++seq_),
              0);
    EXPECT_FALSE(FindNeighbor(fd_.Get(), ipv4, ++seq_).has_value());
    EXPECT_FALSE(FindNeighbor(fd_.Get(), ipv6, ++seq_).has_value());
}

TEST_F(RtnetlinkNeighborSemantics, CreateExclusiveReplaceAndFieldLevelUpdate) {
    NeighborSpec original = MakeNeighbor(AF_INET, "198.18.230.12", veth1_);
    original.flags = NTF_ROUTER;
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(original);
    DeleteNeighborIfPresent(fd_.Get(), original, &seq_);

    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  original, ++seq_),
              ENOENT);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  original, ++seq_),
              0);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  original, ++seq_),
              EEXIST);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_EXCL | NLM_F_REPLACE,
                                  original, ++seq_),
              EEXIST);

    NeighborSpec blocked = original;
    blocked.lladdr = kMacB;
    blocked.state = NUD_NOARP;
    blocked.flags = 0;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK, blocked,
                                  ++seq_),
              0);
    auto current = FindNeighbor(fd_.Get(), original, ++seq_);
    ASSERT_TRUE(current.has_value());
    EXPECT_EQ(current->spec.lladdr, original.lladdr);
    EXPECT_EQ(current->spec.state, NUD_PERMANENT);
    EXPECT_EQ(current->spec.flags, NTF_ROUTER);

    NeighborSpec state_only = original;
    state_only.state = NUD_NOARP;
    state_only.flags = 0;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  state_only, ++seq_),
              0);
    current = FindNeighbor(fd_.Get(), original, ++seq_);
    ASSERT_TRUE(current.has_value());
    EXPECT_EQ(current->spec.state, NUD_NOARP);
    EXPECT_EQ(current->spec.flags, NTF_ROUTER);

    NeighborSpec replace = blocked;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, replace, ++seq_),
              0);
    current = FindNeighbor(fd_.Get(), original, ++seq_);
    ASSERT_TRUE(current.has_value());
    EXPECT_EQ(current->spec.lladdr, kMacB);
    EXPECT_EQ(current->spec.state, NUD_NOARP);
    EXPECT_EQ(current->spec.flags, 0);

    replace.state = NUD_PERMANENT;
    replace.lladdr.reset();
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, replace, ++seq_),
              0);
    current = FindNeighbor(fd_.Get(), original, ++seq_);
    ASSERT_TRUE(current.has_value());
    EXPECT_EQ(current->spec.lladdr, kMacB);
    EXPECT_EQ(current->spec.state, NUD_PERMANENT);
}

TEST_F(RtnetlinkNeighborSemantics, MutationErrorsAndDuplicateAttributesMatchLinux) {
    NeighborSpec spec = MakeNeighbor(AF_INET, "198.18.230.13", veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(spec);
    DeleteNeighborIfPresent(fd_.Get(), spec, &seq_);

    NeighborSpec missing = spec;
    NeighborRequestOptions missing_destination;
    missing_destination.include_destination = false;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, missing, ++seq_,
                                  missing_destination),
              EINVAL);
    NeighborSpec zero = spec;
    zero.ifindex = 0;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, zero, ++seq_),
              EINVAL);
    NeighborSpec zero_unsupported_family = zero;
    zero_unsupported_family.destination.family = AF_PACKET;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
                                  zero_unsupported_family, ++seq_),
              EAFNOSUPPORT);
    NeighborSpec absent_device = spec;
    absent_device.ifindex = 0x7fffffff;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, absent_device,
                                  ++seq_),
              ENODEV);
    NeighborSpec negative_ifindex = spec;
    negative_ifindex.ifindex = std::numeric_limits<uint32_t>::max();
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
                                  negative_ifindex, ++seq_),
              ENODEV);
    NeighborSpec unsupported_family = spec;
    unsupported_family.destination.family = AF_PACKET;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, unsupported_family,
                                  ++seq_),
              EAFNOSUPPORT);
    NeighborSpec unsupported_state = spec;
    unsupported_state.state = NUD_REACHABLE;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, unsupported_state,
                                  ++seq_),
              EOPNOTSUPP);
    NeighborSpec unsupported_flags = spec;
    unsupported_flags.flags = NTF_PROXY;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, unsupported_flags,
                                  ++seq_),
              EOPNOTSUPP);
    NeighborRequestOptions managed;
    managed.flags_ext = NTF_EXT_MANAGED;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  managed),
              EOPNOTSUPP);
    NeighborRequestOptions protocol;
    protocol.protocol = 1;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  spec, ++seq_, protocol),
              ENOENT);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  protocol),
              EOPNOTSUPP);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  spec, ++seq_),
              0);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  spec, ++seq_, protocol),
              EEXIST);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  spec, ++seq_),
              0);
    NeighborRequestOptions oversized_flags_ext;
    oversized_flags_ext.flags_ext = 0;
    oversized_flags_ext.flags_ext_length = sizeof(uint64_t);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  oversized_flags_ext),
              EINVAL);
    NeighborRequestOptions unnested_fdb_ext;
    unnested_fdb_ext.include_unnested_fdb_ext = true;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  unnested_fdb_ext),
              EINVAL);
    NeighborRequestOptions state_mask;
    state_mask.include_state_mask = true;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  state_mask),
              EINVAL);
    EXPECT_FALSE(FindNeighbor(fd_.Get(), spec, ++seq_).has_value());

    NeighborRequestOptions short_destination;
    short_destination.destination_length = sizeof(in_addr) - 1;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  short_destination),
              EINVAL);
    NeighborRequestOptions short_lladdr;
    short_lladdr.lladdr_length = ETH_ALEN - 1;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE, spec, ++seq_,
                                  short_lladdr),
              EINVAL);

    const NeighborAddress ignored = ParseNeighborAddress(AF_INET, "198.18.230.99");
    NeighborRequestOptions duplicate_destination;
    duplicate_destination.first_destination = &ignored;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, spec,
                                  ++seq_, duplicate_destination),
              0);
    EXPECT_TRUE(FindNeighbor(fd_.Get(), spec, ++seq_).has_value());
    NeighborSpec first = spec;
    first.destination = ignored;
    EXPECT_FALSE(FindNeighbor(fd_.Get(), first, ++seq_).has_value());

    // Linux DEL uses nlmsg_find_attr() and therefore selects the first
    // duplicate NDA_DST, unlike NEW's last-attribute-wins policy.
    NeighborSpec delete_duplicate = spec;
    delete_duplicate.destination = ignored;
    NeighborRequestOptions delete_first;
    delete_first.first_destination = &spec.destination;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  delete_duplicate, ++seq_, delete_first),
              0);
    EXPECT_FALSE(FindNeighbor(fd_.Get(), spec, ++seq_).has_value());

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  spec, ++seq_),
              0);

    NeighborSpec normalized = MakeNeighbor(AF_INET, "198.18.230.19", veth1_, kMacB);
    normalized.kind = RTN_BLACKHOLE;
    cleanup.Add(normalized);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  normalized, ++seq_),
              0);
    auto normalized_dump = FindNeighbor(fd_.Get(), normalized, ++seq_);
    ASSERT_TRUE(normalized_dump.has_value());
    EXPECT_EQ(normalized_dump->spec.kind, RTN_UNICAST);

    NeighborSpec proxy_delete = spec;
    proxy_delete.flags = NTF_PROXY;
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  proxy_delete, ++seq_),
              EOPNOTSUPP);
    EXPECT_TRUE(FindNeighbor(fd_.Get(), spec, ++seq_).has_value());

    NeighborSpec delete_selector = spec;
    delete_selector.lladdr = kMacB;
    delete_selector.state = NUD_NOARP;
    NeighborRequestOptions ignored_on_delete;
    ignored_on_delete.protocol = 1;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  delete_selector, ++seq_, ignored_on_delete),
              0);
    EXPECT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK,
                                  delete_selector, ++seq_),
              ENOENT);
}

TEST_F(RtnetlinkNeighborSemantics, DumpFamilyIfindexAndRawBodyFilters) {
    NeighborSpec first = MakeNeighbor(AF_INET, "198.18.230.14", veth1_);
    NeighborSpec second = MakeNeighbor(AF_INET, "198.18.230.15", veth2_, kMacB);
    NeighborSpec ipv6 = MakeNeighbor(AF_INET6, "2001:db8:2233::14", veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(first);
    cleanup.Add(second);
    cleanup.Add(ipv6);
    DeleteNeighborIfPresent(fd_.Get(), first, &seq_);
    DeleteNeighborIfPresent(fd_.Get(), second, &seq_);
    DeleteNeighborIfPresent(fd_.Get(), ipv6, &seq_);
    for (const auto& spec : {first, second, ipv6}) {
        ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      spec, ++seq_),
                  0);
    }

    auto all = DumpNeighbors(fd_.Get(), AF_UNSPEC, ++seq_);
    EXPECT_TRUE(std::any_of(all.begin(), all.end(), [&](const auto& item) {
        return SameAddress(item.spec.destination, first.destination);
    }));
    EXPECT_TRUE(std::any_of(all.begin(), all.end(), [&](const auto& item) {
        return SameAddress(item.spec.destination, ipv6.destination);
    }));

    auto body_ignored = DumpNeighbors(fd_.Get(), AF_INET, ++seq_, static_cast<int>(veth2_));
    EXPECT_TRUE(std::any_of(body_ignored.begin(), body_ignored.end(), [&](const auto& item) {
        return SameAddress(item.spec.destination, first.destination);
    }));

    auto filtered = DumpNeighbors(fd_.Get(), AF_INET, ++seq_, 0, veth2_);
    ASSERT_TRUE(std::any_of(filtered.begin(), filtered.end(), [&](const auto& item) {
        return SameAddress(item.spec.destination, second.destination);
    }));
    EXPECT_FALSE(std::any_of(filtered.begin(), filtered.end(), [&](const auto& item) {
        return SameAddress(item.spec.destination, first.destination);
    }));
    for (const auto& item : filtered) {
        EXPECT_EQ(item.spec.ifindex, veth2_);
        EXPECT_NE(item.message_flags & NLM_F_DUMP_FILTERED, 0);
    }

    int dump_error = -1;
    const auto malformed_filter = DumpNeighbors(fd_.Get(), AF_INET, ++seq_, 0, veth2_,
                                                &dump_error, 0, sizeof(uint16_t));
    EXPECT_EQ(dump_error, 0);
    EXPECT_TRUE(std::any_of(malformed_filter.begin(), malformed_filter.end(),
                            [&](const auto& item) {
                                return SameAddress(item.spec.destination, first.destination);
                            }))
            << "a non-strict malformed filter must fall back to an unfiltered dump";

    dump_error = -1;
    const auto unsupported = DumpNeighbors(fd_.Get(), AF_PACKET, ++seq_, 0, std::nullopt,
                                           &dump_error);
    EXPECT_EQ(dump_error, 0);
    EXPECT_TRUE(unsupported.empty());

    const auto proxy = DumpNeighbors(fd_.Get(), AF_INET, ++seq_, 0, std::nullopt,
                                     &dump_error, NTF_PROXY);
    EXPECT_EQ(dump_error, EOPNOTSUPP);
    EXPECT_TRUE(proxy.empty());
}

TEST_F(RtnetlinkNeighborSemantics, ForkedNetworkNamespaceHasIndependentNeighborTable) {
    NeighborSpec parent = MakeNeighbor(AF_INET, "198.18.230.16", veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(parent);
    DeleteNeighborIfPresent(fd_.Get(), parent, &seq_);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  parent, ++seq_),
              0);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << ErrnoString(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) _exit(10);
        FdGuard child_fd(OpenRouteSocket());
        if (child_fd.Get() < 0) _exit(11);
        const auto entries = DumpNeighbors(child_fd.Get(), AF_UNSPEC, 1);
        const bool leaked = std::any_of(entries.begin(), entries.end(), [&](const auto& item) {
            return SameAddress(item.spec.destination, parent.destination);
        });
        _exit(leaked ? 12 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << ErrnoString(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0) << "child network namespace stage failed";
    EXPECT_TRUE(FindNeighbor(fd_.Get(), parent, ++seq_).has_value());
}

std::optional<DumpedNeighbor> ReceiveNeighborEvent(int fd, const NeighborAddress& destination,
                                                    uint16_t expected_type) {
    std::array<uint8_t, 4096> buffer{};
    // The mutation handler publishes multicast notifications before its ACK
    // reaches the request socket, so the event is already queued here. A
    // nonblocking read also makes the no-op assertion independent of socket
    // receive-timeout support.
    const ssize_t length = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT);
    if (length < 0) return std::nullopt;
    int remaining = static_cast<int>(length);
    for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data()); NLMSG_OK(header, remaining);
         header = NLMSG_NEXT(header, remaining)) {
        if (header->nlmsg_type != expected_type ||
            header->nlmsg_len < NLMSG_LENGTH(sizeof(ndmsg))) {
            continue;
        }
        const auto* message = reinterpret_cast<const ndmsg*>(NLMSG_DATA(header));
        DumpedNeighbor event{};
        event.spec.destination.family = message->ndm_family;
        event.spec.destination.length =
                message->ndm_family == AF_INET ? sizeof(in_addr) : sizeof(in6_addr);
        event.spec.ifindex = message->ndm_ifindex;
        event.spec.state = message->ndm_state;
        event.spec.flags = message->ndm_flags;
        int attr_length = header->nlmsg_len - NLMSG_LENGTH(sizeof(ndmsg));
        auto* attr = reinterpret_cast<const rtattr*>(
                reinterpret_cast<const uint8_t*>(message) + NLMSG_ALIGN(sizeof(ndmsg)));
        for (; RTA_OK(attr, attr_length); attr = RTA_NEXT(attr, attr_length)) {
            if (attr->rta_type == NDA_DST &&
                RTA_PAYLOAD(attr) >= event.spec.destination.length) {
                std::memcpy(event.spec.destination.bytes.data(), RTA_DATA(attr),
                            event.spec.destination.length);
            } else if (attr->rta_type == NDA_LLADDR && RTA_PAYLOAD(attr) >= ETH_ALEN) {
                std::array<uint8_t, ETH_ALEN> mac{};
                std::memcpy(mac.data(), RTA_DATA(attr), mac.size());
                event.spec.lladdr = mac;
            }
        }
        if (SameAddress(event.spec.destination, destination)) return event;
    }
    return std::nullopt;
}

TEST_F(RtnetlinkNeighborSemantics, NotificationsDescribeCommittedChangesAndLinuxDeleteTransition) {
    NeighborSpec spec = MakeNeighbor(AF_INET, "198.18.230.17", veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(spec);
    DeleteNeighborIfPresent(fd_.Get(), spec, &seq_);
    FdGuard listener(OpenRouteListener(RTMGRP_NEIGH));
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, spec,
                                  ++seq_),
              0);
    auto event = ReceiveNeighborEvent(listener.Get(), spec.destination, RTM_NEWNEIGH);
    ASSERT_TRUE(event.has_value());
    EXPECT_EQ(event->spec.lladdr, spec.lladdr);

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    EXPECT_FALSE(ReceiveNeighborEvent(listener.Get(), spec.destination, RTM_NEWNEIGH).has_value())
            << "an unchanged neighbor must not emit a notification";

    spec.lladdr = kMacB;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, spec, ++seq_),
              0);
    event = ReceiveNeighborEvent(listener.Get(), spec.destination, RTM_NEWNEIGH);
    ASSERT_TRUE(event.has_value());
    EXPECT_EQ(event->spec.lladdr, spec.lladdr);

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    auto failed = ReceiveNeighborEvent(listener.Get(), spec.destination, RTM_NEWNEIGH);
    auto removed = ReceiveNeighborEvent(listener.Get(), spec.destination, RTM_DELNEIGH);
    ASSERT_TRUE(failed.has_value());
    ASSERT_TRUE(removed.has_value());
    EXPECT_EQ(failed->spec.state, NUD_FAILED);
    EXPECT_EQ(removed->spec.state, NUD_FAILED);
    EXPECT_FALSE(failed->spec.lladdr.has_value());
    EXPECT_FALSE(removed->spec.lladdr.has_value());
}

size_t CountArpRows(const char* ip) {
    std::ifstream input("/proc/net/arp");
    size_t matches = 0;
    std::string line;
    while (std::getline(input, line)) {
        std::istringstream fields(line);
        std::string actual_ip;
        std::string hardware_type;
        std::string flags;
        std::string actual_mac;
        if (fields >> actual_ip >> hardware_type >> flags >> actual_mac && actual_ip == ip) {
            ++matches;
        }
    }
    return matches;
}

TEST_F(RtnetlinkNeighborSemantics, ProcArpShowsPermanentOnceAndOmitsNoarp) {
    NeighborSpec spec = MakeNeighbor(AF_INET, "198.18.230.18", veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(spec);
    DeleteNeighborIfPresent(fd_.Get(), spec, &seq_);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, spec,
                                  ++seq_),
              0);
    EXPECT_EQ(CountArpRows("198.18.230.18"), 1u);

    spec.state = NUD_NOARP;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    EXPECT_EQ(CountArpRows("198.18.230.18"), 0u);

    spec.state = NUD_PERMANENT;
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    EXPECT_EQ(CountArpRows("198.18.230.18"), 1u);
    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    EXPECT_EQ(CountArpRows("198.18.230.18"), 0u);
}

TEST_F(RtnetlinkNeighborSemantics, Ipv4PermanentMacDrivesEgressAndDeleteRestoresArp) {
    constexpr const char* kDestination = "111.111.11.254";
    NeighborSpec spec = MakeNeighbor(AF_INET, kDestination, veth1_);
    NeighborCleanup cleanup(fd_.Get(), &seq_);
    cleanup.Add(spec);
    DeleteNeighborIfPresent(fd_.Get(), spec, &seq_);

    FdGuard observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(observer.Get(), 0) << ErrnoString(errno);
    sockaddr_ll packet_bind{};
    packet_bind.sll_family = AF_PACKET;
    packet_bind.sll_protocol = htons(ETH_P_ALL);
    packet_bind.sll_ifindex = static_cast<int>(veth1_);
    ASSERT_EQ(bind(observer.Get(), reinterpret_cast<sockaddr*>(&packet_bind), sizeof(packet_bind)),
              0)
            << ErrnoString(errno);
    timeval timeout = {.tv_sec = 2, .tv_usec = 0};
    ASSERT_EQ(setsockopt(observer.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0)
            << ErrnoString(errno);

    auto send_datagram = [&]() {
        FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
        EXPECT_GE(sender.Get(), 0) << ErrnoString(errno);
        constexpr char device[] = "veth1";
        EXPECT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0)
                << ErrnoString(errno);
        sockaddr_in remote{};
        remote.sin_family = AF_INET;
        remote.sin_port = htons(9);
        remote.sin_addr.s_addr = Ipv4(kDestination);
        constexpr char payload[] = "configured-neighbor";
        EXPECT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                         reinterpret_cast<sockaddr*>(&remote), sizeof(remote)),
                  static_cast<ssize_t>(sizeof(payload)))
                << ErrnoString(errno);
    };

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_NEWNEIGH,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, spec,
                                  ++seq_),
              0);
    send_datagram();
    bool saw_static_ip = false;
    bool saw_arp_before_delete = false;
    std::array<uint8_t, 2048> frame{};
    while (!saw_static_ip) {
        const ssize_t length = recv(observer.Get(), frame.data(), frame.size(), 0);
        if (length < 0) break;
        if (length < ETH_HLEN) continue;
        uint16_t protocol = 0;
        std::memcpy(&protocol, frame.data() + 12, sizeof(protocol));
        if (ntohs(protocol) == ETH_P_ARP && length >= ETH_HLEN + 28) {
            uint32_t target = 0;
            std::memcpy(&target, frame.data() + ETH_HLEN + 24, sizeof(target));
            saw_arp_before_delete |= target == Ipv4(kDestination);
            continue;
        }
        if (ntohs(protocol) != ETH_P_IP || length < ETH_HLEN + 20) continue;
        uint32_t destination = 0;
        std::memcpy(&destination, frame.data() + ETH_HLEN + 16, sizeof(destination));
        if (destination != Ipv4(kDestination)) continue;
        saw_static_ip = std::memcmp(frame.data(), kMacA.data(), kMacA.size()) == 0;
    }
    EXPECT_TRUE(saw_static_ip) << "configured MAC was not used for the IPv4 frame";
    EXPECT_FALSE(saw_arp_before_delete) << "permanent neighbor unexpectedly started ARP";

    ASSERT_EQ(SendNeighborRequest(fd_.Get(), RTM_DELNEIGH, NLM_F_REQUEST | NLM_F_ACK, spec,
                                  ++seq_),
              0);
    send_datagram();
    bool saw_arp_after_delete = false;
    while (!saw_arp_after_delete) {
        const ssize_t length = recv(observer.Get(), frame.data(), frame.size(), 0);
        if (length < 0) break;
        if (length < ETH_HLEN + 28) continue;
        uint16_t protocol = 0;
        std::memcpy(&protocol, frame.data() + 12, sizeof(protocol));
        if (ntohs(protocol) != ETH_P_ARP) continue;
        uint32_t target = 0;
        std::memcpy(&target, frame.data() + ETH_HLEN + 24, sizeof(target));
        saw_arp_after_delete = target == Ipv4(kDestination);
    }
    EXPECT_TRUE(saw_arp_after_delete) << "neighbor deletion did not restore ARP resolution";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
