#include <gtest/gtest.h>

#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/capability.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>

namespace {

class Fd {
  public:
    explicit Fd(int value = -1) : value_(value) {}
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    ~Fd() {
        if (value_ >= 0) close(value_);
    }
    int get() const { return value_; }

  private:
    int value_;
};

Fd RouteSocket(uint32_t groups) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return Fd();
    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    local.nl_groups = groups;
    if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) < 0) {
        close(fd);
        return Fd();
    }
    return Fd(fd);
}

uint32_t Port(int fd) {
    sockaddr_nl address{};
    socklen_t length = sizeof(address);
    if (getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length) < 0) return 0;
    return address.nl_pid;
}

std::array<uint8_t, 20> NoopWithTail() {
    std::array<uint8_t, 20> bytes{};
    nlmsghdr header{};
    header.nlmsg_len = sizeof(header);
    header.nlmsg_type = NLMSG_NOOP;
    header.nlmsg_seq = 0x42;
    header.nlmsg_pid = 0xdeadbeef;  // Payload header is not the trusted sender address.
    std::memcpy(bytes.data(), &header, sizeof(header));
    return bytes;
}

ssize_t Send(int fd, const void* bytes, size_t length, uint32_t port, uint32_t groups) {
    sockaddr_nl target{};
    target.nl_family = AF_NETLINK;
    target.nl_pid = port;
    target.nl_groups = groups;
    return sendto(fd, bytes, length, 0, reinterpret_cast<sockaddr*>(&target), sizeof(target));
}

TEST(RtnetlinkMulticastSend, HeaderOnlyProbe) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);
    auto bytes = NoopWithTail();
    ASSERT_EQ(static_cast<ssize_t>(sizeof(nlmsghdr)),
              Send(sender.get(), bytes.data(), sizeof(nlmsghdr), 0, RTMGRP_LINK))
        << std::strerror(errno);
}

TEST(RtnetlinkMulticastSend, OriginalBytesAndTrustedSender) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0) << std::strerror(errno);
    ASSERT_GE(sender.get(), 0) << std::strerror(errno);

    auto bytes = NoopWithTail();
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              Send(sender.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK))
        << std::strerror(errno);

    std::array<uint8_t, 128> received{};
    sockaddr_nl source{};
    socklen_t source_len = sizeof(source);
    ssize_t size = recvfrom(listener.get(), received.data(), received.size(), MSG_DONTWAIT,
                            reinterpret_cast<sockaddr*>(&source), &source_len);
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()), size) << std::strerror(errno);
    EXPECT_EQ(0, std::memcmp(bytes.data(), received.data(), bytes.size()));
    EXPECT_EQ(Port(sender.get()), source.nl_pid);
    EXPECT_EQ(static_cast<uint32_t>(RTMGRP_LINK), source.nl_groups);

    errno = 0;
    EXPECT_EQ(-1, recv(sender.get(), received.data(), received.size(), MSG_DONTWAIT));
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK);
}

TEST(RtnetlinkMulticastSend, LowestGroupBitOnly) {
    auto link_listener = RouteSocket(RTMGRP_LINK);
    auto neigh_listener = RouteSocket(RTMGRP_NEIGH);
    auto sender = RouteSocket(0);
    ASSERT_GE(link_listener.get(), 0);
    ASSERT_GE(neigh_listener.get(), 0);
    ASSERT_GE(sender.get(), 0);

    auto bytes = NoopWithTail();
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              Send(sender.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK | RTMGRP_NEIGH));
    std::array<uint8_t, 128> received{};
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              recv(link_listener.get(), received.data(), received.size(), MSG_DONTWAIT));
    errno = 0;
    EXPECT_EQ(-1, recv(neigh_listener.get(), received.data(), received.size(), MSG_DONTWAIT));
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK);
}

TEST(RtnetlinkMulticastSend, OpaqueBytesNeedNoRtmHeader) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);

    const uint8_t byte = 'x';
    uint8_t received = 0;
    ASSERT_EQ(1, Send(sender.get(), &byte, 1, 0, RTMGRP_LINK));
    received = 0;
    ASSERT_EQ(1, recv(listener.get(), &received, 1, MSG_DONTWAIT));
    EXPECT_EQ(byte, received);
}

