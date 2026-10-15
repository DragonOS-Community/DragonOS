#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/errqueue.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <tuple>
#include <vector>

namespace {

class Fd {
 public:
    explicit Fd(int value) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    int get() const { return value_; }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
 private:
    int value_;
};

sockaddr_storage Loopback(int family) {
    sockaddr_storage result{};
    if (family == AF_INET) {
        auto* address = reinterpret_cast<sockaddr_in*>(&result);
        address->sin_family = AF_INET;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    } else {
        auto* address = reinterpret_cast<sockaddr_in6*>(&result);
        address->sin6_family = AF_INET6;
        address->sin6_addr = in6addr_loopback;
    }
    return result;
}

socklen_t AddressLength(int family) {
    return family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
}

int SocketError(int fd) {
    int error = -1;
    socklen_t size = sizeof(error);
    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size) != 0) return -1;
    return error;
}

class UdpIcmpPolicy : public testing::TestWithParam<std::tuple<int, int>> {};

TEST_P(UdpIcmpPolicy, PmtuPolicyDoesNotSuppressConnectedPortUnreachable) {
    const auto [family, policy] = GetParam();
    Fd receiver(socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(receiver.get(), 0) << errno;
    auto destination = Loopback(family);
    const socklen_t length = AddressLength(family);
    const int reservation = socket(family, SOCK_DGRAM, 0);
    ASSERT_GE(reservation, 0) << errno;
    ASSERT_EQ(bind(reservation, reinterpret_cast<sockaddr*>(&destination), length), 0) << errno;
    socklen_t actual = sizeof(destination);
    ASSERT_EQ(getsockname(reservation, reinterpret_cast<sockaddr*>(&destination), &actual), 0);
    ASSERT_EQ(close(reservation), 0);
    ASSERT_EQ(setsockopt(receiver.get(), family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                        family == AF_INET ? IP_MTU_DISCOVER : IPV6_MTU_DISCOVER,
                        &policy, sizeof(policy)), 0) << errno;
    ASSERT_EQ(connect(receiver.get(), reinterpret_cast<sockaddr*>(&destination), length), 0) << errno;
    constexpr char payload[] = "icmp-policy";
    ASSERT_EQ(send(receiver.get(), payload, sizeof(payload), 0),
              static_cast<ssize_t>(sizeof(payload))) << errno;
    pollfd event{receiver.get(), POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 1000), 1) << errno;
    ASSERT_NE(event.revents & POLLERR, 0);
    EXPECT_EQ(SocketError(receiver.get()), ECONNREFUSED);
    EXPECT_EQ(SocketError(receiver.get()), 0);
    event.revents = 0;
    EXPECT_EQ(poll(&event, 1, 0), 0);
}

INSTANTIATE_TEST_SUITE_P(DualStack, UdpIcmpPolicy,
                        testing::Combine(testing::Values(AF_INET, AF_INET6),
                                         testing::Range(0, 6)));

uint16_t Checksum(const uint8_t* bytes, size_t length) {
    uint32_t sum = 0;
    for (size_t i = 0; i < length; i += 2) {
        sum += static_cast<uint16_t>(bytes[i]) << 8;
        if (i + 1 < length) sum += bytes[i + 1];
    }
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return static_cast<uint16_t>(~sum);
}

void Write16(uint8_t* output, uint16_t value) {
    output[0] = value >> 8;
    output[1] = value & 0xff;
}

