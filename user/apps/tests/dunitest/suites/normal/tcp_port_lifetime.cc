// Exercise observable TCP bind ownership, independently of TCP_INFO on an old
// descriptor: Linux can detach TIME_WAIT while that descriptor reports CLOSE.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstring>

namespace {
class Fd {
 public:
    ~Fd() { reset(); }
    Fd() = default;
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void reset(int fd = -1) { if (fd_ >= 0) close(fd_); fd_ = fd; }
 private:
    int fd_ = -1;
};

struct Address {
    sockaddr_storage storage{};
    socklen_t length = sizeof(storage);
    sockaddr* ptr() { return reinterpret_cast<sockaddr*>(&storage); }
};

class TcpPortLifetime : public testing::TestWithParam<int> {
 protected:
    void Socket(Fd& fd, int reuse) {
        fd.reset(socket(GetParam(), SOCK_STREAM, 0));
        ASSERT_GE(fd.get(), 0) << strerror(errno);
        if (GetParam() == AF_INET6) {
            int one = 1;
            ASSERT_EQ(setsockopt(fd.get(), IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof(one)), 0);
        }
        ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)), 0);
        timeval timeout{3, 0};
        ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
        ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)), 0);
    }
    Address Loopback() {
        Address a;
        if (GetParam() == AF_INET) {
            auto* p = reinterpret_cast<sockaddr_in*>(&a.storage);
            p->sin_family = AF_INET;
            p->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            a.length = sizeof(*p);
        } else {
            auto* p = reinterpret_cast<sockaddr_in6*>(&a.storage);
            p->sin6_family = AF_INET6;
            p->sin6_addr = in6addr_loopback;
            a.length = sizeof(*p);
        }
        return a;
    }
    void BoundAddress(Fd& fd, Address& a) {
        a.length = sizeof(a.storage);
        ASSERT_EQ(getsockname(fd.get(), a.ptr(), &a.length), 0);
    }
    uint16_t Port(const Address& address) {
        if (GetParam() == AF_INET) {
            return ntohs(reinterpret_cast<const sockaddr_in*>(&address.storage)->sin_port);
        }
        return ntohs(reinterpret_cast<const sockaddr_in6*>(&address.storage)->sin6_port);
    }
    void Listener(Fd& fd, int reuse, Address& a) {
        ASSERT_NO_FATAL_FAILURE(Socket(fd, reuse));
        a = Loopback();
        ASSERT_EQ(bind(fd.get(), a.ptr(), a.length), 0);
        ASSERT_NO_FATAL_FAILURE(BoundAddress(fd, a));
        ASSERT_EQ(listen(fd.get(), 4), 0);
    }
    void Accept(Fd& listener, Fd& child) {
        pollfd ready{listener.get(), POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 3000), 1);
        child.reset(accept(listener.get(), nullptr, nullptr));
        ASSERT_GE(child.get(), 0);
        timeval timeout{3, 0};
        ASSERT_EQ(setsockopt(child.get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
    }
    void Exchange(Fd& from, Fd& to) {
        const char sent = 'x';
        ASSERT_EQ(send(from.get(), &sent, 1, MSG_NOSIGNAL), 1);
        char received = 0;
        ASSERT_EQ(recv(to.get(), &received, 1, 0), 1);
        ASSERT_EQ(received, sent);
    }
    void Probe(Address a, int reuse, bool allowed) {
        Fd fd;
        ASSERT_NO_FATAL_FAILURE(Socket(fd, reuse));
        errno = 0;
        int result = bind(fd.get(), a.ptr(), a.length);
        int error = errno;
        EXPECT_EQ(result, allowed ? 0 : -1) << "reuse=" << reuse << " errno=" << error;
        if (!allowed) {
            EXPECT_EQ(error, EADDRINUSE);
        }
    }
    void ChildSurvives(int old_reuse) {
        Fd listener, client, child;
        Address address;
        ASSERT_NO_FATAL_FAILURE(Listener(listener, old_reuse, address));
        ASSERT_NO_FATAL_FAILURE(Socket(client, 0));
        ASSERT_EQ(connect(client.get(), address.ptr(), address.length), 0);
        ASSERT_NO_FATAL_FAILURE(Accept(listener, child));
        listener.reset();
        ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
        Probe(address, 0, false);
        Probe(address, 1, old_reuse != 0);
    }
    void TimeWait(int old_reuse) {
        Fd listener, client, child;
        Address server, local;
        ASSERT_NO_FATAL_FAILURE(Listener(listener, 0, server));
        ASSERT_NO_FATAL_FAILURE(Socket(client, old_reuse));
        ASSERT_EQ(connect(client.get(), server.ptr(), server.length), 0);
        ASSERT_NO_FATAL_FAILURE(Accept(listener, child));
        ASSERT_NO_FATAL_FAILURE(BoundAddress(client, local));
        ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
        ASSERT_EQ(shutdown(client.get(), SHUT_WR), 0);
        char byte;
        ASSERT_EQ(recv(child.get(), &byte, 1, 0), 0);
        ASSERT_EQ(shutdown(child.get(), SHUT_WR), 0);
        ASSERT_EQ(recv(client.get(), &byte, 1, 0), 0);
        // Both FINs have been received; no sleep or TCP_INFO state inference.
        Probe(local, 0, false);
        Probe(local, 1, old_reuse != 0);
        client.reset();
        Probe(local, 0, false);
        Probe(local, 1, old_reuse != 0);
    }
    void Inheritance(int inherited) {
        Fd listener, client, child;
        Address server;
        ASSERT_NO_FATAL_FAILURE(Listener(listener, inherited, server));
        ASSERT_NO_FATAL_FAILURE(Socket(client, 0));
        ASSERT_EQ(connect(client.get(), server.ptr(), server.length), 0);
        // Readiness proves that the final ACK has produced an accept-ready child
        // before changing the parent, not merely that connect returned locally.
        pollfd ready{listener.get(), POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 3000), 1);
        int changed = !inherited;
        ASSERT_EQ(setsockopt(listener.get(), SOL_SOCKET, SO_REUSEADDR, &changed, sizeof(changed)), 0);
        ASSERT_NO_FATAL_FAILURE(Accept(listener, child));
        int actual = -1;
        socklen_t length = sizeof(actual);
        ASSERT_EQ(getsockopt(child.get(), SOL_SOCKET, SO_REUSEADDR, &actual, &length), 0);
        EXPECT_EQ(actual, inherited);
        listener.reset();
        Probe(server, 1, inherited != 0);
    }
    void PeerReset(int binding) {
        Fd listener, client, child, chooser;
        Address remote, local = Loopback();
        ASSERT_NO_FATAL_FAILURE(Listener(listener, 0, remote));
        ASSERT_NO_FATAL_FAILURE(Socket(client, binding == 2 ? 1 : 0));
        if (binding == 2) {
            ASSERT_NO_FATAL_FAILURE(Socket(chooser, 1));
            ASSERT_EQ(bind(chooser.get(), local.ptr(), local.length), 0);
            ASSERT_NO_FATAL_FAILURE(BoundAddress(chooser, local));
        }
        if (binding != 0) {
            ASSERT_EQ(bind(client.get(), local.ptr(), local.length), 0);
        }
        chooser.reset();
        ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), 0);
        ASSERT_NO_FATAL_FAILURE(Accept(listener, child));
        ASSERT_NO_FATAL_FAILURE(BoundAddress(client, local));
        ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
        linger reset{1, 0};
        ASSERT_EQ(setsockopt(child.get(), SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)), 0);
        child.reset();
        char byte;
        ASSERT_EQ(recv(client.get(), &byte, 1, 0), -1);
        ASSERT_EQ(errno, ECONNRESET);
        // The original descriptor deliberately remains alive. Protocol CLOSE
        // releases auto-selected ports, but not a nonzero user bind lock.
        Probe(local, 0, binding != 2);
    }
};

