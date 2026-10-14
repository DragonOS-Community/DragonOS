#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <cstring>
#include <string>

namespace {

class Fd {
  public:
    explicit Fd(int value) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
  private:
    int value_;
};

class UdpDisconnectSemantics : public testing::TestWithParam<int> {
  protected:
    socklen_t length() const {
        return GetParam() == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
    }
    sockaddr_storage loopback(uint16_t port = 0) const {
        sockaddr_storage result {};
        if (GetParam() == AF_INET) {
            auto* address = reinterpret_cast<sockaddr_in*>(&result);
            address->sin_family = AF_INET;
            address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            address->sin_port = htons(port);
        } else {
            auto* address = reinterpret_cast<sockaddr_in6*>(&result);
            address->sin6_family = AF_INET6;
            address->sin6_addr = in6addr_loopback;
            address->sin6_port = htons(port);
        }
        return result;
    }
    sockaddr_storage any(uint16_t port = 0) const {
        auto result = loopback(port);
        if (GetParam() == AF_INET) {
            reinterpret_cast<sockaddr_in*>(&result)->sin_addr.s_addr = htonl(INADDR_ANY);
        } else {
            reinterpret_cast<sockaddr_in6*>(&result)->sin6_addr = in6addr_any;
        }
        return result;
    }
    uint16_t port(const sockaddr_storage& address) const {
        return ntohs(GetParam() == AF_INET
            ? reinterpret_cast<const sockaddr_in*>(&address)->sin_port
            : reinterpret_cast<const sockaddr_in6*>(&address)->sin6_port);
    }
    sockaddr_storage local(int fd) const {
        sockaddr_storage result {};
        socklen_t size = sizeof(result);
        EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&result), &size), 0)
            << std::strerror(errno);
        EXPECT_EQ(size, length());
        EXPECT_EQ(result.ss_family, GetParam());
        return result;
    }
    int bind_at(int fd, const sockaddr_storage& address) const {
        return bind(fd, reinterpret_cast<const sockaddr*>(&address), length());
    }
    int connect_to(int fd, const sockaddr_storage& address) const {
        return connect(fd, reinterpret_cast<const sockaddr*>(&address), length());
    }
    void disconnect(int fd) const {
        sockaddr address {};
        address.sa_family = AF_UNSPEC;
        ASSERT_EQ(connect(fd, &address, sizeof(address.sa_family)), 0)
            << std::strerror(errno);
    }
    void expect_no_peer(int fd) const {
        sockaddr_storage address {};
        socklen_t size = sizeof(address);
        errno = 0;
        EXPECT_EQ(getpeername(fd, reinterpret_cast<sockaddr*>(&address), &size), -1);
        EXPECT_EQ(errno, ENOTCONN);
    }
    void expect_address(const sockaddr_storage& actual,
                        const sockaddr_storage& expected) const {
        EXPECT_EQ(actual.ss_family, expected.ss_family);
        EXPECT_EQ(port(actual), port(expected));
        if (GetParam() == AF_INET) {
            EXPECT_EQ(reinterpret_cast<const sockaddr_in*>(&actual)->sin_addr.s_addr,
                      reinterpret_cast<const sockaddr_in*>(&expected)->sin_addr.s_addr);
        } else {
            EXPECT_EQ(std::memcmp(&reinterpret_cast<const sockaddr_in6*>(&actual)->sin6_addr,
                                  &reinterpret_cast<const sockaddr_in6*>(&expected)->sin6_addr,
                                  sizeof(in6_addr)), 0);
        }
    }
    void send_packet(int sender, const sockaddr_storage& destination,
                     const std::string& data) const {
        ASSERT_EQ(sendto(sender, data.data(), data.size(), 0,
                         reinterpret_cast<const sockaddr*>(&destination), length()),
                  static_cast<ssize_t>(data.size())) << std::strerror(errno);
    }
    // Confirm delivery rather than sleeping. All receives remain nonblocking,
    // so a failed readiness assertion cannot leave the test stuck in recvfrom.
    void expect_packet(int receiver, const std::string& data,
                       const sockaddr_storage& sender, bool peek = false) const {
        pollfd ready {receiver, POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 2000), 1) << std::strerror(errno);
        ASSERT_NE(ready.revents & POLLIN, 0);
        std::array<char, 64> buffer {};
        sockaddr_storage source {};
        socklen_t size = sizeof(source);
        const ssize_t received = recvfrom(receiver, buffer.data(), buffer.size(),
            MSG_DONTWAIT | (peek ? MSG_PEEK : 0), reinterpret_cast<sockaddr*>(&source), &size);
        ASSERT_EQ(received, static_cast<ssize_t>(data.size())) << std::strerror(errno);
        EXPECT_EQ(std::string(buffer.data(), received), data);
        EXPECT_EQ(size, length());
        expect_address(source, sender);
    }
    void queue_packets(int receiver, int sender, const sockaddr_storage& destination) const {
        for (const char* payload : {"first", "second", "third"}) {
            ASSERT_NO_FATAL_FAILURE(send_packet(sender, destination, payload));
        }
        ASSERT_NO_FATAL_FAILURE(expect_packet(receiver, "first", local(sender), true));
    }
    void drain_packets(int receiver, const sockaddr_storage& sender) const {
        for (const char* payload : {"first", "second", "third"}) {
            ASSERT_NO_FATAL_FAILURE(expect_packet(receiver, payload, sender, true));
            ASSERT_NO_FATAL_FAILURE(expect_packet(receiver, payload, sender));
        }
        char byte;
        errno = 0;
        EXPECT_EQ(recv(receiver, &byte, sizeof(byte), MSG_DONTWAIT), -1);
        EXPECT_EQ(errno, EAGAIN);
    }
};

