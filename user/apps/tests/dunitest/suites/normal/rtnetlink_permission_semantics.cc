#include <gtest/gtest.h>

#include <linux/capability.h>
#include <linux/if_addr.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <functional>
#include <string>
#include <vector>

namespace {

constexpr int kInvalidIfindex = 0x3fffffff;
constexpr uint16_t kRtmNewQdisc = 36;
constexpr uint16_t kRtmGetQdisc = 38;

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

struct ChildResult {
    int stage;
    int actual;
};

ChildResult Success() { return {0, 0}; }
ChildResult Failure(int stage, int actual) { return {stage, actual}; }

void ExpectChildSuccess(const std::function<ChildResult()>& child_fn) {
    int pipe_fds[2];
    ASSERT_EQ(0, pipe(pipe_fds)) << std::strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << std::strerror(errno);
    if (child == 0) {
        close(pipe_fds[0]);
        const ChildResult result = child_fn();
        const ssize_t ignored = write(pipe_fds[1], &result, sizeof(result));
        (void)ignored;
        close(pipe_fds[1]);
        _exit(result.stage == 0 ? 0 : 1);
    }

    close(pipe_fds[1]);
    ChildResult result{-1, 0};
    const ssize_t bytes = read(pipe_fds[0], &result, sizeof(result));
    close(pipe_fds[0]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(result)), bytes);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status)) << "stage=" << result.stage
                                      << " actual_errno=" << result.actual << " ("
                                      << std::strerror(result.actual) << ")";
}

int OpenRouteSocket() {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout{};
    timeout.tv_sec = 3;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

bool GetCaps(__user_cap_data_struct data[2]) {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    header.pid = 0;
    return syscall(SYS_capget, &header, data) == 0;
}

bool SetCaps(const __user_cap_data_struct data[2]) {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    header.pid = 0;
    return syscall(SYS_capset, &header, data) == 0;
}

bool DropAllCapabilities() {
    __user_cap_data_struct data[2]{};
    return SetCaps(data);
}

bool SetNetAdminEffective(bool enabled) {
    __user_cap_data_struct data[2]{};
    if (!GetCaps(data)) return false;

    constexpr uint32_t bit = uint32_t{1} << CAP_NET_ADMIN;
    if ((data[0].permitted & bit) == 0) return false;
    if (enabled) {
        data[0].effective |= bit;
    } else {
        data[0].effective &= ~bit;
    }
    return SetCaps(data);
}

void AddAttr(std::vector<uint8_t>* request, uint16_t type, const void* data, size_t len) {
    auto* header = reinterpret_cast<nlmsghdr*>(request->data());
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    const size_t attr_len = RTA_LENGTH(len);
    const size_t end = offset + RTA_ALIGN(attr_len);
    request->resize(end, 0);

    header = reinterpret_cast<nlmsghdr*>(request->data());
    auto* attr = reinterpret_cast<rtattr*>(request->data() + offset);
    attr->rta_type = type;
    attr->rta_len = attr_len;
    std::memcpy(RTA_DATA(attr), data, len);
    header->nlmsg_len = end;
}

std::vector<uint8_t> BuildRequest(uint16_t type, uint16_t flags, uint32_t seq) {
    size_t payload_len = 1;
    if (type >= RTM_NEWLINK && type <= RTM_SETLINK) {
        payload_len = sizeof(ifinfomsg);
    } else if (type >= RTM_NEWADDR && type <= RTM_GETADDR) {
        payload_len = sizeof(ifaddrmsg);
    } else if ((type >= RTM_NEWROUTE && type <= RTM_GETROUTE) || type == RTM_GETRULE) {
        payload_len = sizeof(rtmsg);
    } else if (type >= RTM_NEWNEIGH && type <= RTM_GETNEIGH) {
        payload_len = sizeof(ndmsg);
    }

    std::vector<uint8_t> request(NLMSG_SPACE(payload_len), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = NLMSG_LENGTH(payload_len);
    header->nlmsg_type = type;
    header->nlmsg_flags = flags;
    header->nlmsg_seq = seq;

    if (payload_len == sizeof(ifinfomsg)) {
        auto* msg = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header));
        msg->ifi_family = AF_UNSPEC;
        if (type != RTM_GETLINK) msg->ifi_index = kInvalidIfindex;
    } else if (payload_len == sizeof(ifaddrmsg)) {
        auto* msg = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(header));
        msg->ifa_family = AF_INET;
        if (type != RTM_GETADDR) msg->ifa_index = kInvalidIfindex;
    } else if (payload_len == sizeof(rtmsg)) {
        auto* msg = reinterpret_cast<rtmsg*>(NLMSG_DATA(header));
        msg->rtm_family = AF_INET;
        msg->rtm_table = RT_TABLE_MAIN;
        msg->rtm_protocol = RTPROT_STATIC;
        msg->rtm_scope = RT_SCOPE_LINK;
        msg->rtm_type = RTN_UNICAST;
    } else if (payload_len == sizeof(ndmsg)) {
        auto* msg = reinterpret_cast<ndmsg*>(NLMSG_DATA(header));
        msg->ndm_family = AF_INET;
        if (type != RTM_GETNEIGH) msg->ndm_ifindex = kInvalidIfindex;
    } else {
        *reinterpret_cast<uint8_t*>(NLMSG_DATA(header)) = AF_UNSPEC;
    }

    request.resize(header->nlmsg_len);
    if (type == RTM_NEWROUTE || type == RTM_DELROUTE) {
        const uint32_t oif = kInvalidIfindex;
        AddAttr(&request, RTA_OIF, &oif, sizeof(oif));
    }
    return request;
}