// No outgoing-packet history or external peer is needed. Linux matches RAW
// errors using the quoted IP addresses/protocol and the socket's current state.
std::vector<uint8_t> QuotedRawError(int family, bool hard) {
    const size_t header_length = family == AF_INET ? 20 : 40;
    std::vector<uint8_t> bytes(8 + header_length + 8, 0);
    bytes[0] = hard ? (family == AF_INET ? 3 : 1) : (family == AF_INET ? 11 : 3);
    bytes[1] = hard ? (family == AF_INET ? 3 : 4) : 0;
    uint8_t* quote = bytes.data() + 8;
    if (family == AF_INET) {
        quote[0] = 0x45;
        Write16(quote + 2, header_length + 8);
        quote[8] = 64;
        quote[9] = 253;
        quote[12] = quote[16] = 127;
        quote[15] = quote[19] = 1;
        Write16(quote + 10, Checksum(quote, header_length));
    } else {
        quote[0] = 0x60;
        Write16(quote + 4, 8);
        quote[6] = 253;
        quote[7] = 64;
        quote[23] = quote[39] = 1;
    }
    for (size_t i = 0; i < 8; ++i) quote[header_length + i] = 0xa0 + i;
    if (family == AF_INET) Write16(bytes.data() + 2, Checksum(bytes.data(), bytes.size()));
    return bytes;
}

// family, connected, RECVERR, hard-error.
class RawIcmpPolicy : public testing::TestWithParam<std::tuple<int, bool, bool, bool>> {};

TEST_P(RawIcmpPolicy, ExtendedAndPendingErrorsFollowProtocolPolicy) {
    const auto [family, connected, recverr, hard] = GetParam();
    Fd receiver(socket(family, SOCK_RAW | SOCK_NONBLOCK, 253));
    ASSERT_GE(receiver.get(), 0) << errno;
    auto loopback = Loopback(family);
    const socklen_t length = AddressLength(family);
    ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&loopback), length), 0) << errno;
    if (connected) {
        ASSERT_EQ(connect(receiver.get(), reinterpret_cast<sockaddr*>(&loopback), length), 0) << errno;
    }
    const int enabled = recverr;
    ASSERT_EQ(setsockopt(receiver.get(), family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                        family == AF_INET ? IP_RECVERR : IPV6_RECVERR,
                        &enabled, sizeof(enabled)), 0) << errno;
    // Non-MTU errors must not inherit the socket's PMTU policy.
    const int policy = family == AF_INET ? IP_PMTUDISC_DONT : IPV6_PMTUDISC_DONT;
    ASSERT_EQ(setsockopt(receiver.get(), family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                        family == AF_INET ? IP_MTU_DISCOVER : IPV6_MTU_DISCOVER,
                        &policy, sizeof(policy)), 0) << errno;
    const int protocol = family == AF_INET ? static_cast<int>(IPPROTO_ICMP)
                                           : static_cast<int>(IPPROTO_ICMPV6);
    Fd injector(socket(family, SOCK_RAW, protocol));
    ASSERT_GE(injector.get(), 0) << errno;
    const auto packet = QuotedRawError(family, hard);
    ASSERT_EQ(sendto(injector.get(), packet.data(), packet.size(), 0,
                     reinterpret_cast<sockaddr*>(&loopback), length),
              static_cast<ssize_t>(packet.size())) << errno;
    pollfd event{receiver.get(), POLLERR, 0};
    const bool reported = recverr || (connected && hard);
    ASSERT_EQ(poll(&event, 1, reported ? 1000 : 50), reported ? 1 : 0) << errno;
    if (!reported) {
        EXPECT_EQ(SocketError(receiver.get()), 0);
        return;
    }
    ASSERT_NE(event.revents & POLLERR, 0);
    EXPECT_EQ(SocketError(receiver.get()), hard ? ECONNREFUSED : EHOSTUNREACH);
    EXPECT_EQ(SocketError(receiver.get()), 0);
    if (!recverr) return;
    std::array<uint8_t, 64> payload{};
    alignas(cmsghdr) std::array<uint8_t, 256> control{};
    iovec vector{payload.data(), payload.size()};
    msghdr message{};
    message.msg_iov = &vector;
    message.msg_iovlen = 1;
    message.msg_control = control.data();
    message.msg_controllen = control.size();
    ASSERT_EQ(recvmsg(receiver.get(), &message, MSG_ERRQUEUE | MSG_DONTWAIT), 8) << errno;
    for (size_t i = 0; i < 8; ++i) EXPECT_EQ(payload[i], 0xa0 + i);
    bool found = false;
    for (auto* header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
        if (header->cmsg_level != (family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6) ||
            header->cmsg_type != (family == AF_INET ? IP_RECVERR : IPV6_RECVERR)) continue;
        ASSERT_GE(header->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
        const auto* error = reinterpret_cast<const sock_extended_err*>(CMSG_DATA(header));
        EXPECT_EQ(error->ee_errno, static_cast<unsigned>(hard ? ECONNREFUSED : EHOSTUNREACH));
        EXPECT_EQ(error->ee_type, packet[0]);
        EXPECT_EQ(error->ee_code, packet[1]);
        EXPECT_EQ(error->ee_info, 0U);
        found = true;
    }
    EXPECT_TRUE(found);
}