TEST_P(UdpDisconnectSemantics, ImplicitDisconnectPreservesQueuedDatagrams) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    const auto destination = local(receiver.get());
    ASSERT_NE(port(destination), 0);
    ASSERT_NO_FATAL_FAILURE(queue_packets(receiver.get(), sender.get(), destination));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    expect_no_peer(receiver.get());
    EXPECT_EQ(port(local(receiver.get())), 0);
    ASSERT_NO_FATAL_FAILURE(drain_packets(receiver.get(), local(sender.get())));
}

TEST_P(UdpDisconnectSemantics, RebindAndDifferentPeerDoNotRefilterQueuedDatagrams) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    Fd other(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(other.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(bind_at(other.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    const auto old = local(receiver.get());
    ASSERT_NO_FATAL_FAILURE(queue_packets(receiver.get(), sender.get(), old));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    // Reserve the released port so the new bind must choose a different one.
    Fd reserve(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(reserve.get(), 0);
    ASSERT_EQ(bind_at(reserve.get(), old), 0) << std::strerror(errno);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0) << std::strerror(errno);
    ASSERT_NE(port(local(receiver.get())), port(old));
    ASSERT_EQ(connect_to(receiver.get(), local(other.get())), 0);
    ASSERT_NO_FATAL_FAILURE(drain_packets(receiver.get(), local(sender.get())));
    ASSERT_NO_FATAL_FAILURE(send_packet(other.get(), local(receiver.get()), "new-peer"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "new-peer", local(other.get())));
}

TEST_P(UdpDisconnectSemantics, ExplicitNonzeroPortSurvivesDisconnect) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd probe(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(probe.get(), 0);
    ASSERT_EQ(bind_at(probe.get(), loopback()), 0);
    auto fixed = local(probe.get());
    ASSERT_NO_FATAL_FAILURE(disconnect(probe.get()));
    ASSERT_EQ(bind_at(receiver.get(), fixed), 0) << std::strerror(errno);
    ASSERT_EQ(bind_at(probe.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(probe.get())), 0);
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    expect_address(local(receiver.get()), fixed);
    expect_no_peer(receiver.get());
    ASSERT_NO_FATAL_FAILURE(send_packet(probe.get(), fixed, "still-bound"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "still-bound", local(probe.get())));
}

TEST_P(UdpDisconnectSemantics, BindPortZeroReleasesPortButRetainsExplicitAddress) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd probe(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(probe.get(), 0);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0);
    const auto old = local(receiver.get());
    ASSERT_NE(port(old), 0);
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    expect_address(local(receiver.get()), loopback());
    ASSERT_EQ(bind_at(probe.get(), old), 0) << std::strerror(errno);
}

TEST_P(UdpDisconnectSemantics, ConnectPortZeroIsNotUnspecDisconnect) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_EQ(connect_to(receiver.get(), loopback()), 0) << std::strerror(errno);
    const auto bound = local(receiver.get());
    ASSERT_NE(port(bound), 0);
    expect_address(bound, loopback(port(bound)));
    expect_no_peer(receiver.get());
    // Linux's port-zero association has no getpeername, but send still uses
    // its stored address/port; it is not an AF_UNSPEC disconnection.
    EXPECT_EQ(send(receiver.get(), "x", 1, MSG_DONTWAIT), 1) << std::strerror(errno);
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    EXPECT_EQ(port(local(receiver.get())), 0);
}

TEST_P(UdpDisconnectSemantics, DisconnectClearsBindToDevice) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    constexpr char device[] = "lo";
    ASSERT_EQ(setsockopt(receiver.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         device, sizeof(device)), 0) << std::strerror(errno);
    char actual[16] {};
    socklen_t size = sizeof(actual);
    ASSERT_EQ(getsockopt(receiver.get(), SOL_SOCKET, SO_BINDTODEVICE, actual, &size), 0);
    ASSERT_STREQ(actual, device);
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    size = sizeof(actual);
    ASSERT_EQ(getsockopt(receiver.get(), SOL_SOCKET, SO_BINDTODEVICE, actual, &size), 0);
    EXPECT_EQ(size, 0U);
}

