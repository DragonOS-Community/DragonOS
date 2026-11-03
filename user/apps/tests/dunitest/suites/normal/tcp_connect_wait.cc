// Pending connect must remain interruptible. CPU measurements are diagnostic:
// functional success alone does not prove that the waiter actually slept.
#include "rtnetlink_route_test_support.h"
#include <linux/if_link.h>
#include <linux/veth.h>
#include <poll.h>
#include <signal.h>
#include <sys/utsname.h>
#include <time.h>
#include <chrono>
#include <cstdio>

namespace {
volatile sig_atomic_t alarm_seen = 0;
void Alarm(int) { alarm_seen = 1; }

enum class Mode { BlockingSignal, NonblockingTimeout, NonblockingSignal };

// Same nested veth attributes as ipv6_scope_network.cc. All other rtnetlink
// operations use the existing shared test helpers.
void Append(std::vector<uint8_t>& bytes, uint16_t type, const void* data, size_t size) {
    const size_t offset = bytes.size();
    bytes.resize(offset + RTA_ALIGN(RTA_LENGTH(size)), 0);
    auto* attr = reinterpret_cast<rtattr*>(bytes.data() + offset);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(size);
    if (size) memcpy(RTA_DATA(attr), data, size);
}
int Pair(int fd) {
    std::vector<uint8_t> peer(sizeof(ifinfomsg), 0), data, info, attrs;
    Append(peer, IFLA_IFNAME, "waitpeer", 9);
    Append(data, VETH_INFO_PEER | NLA_F_NESTED, peer.data(), peer.size());
    Append(info, IFLA_INFO_KIND, "veth", 5);
    Append(info, IFLA_INFO_DATA | NLA_F_NESTED, data.data(), data.size());
    Append(attrs, IFLA_IFNAME, "waitclient", 11);
    Append(attrs, IFLA_LINKINFO | NLA_F_NESTED, info.data(), info.size());
    std::vector<uint8_t> request(NLMSG_SPACE(sizeof(ifinfomsg)), 0);
    auto* h = reinterpret_cast<nlmsghdr*>(request.data());
    h->nlmsg_len = request.size() + attrs.size();
    h->nlmsg_type = RTM_NEWLINK;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    h->nlmsg_seq = 1;
    request.insert(request.end(), attrs.begin(), attrs.end());
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size()))
        return errno;
    return RecvAck(fd, 1);
}
int Failure(const char* stage, int error) {
    fprintf(stderr, "connect_wait: %s: %d\n", stage, error);
    return 1;
}
bool Stamp(clockid_t clock, uint64_t* value) {
    timespec ts{};
    if (clock_gettime(clock, &ts)) return false;
    *value = static_cast<uint64_t>(ts.tv_sec) * 1000000000ULL + ts.tv_nsec;
    return true;
}
int Exercise(Mode mode) {
    if (unshare(CLONE_NEWNET)) {
        const int error = errno;
        utsname name{};
        if (!uname(&name) && !strcmp(name.sysname, "Linux") &&
            (error == EPERM || error == EACCES)) return 77;
        return Failure("unshare", error);
    }
    FdGuard route(OpenRouteSocket());
    if (route.Get() < 0) return Failure("route socket", errno);
    timeval timeout{1, 0};
    if (setsockopt(route.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)))
        return Failure("netlink timeout", errno);
    int error = Pair(route.Get());
    if (error) return Failure("veth pair", error);
    const unsigned client = if_nametoindex("waitclient"), peer = if_nametoindex("waitpeer");
    if (!client || !peer) return Failure("veth lookup", errno);
    if ((error = SetLinkUp(route.Get(), client, true, 2)) ||
        (error = SetLinkUp(route.Get(), peer, true, 3))) return Failure("link up", error);

    // This subnet exists only in our fresh netns. Nothing is configured on the
    // peer, so ARP cannot resolve the unassigned destination. No default route.
    const uint32_t network = 0xc6120000u | ((static_cast<unsigned>(getpid()) & 255u) << 8);
    in_addr local{htonl(network | 1u)};
    char text[INET_ADDRSTRLEN];
    if (!inet_ntop(AF_INET, &local, text, sizeof(text))) return Failure("address", errno);
    error = SendAddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, client, text, 24, 4);
    if (error) return Failure("assign address", error);
    RouteSpec subnet;
    subnet.dst = htonl(network); subnet.prefix_len = 24; subnet.table = RT_TABLE_MAIN;
    subnet.oif = client; subnet.scope = RT_SCOPE_LINK;
    // Replace the auto-created connected route if present, otherwise create it.
    error = SendRouteRequest(route.Get(), RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE, subnet, 5);
    if (error) return Failure("connected route", error);

    const bool nonblock = mode != Mode::BlockingSignal;
    FdGuard socket_fd(socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC |
                            (nonblock ? SOCK_NONBLOCK : 0), 0));
    if (socket_fd.Get() < 0) return Failure("TCP socket", errno);
    sockaddr_in destination{};
    destination.sin_family = AF_INET; destination.sin_port = htons(12345);
    destination.sin_addr.s_addr = htonl(network | 2u);
    struct sigaction action{}; action.sa_handler = Alarm;
    sigemptyset(&action.sa_mask);
    sigset_t mask; sigemptyset(&mask); sigaddset(&mask, SIGALRM);
    if (sigaction(SIGALRM, &action, nullptr) || sigprocmask(SIG_UNBLOCK, &mask, nullptr))
        return Failure("signal setup", errno);
    alarm_seen = 0;
    uint64_t w0, w1, c0, c1;
    if (!Stamp(CLOCK_MONOTONIC, &w0) || !Stamp(CLOCK_THREAD_CPUTIME_ID, &c0))
        return Failure("clock before", errno);
    itimerval timer{};
    // Periodic delivery also covers preemption between arming and entering
    // connect/poll; a signal consumed before entry must not leave it unbounded.
    if (mode == Mode::BlockingSignal) {
        timer.it_value.tv_usec = 200000;
        timer.it_interval.tv_usec = 200000;
    }
    if (setitimer(ITIMER_REAL, &timer, nullptr)) return Failure("timer", errno);
    const int connected = connect(socket_fd.Get(), reinterpret_cast<sockaddr*>(&destination),
                                  sizeof(destination));
    const int connect_error = connected < 0 ? errno : 0;
    int polled = -2, poll_error = 0;
    if (nonblock && connected == -1 && connect_error == EINPROGRESS) {
        if (mode == Mode::NonblockingSignal) {
            timer.it_value.tv_usec = 200000;
            timer.it_interval.tv_usec = 200000;
            if (setitimer(ITIMER_REAL, &timer, nullptr)) return Failure("poll timer", errno);
        }
        pollfd event{socket_fd.Get(), POLLOUT, 0};
        polled = poll(&event, 1, mode == Mode::NonblockingTimeout ? 200 : 2000);
        poll_error = polled < 0 ? errno : 0;
    }
    timer = {};
    if (setitimer(ITIMER_REAL, &timer, nullptr)) return Failure("cancel timer", errno);
    if (!Stamp(CLOCK_THREAD_CPUTIME_ID, &c1) || !Stamp(CLOCK_MONOTONIC, &w1) || c1 < c0 || w1 < w0)
        return Failure("clock after", errno);
    printf("connect_wait mode=%d wall_ns=%llu thread_cpu_ns=%llu connect=%d/%d poll=%d/%d signal=%d\n",
           static_cast<int>(mode), static_cast<unsigned long long>(w1-w0),
           static_cast<unsigned long long>(c1-c0), connected, connect_error, polled, poll_error,
           static_cast<int>(alarm_seen));
    if (!nonblock) return connected == -1 && connect_error == EINTR && alarm_seen ? 0 : 1;
    if (connected != -1 || connect_error != EINPROGRESS) return 1;
    if (mode == Mode::NonblockingTimeout) return polled == 0 && !alarm_seen ? 0 : 1;
    return polled == -1 && poll_error == EINTR && alarm_seen ? 0 : 1;
}

