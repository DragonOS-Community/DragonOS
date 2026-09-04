#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/capability.h>
#include <linux/if_link.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <functional>
#include <optional>
#include <string>
#include <utility>

namespace {

constexpr unsigned char kSentinel = 0xa5;
constexpr int kNamespaceUnavailable = 77;
constexpr int kNeighborScenarioUnavailable = 78;

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

struct LinkState {
    uint32_t flags;
    uint32_t mtu;
};

ChildResult Success() { return {0, 0}; }
ChildResult Failure(int stage, int actual) { return {stage, actual}; }

bool IsDragonOS() {
    utsname uts{};
    return uname(&uts) == 0 && std::strstr(uts.release, "dragonos") != nullptr;
}

void ExpectIsolatedChild(const std::function<ChildResult()>& child_fn) {
    int result_pipe[2];
    ASSERT_EQ(pipe(result_pipe), 0) << std::strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << std::strerror(errno);
    if (child == 0) {
        close(result_pipe[0]);
        ChildResult result = child_fn();
        const ssize_t ignored = write(result_pipe[1], &result, sizeof(result));
        (void)ignored;
        close(result_pipe[1]);
        _exit(result.stage == 0 ? 0 : 1);
    }

    close(result_pipe[1]);
    ChildResult result{-1, 0};
    const ssize_t bytes = read(result_pipe[0], &result, sizeof(result));
    close(result_pipe[0]);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_EQ(bytes, static_cast<ssize_t>(sizeof(result)));
    ASSERT_TRUE(WIFEXITED(status));

    if (result.stage == kNamespaceUnavailable && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation: "
                     << std::strerror(result.actual);
    }
    if (result.stage == kNeighborScenarioUnavailable && !IsDragonOS()) {
        GTEST_SKIP() << "isolated host loopback cannot expose a keyed permanent neighbor";
    }
    EXPECT_EQ(result.stage, 0) << "stage=" << result.stage << " actual=" << result.actual
                               << " (" << std::strerror(result.actual) << ")";
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

ChildResult EnterIsolatedNetwork() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) {
        return Failure(kNamespaceUnavailable, errno);
    }
    return Success();
}

int OpenInetSocket() { return socket(AF_INET, SOCK_DGRAM, 0); }

int OpenRouteSocket() {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout{};
    timeout.tv_sec = 3;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

void InitIfreq(ifreq* request, const char* name) {
    std::memset(request, kSentinel, sizeof(*request));
    std::memset(request->ifr_name, 0, IFNAMSIZ);
    std::strncpy(request->ifr_name, name, IFNAMSIZ - 1);
}

int QueryIoctlFlags(int fd, const char* name, short* flags) {
    ifreq request{};
    InitIfreq(&request, name);
    if (ioctl(fd, SIOCGIFFLAGS, &request) != 0) return errno;
    *flags = request.ifr_flags;
    return 0;
}

int QueryIoctlMtu(int fd, const char* name, int* mtu) {
    ifreq request{};
    InitIfreq(&request, name);
    if (ioctl(fd, SIOCGIFMTU, &request) != 0) return errno;
    *mtu = request.ifr_mtu;
    return 0;
}

int QueryIfindex(int fd, const char* name, int* ifindex) {
    ifreq request{};
    InitIfreq(&request, name);
    if (ioctl(fd, SIOCGIFINDEX, &request) != 0) return errno;
    *ifindex = request.ifr_ifindex;
    return 0;
}

int QueryRtnlState(int fd, int ifindex, uint32_t seq, LinkState* state) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request{};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;
    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) return errno;

    std::array<unsigned char, 8192> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? EPROTO : -error->error;
            }
            if (header->nlmsg_type != RTM_NEWLINK ||
                header->nlmsg_len < NLMSG_LENGTH(sizeof(ifinfomsg))) {
                continue;
            }
            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index != ifindex) continue;
            state->flags = link->ifi_flags;
            bool found_mtu = false;
            int attr_length = IFLA_PAYLOAD(header);
            for (auto* attr = IFLA_RTA(link); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type != IFLA_MTU || RTA_PAYLOAD(attr) != sizeof(uint32_t)) continue;
                std::memcpy(&state->mtu, RTA_DATA(attr), sizeof(state->mtu));
                found_mtu = true;
            }
            return found_mtu ? 0 : ENOMSG;
        }
    }
}