TEST_P(UdpDisconnectSemantics, PortZeroPeerMatchesEverySourcePortInSharedPortLookup) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd other(socket(GetParam(), SOCK_DGRAM, 0));
    Fd first_sender(socket(GetParam(), SOCK_DGRAM, 0));
    Fd second_sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(other.get(), 0);
    ASSERT_GE(first_sender.get(), 0);
    ASSERT_GE(second_sender.get(), 0);
    int enabled = 1;
    ASSERT_EQ(setsockopt(receiver.get(), SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled)), 0);
    ASSERT_EQ(setsockopt(other.get(), SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled)), 0);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0);
    const auto destination = local(receiver.get());
    ASSERT_NE(port(destination), 0);
    ASSERT_EQ(bind_at(other.get(), destination), 0) << std::strerror(errno);
    ASSERT_EQ(bind_at(first_sender.get(), loopback()), 0);
    ASSERT_EQ(bind_at(second_sender.get(), loopback()), 0);
    ASSERT_NE(port(local(first_sender.get())), port(local(second_sender.get())));
    ASSERT_EQ(connect_to(receiver.get(), loopback()), 0) << std::strerror(errno);
    expect_no_peer(receiver.get());
    // A port-zero association still constrains the remote address and wins
    // lookup over the unconnected reuse candidate, but does not constrain the
    // source port. Two candidates exercise the registry, not its fallback.
    ASSERT_NO_FATAL_FAILURE(send_packet(first_sender.get(), destination, "first-source"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "first-source", local(first_sender.get())));
    ASSERT_NO_FATAL_FAILURE(send_packet(second_sender.get(), destination, "second-source"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "second-source", local(second_sender.get())));
    char byte;
    errno = 0;
    EXPECT_EQ(recv(other.get(), &byte, sizeof(byte), MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
}

TEST_P(UdpDisconnectSemantics, RepeatedUnconnectedDisconnectPreservesQueue) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_NO_FATAL_FAILURE(queue_packets(receiver.get(), sender.get(), local(receiver.get())));
    for (int i = 0; i < 3; ++i) {
        ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
        expect_no_peer(receiver.get());
    }
    ASSERT_NO_FATAL_FAILURE(drain_packets(receiver.get(), local(sender.get())));
}

