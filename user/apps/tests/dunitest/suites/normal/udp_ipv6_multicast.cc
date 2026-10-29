#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <cstddef>
#include <unistd.h>
#include <cerrno>
#include <cstring>

namespace {
class Ipv6Multicast : public testing::Test {
 protected:
    int fd = -1;
    unsigned lo = 0;
    void SetUp() override {
        fd = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        ASSERT_GE(fd, 0);
        lo = if_nametoindex("lo");
        ASSERT_NE(lo, 0U);
    }
    void TearDown() override { if (fd >= 0) close(fd); }
    ipv6_mreq Request(const char* group = "ff02::1234") {
        ipv6_mreq req{};
        EXPECT_EQ(inet_pton(AF_INET6, group, &req.ipv6mr_multiaddr), 1);
        req.ipv6mr_interface = lo;
        return req;
    }
    int Set(int opt, int value) {
        return setsockopt(fd, IPPROTO_IPV6, opt, &value, sizeof(value));
    }
    void ExpectValue(int opt, int expected) {
        int value = -99;
        socklen_t length = sizeof(value);
        ASSERT_EQ(getsockopt(fd, IPPROTO_IPV6, opt, &value, &length), 0) << errno;
        EXPECT_EQ(length, sizeof(value));
        EXPECT_EQ(value, expected);
    }
    int Membership(int opt, const ipv6_mreq& req) {
        return setsockopt(fd, IPPROTO_IPV6, opt, &req, sizeof(req));
    }
};

TEST_F(Ipv6Multicast, OutputInterfaceRoundTripAndReset) {
    ExpectValue(IPV6_MULTICAST_IF, 0);
    ASSERT_EQ(Set(IPV6_MULTICAST_IF, lo), 0) << errno;
    ExpectValue(IPV6_MULTICAST_IF, lo);
    ASSERT_EQ(Set(IPV6_MULTICAST_IF, 0), 0);
    ExpectValue(IPV6_MULTICAST_IF, 0);
}
TEST_F(Ipv6Multicast, InvalidOutputInterfaceDoesNotCommit) {
    ASSERT_EQ(Set(IPV6_MULTICAST_IF, lo), 0) << errno;
    EXPECT_EQ(Set(IPV6_MULTICAST_IF, -1), -1);
    EXPECT_EQ(errno, ENODEV);
    ExpectValue(IPV6_MULTICAST_IF, lo);
}
TEST_F(Ipv6Multicast, MulticastAllDefaultsAndBooleanValue) {
    ExpectValue(IPV6_MULTICAST_ALL, 1);
    ASSERT_EQ(Set(IPV6_MULTICAST_ALL, 0), 0) << errno;
    ExpectValue(IPV6_MULTICAST_ALL, 0);
    ASSERT_EQ(Set(IPV6_MULTICAST_ALL, -1), 0);
    ExpectValue(IPV6_MULTICAST_ALL, 1);
}
TEST_F(Ipv6Multicast, JoinDuplicateLeaveAndRejoin) {
    const auto req = Request();
    ASSERT_EQ(Membership(IPV6_JOIN_GROUP, req), 0) << errno;
    EXPECT_EQ(Membership(IPV6_JOIN_GROUP, req), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    ASSERT_EQ(Membership(IPV6_LEAVE_GROUP, req), 0) << errno;
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), -1);
    EXPECT_EQ(errno, EADDRNOTAVAIL);
    ASSERT_EQ(Membership(IPV6_JOIN_GROUP, req), 0) << errno;
}
TEST_F(Ipv6Multicast, JoinZeroDetectsExistingGroupBeforeRouteLookup) {
    auto req = Request();
    ASSERT_EQ(Membership(IPV6_JOIN_GROUP, req), 0) << errno;
    req.ipv6mr_interface = 0;
    EXPECT_EQ(Membership(IPV6_JOIN_GROUP, req), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), 0) << errno;
}
TEST_F(Ipv6Multicast, DropUnknownInterfaceUsesMembershipNotDeviceLookup) {
    auto req = Request();
    req.ipv6mr_interface = 0x7fffffff;
    EXPECT_EQ(Membership(IPV6_JOIN_GROUP, req), -1);
    EXPECT_EQ(errno, ENODEV);
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), -1);
    EXPECT_EQ(errno, EADDRNOTAVAIL);
}
TEST_F(Ipv6Multicast, RejectUnicastBeforeDeviceLookup) {
    auto req = Request("::1");
    req.ipv6mr_interface = 0x7fffffff;
    for (int opt : {IPV6_JOIN_GROUP, IPV6_LEAVE_GROUP}) {
        EXPECT_EQ(Membership(opt, req), -1);
        EXPECT_EQ(errno, EINVAL);
    }
}
TEST_F(Ipv6Multicast, MembershipRequiresCompleteRequest) {
    const auto req = Request();
    for (int opt : {IPV6_JOIN_GROUP, IPV6_LEAVE_GROUP}) {
        for (socklen_t length : {0U, 4U, 16U, 19U}) {
            EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, &req, length), -1);
            EXPECT_EQ(errno, EINVAL);
        }
    }
}
TEST_F(Ipv6Multicast, SocketsHoldIndependentMemberships) {
    const auto req = Request();
    ASSERT_EQ(Membership(IPV6_JOIN_GROUP, req), 0) << errno;
    const int other = socket(AF_INET6, SOCK_DGRAM, 0);
    ASSERT_GE(other, 0);
    EXPECT_EQ(setsockopt(other, IPPROTO_IPV6, IPV6_JOIN_GROUP, &req, sizeof(req)), 0)
        << errno;
    close(other);
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), 0) << errno;
}

