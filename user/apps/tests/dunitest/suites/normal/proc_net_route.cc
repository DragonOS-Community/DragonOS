#include "rtnetlink_route_test_support.h"

#include <cstdio>
#include <limits>

namespace {

constexpr char kRoutePath[] = "/proc/net/route";
constexpr size_t kRecordSize = 128;
constexpr uint16_t kAdd = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;

std::string ReadAll(int fd, size_t block_size) {
    std::string result;
    char buffer[4096];
    for (;;) {
        const ssize_t count = read(fd, buffer, block_size);
        if (count <= 0) {
            EXPECT_EQ(0, count) << ErrnoString(errno);
            return result;
        }
        result.append(buffer, static_cast<size_t>(count));
    }
}

std::string Record(std::string line) {
    line.resize(kRecordSize - 1, ' ');
    line.push_back('\n');
    return line;
}

std::string ExpectedRoute(const char* iface, const RouteSpec& route) {
    const uint32_t mask = route.prefix_len == 0 ? 0 : UINT32_MAX << (32 - route.prefix_len);
    const uint32_t gateway = route.gateway.value_or(0);
    const unsigned flags = 1 | (gateway != 0 ? 2 : 0) | (route.prefix_len == 32 ? 4 : 0);
    char content[kRecordSize];
    std::snprintf(content, sizeof(content), "%s\t%08X\t%08X\t%04X\t0\t0\t%d\t%08X\t0\t0\t0",
                  iface, route.dst, gateway, flags, static_cast<int32_t>(route.priority),
                  htonl(mask));
    return Record(content);
}

int RenameLink(int fd, uint32_t ifindex, const char* name, uint32_t seq) {
    alignas(nlmsghdr) char storage[128] = {};
    auto* header = reinterpret_cast<nlmsghdr*>(storage);
    auto* link = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(*link));
    header->nlmsg_type = RTM_SETLINK;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    header->nlmsg_seq = seq;
    link->ifi_family = AF_UNSPEC;
    link->ifi_index = static_cast<int>(ifindex);
    AddAttr(header, sizeof(storage), IFLA_IFNAME, name, std::strlen(name) + 1);
    if (send(fd, header, header->nlmsg_len, 0) != static_cast<ssize_t>(header->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

TEST(ProcNetRoute, MainTableRecordsMatchLinuxAndSupportSmallReadsAndSeeks) {
    ASSERT_EQ(0, unshare(CLONE_NEWNET)) << ErrnoString(errno);
    FdGuard nl(OpenRouteSocket());
    ASSERT_GE(nl.Get(), 0) << ErrnoString(errno);
    const uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(0u, lo);
    uint32_t seq = 1;
    constexpr char iface[] = "r\xC3\xB6ute";
    ASSERT_EQ(0, RenameLink(nl.Get(), lo, iface, ++seq));
    ASSERT_EQ(0, SetLinkUp(nl.Get(), lo, true, ++seq));

    // Order by destination first, then longest prefix for aliases at the
    // same destination. The equal-prefix rows are ordered by metric.
    std::vector<RouteSpec> routes = {
        {.dst = Ipv4("10.3.0.0"), .prefix_len = 24, .table = RT_TABLE_MAIN, .oif = lo,
         .priority = 7},
        {.dst = Ipv4("10.3.0.0"), .prefix_len = 24, .table = RT_TABLE_MAIN, .oif = lo,
         .priority = 42},
        {.dst = Ipv4("192.0.0.0"), .prefix_len = 32, .table = RT_TABLE_MAIN, .oif = lo,
         .priority = std::numeric_limits<uint32_t>::max()},
        {.dst = Ipv4("192.0.0.0"), .prefix_len = 8, .table = RT_TABLE_MAIN, .oif = lo},
    };
    for (uint32_t host = 1; host <= 36; ++host) {
        routes.push_back({.dst = htonl(0xc6336400 + host), .prefix_len = 32,
                          .table = RT_TABLE_MAIN, .oif = lo});
    }
    for (auto route = routes.rbegin(); route != routes.rend(); ++route) {
        ASSERT_EQ(0, SendRouteRequest(nl.Get(), RTM_NEWROUTE, kAdd, *route, ++seq));
    }
    const RouteSpec other_table = {
        .dst = Ipv4("203.0.113.0"), .prefix_len = 24, .table = 100, .oif = lo,
    };
    ASSERT_EQ(0, SendRouteRequest(nl.Get(), RTM_NEWROUTE, kAdd, other_table, ++seq));
    ASSERT_EQ(0, SendIpv6RouteRequest(nl.Get(), RTM_NEWROUTE, kAdd, "2001:db8:1::", 64,
                                      lo, RT_SCOPE_LINK, ++seq));

    std::string expected = Record(
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT");
    for (const auto& route : routes) {
        expected += ExpectedRoute(iface, route);
    }

    FdGuard file(open(kRoutePath, O_RDONLY));
    ASSERT_GE(file.Get(), 0) << ErrnoString(errno);
    EXPECT_EQ(expected, ReadAll(file.Get(), 4096));
    ASSERT_EQ(0, lseek(file.Get(), 0, SEEK_SET));
    EXPECT_EQ(expected, ReadAll(file.Get(), 1));
    constexpr off_t offset = 4105;
    ASSERT_EQ(offset, lseek(file.Get(), offset, SEEK_SET));
    EXPECT_EQ(expected.substr(offset), ReadAll(file.Get(), 173));

    errno = 0;
    FdGuard writable(open(kRoutePath, O_RDWR));
    EXPECT_EQ(-1, writable.Get());
    EXPECT_EQ(EACCES, errno);
}

TEST(ProcNetRoute, OpenFileRetainsNamespaceAcrossUnshare) {
    ASSERT_EQ(0, unshare(CLONE_NEWNET)) << ErrnoString(errno);
    FdGuard nl(OpenRouteSocket());
    ASSERT_GE(nl.Get(), 0);
    const uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(0u, lo);
    ASSERT_EQ(0, SetLinkUp(nl.Get(), lo, true, 1));
    const RouteSpec marker = {
        .dst = Ipv4("198.18.77.0"), .prefix_len = 24, .table = RT_TABLE_MAIN, .oif = lo,
    };
    ASSERT_EQ(0, SendRouteRequest(nl.Get(), RTM_NEWROUTE, kAdd, marker, 2));
    nl.Reset();

    FdGuard old_file(open(kRoutePath, O_RDONLY));
    ASSERT_GE(old_file.Get(), 0) << ErrnoString(errno);
    ASSERT_EQ(0, unshare(CLONE_NEWNET)) << ErrnoString(errno);
    FdGuard new_file(open(kRoutePath, O_RDONLY));
    ASSERT_GE(new_file.Get(), 0) << ErrnoString(errno);

    const auto line = ExpectedRoute("lo", marker);
    EXPECT_NE(std::string::npos, ReadAll(old_file.Get(), 256).find(line));
    EXPECT_EQ(std::string::npos, ReadAll(new_file.Get(), 256).find(line));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