void RunIsolated(Mode mode) {
    fflush(nullptr);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const int result = Exercise(mode);
        fflush(nullptr);
        _exit(result);
    }
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    int status = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        const pid_t done = waitpid(child, &status, WNOHANG);
        if (done == child) {
            ASSERT_TRUE(WIFEXITED(status)) << status;
            if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "Linux requires CAP_SYS_ADMIN for private netns";
            ASSERT_EQ(WEXITSTATUS(status), 0);
            return;
        }
        if (done < 0 && errno != EINTR) break;
        usleep(1000);  // Parent watchdog only, never a network readiness predicate.
    }
    const int killed = kill(child, SIGKILL);
    EXPECT_TRUE(killed == 0 || errno == ESRCH);
    bool reaped = false;
    const auto reap_deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (std::chrono::steady_clock::now() < reap_deadline) {
        const pid_t done = waitpid(child, &status, WNOHANG);
        if (done == child || (done < 0 && errno == ECHILD)) { reaped = true; break; }
        if (done < 0 && errno != EINTR) break;
        usleep(1000);
    }
    EXPECT_TRUE(reaped) << "child still present after SIGKILL; cleanup not verified";
    FAIL() << "connect child exceeded watchdog";
}
TEST(TcpConnectWait, BlockingPendingConnectIsInterruptible) { RunIsolated(Mode::BlockingSignal); }
TEST(TcpConnectWait, NonblockingPendingConnectPollTimesOut) { RunIsolated(Mode::NonblockingTimeout); }
TEST(TcpConnectWait, NonblockingPendingConnectPollIsInterruptible) { RunIsolated(Mode::NonblockingSignal); }
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