INSTANTIATE_TEST_SUITE_P(DualStack, RawIcmpPolicy,
                        testing::Combine(testing::Values(AF_INET, AF_INET6), testing::Bool(),
                                         testing::Bool(), testing::Bool()));

int SetErrorReporting(int fd, int family, bool enabled) {
    const int value = enabled;
    return setsockopt(fd, family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                      family == AF_INET ? IP_RECVERR : IPV6_RECVERR, &value, sizeof(value));
}

ssize_t ReadExtendedError(int fd, int family, sock_extended_err* result) {
    std::array<uint8_t, 128> payload{};
    alignas(cmsghdr) std::array<uint8_t, 256> control{};
    iovec vector{payload.data(), payload.size()};
    msghdr message{};
    message.msg_iov = &vector; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    const ssize_t size = recvmsg(fd, &message, MSG_ERRQUEUE | MSG_DONTWAIT);
    if (size < 0) return size;
    for (auto* header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
        if (header->cmsg_level == (family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6) &&
            header->cmsg_type == (family == AF_INET ? IP_RECVERR : IPV6_RECVERR) &&
            header->cmsg_len >= CMSG_LEN(sizeof(*result))) {
            memcpy(result, CMSG_DATA(header), sizeof(*result));
            return size;
        }
    }
    errno = EPROTO;
    return -1;
}

void FixChecksums(int family, std::vector<uint8_t>* packet) {
    if (family != AF_INET) return;  // The ICMPv6 raw sender supplies its checksum.
    auto& bytes = *packet;
    uint8_t* quote = bytes.data() + 8;
    Write16(quote + 10, 0);
    Write16(quote + 10, Checksum(quote, 20));
    Write16(bytes.data() + 2, 0);
    Write16(bytes.data() + 2, Checksum(bytes.data(), bytes.size()));
}

ssize_t InjectError(int family, std::vector<uint8_t> packet, bool bad_checksum = false) {
    const int protocol = family == AF_INET ? static_cast<int>(IPPROTO_ICMP)
                                           : static_cast<int>(IPPROTO_ICMPV6);
    Fd injector(socket(family, SOCK_RAW, protocol));
    if (injector.get() < 0) return -1;
    FixChecksums(family, &packet);
    if (bad_checksum && family == AF_INET) packet[2] ^= 1;
    if (bad_checksum && family == AF_INET6) {
        const int enabled = 1;
        if (setsockopt(injector.get(), IPPROTO_IPV6, IPV6_HDRINCL,
                       &enabled, sizeof(enabled)) < 0) return -1;
        std::vector<uint8_t> pseudo(40 + packet.size(), 0);
        pseudo[15] = pseudo[31] = 1;
        Write16(pseudo.data() + 34, packet.size());
        pseudo[39] = IPPROTO_ICMPV6;
        std::copy(packet.begin(), packet.end(), pseudo.begin() + 40);
        // Corrupt the actual checksum, not a guessed value which might be
        // correct for an unlucky pair of automatically allocated UDP ports.
        Write16(packet.data() + 2, Checksum(pseudo.data(), pseudo.size()) ^ 1);
        std::vector<uint8_t> outer(40 + packet.size(), 0);
        outer[0] = 0x60;
        Write16(outer.data() + 4, packet.size());
        outer[6] = IPPROTO_ICMPV6; outer[7] = 64;
        outer[23] = outer[39] = 1;
        // HDRINCL preserves the supplied invalid checksum.
        std::copy(packet.begin(), packet.end(), outer.begin() + 40);
        packet = std::move(outer);
    }
    auto destination = Loopback(family);
    return sendto(injector.get(), packet.data(), packet.size(), 0,
                  reinterpret_cast<sockaddr*>(&destination), AddressLength(family));
}

