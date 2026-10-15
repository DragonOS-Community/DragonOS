#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <linux/errqueue.h>
#include <fcntl.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <array>
#include <chrono>
#include <cstring>
#include <tuple>
#include <vector>

namespace {
class Fd {
 public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    int get() const { return value_; }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
 private:
    int value_;
};

sockaddr_storage Loopback(int family) {
    sockaddr_storage storage{};
    if (family == AF_INET) {
        auto* address = reinterpret_cast<sockaddr_in*>(&storage);
        address->sin_family = family;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    } else {
        auto* address = reinterpret_cast<sockaddr_in6*>(&storage);
        address->sin6_family = family;
        address->sin6_addr = in6addr_loopback;
    }
    return storage;
}

socklen_t AddressLength(int family) {
    return family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
}

uint16_t Port(const sockaddr_storage& storage) {
    return storage.ss_family == AF_INET ? reinterpret_cast<const sockaddr_in*>(&storage)->sin_port
                                      : reinterpret_cast<const sockaddr_in6*>(&storage)->sin6_port;
}

void SetErrorOption(int fd, int family, int enabled) {
    ASSERT_EQ(setsockopt(fd, family == AF_INET ? SOL_IP : SOL_IPV6,
                        family == AF_INET ? IP_RECVERR : IPV6_RECVERR,
                        &enabled, sizeof(enabled)), 0) << errno;
}

uint16_t Checksum(const uint8_t* bytes, size_t size) {
    unsigned sum = 0;
    for (size_t i = 0; i < size; i += 2) {
        sum += unsigned(bytes[i]) << 8;
        if (i + 1 < size) sum += bytes[i + 1];
    }
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return static_cast<uint16_t>(~sum);
}

// Capture the real final handshake ACK. Unlike a data quotation, this remains
// in the inclusive snd_una..snd_nxt interval after the peer's immediate ACKs.
std::vector<uint8_t> CaptureTcp(int fd, int family, uint16_t source, uint16_t destination, bool syn = false) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(1);
    std::array<uint8_t, 2048> bytes{};
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd ready{fd, POLLIN, 0};
        if (poll(&ready, 1, 100) <= 0) continue;
        const ssize_t got = recv(fd, bytes.data(), bytes.size(), MSG_DONTWAIT);
        if (got <= 0) continue;
        const size_t offset = family == AF_INET ? (bytes[0] & 15u) * 4u : 0u;
        if (static_cast<size_t>(got) < offset + 20) continue;
        const auto* tcp = bytes.data() + offset;
        if (memcmp(tcp, &source, 2) || memcmp(tcp + 2, &destination, 2) ||
            (tcp[13] & 0x16) != (syn ? 0x02 : 0x10)) continue;
        if (family == AF_INET) return {bytes.begin(), bytes.begin() + got};
        std::vector<uint8_t> quote(40 + got, 0);
        quote[0] = 0x60;
        quote[4] = static_cast<uint8_t>(got >> 8);
        quote[5] = static_cast<uint8_t>(got);
        quote[6] = IPPROTO_TCP; quote[7] = 64;
        quote[23] = 1; quote[39] = 1;
        memcpy(quote.data() + 40, bytes.data(), got);
        return quote;
    }
    return {};
}

void InjectError(int family, const std::vector<uint8_t>& quote, uint8_t type, uint8_t code) {
    Fd sender(socket(family, SOCK_RAW, family == AF_INET ? int(IPPROTO_ICMP) : int(IPPROTO_ICMPV6)));
    ASSERT_GE(sender.get(), 0) << errno;
    std::vector<uint8_t> bytes(8 + quote.size(), 0);
    bytes[0] = type; bytes[1] = code;
    memcpy(bytes.data() + 8, quote.data(), quote.size());
    if (family == AF_INET) {
        const uint16_t checksum = Checksum(bytes.data(), bytes.size());
        bytes[2] = checksum >> 8; bytes[3] = checksum;
    }
    auto destination = Loopback(family);
    ASSERT_EQ(sendto(sender.get(), bytes.data(), bytes.size(), 0,
                     reinterpret_cast<const sockaddr*>(&destination), AddressLength(family)),
              static_cast<ssize_t>(bytes.size())) << errno;
}

class TcpIcmpErrors : public testing::TestWithParam<std::tuple<int, bool>> {};