int SetFlags(int fd, const char* name, short flags, bool verify_no_copyout) {
    ifreq request{};
    InitIfreq(&request, name);
    request.ifr_flags = flags;
    const ifreq before = request;
    if (ioctl(fd, SIOCSIFFLAGS, &request) != 0) return errno;
    if (verify_no_copyout && std::memcmp(&request, &before, sizeof(request)) != 0) return EIO;
    return 0;
}

int SetMtu(int fd, const char* name, int mtu, bool verify_no_copyout) {
    ifreq request{};
    InitIfreq(&request, name);
    request.ifr_mtu = mtu;
    const ifreq before = request;
    if (ioctl(fd, SIOCSIFMTU, &request) != 0) return errno;
    if (verify_no_copyout && std::memcmp(&request, &before, sizeof(request)) != 0) return EIO;
    return 0;
}

bool DropAllCapabilities() {
    __user_cap_header_struct header{};
    std::array<__user_cap_data_struct, 2> data{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    header.pid = 0;
    return syscall(SYS_capset, &header, data.data()) == 0;
}

void AddAttr(nlmsghdr* header, size_t capacity, uint16_t type, const void* data, size_t length) {
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    const size_t attr_length = RTA_LENGTH(length);
    const size_t end = offset + RTA_ALIGN(attr_length);
    if (end > capacity) return;
    auto* attr = reinterpret_cast<rtattr*>(reinterpret_cast<unsigned char*>(header) + offset);
    attr->rta_type = type;
    attr->rta_len = attr_length;
    if (length != 0) std::memcpy(RTA_DATA(attr), data, length);
    header->nlmsg_len = end;
}

int RecvAck(int fd, uint32_t seq) {
    std::array<unsigned char, 4096> buffer{};
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

int SetRtnlMtu(int fd, int ifindex, uint32_t mtu, uint32_t seq) {
    std::array<unsigned char, 256> request{};
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    auto* link = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    header->nlmsg_type = RTM_SETLINK;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    header->nlmsg_seq = seq;
    link->ifi_family = AF_UNSPEC;
    link->ifi_index = ifindex;
    AddAttr(header, request.size(), IFLA_MTU, &mtu, sizeof(mtu));
    if (send(fd, request.data(), header->nlmsg_len, 0) != header->nlmsg_len) return errno;
    return RecvAck(fd, seq);
}

int MutatePermanentNeighbor(int fd, uint16_t type, uint16_t flags, int ifindex,
                            uint32_t destination, const std::array<uint8_t, 6>* mac,
                            uint32_t seq) {
    alignas(nlmsghdr) std::array<unsigned char, 256> request{};
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    auto* message = reinterpret_cast<ndmsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(ndmsg));
    header->nlmsg_type = type;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    header->nlmsg_seq = seq;
    message->ndm_family = AF_INET;
    message->ndm_ifindex = ifindex;
    message->ndm_state = NUD_PERMANENT;
    message->ndm_type = RTN_UNICAST;
    AddAttr(header, request.size(), NDA_DST, &destination, sizeof(destination));
    if (mac != nullptr) AddAttr(header, request.size(), NDA_LLADDR, mac->data(), mac->size());
    if (send(fd, request.data(), header->nlmsg_len, 0) != header->nlmsg_len) return errno;
    return RecvAck(fd, seq);
}

int HasPermanentNeighbor(int fd, int ifindex, uint32_t destination, uint32_t seq, bool* found) {
    struct {
        nlmsghdr header;
        ndmsg message;
    } request{};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ndmsg));
    request.header.nlmsg_type = RTM_GETNEIGH;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    request.header.nlmsg_seq = seq;
    request.message.ndm_family = AF_INET;
    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) return errno;

    *found = false;
    std::array<unsigned char, 8192> buffer{};
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
                return error->error == 0 ? EPROTO : -error->error;
            }
            if (header->nlmsg_type != RTM_NEWNEIGH ||
                header->nlmsg_len < NLMSG_LENGTH(sizeof(ndmsg))) {
                continue;
            }
            const auto* message = reinterpret_cast<const ndmsg*>(NLMSG_DATA(header));
            if (message->ndm_ifindex != ifindex || !(message->ndm_state & NUD_PERMANENT)) continue;
            int attr_length = static_cast<int>(header->nlmsg_len - NLMSG_LENGTH(sizeof(ndmsg)));
            for (auto* attr = reinterpret_cast<const rtattr*>(
                     reinterpret_cast<const unsigned char*>(message) + NLMSG_ALIGN(sizeof(ndmsg)));
                 RTA_OK(attr, attr_length); attr = RTA_NEXT(attr, attr_length)) {
                uint32_t dumped = 0;
                if (attr->rta_type == NDA_DST && RTA_PAYLOAD(attr) >= sizeof(dumped)) {
                    std::memcpy(&dumped, RTA_DATA(attr), sizeof(dumped));
                    *found |= dumped == destination;
                }
            }
        }
    }
}