int RecvAck(int fd, uint32_t seq) {
    std::array<uint8_t, 8192> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq || header->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int SendAndRecvAck(int fd, const std::vector<uint8_t>& request, bool explicit_destination) {
    ssize_t sent = -1;
    if (explicit_destination) {
        sockaddr_nl kernel{};
        kernel.nl_family = AF_NETLINK;
        iovec iov{};
        iov.iov_base = const_cast<uint8_t*>(request.data());
        iov.iov_len = request.size();
        msghdr msg{};
        msg.msg_name = &kernel;
        msg.msg_namelen = sizeof(kernel);
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        sent = sendmsg(fd, &msg, 0);
    } else {
        sent = send(fd, request.data(), request.size(), 0);
    }
    if (sent != static_cast<ssize_t>(request.size())) return errno;
    return RecvAck(fd, reinterpret_cast<const nlmsghdr*>(request.data())->nlmsg_seq);
}

bool IsPostAuthorizationError(int error) { return error == EINVAL || error == EOPNOTSUPP; }

int SendDumpAndWaitDone(int fd, uint16_t type, uint32_t seq) {
    const auto request = BuildRequest(type, NLM_F_REQUEST | NLM_F_DUMP, seq);
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }

    std::array<uint8_t, 16384> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_DONE) return 0;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? 0 : -error->error;
            }
        }
    }
}

TEST(RtnetlinkPermissionSemantics, AllSupportedMutationClassesRequireNetAdmin) {
    ExpectChildSuccess([] {
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(1, errno);
        if (!DropAllCapabilities()) return Failure(2, errno);

        constexpr uint16_t types[] = {RTM_NEWLINK,  RTM_DELLINK,  RTM_SETLINK,
                                      RTM_NEWADDR,  RTM_DELADDR,  RTM_NEWROUTE,
                                      RTM_DELROUTE, RTM_NEWNEIGH, RTM_DELNEIGH};
        uint32_t seq = 100;
        for (uint16_t type : types) {
            const auto request = BuildRequest(type, NLM_F_REQUEST | NLM_F_ACK, seq++);
            const int actual = SendAndRecvAck(fd.Get(), request, false);
            if (actual != EPERM) return Failure(1000 + type, actual);
        }
        return Success();
    });
}

TEST(RtnetlinkPermissionSemantics, GetDumpsRemainAvailableWithoutNetAdmin) {
    ExpectChildSuccess([] {
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(1, errno);
        if (!DropAllCapabilities()) return Failure(2, errno);

        constexpr uint16_t types[] = {RTM_GETLINK, RTM_GETADDR, RTM_GETROUTE, RTM_GETNEIGH,
                                      RTM_GETRULE};
        uint32_t seq = 200;
        for (uint16_t type : types) {
            const int actual = SendDumpAndWaitDone(fd.Get(), type, seq++);
            if (actual != 0) return Failure(2000 + type, actual);
        }
        return Success();
    });
}