class IcmpBoundary : public testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        family = GetParam();
        receiver = socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        ASSERT_GE(receiver, 0) << errno;
        peer = socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        ASSERT_GE(peer, 0) << errno;
        remote = Loopback(family);
        ASSERT_EQ(bind(peer, reinterpret_cast<sockaddr*>(&remote), AddressLength(family)), 0);
        socklen_t length = sizeof(remote);
        ASSERT_EQ(getsockname(peer, reinterpret_cast<sockaddr*>(&remote), &length), 0);
        ASSERT_EQ(connect(receiver, reinterpret_cast<sockaddr*>(&remote), AddressLength(family)), 0);
        length = sizeof(local);
        ASSERT_EQ(getsockname(receiver, reinterpret_cast<sockaddr*>(&local), &length), 0);
        ASSERT_EQ(SetErrorReporting(receiver, family, true), 0);
    }
    void TearDown() override {
        if (receiver >= 0) close(receiver);
        if (peer >= 0) close(peer);
    }
    std::vector<uint8_t> UdpError() const {
        auto packet = QuotedRawError(family, true);
        const size_t header_length = family == AF_INET ? 20 : 40;
        packet.resize(packet.size() + 8, 0);
        auto* quote = packet.data() + 8;
        if (family == AF_INET) {
            quote[9] = IPPROTO_UDP;
            Write16(quote + 2, header_length + 16);
        } else {
            quote[6] = IPPROTO_UDP;
            Write16(quote + 4, 16);
        }
        const uint16_t source_port = family == AF_INET
            ? reinterpret_cast<const sockaddr_in*>(&local)->sin_port
            : reinterpret_cast<const sockaddr_in6*>(&local)->sin6_port;
        const uint16_t destination_port = family == AF_INET
            ? reinterpret_cast<const sockaddr_in*>(&remote)->sin_port
            : reinterpret_cast<const sockaddr_in6*>(&remote)->sin6_port;
        Write16(quote + header_length, ntohs(source_port));
        Write16(quote + header_length + 2, ntohs(destination_port));
        Write16(quote + header_length + 4, 16);
        FixChecksums(family, &packet);
        return packet;
    }
    void AwaitError(int fd, int expected_errno, uint32_t expected_info = 0) {
        pollfd event{fd, POLLERR, 0};
        ASSERT_EQ(poll(&event, 1, 1000), 1) << errno;
        EXPECT_EQ(SocketError(fd), expected_errno);
        sock_extended_err error{};
        ASSERT_GE(ReadExtendedError(fd, family, &error), 0) << errno;
        EXPECT_EQ(error.ee_errno, static_cast<unsigned>(expected_errno));
        EXPECT_EQ(error.ee_info, expected_info);
        EXPECT_EQ(SocketError(fd), 0);
    }
    int family = AF_UNSPEC;
    int receiver = -1;
    int peer = -1;
    sockaddr_storage local{};
    sockaddr_storage remote{};
};

TEST_P(IcmpBoundary, ParameterProblemPreservesPointerForUdpAndRaw) {
    for (bool raw : {false, true}) {
        SCOPED_TRACE(raw);
        Fd raw_socket(raw ? socket(family, SOCK_RAW | SOCK_NONBLOCK, 253) : -1);
        const int fd = raw ? raw_socket.get() : receiver;
        ASSERT_GE(fd, 0);
        if (raw) {
            auto address = Loopback(family);
            ASSERT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&address), AddressLength(family)), 0);
            ASSERT_EQ(SetErrorReporting(fd, family, true), 0);
        }
        auto packet = raw ? QuotedRawError(family, true) : UdpError();
        packet[0] = family == AF_INET ? 12 : 4;
        packet[1] = 0;
        const uint32_t pointer = family == AF_INET ? 7 : 0x010203;
        if (family == AF_INET) packet[4] = pointer;
        else { packet[5] = 1; packet[6] = 2; packet[7] = 3; }
        ASSERT_GT(InjectError(family, packet), 0) << errno;
        ASSERT_NO_FATAL_FAILURE(AwaitError(fd, EPROTO, pointer));
    }
}