TEST_P(TcpIcmpErrors, EstablishedErrorPolicyPreservesConnection) {
    const auto [family, enabled] = GetParam();
    Fd capture(socket(family, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_TCP));
    Fd listener(socket(family, SOCK_STREAM, 0));
    Fd client(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(capture.get(), 0) << errno;
    ASSERT_GE(listener.get(), 0); ASSERT_GE(client.get(), 0);
    SetErrorOption(client.get(), family, enabled);
    auto address = Loopback(family); socklen_t length = AddressLength(family);
    ASSERT_EQ(bind(listener.get(), reinterpret_cast<sockaddr*>(&address), length), 0) << errno;
    ASSERT_EQ(getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
    ASSERT_EQ(listen(listener.get(), 1), 0);
    ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&address), length), 0) << errno;
    Fd accepted(accept(listener.get(), nullptr, nullptr)); ASSERT_GE(accepted.get(), 0) << errno;
    auto source = Loopback(family); length = AddressLength(family);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&source), &length), 0);
    const auto quote = CaptureTcp(capture.get(), family, Port(source), Port(address));
    ASSERT_FALSE(quote.empty()) << "handshake ACK capture";
    const size_t transport = family == AF_INET ? (quote[0] & 15u) * 4u : 40u;
    auto future = quote;
    uint32_t seq = 0;
    memcpy(&seq, future.data() + transport + 4, sizeof(seq));
    seq = htonl(ntohl(seq) + 1);
    memcpy(future.data() + transport + 4, &seq, sizeof(seq));
    InjectError(family, future, family == AF_INET ? 3 : 1, family == AF_INET ? 3 : 4);
    pollfd ignored{client.get(), POLLERR, 0};
    EXPECT_EQ(poll(&ignored, 1, 30), 0) << "future quoted sequence must be ignored";
    int absent = -1; length = sizeof(absent);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &absent, &length), 0);
    EXPECT_EQ(absent, 0);
    auto wrong_port = quote;
    wrong_port[transport] ^= 0x40;
    InjectError(family, wrong_port, family == AF_INET ? 3 : 1, family == AF_INET ? 3 : 4);
    ignored.revents = 0;
    EXPECT_EQ(poll(&ignored, 1, 30), 0) << "different quoted tuple must be ignored";
    absent = -1; length = sizeof(absent);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &absent, &length), 0);
    EXPECT_EQ(absent, 0);
    if (family == AF_INET6) {
        InjectError(family, quote, 5, 17);
        ignored.revents = 0;
        EXPECT_EQ(poll(&ignored, 1, 200), enabled ? 1 : 0);
        int unknown = -1; length = sizeof(unknown);
        ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &unknown, &length), 0);
        EXPECT_EQ(unknown, EPROTO);
        unknown = -1; length = sizeof(unknown);
        ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &unknown, &length), 0);
        EXPECT_EQ(unknown, 0);
    }
    InjectError(family, quote, family == AF_INET ? 3 : 1, family == AF_INET ? 3 : 4);
    pollfd error{client.get(), POLLERR, 0};
    EXPECT_EQ(poll(&error, 1, 200), enabled ? 1 : 0);
    if (enabled) { EXPECT_NE(error.revents & POLLERR, 0); }
    if (!enabled) {
        // An empty ordinary read neither fails for nor consumes a soft error.
        // Successful ACK progress is separately allowed to clear stale errors.
        char delivered{};
        EXPECT_EQ(recv(client.get(), &delivered, 1, MSG_DONTWAIT), -1);
        EXPECT_EQ(errno, EAGAIN);
    }
    int pending = 0; length = sizeof(pending);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &pending, &length), 0);
    EXPECT_EQ(pending, ECONNREFUSED);
    pending = -1; length = sizeof(pending);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &pending, &length), 0);
    EXPECT_EQ(pending, 0);
    char byte{}; iovec iov{&byte, 1}; msghdr message{};
    message.msg_iov = &iov; message.msg_iovlen = 1;
    EXPECT_EQ(recvmsg(client.get(), &message, MSG_ERRQUEUE | MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
    EXPECT_EQ(send(client.get(), "x", 1, MSG_NOSIGNAL), 1);
    pollfd data{accepted.get(), POLLIN, 0};
    ASSERT_EQ(poll(&data, 1, 1000), 1);
    EXPECT_EQ(recv(accepted.get(), &byte, 1, MSG_DONTWAIT), 1);
    EXPECT_EQ(byte, 'x');
}

TEST_P(TcpIcmpErrors, PassiveOpenInheritsAtSynNotAccept) {
    const auto [family, enabled] = GetParam();
    Fd listener(socket(family, SOCK_STREAM, 0));
    Fd client(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(listener.get(), 0); ASSERT_GE(client.get(), 0);
    SetErrorOption(listener.get(), family, enabled);
    auto address = Loopback(family); socklen_t length = AddressLength(family);
    ASSERT_EQ(bind(listener.get(), reinterpret_cast<sockaddr*>(&address), length), 0);
    ASSERT_EQ(getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
    ASSERT_EQ(listen(listener.get(), 1), 0);
    ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&address), length), 0);
    SetErrorOption(listener.get(), family, !enabled);
    Fd accepted(accept(listener.get(), nullptr, nullptr)); ASSERT_GE(accepted.get(), 0);
    int value = -1; length = sizeof(value);
    ASSERT_EQ(getsockopt(accepted.get(), family == AF_INET ? SOL_IP : SOL_IPV6,
                        family == AF_INET ? IP_RECVERR : IPV6_RECVERR, &value, &length), 0);
    EXPECT_EQ(value, enabled);
}