TEST(RtnetlinkPermissionSemantics, PrivilegedOpenerDoesNotAuthorizeSenderAfterCapabilityDrop) {
    ExpectChildSuccess([] {
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(1, errno);
        if (!DropAllCapabilities()) return Failure(2, errno);

        const auto request = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 300);
        const int actual = SendAndRecvAck(fd.Get(), request, false);
        return actual == EPERM ? Success() : Failure(3, actual);
    });
}

TEST(RtnetlinkPermissionSemantics, ExplicitDestinationBypassesOnlyOpenerCapabilityCheck) {
    ExpectChildSuccess([] {
        if (!SetNetAdminEffective(false)) return Failure(1, errno);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(2, errno);
        if (!SetNetAdminEffective(true)) return Failure(3, errno);

        const auto implicit = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 400);
        int actual = SendAndRecvAck(fd.Get(), implicit, false);
        if (actual != EPERM) return Failure(4, actual);

        const auto explicit_request = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 401);
        actual = SendAndRecvAck(fd.Get(), explicit_request, true);
        if (actual != ENODEV) return Failure(5, actual);

        if (!DropAllCapabilities()) return Failure(6, errno);
        const auto unprivileged_explicit =
            BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 402);
        actual = SendAndRecvAck(fd.Get(), unprivileged_explicit, true);
        return actual == EPERM ? Success() : Failure(7, actual);
    });
}

TEST(RtnetlinkPermissionSemantics, UsesOwningUserNamespaceOfSocketNetns) {
    ExpectChildSuccess([] {
        if (unshare(CLONE_NEWUSER) != 0) return Failure(1, errno);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(2, errno);
        const auto request = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 500);
        const int actual = SendAndRecvAck(fd.Get(), request, false);
        return actual == EPERM ? Success() : Failure(3, actual);
    });

    ExpectChildSuccess([] {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return Failure(1, errno);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(2, errno);
        const auto request = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 501);
        const int actual = SendAndRecvAck(fd.Get(), request, false);
        return actual == ENODEV ? Success() : Failure(3, actual);
    });
}

TEST(RtnetlinkPermissionSemantics, UnsupportedRtmTypesUseLinuxPermissionOrdering) {
    ExpectChildSuccess([] {
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(1, errno);

        auto request = BuildRequest(kRtmNewQdisc, NLM_F_REQUEST | NLM_F_ACK, 600);
        int actual = SendAndRecvAck(fd.Get(), request, false);
        if (!IsPostAuthorizationError(actual)) return Failure(2, actual);

        if (!DropAllCapabilities()) return Failure(3, errno);
        request = BuildRequest(kRtmNewQdisc, NLM_F_REQUEST | NLM_F_ACK, 601);
        actual = SendAndRecvAck(fd.Get(), request, false);
        if (actual != EPERM) return Failure(4, actual);

        request = BuildRequest(kRtmGetQdisc, NLM_F_REQUEST | NLM_F_ACK, 602);
        actual = SendAndRecvAck(fd.Get(), request, false);
        return IsPostAuthorizationError(actual) ? Success() : Failure(5, actual);
    });
}

TEST(RtnetlinkPermissionSemantics, BatchedMutationsAreIndependentlyRejected) {
    ExpectChildSuccess([] {
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) return Failure(1, errno);
        if (!DropAllCapabilities()) return Failure(2, errno);

        auto first = BuildRequest(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 700);
        auto second = BuildRequest(RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, 701);
        std::vector<uint8_t> batch(NLMSG_ALIGN(first.size()) + second.size(), 0);
        std::memcpy(batch.data(), first.data(), first.size());
        std::memcpy(batch.data() + NLMSG_ALIGN(first.size()), second.data(), second.size());

        if (send(fd.Get(), batch.data(), batch.size(), 0) != static_cast<ssize_t>(batch.size())) {
            return Failure(3, errno);
        }
        int actual = RecvAck(fd.Get(), 700);
        if (actual != EPERM) return Failure(4, actual);
        actual = RecvAck(fd.Get(), 701);
        return actual == EPERM ? Success() : Failure(5, actual);
    });
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