TEST_P(IcmpBoundary, InvalidQuotesDoNotReportAndValidCounterpartDoes) {
    const auto valid = UdpError();
    const size_t header_length = family == AF_INET ? 20 : 40;
    // Source, destination, local port, remote port, malformed IHL/noninitial
    // IPv6 fragment, truncated quote, corrupt outer checksum. Every rejection
    // is followed by a valid positive. Linux accepts a synthetic IPv4 fragment
    // quote containing UDP-looking bytes, so it is not a rejection oracle.
    for (int mutation = 0; mutation < 7; ++mutation) {
        SCOPED_TRACE(mutation);
        auto packet = valid;
        auto* quote = packet.data() + 8;
        switch (mutation) {
            case 0: quote[family == AF_INET ? 15 : 23] = 2; break;
            case 1: quote[family == AF_INET ? 19 : 39] = 2; break;
            case 2: quote[header_length + 1] ^= 1; break;
            case 3: quote[header_length + 3] ^= 1; break;
            case 4:
                if (family == AF_INET) quote[0] = 0x44;
                else {
                    packet.insert(packet.begin() + 8 + 40, 8, 0);
                    quote = packet.data() + 8;
                    quote[6] = IPPROTO_FRAGMENT; quote[40] = IPPROTO_UDP;
                    Write16(quote + 42, 8); Write16(quote + 4, 24);
                }
                break;
            case 5: packet.resize(8 + header_length + 7); break;
            default: break;
        }
        ASSERT_GT(InjectError(family, packet, mutation == 6), 0) << errno;
        pollfd event{receiver, POLLERR, 0};
        ASSERT_EQ(poll(&event, 1, 50), 0);
        EXPECT_EQ(SocketError(receiver), 0);
        ASSERT_GT(InjectError(family, valid), 0) << errno;
        ASSERT_NO_FATAL_FAILURE(AwaitError(receiver, ECONNREFUSED));
    }
}

TEST_P(IcmpBoundary, FamilySpecificUnknownCodesAndReassemblyTimeout) {
    auto packet = UdpError();
    packet[1] = 31;
    ASSERT_GT(InjectError(family, packet), 0);
    if (family == AF_INET6) {
        ASSERT_NO_FATAL_FAILURE(AwaitError(receiver, EPROTO));
        Fd raw_socket(socket(family, SOCK_RAW | SOCK_NONBLOCK, 253));
        ASSERT_GE(raw_socket.get(), 0);
        ASSERT_EQ(SetErrorReporting(raw_socket.get(), family, true), 0);
        // Linux forwards unknown error types (<128), not informational types.
        // They use the default soft EPROTO conversion and preserve raw info.
        for (uint8_t type : {uint8_t{0}, uint8_t{5}, uint8_t{127}}) {
            for (bool raw : {false, true}) {
                auto unknown = raw ? QuotedRawError(family, true) : UdpError();
                unknown[0] = type; unknown[1] = 17;
                unknown[4] = 1; unknown[5] = 2; unknown[6] = 3; unknown[7] = 4;
                ASSERT_GT(InjectError(family, unknown), 0);
                ASSERT_NO_FATAL_FAILURE(AwaitError(raw ? raw_socket.get() : receiver,
                                                  EPROTO, 0x01020304));
                sock_extended_err empty{};
                EXPECT_EQ(ReadExtendedError(raw ? raw_socket.get() : receiver, family, &empty), -1);
                EXPECT_EQ(errno, EAGAIN);
            }
        }
        auto informational = UdpError();
        informational[0] = 128;
        ASSERT_GT(InjectError(family, informational), 0);
        pollfd ignored{receiver, POLLERR, 0};
        EXPECT_EQ(poll(&ignored, 1, 50), 0);
        auto final_error = UdpError(); final_error[0] = 5;
        ASSERT_GT(InjectError(family, final_error), 0);
        ASSERT_NO_FATAL_FAILURE(AwaitError(receiver, EPROTO));
        return;
    }
    pollfd event{receiver, POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 50), 0);
    packet[0] = 11; packet[1] = 1;
    ASSERT_GT(InjectError(family, packet), 0);
    ASSERT_EQ(poll(&event, 1, 50), 0);
    ASSERT_GT(InjectError(family, UdpError()), 0);
    ASSERT_NO_FATAL_FAILURE(AwaitError(receiver, ECONNREFUSED));
}