std::optional<std::string> FindEthernetName(int fd) {
    struct if_nameindex* interfaces = if_nameindex();
    if (interfaces == nullptr) return std::nullopt;

    std::optional<std::string> result;
    for (const struct if_nameindex* interface = interfaces; interface->if_index != 0;
         ++interface) {
        ifreq request{};
        InitIfreq(&request, interface->if_name);
        if (ioctl(fd, SIOCGIFHWADDR, &request) == 0 &&
            request.ifr_hwaddr.sa_family == ARPHRD_ETHER) {
            result = interface->if_name;
            break;
        }
    }
    if_freenameindex(interfaces);
    return result;
}

class NeighborMutationCleanup {
  public:
    NeighborMutationCleanup(int inet_fd, int route_fd, std::string name, int ifindex,
                            short original_flags, uint32_t destination,
                            const std::array<uint8_t, 6>* mac)
        : inet_fd_(inet_fd),
          route_fd_(route_fd),
          name_(std::move(name)),
          ifindex_(ifindex),
          original_flags_(original_flags),
          destination_(destination),
          mac_(mac) {}

    ~NeighborMutationCleanup() {
        (void)MutatePermanentNeighbor(route_fd_, RTM_DELNEIGH, 0, ifindex_, destination_, mac_,
                                      902);
        (void)SetFlags(inet_fd_, name_.c_str(), original_flags_, false);
    }

  private:
    int inet_fd_;
    int route_fd_;
    std::string name_;
    int ifindex_;
    short original_flags_;
    uint32_t destination_;
    const std::array<uint8_t, 6>* mac_;
};

