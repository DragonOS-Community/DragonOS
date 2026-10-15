#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include "rtnetlink_route_test_support.h"
#include <sys/mman.h>

namespace {
class Ipv6Scope : public testing::TestWithParam<int> {
 protected:
    unsigned loopback = 0;
    void SetUp() override {
        ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
        FdGuard route(OpenRouteSocket());
        ASSERT_GE(route.Get(), 0);
        loopback = if_nametoindex("lo");
        ASSERT_NE(loopback, 0U);
        ASSERT_EQ(SetLinkUp(route.Get(), loopback, true, 1), 0);
        ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
            loopback, "fe80::92", 64, 2), 0);
    }
    int Open() {
        return socket(AF_INET6, GetParam(), GetParam() == SOCK_RAW ? IPPROTO_ICMPV6 : 0);
    }
    sockaddr_in6 Address(const char* text, unsigned scope) {
        sockaddr_in6 address{};
        address.sin6_family = AF_INET6;
        address.sin6_scope_id = scope;
        EXPECT_EQ(inet_pton(AF_INET6, text, &address.sin6_addr), 1);
        return address;
    }
    void BindName(const char* text, unsigned scope, unsigned expected, socklen_t size = 28) {
        FdGuard fd(Open());
        ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
        auto address = Address(text, scope);
        ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), size), 0)
            << ErrnoString(errno);
        sockaddr_in6 actual{};
        socklen_t length = sizeof(actual);
        ASSERT_EQ(getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&actual), &length), 0);
        EXPECT_EQ(length, sizeof(actual));
        EXPECT_EQ(actual.sin6_scope_id, expected);
        EXPECT_EQ(memcmp(&actual.sin6_addr, &address.sin6_addr, sizeof(in6_addr)), 0);
    }
};

TEST_P(Ipv6Scope, LinkLocalBindReportsScope) {
    BindName("fe80::92", loopback, loopback);
}
TEST_P(Ipv6Scope, LinkLocalBindRequiresInterface) {
    FdGuard fd(Open());
    ASSERT_GE(fd.Get(), 0);
    auto address = Address("fe80::92", 0);
    EXPECT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
    EXPECT_EQ(errno, EINVAL);
}
TEST_P(Ipv6Scope, ScopedBindChecksNamespaceDevice) {
    FdGuard fd(Open());
    ASSERT_GE(fd.Get(), 0);
    auto address = Address("fe80::92", 0x7ffffffeU);
    EXPECT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
    EXPECT_EQ(errno, ENODEV);
}
TEST_P(Ipv6Scope, BoundDeviceSuppliesMissingScope) {
    FdGuard fd(Open());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, "lo", 3), 0);
    auto address = Address("fe80::92", 0);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
        << ErrnoString(errno);
    socklen_t length = sizeof(address);
    ASSERT_EQ(getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
    EXPECT_EQ(address.sin6_scope_id, loopback);
}
TEST_P(Ipv6Scope, LoopbackIgnoresNonzeroScope) {
    BindName("::1", 0x7ffffffeU, 0);
}
TEST_P(Ipv6Scope, WildcardIgnoresNonzeroScope) {
    BindName("::", 0x7ffffffeU, 0);
}
TEST_P(Ipv6Scope, LegacyTwentyFourByteAddressHasNoScope) {
    BindName("::1", 0x7ffffffeU, 0, 24);
}
TEST_P(Ipv6Scope, PartialScopeTailIsIgnored) {
    for (socklen_t length : {24U, 25U, 26U, 27U}) {
        FdGuard fd(Open());
        ASSERT_GE(fd.Get(), 0);
        auto address = Address("fe80::92", loopback);
        EXPECT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), length), -1);
        EXPECT_EQ(errno, EINVAL);
    }
}
TEST_P(Ipv6Scope, FailedBindRetainsNumericScope) {
    FdGuard fd(Open());
    ASSERT_GE(fd.Get(), 0);
    auto address = Address("fe80::dead", loopback);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
    EXPECT_EQ(errno, EADDRNOTAVAIL);
    char name[IFNAMSIZ]{};
    socklen_t length = sizeof(name);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
    EXPECT_STREQ(name, "lo");
}
TEST_P(Ipv6Scope, InterfaceLocalFlagsMulticastUsesScope) {
    if (GetParam() == SOCK_STREAM) {
        FdGuard fd(Open());
        auto address = Address("ff11::92", loopback);
        ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
        EXPECT_EQ(errno, EINVAL);
    } else {
        BindName("ff11::92", loopback, loopback);
    }
}
TEST_P(Ipv6Scope, WiderMulticastIgnoresScope) {
    if (GetParam() == SOCK_STREAM) {
        FdGuard fd(Open());
        auto address = Address("ff05::92", 0x7ffffffeU);
        ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
        EXPECT_EQ(errno, EINVAL);
    } else {
        BindName("ff05::92", 0x7ffffffeU, 0);
    }
}
TEST_P(Ipv6Scope, LegacyInputAndShortOutputRespectGuardPages) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* bytes = static_cast<unsigned char*>(mmap(nullptr, page * 2,
        PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(bytes, MAP_FAILED);
    ASSERT_EQ(mprotect(bytes + page, page, PROT_NONE), 0);
    for (socklen_t length : {24U, 25U, 27U, 28U}) {
        FdGuard fd(Open());
        ASSERT_GE(fd.Get(), 0);
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, "lo", 3), 0);
        auto address = Address("fe80::92", loopback);
        memcpy(bytes + page - length, &address, length);
        ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(bytes + page - length), length), 0)
            << ErrnoString(errno);
        socklen_t actual = length;
        ASSERT_EQ(getsockname(fd.Get(), reinterpret_cast<sockaddr*>(bytes + page - length),
                              &actual), 0) << ErrnoString(errno);
        EXPECT_EQ(actual, 28U);
        EXPECT_EQ(memcmp(bytes + page - length + 8, &address.sin6_addr, sizeof(in6_addr)), 0);
        if (length > 24) {
            EXPECT_EQ(memcmp(bytes + page - length + 24, &address.sin6_scope_id, length - 24), 0);
        }
    }
    FdGuard fd(Open());
    auto address = Address("::1", 0);
    memcpy(bytes + page - 28, &address, 28);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(bytes + page - 28), 29), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(munmap(bytes, page * 2), 0);
}
TEST_P(Ipv6Scope, ConnectCopiesDeclaredTailBeforeProtocolScopeValidation) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* bytes = static_cast<unsigned char*>(mmap(nullptr, page * 2,
        PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(bytes, MAP_FAILED);
    ASSERT_EQ(mprotect(bytes + page, page, PROT_NONE), 0);
    FdGuard fd(Open()); ASSERT_GE(fd.Get(), 0);
    auto address = Address("::1", 0);
    memcpy(bytes + page - 28, &address, 28);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(bytes + page - 28), 29), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(munmap(bytes, page * 2), 0);
}

INSTANTIATE_TEST_SUITE_P(Protocols, Ipv6Scope,
    testing::Values(SOCK_DGRAM, SOCK_STREAM, SOCK_RAW));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