TEST_P(UdpDisconnectSemantics, FailedRebindDoesNotDiscardQueue) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    Fd occupied(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(occupied.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(bind_at(occupied.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    ASSERT_NO_FATAL_FAILURE(queue_packets(receiver.get(), sender.get(), local(receiver.get())));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    errno = 0;
    ASSERT_EQ(bind_at(receiver.get(), local(occupied.get())), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    EXPECT_EQ(port(local(receiver.get())), 0);
    ASSERT_NO_FATAL_FAILURE(drain_packets(receiver.get(), local(sender.get())));
}

TEST_P(UdpDisconnectSemantics, WildcardPortZeroDisconnectClearsSelectedAddressAndPort) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd peer(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(peer.get(), 0);
    ASSERT_EQ(bind_at(peer.get(), loopback()), 0);
    ASSERT_EQ(bind_at(receiver.get(), any()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(peer.get())), 0);
    const auto selected = local(receiver.get());
    ASSERT_NE(port(selected), 0);
    expect_address(selected, loopback(port(selected)));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    expect_address(local(receiver.get()), any());
    expect_no_peer(receiver.get());
}

TEST_P(UdpDisconnectSemantics, WildcardFixedPortDisconnectClearsOnlySelectedAddress) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd peer(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(peer.get(), 0);
    ASSERT_EQ(bind_at(peer.get(), any()), 0);
    const auto fixed = local(peer.get());
    ASSERT_NO_FATAL_FAILURE(disconnect(peer.get()));
    ASSERT_EQ(bind_at(receiver.get(), fixed), 0);
    ASSERT_EQ(bind_at(peer.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(peer.get())), 0);
    expect_address(local(receiver.get()), loopback(port(fixed)));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    expect_address(local(receiver.get()), fixed);
    expect_no_peer(receiver.get());
    ASSERT_NO_FATAL_FAILURE(send_packet(peer.get(), loopback(port(fixed)), "wildcard"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "wildcard", local(peer.get())));
}

TEST_P(UdpDisconnectSemantics, PeekInqAndPollRetainReadinessUntilConsumption) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    constexpr char payload[] = "old-readable";
    ASSERT_NO_FATAL_FAILURE(send_packet(sender.get(), local(receiver.get()), payload));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), payload, local(sender.get()), true));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    int available = -1;
    ASSERT_EQ(ioctl(receiver.get(), FIONREAD, &available), 0) << std::strerror(errno);
    EXPECT_EQ(available, static_cast<int>(std::strlen(payload)));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), payload, local(sender.get()), true));
    ASSERT_EQ(ioctl(receiver.get(), FIONREAD, &available), 0);
    EXPECT_EQ(available, static_cast<int>(std::strlen(payload)));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), payload, local(sender.get())));
    ASSERT_EQ(ioctl(receiver.get(), FIONREAD, &available), 0);
    EXPECT_EQ(available, 0);
    pollfd ready {receiver.get(), POLLIN, 0};
    EXPECT_EQ(poll(&ready, 1, 0), 0);
    EXPECT_EQ(ready.revents & POLLIN, 0);
}

TEST_P(UdpDisconnectSemantics, FirstConnectDoesNotRefilterPreviouslyQueuedDatagrams) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    Fd other(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(other.get(), 0);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(bind_at(other.get(), loopback()), 0);
    ASSERT_NO_FATAL_FAILURE(queue_packets(receiver.get(), sender.get(), local(receiver.get())));
    ASSERT_EQ(connect_to(receiver.get(), local(other.get())), 0);
    ASSERT_NO_FATAL_FAILURE(drain_packets(receiver.get(), local(sender.get())));
    ASSERT_NO_FATAL_FAILURE(send_packet(other.get(), local(receiver.get()), "current-peer"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "current-peer", local(other.get())));
}