TEST_F(Ipv6Multicast, GenericGroupRequestSharesMembership) {
    group_req req{};
    req.gr_interface = lo;
    auto* addr = reinterpret_cast<sockaddr_in6*>(&req.gr_group);
    addr->sin6_family = AF_INET6;
    ASSERT_EQ(inet_pton(AF_INET6, "ff02::1234", &addr->sin6_addr), 1);
    ASSERT_EQ(setsockopt(fd, IPPROTO_IPV6, MCAST_JOIN_GROUP, &req, sizeof(req)), 0) << errno;
    EXPECT_EQ(Membership(IPV6_JOIN_GROUP, Request()), -1); EXPECT_EQ(errno, EADDRINUSE);
    ASSERT_EQ(setsockopt(fd, IPPROTO_IPV6, MCAST_LEAVE_GROUP, &req, sizeof(req)), 0) << errno;
}
TEST_F(Ipv6Multicast, GenericGroupRequestRejectsWrongFamily) {
    group_req req{};
    req.gr_interface = lo; req.gr_group.ss_family = AF_INET;
    for (int opt : {MCAST_JOIN_GROUP, MCAST_LEAVE_GROUP}) {
        EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, &req, sizeof(req)), -1);
        EXPECT_EQ(errno, EADDRNOTAVAIL);
    }
}
TEST_F(Ipv6Multicast, ShortRequestsDoNotReadBeyondScalarPrefix) {
    const size_t page = sysconf(_SC_PAGESIZE);
    void* mapping = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    char* boundary = static_cast<char*>(mapping) + page;
    ASSERT_EQ(mprotect(boundary, page, PROT_NONE), 0);
    memset(boundary - 4, 0, 4);
    for (int opt : {IPV6_JOIN_GROUP, IPV6_LEAVE_GROUP, MCAST_JOIN_GROUP, MCAST_LEAVE_GROUP}) {
        const socklen_t short_length = opt == IPV6_JOIN_GROUP || opt == IPV6_LEAVE_GROUP ? 19 : 135;
        EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, boundary - 4, short_length), -1);
        EXPECT_EQ(errno, EINVAL);
        EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, boundary, 3), -1);
        EXPECT_EQ(errno, EINVAL);
    }
    munmap(mapping, 2 * page);
}
TEST_F(Ipv6Multicast, CompleteRequestIsProtectedAndDoesNotReadLongTail) {
    const size_t page = sysconf(_SC_PAGESIZE);
    void* mapping = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    char* boundary = static_cast<char*>(mapping) + page;
    ASSERT_EQ(mprotect(boundary, page, PROT_NONE), 0);
    const auto req = Request();
    memcpy(boundary - sizeof(req), &req, sizeof(req));
    EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, IPV6_JOIN_GROUP, boundary - sizeof(req), page), 0)
        << errno;
    EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, IPV6_LEAVE_GROUP, boundary - 4, sizeof(req)), -1);
    EXPECT_EQ(errno, EFAULT);
    group_req generic{};
    generic.gr_interface = lo;
    auto* addr = reinterpret_cast<sockaddr_in6*>(&generic.gr_group);
    addr->sin6_family = AF_INET6; addr->sin6_addr = req.ipv6mr_multiaddr;
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), 0);
    memcpy(boundary - sizeof(generic), &generic, sizeof(generic));
    EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, MCAST_JOIN_GROUP, boundary - sizeof(generic), page), 0)
        << errno;
    EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, MCAST_LEAVE_GROUP, boundary - 4, sizeof(generic)), -1);
    EXPECT_EQ(errno, EFAULT);
    munmap(mapping, 2 * page);
}
TEST_F(Ipv6Multicast, OutputInterfaceIsSendOnlyNotMembership) {
    const auto req = Request();
    ASSERT_EQ(Set(IPV6_MULTICAST_IF, lo), 0);
    EXPECT_EQ(Membership(IPV6_LEAVE_GROUP, req), -1);
    EXPECT_EQ(errno, EADDRNOTAVAIL);
}
TEST_F(Ipv6Multicast, NullScalarAndShortMembershipFollowLinuxOrdering) {
    for (int opt : {IPV6_MULTICAST_IF, IPV6_MULTICAST_ALL}) {
        EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, nullptr, 4), 0) << errno;
        ExpectValue(opt, 0);
    }
    for (int opt : {IPV6_JOIN_GROUP, IPV6_LEAVE_GROUP, MCAST_JOIN_GROUP, MCAST_LEAVE_GROUP}) {
        EXPECT_EQ(setsockopt(fd, IPPROTO_IPV6, opt, nullptr, 4), -1); EXPECT_EQ(errno, EINVAL);
    }
}
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