TEST_P(TcpPortLifetime, ChildWithoutReuseRetainsPort) { ChildSurvives(0); }
TEST_P(TcpPortLifetime, ChildWithReuseRequiresBothSides) { ChildSurvives(1); }
TEST_P(TcpPortLifetime, TimeWaitWithoutReuseSurvivesDescriptorClose) { TimeWait(0); }
TEST_P(TcpPortLifetime, TimeWaitWithReuseRequiresBothSides) { TimeWait(1); }
TEST_P(TcpPortLifetime, ChildDoesNotGainParentLaterReuse) { Inheritance(0); }
TEST_P(TcpPortLifetime, ChildDoesNotLoseParentEarlierReuse) { Inheritance(1); }
TEST_P(TcpPortLifetime, PeerResetWithLiveFdReleasesAutomaticallySelectedPort) {
    ASSERT_NO_FATAL_FAILURE(PeerReset(0));
    ASSERT_NO_FATAL_FAILURE(PeerReset(1));
}
TEST_P(TcpPortLifetime, PeerResetWithLiveFdRetainsExplicitNonzeroPort) { PeerReset(2); }

TEST_P(TcpPortLifetime, SharedBindListenConflictAndRetry) {
    Fd first, second;
    ASSERT_NO_FATAL_FAILURE(Socket(first, 1));
    ASSERT_NO_FATAL_FAILURE(Socket(second, 1));
    Address local = Loopback();
    ASSERT_EQ(bind(first.get(), local.ptr(), local.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(first, local));
    ASSERT_EQ(bind(second.get(), local.ptr(), local.length), 0);
    ASSERT_EQ(listen(first.get(), 4), 0);
    ASSERT_EQ(listen(first.get(), 8), 0);
    ASSERT_EQ(listen(second.get(), 4), -1);
    ASSERT_EQ(errno, EADDRINUSE);
    first.reset();
    ASSERT_EQ(listen(second.get(), 4), 0);
}

TEST_P(TcpPortLifetime, ChildReuseChangeDoesNotModifyParentOrSibling) {
    Fd listener, client1, client2, child1, child2;
    Address server;
    ASSERT_NO_FATAL_FAILURE(Listener(listener, 1, server));
    ASSERT_NO_FATAL_FAILURE(Socket(client1, 0));
    ASSERT_NO_FATAL_FAILURE(Socket(client2, 0));
    ASSERT_EQ(connect(client1.get(), server.ptr(), server.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(listener, child1));
    ASSERT_EQ(connect(client2.get(), server.ptr(), server.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(listener, child2));
    int disabled = 0;
    ASSERT_EQ(setsockopt(child1.get(), SOL_SOCKET, SO_REUSEADDR, &disabled, sizeof(disabled)), 0);
    for (int fd : {listener.get(), child2.get()}) {
        int reuse = -1;
        socklen_t length = sizeof(reuse);
        ASSERT_EQ(getsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &reuse, &length), 0);
        EXPECT_EQ(reuse, 1);
    }
    listener.reset();
    // The non-reusing child must still block reuse despite its sibling's flag.
    Probe(server, 1, false);
}

TEST_P(TcpPortLifetime, DistinctRemoteAllowedButDuplicateTupleRejected) {
    Fd listener1, listener2, client1, client2, duplicate, child1, child2;
    Address server1, server2, local;
    ASSERT_NO_FATAL_FAILURE(Listener(listener1, 0, server1));
    ASSERT_NO_FATAL_FAILURE(Listener(listener2, 0, server2));
    ASSERT_NO_FATAL_FAILURE(Socket(client1, 1));
    local = Loopback();
    ASSERT_EQ(bind(client1.get(), local.ptr(), local.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client1, local));
    ASSERT_EQ(connect(client1.get(), server1.ptr(), server1.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(listener1, child1));
    ASSERT_NO_FATAL_FAILURE(Socket(client2, 1));
    ASSERT_EQ(bind(client2.get(), local.ptr(), local.length), 0);
    ASSERT_EQ(connect(client2.get(), server2.ptr(), server2.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(listener2, child2));
    ASSERT_NO_FATAL_FAILURE(Socket(duplicate, 1));
    ASSERT_EQ(bind(duplicate.get(), local.ptr(), local.length), 0);
    ASSERT_EQ(connect(duplicate.get(), server1.ptr(), server1.length), -1);
    ASSERT_EQ(errno, EADDRNOTAVAIL);
    ASSERT_NO_FATAL_FAILURE(Exchange(client1, child1));
    ASSERT_NO_FATAL_FAILURE(Exchange(client2, child2));
}

TEST_P(TcpPortLifetime, ZeroLingerResetReleasesPort) {
    Fd listener, client, child;
    Address server, local;
    ASSERT_NO_FATAL_FAILURE(Listener(listener, 0, server));
    ASSERT_NO_FATAL_FAILURE(Socket(client, 0));
    ASSERT_EQ(connect(client.get(), server.ptr(), server.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(listener, child));
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client, local));
    ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
    linger reset{1, 0};
    ASSERT_EQ(setsockopt(client.get(), SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)), 0);
    client.reset();
    char byte;
    ASSERT_EQ(recv(child.get(), &byte, 1, 0), -1);
    ASSERT_EQ(errno, ECONNRESET);
    Probe(local, 0, true);
}

TEST_P(TcpPortLifetime, RefusedConnectRetainsExplicitNonzeroPort) {
    Fd target, chooser, client, child;
    ASSERT_NO_FATAL_FAILURE(Socket(target, 0));
    Address remote = Loopback();
    ASSERT_EQ(bind(target.get(), remote.ptr(), remote.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(target, remote));
    // Reserve an unused local port without a close/rebind race. Both sockets
    // permit sharing while bound; the chooser disappears before connect.
    ASSERT_NO_FATAL_FAILURE(Socket(chooser, 1));
    Address local = Loopback();
    ASSERT_EQ(bind(chooser.get(), local.ptr(), local.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(chooser, local));
    ASSERT_NO_FATAL_FAILURE(Socket(client, 1));
    ASSERT_EQ(bind(client.get(), local.ptr(), local.length), 0);
    chooser.reset();
    ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), -1);
    ASSERT_EQ(errno, ECONNREFUSED);
    Address after;
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client, after));
    EXPECT_EQ(Port(after), Port(local));
    Probe(local, 0, false);
    ASSERT_EQ(listen(target.get(), 1), 0);
    ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(target, child));
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client, after));
    EXPECT_EQ(Port(after), Port(local));
    ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
}