TEST_P(UdpDisconnectSemantics, ReleasedPortSendtoAutobindsAndPreservesExplicitAddress) {
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd reserve(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(reserve.get(), 0);
    ASSERT_EQ(bind_at(receiver.get(), loopback()), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    const auto old = local(sender.get());
    ASSERT_NO_FATAL_FAILURE(disconnect(sender.get()));
    expect_address(local(sender.get()), loopback());
    ASSERT_EQ(bind_at(reserve.get(), old), 0);
    ASSERT_NO_FATAL_FAILURE(send_packet(sender.get(), local(receiver.get()), "autobind"));
    const auto rebound = local(sender.get());
    ASSERT_NE(port(rebound), 0);
    ASSERT_NE(port(rebound), port(old));
    expect_address(rebound, loopback(port(rebound)));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "autobind", rebound));
}

TEST_P(UdpDisconnectSemantics, ZeroLengthReceiveConsumesQueuedDatagramAfterDisconnect) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    ASSERT_NO_FATAL_FAILURE(send_packet(sender.get(), local(receiver.get()), "consume-whole"));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "consume-whole", local(sender.get()), true));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    sockaddr_storage source {};
    socklen_t size = sizeof(source);
    ASSERT_EQ(recvfrom(receiver.get(), nullptr, 0, MSG_DONTWAIT,
                       reinterpret_cast<sockaddr*>(&source), &size), 0)
        << std::strerror(errno);
    EXPECT_EQ(size, length());
    expect_address(source, local(sender.get()));
    pollfd ready {receiver.get(), POLLIN, 0};
    EXPECT_EQ(poll(&ready, 1, 0), 0);
    char byte;
    errno = 0;
    EXPECT_EQ(recv(receiver.get(), &byte, sizeof(byte), MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
}

TEST_P(UdpDisconnectSemantics, ZeroLengthPeekAndTruncPreserveQueuedDatagramAfterDisconnect) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    constexpr char payload[] = "peek-whole";
    ASSERT_NO_FATAL_FAILURE(send_packet(sender.get(), local(receiver.get()), payload));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), payload, local(sender.get()), true));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    sockaddr_storage source {};
    socklen_t size = sizeof(source);
    ASSERT_EQ(recvfrom(receiver.get(), nullptr, 0, MSG_PEEK | MSG_DONTWAIT,
                       reinterpret_cast<sockaddr*>(&source), &size), 0);
    expect_address(source, local(sender.get()));
    EXPECT_EQ(size, length());
    EXPECT_EQ(recv(receiver.get(), nullptr, 0, MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT),
              static_cast<ssize_t>(std::strlen(payload))) << std::strerror(errno);
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), payload, local(sender.get())));
}

TEST_P(UdpDisconnectSemantics, EmptyDatagramRemainsReadableAfterDisconnect) {
    Fd receiver(socket(GetParam(), SOCK_DGRAM, 0));
    Fd sender(socket(GetParam(), SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_EQ(bind_at(sender.get(), loopback()), 0);
    ASSERT_EQ(connect_to(receiver.get(), local(sender.get())), 0);
    ASSERT_NO_FATAL_FAILURE(send_packet(sender.get(), local(receiver.get()), ""));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "", local(sender.get()), true));
    ASSERT_NO_FATAL_FAILURE(disconnect(receiver.get()));
    int available = -1;
    ASSERT_EQ(ioctl(receiver.get(), FIONREAD, &available), 0);
    EXPECT_EQ(available, 0);
    // FIONREAD=0 means zero payload here, not an empty receive queue.
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "", local(sender.get()), true));
    ASSERT_NO_FATAL_FAILURE(expect_packet(receiver.get(), "", local(sender.get())));
    pollfd ready {receiver.get(), POLLIN, 0};
    EXPECT_EQ(poll(&ready, 1, 0), 0);
}