TEST(SocketIoctlNetdevMutation, FlagsAndMtuAreSharedWithRtnetlinkAndRestored) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard inet(OpenInetSocket());
        FdGuard route(OpenRouteSocket());
        if (inet.Get() < 0 || route.Get() < 0) return Failure(1, errno);

        int ifindex = 0;
        short original_flags = 0;
        int original_mtu = 0;
        if (int error = QueryIfindex(inet.Get(), "lo", &ifindex)) return Failure(2, error);
        if (int error = QueryIoctlFlags(inet.Get(), "lo", &original_flags)) {
            return Failure(3, error);
        }
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &original_mtu)) return Failure(4, error);
        if (original_mtu <= 128) return Failure(5, original_mtu);

        const short changed_flags = original_flags ^ IFF_UP;
        if (int error = SetFlags(inet.Get(), "lo", changed_flags, true)) return Failure(6, error);
        short ioctl_flags = 0;
        if (int error = QueryIoctlFlags(inet.Get(), "lo", &ioctl_flags)) return Failure(7, error);
        LinkState rtnl{};
        if (int error = QueryRtnlState(route.Get(), ifindex, 100, &rtnl)) return Failure(8, error);
        if ((ioctl_flags & IFF_UP) != (changed_flags & IFF_UP)) return Failure(9, ioctl_flags);
        if (static_cast<uint16_t>(ioctl_flags) != static_cast<uint16_t>(rtnl.flags)) {
            return Failure(10, static_cast<int>(rtnl.flags));
        }

        const int changed_mtu = original_mtu - 1;
        if (int error = SetMtu(inet.Get(), "lo", changed_mtu, true)) return Failure(11, error);
        int ioctl_mtu = 0;
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(12, error);
        if (int error = QueryRtnlState(route.Get(), ifindex, 101, &rtnl)) return Failure(13, error);
        if (ioctl_mtu != changed_mtu || rtnl.mtu != static_cast<uint32_t>(changed_mtu)) {
            return Failure(14, ioctl_mtu);
        }

        if (int error = SetMtu(inet.Get(), "lo", -1, false); error != EINVAL) {
            return Failure(15, error);
        }
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(16, error);
        if (ioctl_mtu != changed_mtu) return Failure(17, ioctl_mtu);
        if (int error = SetMtu(inet.Get(), "no-such-netdev", -1, false); error != ENODEV) {
            return Failure(18, error);
        }

        if (int error = SetMtu(inet.Get(), "lo", original_mtu, false)) return Failure(19, error);
        if (int error = SetFlags(inet.Get(), "lo", original_flags, false)) return Failure(20, error);
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(21, error);
        if (int error = QueryIoctlFlags(inet.Get(), "lo", &ioctl_flags)) return Failure(22, error);
        if (ioctl_mtu != original_mtu || (ioctl_flags & IFF_UP) != (original_flags & IFF_UP)) {
            return Failure(23, ioctl_mtu);
        }
        if (int error = QueryRtnlState(route.Get(), ifindex, 102, &rtnl)) return Failure(24, error);
        if (rtnl.mtu != static_cast<uint32_t>(original_mtu) ||
            (rtnl.flags & IFF_UP) != (static_cast<uint32_t>(original_flags) & IFF_UP)) {
            return Failure(25, static_cast<int>(rtnl.mtu));
        }
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, LoopbackAcceptsLinuxStandardMtu) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard inet(OpenInetSocket());
        FdGuard route(OpenRouteSocket());
        if (inet.Get() < 0 || route.Get() < 0) return Failure(1, errno);

        int ifindex = 0;
        if (int error = QueryIfindex(inet.Get(), "lo", &ifindex)) return Failure(2, error);
        constexpr int kLinuxLoopbackMtu = 65536;
        int ioctl_mtu = 0;
        LinkState rtnl{};
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(3, error);
        if (int error = QueryRtnlState(route.Get(), ifindex, 199, &rtnl)) {
            return Failure(4, error);
        }
        if (ioctl_mtu != kLinuxLoopbackMtu || rtnl.mtu != kLinuxLoopbackMtu) {
            return Failure(5, ioctl_mtu);
        }

        constexpr int kAlternateMtu = kLinuxLoopbackMtu - 1;
        if (int error = SetMtu(inet.Get(), "lo", kAlternateMtu, true)) {
            return Failure(6, error);
        }
        if (int error = SetMtu(inet.Get(), "lo", kLinuxLoopbackMtu, true)) {
            return Failure(7, error);
        }
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(8, error);
        if (int error = QueryRtnlState(route.Get(), ifindex, 200, &rtnl)) {
            return Failure(9, error);
        }
        if (ioctl_mtu != kLinuxLoopbackMtu || rtnl.mtu != kLinuxLoopbackMtu) {
            return Failure(10, ioctl_mtu);
        }

        if (int error = SetRtnlMtu(route.Get(), ifindex, kAlternateMtu, 201)) {
            return Failure(11, error);
        }
        if (int error = SetRtnlMtu(route.Get(), ifindex, kLinuxLoopbackMtu, 202)) {
            return Failure(12, error);
        }
        if (int error = QueryIoctlMtu(inet.Get(), "lo", &ioctl_mtu)) return Failure(13, error);
        if (int error = QueryRtnlState(route.Get(), ifindex, 203, &rtnl)) {
            return Failure(14, error);
        }
        if (ioctl_mtu != kLinuxLoopbackMtu || rtnl.mtu != kLinuxLoopbackMtu) {
            return Failure(15, ioctl_mtu);
        }
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, UserCopyFaultsPrecedeMutation) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard fd(OpenInetSocket());
        if (fd.Get() < 0) return Failure(1, errno);

        constexpr std::array<unsigned long, 2> commands = {SIOCSIFFLAGS, SIOCSIFMTU};
        for (size_t i = 0; i < commands.size(); ++i) {
            errno = 0;
            if (ioctl(fd.Get(), commands[i], nullptr) != -1 || errno != EFAULT) {
                return Failure(10 + static_cast<int>(i), errno);
            }
        }

        const long page_size = sysconf(_SC_PAGESIZE);
        if (page_size <= 0) return Failure(20, errno);
        void* mapping = mmap(nullptr, page_size * 2, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) return Failure(21, errno);
        if (mprotect(static_cast<char*>(mapping) + page_size, page_size, PROT_NONE) != 0) {
            return Failure(22, errno);
        }
        auto* partial = reinterpret_cast<ifreq*>(static_cast<char*>(mapping) + page_size -
                                                 sizeof(ifreq) + 1);
        std::memset(partial, 0, sizeof(ifreq) - 1);
        std::memcpy(partial->ifr_name, "lo", 3);
        for (size_t i = 0; i < commands.size(); ++i) {
            errno = 0;
            if (ioctl(fd.Get(), commands[i], partial) != -1 || errno != EFAULT) {
                return Failure(30 + static_cast<int>(i), errno);
            }
        }
        if (mprotect(static_cast<char*>(mapping) + page_size, page_size,
                     PROT_READ | PROT_WRITE) != 0) {
            return Failure(40, errno);
        }
        if (munmap(mapping, page_size * 2) != 0) return Failure(41, errno);
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, NetAdminIsCheckedBeforeDeviceLookup) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard fd(OpenInetSocket());
        if (fd.Get() < 0) return Failure(1, errno);
        if (!DropAllCapabilities()) return Failure(2, errno);

        constexpr std::array<unsigned long, 2> commands = {SIOCSIFFLAGS, SIOCSIFMTU};
        constexpr std::array<const char*, 2> names = {"lo", "no-such-netdev"};
        for (size_t command = 0; command < commands.size(); ++command) {
            for (size_t name = 0; name < names.size(); ++name) {
                ifreq request{};
                InitIfreq(&request, names[name]);
                request.ifr_flags = 0;
                errno = 0;
                if (ioctl(fd.Get(), commands[command], &request) != -1 || errno != EPERM) {
                    return Failure(10 + static_cast<int>(command * 2 + name), errno);
                }
            }
        }
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, PacketSendRejectsAdministrativelyDownInterface) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard inet(OpenInetSocket());
        if (inet.Get() < 0) return Failure(1, errno);
        int ifindex = 0;
        if (int error = QueryIfindex(inet.Get(), "lo", &ifindex)) return Failure(2, error);

        FdGuard packet(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
        if (packet.Get() < 0) return Failure(3, errno);
        sockaddr_ll destination{};
        destination.sll_family = AF_PACKET;
        destination.sll_protocol = htons(ETH_P_ALL);
        destination.sll_ifindex = ifindex;
        std::array<uint8_t, ETH_HLEN> frame{};
        errno = 0;
        if (sendto(packet.Get(), frame.data(), frame.size(), 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != ENETDOWN) {
            return Failure(4, errno);
        }
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, PacketSendHonorsRuntimeMtu) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard inet(OpenInetSocket());
        if (inet.Get() < 0) return Failure(1, errno);

        int ifindex = 0;
        short flags = 0;
        if (int error = QueryIfindex(inet.Get(), "lo", &ifindex)) return Failure(2, error);
        if (int error = QueryIoctlFlags(inet.Get(), "lo", &flags)) return Failure(3, error);
        if (int error = SetFlags(inet.Get(), "lo", static_cast<short>(flags | IFF_UP), false)) {
            return Failure(4, error);
        }
        constexpr int kMtu = 1400;
        if (int error = SetMtu(inet.Get(), "lo", kMtu, false)) return Failure(5, error);

        sockaddr_ll destination{};
        destination.sll_family = AF_PACKET;
        destination.sll_protocol = htons(ETH_P_IP);
        destination.sll_ifindex = ifindex;
        destination.sll_halen = ETH_ALEN;

        FdGuard raw(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
        if (raw.Get() < 0) return Failure(6, errno);
        std::array<uint8_t, kMtu + ETH_HLEN + 5> raw_frame{};
        raw_frame[12] = 0x08;
        raw_frame[13] = 0x00;
        if (sendto(raw.Get(), raw_frame.data(), kMtu + ETH_HLEN, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) !=
            kMtu + ETH_HLEN) {
            return Failure(7, errno);
        }
        errno = 0;
        if (sendto(raw.Get(), raw_frame.data(), kMtu + ETH_HLEN + 1, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(8, errno);
        }

        raw_frame[12] = 0x81;
        raw_frame[13] = 0x00;
        errno = 0;
        if (sendto(raw.Get(), raw_frame.data(), kMtu + ETH_HLEN + 4, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(9, errno);
        }
        raw_frame[12] = 0x88;
        raw_frame[13] = 0xa8;
        errno = 0;
        if (sendto(raw.Get(), raw_frame.data(), kMtu + ETH_HLEN + 4, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(10, errno);
        }

        FdGuard dgram(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
        if (dgram.Get() < 0) return Failure(11, errno);
        std::array<uint8_t, kMtu + 5> payload{};
        if (sendto(dgram.Get(), payload.data(), kMtu, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != kMtu) {
            return Failure(12, errno);
        }
        errno = 0;
        if (sendto(dgram.Get(), payload.data(), kMtu + 1, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(13, errno);
        }

        destination.sll_protocol = htons(0x8100);
        errno = 0;
        if (sendto(dgram.Get(), payload.data(), kMtu + 4, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(14, errno);
        }
        destination.sll_protocol = htons(0x88a8);
        errno = 0;
        if (sendto(dgram.Get(), payload.data(), kMtu + 4, 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != EMSGSIZE) {
            return Failure(15, errno);
        }

        destination.sll_protocol = htons(ETH_P_IP);
        std::array<iovec, 2> iov{};
        iov[0].iov_base = payload.data();
        iov[0].iov_len = kMtu / 2;
        iov[1].iov_base = payload.data() + kMtu / 2;
        iov[1].iov_len = kMtu / 2;
        msghdr message{};
        message.msg_name = &destination;
        message.msg_namelen = sizeof(destination);
        message.msg_iov = iov.data();
        message.msg_iovlen = iov.size();
        if (sendmsg(dgram.Get(), &message, 0) != kMtu) {
            return Failure(16, errno);
        }
        ++iov[1].iov_len;
        errno = 0;
        if (sendmsg(dgram.Get(), &message, 0) != -1 || errno != EMSGSIZE) {
            return Failure(17, errno);
        }

        if (int error = SetFlags(inet.Get(), "lo", static_cast<short>(flags & ~IFF_UP), false)) {
            return Failure(18, error);
        }
        errno = 0;
        if (sendto(raw.Get(), raw_frame.data(), raw_frame.size(), 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) != -1 ||
            errno != ENETDOWN) {
            return Failure(19, errno);
        }
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, NeighborPurgeDependsOnLinkLifecycle) {
    ExpectIsolatedChild([] {
        const bool dragonos = IsDragonOS();
        if (!dragonos) {
            ChildResult isolated = EnterIsolatedNetwork();
            if (isolated.stage != 0) return isolated;
        }
        FdGuard inet(OpenInetSocket());
        FdGuard route(OpenRouteSocket());
        if (inet.Get() < 0 || route.Get() < 0) return Failure(1, errno);

        const auto ethernet_name = dragonos ? FindEthernetName(inet.Get()) : std::nullopt;
        if (dragonos && !ethernet_name.has_value()) return Failure(2, ENODEV);
        const std::string name = ethernet_name.value_or("lo");
        int ifindex = 0;
        short original_flags = 0;
        if (int error = QueryIfindex(inet.Get(), name.c_str(), &ifindex)) {
            return Failure(3, error);
        }
        if (int error = QueryIoctlFlags(inet.Get(), name.c_str(), &original_flags)) {
            return Failure(4, error);
        }

        constexpr std::array<uint8_t, 6> mac = {0x02, 0x22, 0x33, 0x44, 0x55, 0x71};
        const auto* lladdr = ethernet_name.has_value() ? &mac : nullptr;
        in_addr address{};
        if (inet_pton(AF_INET, "198.18.230.44", &address) != 1) return Failure(5, errno);
        NeighborMutationCleanup cleanup(inet.Get(), route.Get(), name, ifindex, original_flags,
                                        address.s_addr, lladdr);

        (void)MutatePermanentNeighbor(route.Get(), RTM_DELNEIGH, 0, ifindex, address.s_addr,
                                      lladdr, 900);
        const short arp_flags = static_cast<short>((original_flags | IFF_UP) & ~IFF_NOARP);
        if (int error = SetFlags(inet.Get(), name.c_str(), arp_flags, false)) {
            return Failure(6, error);
        }
        if (int error = MutatePermanentNeighbor(route.Get(), RTM_NEWNEIGH,
                                                NLM_F_CREATE | NLM_F_EXCL, ifindex,
                                                address.s_addr, lladdr, 901)) {
            if (!dragonos) return Failure(kNeighborScenarioUnavailable, error);
            return Failure(7, error);
        }

        bool found = false;
        if (int error = HasPermanentNeighbor(route.Get(), ifindex, address.s_addr, 903, &found)) {
            return Failure(8, error);
        }
        if (!found) {
            if (!dragonos) return Failure(kNeighborScenarioUnavailable, 0);
            return Failure(9, 0);
        }

        // Reapplying the current flags is a no-op and must not purge state.
        if (int error = SetFlags(inet.Get(), name.c_str(), arp_flags, false)) {
            return Failure(10, error);
        }
        if (int error = HasPermanentNeighbor(route.Get(), ifindex, address.s_addr, 904, &found)) {
            return Failure(11, error);
        }
        if (!found) return Failure(12, 0);

        const short down_flags = static_cast<short>(arp_flags & ~IFF_UP);
        if (int error = SetFlags(inet.Get(), name.c_str(), down_flags, false)) {
            return Failure(13, error);
        }
        if (int error = HasPermanentNeighbor(route.Get(), ifindex, address.s_addr, 905, &found)) {
            return Failure(14, error);
        }
        if (found) return Failure(15, 0);

        // Linux permits an administrative permanent entry to be installed
        // while the link is down. A later NOARP-only change does not emit the
        // NETDEV_CHANGE event that purges neighbors unless the device is up.
        if (int error = MutatePermanentNeighbor(route.Get(), RTM_NEWNEIGH,
                                                NLM_F_CREATE | NLM_F_EXCL, ifindex,
                                                address.s_addr, lladdr, 906)) {
            return Failure(16, error);
        }
        if (int error = HasPermanentNeighbor(route.Get(), ifindex, address.s_addr, 907, &found)) {
            return Failure(17, error);
        }
        if (!found) return Failure(18, 0);
        const short down_noarp_flags = static_cast<short>(down_flags | IFF_NOARP);
        if (int error = SetFlags(inet.Get(), name.c_str(), down_noarp_flags, false)) {
            return Failure(19, error);
        }
        if (int error = HasPermanentNeighbor(route.Get(), ifindex, address.s_addr, 908, &found)) {
            return Failure(20, error);
        }
        if (!found) return Failure(21, 0);
        return Success();
    });
}

TEST(SocketIoctlNetdevMutation, SocketKeepsItsCreationNetworkNamespace) {
    ExpectIsolatedChild([] {
        ChildResult isolated = EnterIsolatedNetwork();
        if (isolated.stage != 0) return isolated;
        FdGuard old_fd(OpenInetSocket());
        FdGuard old_route(OpenRouteSocket());
        if (old_fd.Get() < 0 || old_route.Get() < 0) return Failure(1, errno);

        int old_ifindex = 0;
        int old_original_mtu = 0;
        if (int error = QueryIfindex(old_fd.Get(), "lo", &old_ifindex)) return Failure(2, error);
        if (int error = QueryIoctlMtu(old_fd.Get(), "lo", &old_original_mtu)) {
            return Failure(3, error);
        }
        if (old_original_mtu <= 129) return Failure(4, old_original_mtu);

        if (unshare(CLONE_NEWNET) != 0) return Failure(kNamespaceUnavailable, errno);
        FdGuard new_fd(OpenInetSocket());
        FdGuard new_route(OpenRouteSocket());
        if (new_fd.Get() < 0 || new_route.Get() < 0) return Failure(5, errno);
        int new_ifindex = 0;
        int new_original_mtu = 0;
        if (int error = QueryIfindex(new_fd.Get(), "lo", &new_ifindex)) return Failure(6, error);
        if (int error = QueryIoctlMtu(new_fd.Get(), "lo", &new_original_mtu)) {
            return Failure(7, error);
        }

        const int old_changed_mtu = old_original_mtu - 2;
        if (int error = SetMtu(old_fd.Get(), "lo", old_changed_mtu, false)) {
            return Failure(8, error);
        }
        int observed = 0;
        if (int error = QueryIoctlMtu(old_fd.Get(), "lo", &observed)) return Failure(9, error);
        if (observed != old_changed_mtu) return Failure(10, observed);
        if (int error = QueryIoctlMtu(new_fd.Get(), "lo", &observed)) return Failure(11, error);
        if (observed != new_original_mtu) return Failure(12, observed);

        LinkState old_state{};
        LinkState new_state{};
        if (int error = QueryRtnlState(old_route.Get(), old_ifindex, 200, &old_state)) {
            return Failure(13, error);
        }
        if (int error = QueryRtnlState(new_route.Get(), new_ifindex, 201, &new_state)) {
            return Failure(14, error);
        }
        if (old_state.mtu != static_cast<uint32_t>(old_changed_mtu) ||
            new_state.mtu != static_cast<uint32_t>(new_original_mtu)) {
            return Failure(15, static_cast<int>(old_state.mtu));
        }
        return Success();
    });
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