TEST_P(IcmpBoundary, DisablingRecvErrorPurgesQueueButPreservesPendingError) {
    for (bool raw : {false, true}) {
        SCOPED_TRACE(raw);
        Fd raw_socket(raw ? socket(family, SOCK_RAW | SOCK_NONBLOCK, 253) : -1);
        const int fd = raw ? raw_socket.get() : receiver;
        ASSERT_GE(fd, 0);
        if (raw) {
            auto address = Loopback(family);
            ASSERT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&address), AddressLength(family)), 0);
            ASSERT_EQ(SetErrorReporting(fd, family, true), 0);
        }
        ASSERT_GT(InjectError(family, raw ? QuotedRawError(family, true) : UdpError()), 0);
        pollfd event{fd, POLLERR, 0};
        ASSERT_EQ(poll(&event, 1, 1000), 1);
        ASSERT_EQ(SetErrorReporting(fd, family, false), 0);
        sock_extended_err error{};
        ASSERT_EQ(ReadExtendedError(fd, family, &error), -1);
        EXPECT_EQ(errno, EAGAIN);
        EXPECT_EQ(SocketError(fd), ECONNREFUSED);
        EXPECT_EQ(SocketError(fd), 0);
        ASSERT_EQ(SetErrorReporting(fd, family, true), 0);
        ASSERT_EQ(ReadExtendedError(fd, family, &error), -1);
        EXPECT_EQ(errno, EAGAIN);
    }
}

TEST_P(IcmpBoundary, OrdinaryDataAndExtendedErrorRemainIndependent) {
    constexpr char payload[] = "ordinary-data";
    ASSERT_EQ(sendto(peer, payload, sizeof(payload), 0, reinterpret_cast<sockaddr*>(&local),
                     AddressLength(family)), static_cast<ssize_t>(sizeof(payload)));
    pollfd data_ready{receiver, POLLIN, 0};
    ASSERT_EQ(poll(&data_ready, 1, 1000), 1);
    ASSERT_NE(data_ready.revents & POLLIN, 0);
    ASSERT_GT(InjectError(family, UdpError()), 0);
    pollfd event{receiver, POLLIN | POLLERR, 0};
    ASSERT_EQ(poll(&event, 1, 1000), 1);
    ASSERT_NE(event.revents & POLLIN, 0);
    ASSERT_NE(event.revents & POLLERR, 0);
    EXPECT_EQ(SocketError(receiver), ECONNREFUSED);
    std::array<char, 64> bytes{};
    ASSERT_EQ(recv(receiver, bytes.data(), bytes.size(), MSG_DONTWAIT),
              static_cast<ssize_t>(sizeof(payload))) << errno;
    EXPECT_EQ(memcmp(bytes.data(), payload, sizeof(payload)), 0);
    sock_extended_err error{};
    ASSERT_EQ(ReadExtendedError(receiver, family, &error), 8);
    EXPECT_EQ(error.ee_errno, static_cast<unsigned>(ECONNREFUSED));
}

INSTANTIATE_TEST_SUITE_P(DualStack, IcmpBoundary, testing::Values(AF_INET, AF_INET6));

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