TEST(RtnetlinkMulticastSend, MalformedKernelHeadersStillSendRawGroupBytes) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);
    for (uint32_t malformed_len : {8U, 1000U}) {
        nlmsghdr header{};
        header.nlmsg_len = malformed_len;
        header.nlmsg_type = RTM_GETLINK;
        header.nlmsg_flags = NLM_F_REQUEST;
        ASSERT_EQ(static_cast<ssize_t>(sizeof(header)),
                  Send(sender.get(), &header, sizeof(header), 0, RTMGRP_LINK));
        nlmsghdr received{};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(received)),
                  recv(listener.get(), &received, sizeof(received), MSG_DONTWAIT));
        EXPECT_EQ(malformed_len, received.nlmsg_len);
    }
}

TEST(RtnetlinkMulticastSend, InvalidRtmPayloadDoesNotUndoRawSend) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);
    std::array<uint8_t, sizeof(nlmsghdr) + 1> bytes{};
    nlmsghdr header{};
    header.nlmsg_len = bytes.size();
    header.nlmsg_type = RTM_GETLINK;
    header.nlmsg_flags = NLM_F_REQUEST;
    std::memcpy(bytes.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              Send(sender.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK));
    std::array<uint8_t, 32> received{};
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              recv(listener.get(), received.data(), received.size(), MSG_DONTWAIT));
    EXPECT_EQ(0, std::memcmp(bytes.data(), received.data(), bytes.size()));
}

TEST(RtnetlinkMulticastSend, NoopRequestAcknowledged) {
    auto sender = RouteSocket(0);
    ASSERT_GE(sender.get(), 0);
    nlmsghdr header{};
    header.nlmsg_len = sizeof(header);
    header.nlmsg_type = NLMSG_NOOP;
    header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    header.nlmsg_seq = 0x777;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(header)), Send(sender.get(), &header, sizeof(header), 0, 0));
    std::array<uint8_t, 128> bytes{};
    ASSERT_GE(recv(sender.get(), bytes.data(), bytes.size(), MSG_DONTWAIT),
              static_cast<ssize_t>(sizeof(nlmsghdr) + sizeof(nlmsgerr)));
    nlmsgerr error{};
    std::memcpy(&error, bytes.data() + sizeof(nlmsghdr), sizeof(error));
    EXPECT_EQ(0, error.error);
}

TEST(RtnetlinkMulticastSend, EmptySendReturnsNoData) {
    auto sender = RouteSocket(0);
    ASSERT_GE(sender.get(), 0);
    const uint8_t unused = 0;
    errno = 0;
    EXPECT_EQ(-1, Send(sender.get(), &unused, 0, 0, RTMGRP_LINK));
    EXPECT_EQ(ENODATA, errno);
}

TEST(RtnetlinkMulticastSend, OutOfBandSendDoesNotBroadcast) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);
    auto bytes = NoopWithTail();
    sockaddr_nl target{};
    target.nl_family = AF_NETLINK;
    target.nl_groups = RTMGRP_LINK;
    errno = 0;
    EXPECT_EQ(-1, sendto(sender.get(), bytes.data(), bytes.size(), MSG_OOB,
                         reinterpret_cast<sockaddr*>(&target), sizeof(target)));
    EXPECT_EQ(EOPNOTSUPP, errno);
    std::array<uint8_t, 64> received{};
    errno = 0;
    EXPECT_EQ(-1, recv(listener.get(), received.data(), received.size(), MSG_DONTWAIT));
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK);
}