TEST_P(TcpPortLifetime, RefusedConnectReleasesBindZeroPort) {
    Fd target, client, blocker, child;
    ASSERT_NO_FATAL_FAILURE(Socket(target, 0));
    Address remote = Loopback();
    ASSERT_EQ(bind(target.get(), remote.ptr(), remote.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(target, remote));
    ASSERT_NO_FATAL_FAILURE(Socket(client, 0));
    Address local = Loopback();
    ASSERT_EQ(bind(client.get(), local.ptr(), local.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client, local));
    ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), -1);
    ASSERT_EQ(errno, ECONNREFUSED);
    // Linux does not set SOCK_BINDPORT_LOCK for bind(port=0). The reported
    // old local port is not proof of ownership after the failed connection.
    ASSERT_NO_FATAL_FAILURE(Socket(blocker, 0));
    ASSERT_EQ(bind(blocker.get(), local.ptr(), local.length), 0);
    ASSERT_EQ(listen(target.get(), 1), 0);
    ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), 0);
    ASSERT_NO_FATAL_FAILURE(Accept(target, child));
    Address after;
    ASSERT_NO_FATAL_FAILURE(BoundAddress(client, after));
    EXPECT_NE(Port(after), Port(local));
    ASSERT_NO_FATAL_FAILURE(Exchange(client, child));
}

