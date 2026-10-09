#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <sys/ioctl.h>
#include "rtnetlink_route_test_support.h"

namespace {

// Each case runs in a loopback-only namespace: a physical default route must
// not hide loss of the source-selected output device during PMTU lookup.
class UdpSourceRoute : public testing::TestWithParam<bool> {};

TEST_P(UdpSourceRoute, BoundSourceSurvivesSendAndConnectedMtuLookup) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        auto run = [&]() {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
            ASSERT_GE(sender.Get(), 0);
            ifreq request{};
            strcpy(request.ifr_name, "lo");
            ASSERT_EQ(ioctl(sender.Get(), SIOCGIFFLAGS, &request), 0);
            request.ifr_flags |= IFF_UP;
            ASSERT_EQ(ioctl(sender.Get(), SIOCSIFFLAGS, &request), 0);

            sockaddr_in local{};
            local.sin_family = AF_INET;
            local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            ASSERT_EQ(bind(sender.Get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
            const int enabled = 1;
            ASSERT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_BROADCAST, &enabled, sizeof(enabled)), 0);
            sockaddr_in destination{};
            destination.sin_family = AF_INET;
            destination.sin_port = htons(12345);
            destination.sin_addr.s_addr = Ipv4(GetParam() ? "255.255.255.255" : "239.1.2.3");
            const char payload[] = "source route";
            ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                             reinterpret_cast<sockaddr*>(&destination), sizeof(destination)),
                      static_cast<ssize_t>(sizeof(payload))) << ErrnoString(errno);
            ASSERT_EQ(connect(sender.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0)
                << ErrnoString(errno);
            int mtu = 0;
            socklen_t length = sizeof(mtu);
            ASSERT_EQ(getsockopt(sender.Get(), IPPROTO_IP, IP_MTU, &mtu, &length), 0)
                << ErrnoString(errno);
            EXPECT_GT(mtu, 0);
            EXPECT_EQ(length, sizeof(mtu));
        };
        run();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

INSTANTIATE_TEST_SUITE_P(Destinations, UdpSourceRoute, testing::Bool());
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