TEST_P(TcpIcmpErrors, SynErrorCompletesConnectAndPreservesQuotedTcpHeader) {
    const auto [family, enabled] = GetParam();
    Fd capture(socket(family, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_TCP));
    Fd listener(socket(family, SOCK_STREAM, 0));
    Fd blocker(socket(family, SOCK_STREAM, 0));
    Fd client(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(capture.get(), 0); ASSERT_GE(listener.get(), 0);
    ASSERT_GE(blocker.get(), 0); ASSERT_GE(client.get(), 0);
    auto address = Loopback(family); socklen_t length = AddressLength(family);
    ASSERT_EQ(bind(listener.get(), reinterpret_cast<sockaddr*>(&address), length), 0);
    ASSERT_EQ(getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
    // A zero-backlog listener admits its first child, then silently leaves the
    // next SYN pending. This gives us a real SYN_SENT without firewall helpers.
    ASSERT_EQ(listen(listener.get(), 0), 0);
    ASSERT_EQ(connect(blocker.get(), reinterpret_cast<sockaddr*>(&address), length), 0);
    SetErrorOption(client.get(), family, enabled);
    ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&address), length), -1);
    ASSERT_EQ(errno, EINPROGRESS);
    pollfd progress{client.get(), POLLOUT, 0};
    ASSERT_EQ(poll(&progress, 1, 20), 0) << "backlog did not hold SYN_SENT";
    auto source = Loopback(family); length = AddressLength(family);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&source), &length), 0);
    const auto quote = CaptureTcp(capture.get(), family, Port(source), Port(address), true);
    ASSERT_FALSE(quote.empty()) << "real outstanding SYN";
    InjectError(family, quote, family == AF_INET ? 3 : 1, family == AF_INET ? 3 : 4);
    progress = {client.get(), POLLOUT, 0};
    ASSERT_EQ(poll(&progress, 1, 1000), 1);
    EXPECT_NE(progress.revents & POLLERR, 0);
    EXPECT_NE(progress.revents & POLLHUP, 0);
    auto expect_no_peer = [&] {
        sockaddr_storage peer{}; socklen_t size = sizeof(peer);
        ASSERT_EQ(getpeername(client.get(), reinterpret_cast<sockaddr*>(&peer), &size), -1);
        ASSERT_EQ(errno, ENOTCONN);
    };
    ASSERT_NO_FATAL_FAILURE(expect_no_peer());
    int pending = 0; length = sizeof(pending);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &pending, &length), 0);
    EXPECT_EQ(pending, ECONNREFUSED);
    ASSERT_NO_FATAL_FAILURE(expect_no_peer());
    std::array<char, 256> payload{}, control{};
    iovec iov{payload.data(), payload.size()}; msghdr message{};
    message.msg_iov = &iov; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    const ssize_t got = recvmsg(client.get(), &message, MSG_ERRQUEUE | MSG_DONTWAIT);
    if (family == AF_INET || enabled) {
        ASSERT_GE(got, 20);
        const uint16_t source_port = Port(source), destination_port = Port(address);
        EXPECT_EQ(memcmp(payload.data(), &source_port, 2), 0);
        EXPECT_EQ(memcmp(payload.data() + 2, &destination_port, 2), 0);
        bool found = false;
        for (auto* cmsg = CMSG_FIRSTHDR(&message); cmsg; cmsg = CMSG_NXTHDR(&message, cmsg)) {
            if (cmsg->cmsg_level != (family == AF_INET ? SOL_IP : SOL_IPV6) ||
                cmsg->cmsg_type != (family == AF_INET ? IP_RECVERR : IPV6_RECVERR)) continue;
            ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
            const auto* ee = reinterpret_cast<const sock_extended_err*>(CMSG_DATA(cmsg));
            EXPECT_EQ(ee->ee_errno, static_cast<unsigned>(ECONNREFUSED));
            EXPECT_EQ(ee->ee_type, family == AF_INET ? 3 : 1);
            EXPECT_EQ(ee->ee_code, family == AF_INET ? 3 : 4);
            found = true;
        }
        EXPECT_TRUE(found);
    } else {
        EXPECT_EQ(got, -1); EXPECT_EQ(errno, EAGAIN);
    }
    pending = -1; length = sizeof(pending);
    ASSERT_EQ(getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &pending, &length), 0);
    EXPECT_EQ(pending, 0);
    ASSERT_NO_FATAL_FAILURE(expect_no_peer());
}

INSTANTIATE_TEST_SUITE_P(DualStack, TcpIcmpErrors,
    testing::Combine(testing::Values(AF_INET, AF_INET6), testing::Bool()));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