TEST(UdpDisconnectAncillary, OriginalDestinationSurvivesDisconnectAndDifferentPortRebind) {
    Fd receiver(socket(AF_INET, SOCK_DGRAM, 0));
    Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
    Fd reserve(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(reserve.get(), 0);
    sockaddr_in peer {};
    peer.sin_family = AF_INET;
    peer.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(sender.get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), 0);
    socklen_t size = sizeof(peer);
    ASSERT_EQ(getsockname(sender.get(), reinterpret_cast<sockaddr*>(&peer), &size), 0);
    ASSERT_EQ(connect(receiver.get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), 0);
    sockaddr_in original {};
    size = sizeof(original);
    ASSERT_EQ(getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&original), &size), 0);
    int enabled = 1;
    ASSERT_EQ(setsockopt(receiver.get(), IPPROTO_IP, IP_RECVORIGDSTADDR,
                         &enabled, sizeof(enabled)), 0) << std::strerror(errno);
    constexpr char payload[] = "old-destination";
    ASSERT_EQ(sendto(sender.get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&original), sizeof(original)),
              static_cast<ssize_t>(sizeof(payload)));
    pollfd ready {receiver.get(), POLLIN, 0};
    ASSERT_EQ(poll(&ready, 1, 2000), 1);
    char peek[sizeof(payload)];
    ASSERT_EQ(recv(receiver.get(), peek, sizeof(peek), MSG_PEEK | MSG_DONTWAIT),
              static_cast<ssize_t>(sizeof(payload)));
    sockaddr disconnected {};
    disconnected.sa_family = AF_UNSPEC;
    ASSERT_EQ(connect(receiver.get(), &disconnected, sizeof(disconnected.sa_family)), 0);

    // A peek after disconnect verifies metadata before any new binding. The
    // consuming recvmsg below verifies it again after selecting another port.
    auto receive_original = [&](bool peek_only) {
        char data[sizeof(payload)] {};
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(sockaddr_in))] {};
        sockaddr_in source {};
        iovec vector {data, sizeof(data)};
        msghdr message {};
        message.msg_name = &source;
        message.msg_namelen = sizeof(source);
        message.msg_iov = &vector;
        message.msg_iovlen = 1;
        message.msg_control = control;
        message.msg_controllen = sizeof(control);
        ASSERT_EQ(recvmsg(receiver.get(), &message,
                         MSG_DONTWAIT | (peek_only ? MSG_PEEK : 0)),
                  static_cast<ssize_t>(sizeof(payload))) << std::strerror(errno);
        EXPECT_EQ(std::memcmp(data, payload, sizeof(payload)), 0);
        EXPECT_EQ(source.sin_port, peer.sin_port);
        EXPECT_EQ(source.sin_addr.s_addr, peer.sin_addr.s_addr);
        EXPECT_EQ(message.msg_flags & MSG_CTRUNC, 0);
        const cmsghdr* header = CMSG_FIRSTHDR(&message);
        ASSERT_NE(header, nullptr);
        ASSERT_EQ(header->cmsg_level, IPPROTO_IP);
        ASSERT_EQ(header->cmsg_type, IP_ORIGDSTADDR);
        ASSERT_EQ(header->cmsg_len, CMSG_LEN(sizeof(sockaddr_in)));
        sockaddr_in destination {};
        std::memcpy(&destination, CMSG_DATA(header), sizeof(destination));
        EXPECT_EQ(destination.sin_family, AF_INET);
        EXPECT_EQ(destination.sin_port, original.sin_port);
        EXPECT_EQ(destination.sin_addr.s_addr, original.sin_addr.s_addr);
    };
    ASSERT_NO_FATAL_FAILURE(receive_original(true));
    ASSERT_EQ(bind(reserve.get(), reinterpret_cast<sockaddr*>(&original), sizeof(original)), 0);
    sockaddr_in rebound = original;
    rebound.sin_port = 0;
    ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&rebound), sizeof(rebound)), 0);
    size = sizeof(rebound);
    ASSERT_EQ(getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&rebound), &size), 0);
    ASSERT_NE(rebound.sin_port, original.sin_port);
    ASSERT_NO_FATAL_FAILURE(receive_original(false));
}

INSTANTIATE_TEST_SUITE_P(IpFamilies, UdpDisconnectSemantics,
    testing::Values(AF_INET, AF_INET6), [](const testing::TestParamInfo<int>& info) {
        return info.param == AF_INET ? "IPv4" : "IPv6";
    });

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