TEST(RtnetlinkMulticastSend, FullListenerReportsAndRecovers) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);

    std::array<uint8_t, 512> bytes{};
    nlmsghdr header{};
    header.nlmsg_len = sizeof(header);
    header.nlmsg_type = NLMSG_NOOP;
    std::memcpy(bytes.data(), &header, sizeof(header));
    for (int i = 0; i < 600; ++i) {
        ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
                  Send(sender.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK));
    }

    int error = 0;
    socklen_t error_len = sizeof(error);
    ASSERT_EQ(0, getsockopt(listener.get(), SOL_SOCKET, SO_ERROR, &error, &error_len));
    EXPECT_EQ(ENOBUFS, error);

    int received_count = 0;
    while (recv(listener.get(), bytes.data(), bytes.size(), MSG_DONTWAIT) > 0) {
        ++received_count;
    }
    EXPECT_GT(received_count, 0);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK);

    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              Send(sender.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK));
    EXPECT_EQ(static_cast<ssize_t>(bytes.size()),
              recv(listener.get(), bytes.data(), bytes.size(), MSG_DONTWAIT));
}

TEST(RtnetlinkMulticastSend, TinyDatagramsHaveBoundedQueueOverhead) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);
    const uint8_t byte = 'x';
    for (int i = 0; i < 600; ++i) {
        ASSERT_EQ(1, Send(sender.get(), &byte, 1, 0, RTMGRP_LINK));
    }
    int error = 0;
    socklen_t error_len = sizeof(error);
    ASSERT_EQ(0, getsockopt(listener.get(), SOL_SOCKET, SO_ERROR, &error, &error_len));
    EXPECT_EQ(ENOBUFS, error);
}

TEST(RtnetlinkMulticastSend, GroupSendAlsoRunsKernelRequest) {
    auto listener = RouteSocket(RTMGRP_LINK);
    auto sender = RouteSocket(0);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(sender.get(), 0);

    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request{};
    request.header.nlmsg_len = sizeof(request);
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    request.header.nlmsg_seq = 0x5678;
    request.link.ifi_family = AF_UNSPEC;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(request)),
              Send(sender.get(), &request, sizeof(request), 0, RTMGRP_LINK));

    std::array<uint8_t, 8192> received{};
    sockaddr_nl source{};
    socklen_t source_len = sizeof(source);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(request)),
              recvfrom(listener.get(), received.data(), received.size(), MSG_DONTWAIT,
                       reinterpret_cast<sockaddr*>(&source), &source_len));
    EXPECT_EQ(Port(sender.get()), source.nl_pid);
    EXPECT_EQ(static_cast<uint32_t>(RTMGRP_LINK), source.nl_groups);

    source = {};
    source_len = sizeof(source);
    ssize_t size = recvfrom(sender.get(), received.data(), received.size(), MSG_DONTWAIT,
                            reinterpret_cast<sockaddr*>(&source), &source_len);
    ASSERT_GE(size, static_cast<ssize_t>(sizeof(nlmsghdr))) << std::strerror(errno);
    nlmsghdr reply{};
    std::memcpy(&reply, received.data(), sizeof(reply));
    EXPECT_EQ(RTM_NEWLINK, reply.nlmsg_type);
    EXPECT_EQ(request.header.nlmsg_seq, reply.nlmsg_seq);
    EXPECT_EQ(0U, source.nl_pid);
    EXPECT_EQ(0U, source.nl_groups);
}

TEST(RtnetlinkMulticastSend, DeniedExplicitDestinationDoesNotAutobind) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        Fd socket_fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
        if (socket_fd.get() < 0) _exit(1);
        __user_cap_header_struct cap_header{};
        cap_header.version = _LINUX_CAPABILITY_VERSION_3;
        __user_cap_data_struct caps[2]{};
        if (syscall(SYS_capset, &cap_header, caps) < 0) _exit(2);

        auto bytes = NoopWithTail();
        if (Send(socket_fd.get(), bytes.data(), bytes.size(), 0, RTMGRP_LINK) != -1 ||
            errno != EPERM)
            _exit(3);
        if (Port(socket_fd.get()) != 0) _exit(4);

        sockaddr_nl target{};
        target.nl_family = AF_NETLINK;
        target.nl_groups = RTMGRP_LINK;
        if (connect(socket_fd.get(), reinterpret_cast<sockaddr*>(&target), sizeof(target)) != -1 ||
            errno != EPERM)
            _exit(5);
        if (Port(socket_fd.get()) != 0) _exit(6);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