TEST_P(TcpPortLifetime, CloseUnconsumedNonblockingRefusalReleasesExplicitBind) {
    Fd target, chooser;
    ASSERT_NO_FATAL_FAILURE(Socket(target, 0));
    Address remote = Loopback();
    ASSERT_EQ(bind(target.get(), remote.ptr(), remote.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(target, remote));
    ASSERT_NO_FATAL_FAILURE(Socket(chooser, 1));
    Address local = Loopback();
    ASSERT_EQ(bind(chooser.get(), local.ptr(), local.length), 0);
    ASSERT_NO_FATAL_FAILURE(BoundAddress(chooser, local));

    for (int iteration = 0; iteration < 16; ++iteration) {
        SCOPED_TRACE(iteration);
        Fd client;
        ASSERT_NO_FATAL_FAILURE(Socket(client, 1));
        ASSERT_EQ(bind(client.get(), local.ptr(), local.length), 0);
        chooser.reset();
        int flags = fcntl(client.get(), F_GETFL, 0);
        ASSERT_GE(flags, 0);
        ASSERT_EQ(fcntl(client.get(), F_SETFL, flags | O_NONBLOCK), 0);
        ASSERT_EQ(connect(client.get(), remote.ptr(), remote.length), -1);
        ASSERT_EQ(errno, EINPROGRESS);
        pollfd ready{client.get(), POLLOUT, 0};
        ASSERT_EQ(poll(&ready, 1, 3000), 1);
        ASSERT_NE(ready.revents & POLLERR, 0);
        // Do not consume SO_ERROR or call connect/recv again: close must
        // dispose of the failed Connecting state and its protocol handle.
        client.reset();
        Probe(local, 0, true);
    }
}

void CheckAutomaticReuseWithoutOtherTraffic() {
    ASSERT_EQ(unshare(CLONE_NEWNET), 0) << strerror(errno);
    Fd listener;
    listener.reset(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(listener.get(), 0);
    ifreq request{};
    strcpy(request.ifr_name, "lo");
    ASSERT_EQ(ioctl(listener.get(), SIOCGIFFLAGS, &request), 0);
    request.ifr_flags |= IFF_UP;
    ASSERT_EQ(ioctl(listener.get(), SIOCSIFFLAGS, &request), 0);
    sockaddr_in server{};
    server.sin_family = AF_INET;
    server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(listener.get(), reinterpret_cast<sockaddr*>(&server), sizeof(server)), 0);
    socklen_t length = sizeof(server);
    ASSERT_EQ(getsockname(listener.get(), reinterpret_cast<sockaddr*>(&server), &length), 0);
    ASSERT_EQ(listen(listener.get(), 4), 0);

    constexpr const char* path = "/proc/sys/net/ipv4/ip_local_port_range";
    char saved[64]{};
    Fd range;
    range.reset(open(path, O_RDONLY));
    ASSERT_GE(range.get(), 0);
    ssize_t count = read(range.get(), saved, sizeof(saved) - 1);
    ASSERT_GT(count, 0);
    range.reset();
    const int first = ntohs(server.sin_port) == 48000 || ntohs(server.sin_port) == 48001
                          ? 48010 : 48000;
    char setting[32];
    const int size = snprintf(setting, sizeof(setting), "%d %d\n", first, first + 1);
    range.reset(open(path, O_WRONLY));
    ASSERT_GE(range.get(), 0);
    ASSERT_EQ(write(range.get(), setting, size), size);
    range.reset();

    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    for (int connection = 0; connection < 10; ++connection) {
        SCOPED_TRACE(connection);
        Fd client, accepted;
        for (;;) {
            client.reset(socket(AF_INET, SOCK_STREAM, 0));
            ASSERT_GE(client.get(), 0);
            timeval timeout{3, 0};
            ASSERT_EQ(setsockopt(client.get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)), 0);
            ASSERT_EQ(setsockopt(client.get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
            if (connect(client.get(), reinterpret_cast<sockaddr*>(&server), sizeof(server)) == 0) {
                break;
            }
            const int error = errno;
            client.reset();
            ASSERT_EQ(error, EADDRNOTAVAIL);
            ASSERT_LT(std::chrono::steady_clock::now(), deadline)
                << "TIME_WAIT reuse must progress without an unrelated packet refreshing the clock";
            // This only bounds retry frequency. No payload, diagnostic socket,
            // or unrelated TCP traffic may advance the interface poll clock.
            ASSERT_EQ(poll(nullptr, 0, 25), 0);
        }
        sockaddr_in source{};
        length = sizeof(source);
        ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&source), &length), 0);
        ASSERT_GE(ntohs(source.sin_port), first);
        ASSERT_LE(ntohs(source.sin_port), first + 1);
        pollfd ready{listener.get(), POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 3000), 1);
        accepted.reset(accept(listener.get(), nullptr, nullptr));
        ASSERT_GE(accepted.get(), 0);
        timeval timeout{3, 0};
        ASSERT_EQ(setsockopt(accepted.get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
        ASSERT_EQ(shutdown(client.get(), SHUT_WR), 0);
        char byte;
        ASSERT_EQ(recv(accepted.get(), &byte, 1, 0), 0);
        ASSERT_EQ(shutdown(accepted.get(), SHUT_WR), 0);
        ASSERT_EQ(recv(client.get(), &byte, 1, 0), 0);
    }
    range.reset(open(path, O_WRONLY));
    ASSERT_GE(range.get(), 0);
    ASSERT_EQ(write(range.get(), saved, count), count);
    // Early assertion failures also exit the child and discard its private
    // namespace; this test never modifies the runner's ephemeral port range.
}

TEST(TcpPortLifetimeNamespace, AutomaticReuseAdvancesWithoutOtherTraffic) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        CheckAutomaticReuseWithoutOtherTraffic();
        const bool failed = testing::Test::HasFailure();
        fflush(nullptr);
        _exit(failed ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);
}

INSTANTIATE_TEST_SUITE_P(Loopback, TcpPortLifetime, testing::Values(AF_INET, AF_INET6),
                        [](const testing::TestParamInfo<int>& info) {
                            return info.param == AF_INET ? "IPv4" : "IPv6Only";
                        });
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
