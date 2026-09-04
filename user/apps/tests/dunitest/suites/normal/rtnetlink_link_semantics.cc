#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/if_link.h>
#include <linux/if.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <poll.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <optional>
#include <string>
#include <string_view>

namespace {

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

bool IsDragonOS() {
    struct utsname uts {};
    return uname(&uts) == 0 && std::strstr(uts.release, "dragonos") != nullptr;
}

int OpenRouteSocket() {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout {};
    timeout.tv_sec = 2;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    sockaddr_nl address {};
    address.nl_family = AF_NETLINK;
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

int OpenUeventSocket() {
    int fd = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_KOBJECT_UEVENT);
    if (fd < 0) return -1;

    sockaddr_nl address {};
    address.nl_family = AF_NETLINK;
    address.nl_groups = 1;
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

bool UeventHasField(const unsigned char* payload, size_t length, std::string_view expected) {
    size_t offset = 0;
    while (offset < length) {
        const char* field = reinterpret_cast<const char*>(payload + offset);
        const size_t remaining = length - offset;
        const size_t field_length = strnlen(field, remaining);
        if (field_length == remaining) return false;
        if (std::string_view(field, field_length) == expected) return true;
        offset += field_length + 1;
    }
    return false;
}

bool UeventHasNonemptyField(const unsigned char* payload, size_t length,
                           std::string_view prefix) {
    size_t offset = 0;
    while (offset < length) {
        const char* field = reinterpret_cast<const char*>(payload + offset);
        const size_t remaining = length - offset;
        const size_t field_length = strnlen(field, remaining);
        if (field_length == remaining) return false;
        const std::string_view value(field, field_length);
        if (value.size() > prefix.size() && value.substr(0, prefix.size()) == prefix) {
            return true;
        }
        offset += field_length + 1;
    }
    return false;
}

bool IsExpectedMoveUevent(const unsigned char* payload, size_t length,
                          const std::string& interface, int ifindex) {
    return UeventHasField(payload, length, "ACTION=move") &&
           UeventHasField(payload, length, "SUBSYSTEM=net") &&
           UeventHasField(payload, length, "INTERFACE=" + interface) &&
           UeventHasField(payload, length, "IFINDEX=" + std::to_string(ifindex));
}

std::optional<bool> ReceiveExpectedMoveUevent(int fd, const std::string& interface,
                                             int ifindex) {
    std::array<unsigned char, 8192> payload {};
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    // A unique temporary interface name filters unrelated host uevents. Bound
    // both the scan and total wall time so sustained noise cannot hang CI.
    for (int attempts = 0; attempts < 64; ++attempts) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now());
        if (remaining.count() <= 0) return std::nullopt;
        pollfd descriptor {fd, POLLIN, 0};
        const int ready = poll(&descriptor, 1, static_cast<int>(remaining.count()));
        if (ready == 0) return std::nullopt;
        if (ready < 0) {
            if (errno == EINTR) {
                --attempts;
                continue;
            }
            return false;
        }
        if ((descriptor.revents & POLLIN) == 0) return false;

        const ssize_t received =
            recv(fd, payload.data(), payload.size(), MSG_DONTWAIT);
        if (received < 0) {
            if (errno == EINTR) {
                --attempts;
                continue;
            }
            if (errno == EAGAIN || errno == EWOULDBLOCK) return std::nullopt;
            return false;
        }
        if (!IsExpectedMoveUevent(payload.data(), received, interface, ifindex)) continue;
        return UeventHasNonemptyField(payload.data(), received, "DEVPATH_OLD=");
    }
    return std::nullopt;
}

std::optional<bool> HasExpectedMoveUeventQueued(int fd, const std::string& interface,
                                               int ifindex) {
    std::array<unsigned char, 8192> payload {};
    for (int attempts = 0; attempts < 64; ++attempts) {
        const ssize_t received = recv(fd, payload.data(), payload.size(), MSG_DONTWAIT);
        if (received < 0) {
            if (errno == EINTR) {
                --attempts;
                continue;
            }
            if (errno == EAGAIN || errno == EWOULDBLOCK) return false;
            return std::nullopt;
        }
        if (IsExpectedMoveUevent(payload.data(), received, interface, ifindex)) return true;
    }
    return std::nullopt;
}

int RecvAck(int fd, uint32_t seq) {
    std::array<unsigned char, 4096> buffer {};
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

struct LinkSnapshot {
    uint32_t flags;
    uint32_t mtu;
    std::string name;
};

struct RouteProbe {
    uint8_t family;
    uint8_t prefix_len;
    uint8_t table;
    uint8_t type;
    uint32_t ifindex;
    std::array<uint8_t, 16> destination;
};

int CountRoute(int fd, const RouteProbe& probe, uint32_t seq) {
    struct {
        nlmsghdr header;
        rtmsg route;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    request.header.nlmsg_type = RTM_GETROUTE;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    request.header.nlmsg_seq = seq;
    request.route.rtm_family = probe.family;
    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return -errno;
    }

    const size_t address_length = probe.family == AF_INET ? 4 : 16;
    int count = 0;
    std::array<unsigned char, 16384> buffer {};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return -errno;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_DONE) return count;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? count : error->error;
            }
            if (header->nlmsg_type != RTM_NEWROUTE) continue;

            const auto* route = reinterpret_cast<const rtmsg*>(NLMSG_DATA(header));
            if (route->rtm_family != probe.family || route->rtm_dst_len != probe.prefix_len ||
                route->rtm_table != probe.table || route->rtm_type != probe.type) {
                continue;
            }

            uint32_t ifindex = 0;
            std::array<uint8_t, 16> destination {};
            bool has_destination = false;
            int attr_length = RTM_PAYLOAD(header);
            for (auto* attr = RTM_RTA(route); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) == address_length) {
                    std::memcpy(destination.data(), RTA_DATA(attr), address_length);
                    has_destination = true;
                } else if (attr->rta_type == RTA_OIF &&
                           RTA_PAYLOAD(attr) == sizeof(ifindex)) {
                    std::memcpy(&ifindex, RTA_DATA(attr), sizeof(ifindex));
                }
            }
            if (has_destination && ifindex == probe.ifindex &&
                std::memcmp(destination.data(), probe.destination.data(), address_length) == 0) {
                ++count;
            }
        }
    }
}

int SendIpv4LoopbackDatagram() {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return errno;

    sockaddr_in destination {};
    destination.sin_family = AF_INET;
    destination.sin_port = htons(9);
    if (inet_pton(AF_INET, "127.0.0.1", &destination.sin_addr) != 1) return EINVAL;
    const char payload = 'x';
    if (sendto(fd.Get(), &payload, sizeof(payload), 0,
               reinterpret_cast<const sockaddr*>(&destination), sizeof(destination)) < 0) {
        return errno;
    }
    return 0;
}

bool AddAttribute(nlmsghdr* header, size_t capacity, uint16_t type, const void* data,
                  size_t length) {
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    const size_t attribute_length = RTA_LENGTH(length);
    const size_t next = offset + RTA_ALIGN(attribute_length);
    if (next > capacity) return false;

    auto* attribute = reinterpret_cast<rtattr*>(reinterpret_cast<unsigned char*>(header) + offset);
    attribute->rta_type = type;
    attribute->rta_len = attribute_length;
    std::memcpy(RTA_DATA(attribute), data, length);
    header->nlmsg_len = next;
    return true;
}

int SetLink(int fd, int ifindex, uint32_t flags, uint32_t change,
            std::optional<uint32_t> mtu, const std::optional<std::string>& name, uint32_t seq) {
    std::array<unsigned char, 512> storage {};
    auto* header = reinterpret_cast<nlmsghdr*>(storage.data());
    auto* link = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header));
    header->nlmsg_len = NLMSG_LENGTH(sizeof(*link));
    header->nlmsg_type = RTM_SETLINK;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    header->nlmsg_seq = seq;
    link->ifi_family = AF_UNSPEC;
    link->ifi_index = ifindex;
    link->ifi_flags = flags;
    link->ifi_change = change;

    if (mtu.has_value() &&
        !AddAttribute(header, storage.size(), IFLA_MTU, &*mtu, sizeof(*mtu))) {
        return EMSGSIZE;
    }
    if (name.has_value() &&
        !AddAttribute(header, storage.size(), IFLA_IFNAME, name->c_str(), name->size() + 1)) {
        return EMSGSIZE;
    }
    if (send(fd, header, header->nlmsg_len, 0) != static_cast<ssize_t>(header->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

std::optional<LinkSnapshot> QueryLink(int fd, int ifindex, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;
    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return std::nullopt;
    }

    std::array<unsigned char, 4096> buffer {};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return std::nullopt;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR) return std::nullopt;
            if (header->nlmsg_type != RTM_NEWLINK) continue;

            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index != ifindex) continue;
            LinkSnapshot result {link->ifi_flags, 0, {}};
            bool found_mtu = false;
            bool found_name = false;
            int attr_length = IFLA_PAYLOAD(header);
            for (auto* attr = IFLA_RTA(link); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type == IFLA_MTU && RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    std::memcpy(&result.mtu, RTA_DATA(attr), sizeof(result.mtu));
                    found_mtu = true;
                } else if (attr->rta_type == IFLA_IFNAME && RTA_PAYLOAD(attr) > 0) {
                    const auto* value = static_cast<const char*>(RTA_DATA(attr));
                    const size_t payload = RTA_PAYLOAD(attr);
                    const size_t length = strnlen(value, payload);
                    if (length < payload) {
                        result.name.assign(value, length);
                        found_name = true;
                    }
                }
            }
            if (found_mtu && found_name) return result;
            return std::nullopt;
        }
    }
}

std::optional<int> FindIfindex(const char* name) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return std::nullopt;
    ifreq request {};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd.Get(), SIOCGIFINDEX, &request) < 0) return std::nullopt;
    return request.ifr_ifindex;
}

int SetLinkUp(int fd, int ifindex, bool up, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_SETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;
    request.link.ifi_flags = up ? IFF_UP : 0;
    request.link.ifi_change = IFF_UP;

    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return errno;
    }
    return RecvAck(fd, seq);
}

int RunCombinedMutationCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;
    FdGuard fd(OpenRouteSocket());
    if (fd.Get() < 0) return 10;
    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;

    uint32_t seq = 1;
    const auto original = QueryLink(fd.Get(), *ifindex, seq++);
    if (!original.has_value()) return 12;
    const uint32_t changed_mtu = original->mtu == 1500 ? 1400 : 1500;
    const bool changed_up = (original->flags & IFF_UP) == 0;
    const uint32_t changed_flags = changed_up ? IFF_UP : 0;
    if (SetLink(fd.Get(), *ifindex, changed_flags, IFF_UP, changed_mtu, std::nullopt,
                seq++) != 0) {
        return 13;
    }
    const auto changed = QueryLink(fd.Get(), *ifindex, seq++);
    if (!changed.has_value() || changed->mtu != changed_mtu ||
        ((changed->flags & IFF_UP) != 0) != changed_up ||
        (changed->flags & IFF_LOOPBACK) == 0) {
        return 14;
    }

    const uint32_t original_up = original->flags & IFF_UP;
    if (SetLink(fd.Get(), *ifindex, original_up, IFF_UP, original->mtu, std::nullopt,
                seq++) != 0) {
        return 15;
    }
    const auto before_failure = QueryLink(fd.Get(), *ifindex, seq++);
    if (!before_failure.has_value()) return 16;
    const uint32_t failed_mtu = before_failure->mtu == 1600 ? 1400 : 1600;
    const uint32_t failed_up = (before_failure->flags & IFF_UP) == 0 ? IFF_UP : 0;
    const int error = SetLink(fd.Get(), *ifindex, failed_up, IFF_UP, failed_mtu,
                              std::string("bad/name"), seq++);
    if (error == 0) return 17;
    const auto after_failure = QueryLink(fd.Get(), *ifindex, seq++);
    if (!after_failure.has_value() || after_failure->name != before_failure->name) return 18;

    // DragonOS intentionally gives the shared transaction stronger failure
    // atomicity than Linux do_setlink(), which has already changed MTU when a
    // later rename fails. Keep the host path useful without asserting the
    // stronger DragonOS guarantee there.
    if (IsDragonOS() &&
        (after_failure->mtu != before_failure->mtu ||
         (after_failure->flags & IFF_UP) != (before_failure->flags & IFF_UP))) {
        return 19;
    }
    if (SetLink(fd.Get(), *ifindex, before_failure->flags & IFF_UP, IFF_UP,
                before_failure->mtu, std::nullopt, seq++) != 0) {
        return 20;
    }
    return 0;
}

int RunReplaceFlagsCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;
    FdGuard fd(OpenRouteSocket());
    if (fd.Get() < 0) return 10;
    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;

    uint32_t seq = 1;
    const auto original = QueryLink(fd.Get(), *ifindex, seq++);
    if (!original.has_value()) return 12;
    if (SetLink(fd.Get(), *ifindex, IFF_UP | IFF_NOARP, IFF_UP | IFF_NOARP,
                std::nullopt, std::nullopt, seq++) != 0) {
        return 13;
    }
    const auto prepared = QueryLink(fd.Get(), *ifindex, seq++);
    if (!prepared.has_value() || (prepared->flags & (IFF_UP | IFF_NOARP)) !=
                                     (IFF_UP | IFF_NOARP)) {
        return 14;
    }

    // Linux treats ifi_change == 0 as a full replace for compatibility. The
    // omitted configurable NOARP bit is cleared, while device/volatile bits
    // cannot be overwritten by userspace.
    if (SetLink(fd.Get(), *ifindex, IFF_UP, 0, std::nullopt, std::nullopt, seq++) != 0) {
        return 15;
    }
    const auto replaced = QueryLink(fd.Get(), *ifindex, seq++);
    if (!replaced.has_value() || (replaced->flags & IFF_UP) == 0 ||
        (replaced->flags & IFF_NOARP) != 0) {
        return 16;
    }
    constexpr uint32_t kVolatileAndDevice =
        IFF_LOOPBACK | IFF_POINTOPOINT | IFF_BROADCAST | IFF_ECHO | IFF_MASTER |
        IFF_SLAVE | IFF_RUNNING | IFF_LOWER_UP | IFF_DORMANT;
    if ((replaced->flags & kVolatileAndDevice) != (prepared->flags & kVolatileAndDevice)) {
        return 17;
    }

    const uint32_t restore_flags = original->flags & (IFF_UP | IFF_NOARP);
    if (SetLink(fd.Get(), *ifindex, restore_flags, IFF_UP | IFF_NOARP, std::nullopt,
                std::nullopt, seq++) != 0) {
        return 18;
    }
    return 0;
}

int RunRenameValidationCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;
    FdGuard fd(OpenRouteSocket());
    if (fd.Get() < 0) return 10;
    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;

    uint32_t seq = 1;
    const auto original = QueryLink(fd.Get(), *ifindex, seq++);
    if (!original.has_value()) return 12;
    const std::array<std::string, 9> invalid_names = {
        ".", "..", "bad/name", "bad:name", "bad name", "abcdefghijklmnop", "bad%x",
        "bad%", "bad%d%d"};
    for (const auto& invalid : invalid_names) {
        if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, invalid, seq++) == 0) return 13;
        const auto unchanged = QueryLink(fd.Get(), *ifindex, seq++);
        if (!unchanged.has_value() || unchanged->name != original->name) return 14;
    }

    const std::string renamed = "dunitlo0";
    struct stat old_target {};
    const std::string old_path = "/sys/class/net/" + original->name;
    const std::string new_path = "/sys/class/net/" + renamed;
    const bool has_sysfs_projection = stat(old_path.c_str(), &old_target) == 0;
    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, renamed, seq++) != 0) return 15;
    const auto after_rename = QueryLink(fd.Get(), *ifindex, seq++);
    if (!after_rename.has_value() || after_rename->name != renamed) return 16;
    int projection_result = 0;
    if (has_sysfs_projection) {
        struct stat new_target {};
        struct stat old_after {};
        const bool new_visible = stat(new_path.c_str(), &new_target) == 0;
        const bool old_visible = stat(old_path.c_str(), &old_after) == 0;
        if (IsDragonOS()) {
            // DragonOS intentionally has no sysfs projection for the fresh
            // namespace. Renaming its lo must not touch the initial-netns
            // inode visible through this mount.
            if (new_visible || !old_visible || old_after.st_ino != old_target.st_ino) {
                projection_result = 17;
            }
        } else if (new_visible) {
            if (old_visible || new_target.st_ino != old_target.st_ino) projection_result = 17;
        } else if (!old_visible || old_after.st_ino != old_target.st_ino) {
            // This sysfs mount remains bound to the parent network namespace
            // after unshare(CLONE_NEWNET), so it legitimately continues to
            // expose the parent namespace's lo on both Linux and DragonOS.
            projection_result = 17;
        }
    }
    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, original->name, seq++) != 0) {
        return 18;
    }
    if (projection_result != 0) return projection_result;

    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, std::string("du%d"), seq++) != 0) {
        return 19;
    }
    const auto formatted = QueryLink(fd.Get(), *ifindex, seq++);
    if (!formatted.has_value() || formatted->name != "du0") return 20;
    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, original->name, seq++) != 0) {
        return 21;
    }
    return 0;
}

int RunDragonOsSysfsRenameCase() {
    FdGuard fd(OpenRouteSocket());
    if (fd.Get() < 0) return 10;
    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;

    uint32_t seq = 1;
    const auto original = QueryLink(fd.Get(), *ifindex, seq++);
    if (!original.has_value()) return 12;
    const std::string renamed = "dunitlo0";
    const std::string old_path = "/sys/class/net/" + original->name;
    const std::string new_path = "/sys/class/net/" + renamed;
    struct stat old_link {};
    struct stat old_target {};
    if (lstat(old_path.c_str(), &old_link) != 0 || stat(old_path.c_str(), &old_target) != 0) {
        return 13;
    }
    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, renamed, seq++) != 0) return 14;

    int result = 0;
    struct stat new_link {};
    struct stat new_target {};
    struct stat ignored {};
    if (lstat(old_path.c_str(), &ignored) == 0 || errno != ENOENT ||
        lstat(new_path.c_str(), &new_link) != 0 || stat(new_path.c_str(), &new_target) != 0) {
        result = 15;
    } else if (new_link.st_ino != old_link.st_ino || new_target.st_ino != old_target.st_ino) {
        result = 16;
    }

    // Always restore the globally visible initial-namespace loopback before
    // reporting a failed assertion.
    if (SetLink(fd.Get(), *ifindex, 0, 0, std::nullopt, original->name, seq++) != 0) {
        return 17;
    }
    if (lstat(old_path.c_str(), &ignored) != 0 || lstat(new_path.c_str(), &ignored) == 0 ||
        errno != ENOENT) {
        return 18;
    }
    return result;
}

int RunOwnerNetnsMoveUeventCase() {
    // Netlink sockets retain the network namespace in which they were opened.
    // Keep both owner sockets alive across unshare so the mutation and its
    // kobject event still target the initial namespace.
    FdGuard owner_route(OpenRouteSocket());
    FdGuard owner_uevent(OpenUeventSocket());
    if (owner_route.Get() < 0 || owner_uevent.Get() < 0) return 10;

    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;
    uint32_t seq = 1;
    const auto original = QueryLink(owner_route.Get(), *ifindex, seq++);
    if (!original.has_value()) return 12;

    // Retain the initial user namespace and its CAP_NET_ADMIN while changing
    // only the current network namespace. Linux checks current credentials
    // when a request is sent on the retained owner RTNL socket.
    if (unshare(CLONE_NEWNET) != 0) return 77;
    FdGuard foreign_uevent(OpenUeventSocket());
    if (foreign_uevent.Get() < 0) return 13;

    const std::string renamed = "dunitloev0";
    const int rename_error = SetLink(owner_route.Get(), *ifindex, 0, 0, std::nullopt,
                                     renamed, seq++);
    if (rename_error != 0) {
        if (rename_error == EPERM && !IsDragonOS()) return 77;
        return 14;
    }

    int result = 0;
    const auto owner_event =
        ReceiveExpectedMoveUevent(owner_uevent.Get(), renamed, *ifindex);
    if (!owner_event.has_value()) {
        result = 15;
    } else if (!*owner_event) {
        result = 16;
    }

    const auto foreign_event =
        HasExpectedMoveUeventQueued(foreign_uevent.Get(), renamed, *ifindex);
    if (!foreign_event.has_value()) {
        if (result == 0) result = 17;
    } else if (*foreign_event && result == 0) {
        result = 18;
    }

    // Always restore the initial namespace's globally visible loopback name,
    // including when event validation fails.
    if (SetLink(owner_route.Get(), *ifindex, 0, 0, std::nullopt, original->name,
                seq++) != 0) {
        return 19;
    }
    const auto restored = QueryLink(owner_route.Get(), *ifindex, seq++);
    if (!restored.has_value() || restored->name != original->name) return 20;
    return result;
}

int RunChild(int (*test_case)()) {
    const pid_t child = fork();
    if (child < 0) return 255;
    if (child == 0) _exit(test_case());
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status)) return 254;
    return WEXITSTATUS(status);
}

std::optional<short> QueryIoctlFlags(int fd, const char* name) {
    ifreq request {};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd, SIOCGIFFLAGS, &request) < 0) return std::nullopt;
    return request.ifr_flags;
}

std::optional<uint32_t> QueryLinkFlags(int fd, int ifindex, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;

    if (send(fd, &request, request.header.nlmsg_len, 0) != request.header.nlmsg_len) {
        return std::nullopt;
    }

    std::array<unsigned char, 4096> buffer {};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return std::nullopt;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR) return std::nullopt;
            if (header->nlmsg_type != RTM_NEWLINK) continue;
            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index == ifindex) return link->ifi_flags;
        }
    }
}

std::optional<int> FindEthernetIfindex() {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return std::nullopt;

    for (int i = 0; i <= 20; ++i) {
        const std::string name = "eth" + std::to_string(i);
        ifreq ifr{};
        std::strncpy(ifr.ifr_name, name.c_str(), IFNAMSIZ - 1);
        if (ioctl(fd.Get(), SIOCGIFINDEX, &ifr) == 0) return ifr.ifr_ifindex;
    }
    return std::nullopt;
}

std::optional<uint32_t> QueryLinkMtu(int ifindex) {
    FdGuard fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    if (fd.Get() < 0) return std::nullopt;

    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    if (bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        return std::nullopt;
    }

    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request{};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_GETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST;
    request.header.nlmsg_seq = 1;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = ifindex;

    if (send(fd.Get(), &request, request.header.nlmsg_len, 0) < 0) return std::nullopt;

    char buffer[4096]{};
    for (;;) {
        ssize_t length = recv(fd.Get(), buffer, sizeof(buffer), 0);
        if (length < 0) return std::nullopt;

        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer); NLMSG_OK(header, length);
             header = NLMSG_NEXT(header, length)) {
            if (header->nlmsg_seq != request.header.nlmsg_seq) continue;
            if (header->nlmsg_type == NLMSG_ERROR || header->nlmsg_type == NLMSG_DONE) {
                return std::nullopt;
            }
            if (header->nlmsg_type != RTM_NEWLINK) continue;

            const auto* link = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(header));
            if (link->ifi_index != ifindex) continue;

            int attr_length = IFLA_PAYLOAD(header);
            for (auto* attr = IFLA_RTA(link); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type != IFLA_MTU || RTA_PAYLOAD(attr) != sizeof(uint32_t)) {
                    continue;
                }
                uint32_t mtu = 0;
                std::memcpy(&mtu, RTA_DATA(attr), sizeof(mtu));
                return mtu;
            }
            return std::nullopt;
        }
    }
}

TEST(RtnetlinkLinkSemantics, VirtioEthernetReportsStandardIpMtu) {
    const auto ifindex = FindEthernetIfindex();
    if (!ifindex.has_value()) GTEST_SKIP() << "No eth0-eth20 interface found";

    const auto mtu = QueryLinkMtu(*ifindex);
    ASSERT_TRUE(mtu.has_value()) << "RTM_GETLINK did not return IFLA_MTU: errno=" << errno
                                << " (" << std::strerror(errno) << ")";
    EXPECT_EQ(*mtu, 1500U);
}

int RunFreshNetnsLoopbackLifecycleCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;

    FdGuard ioctl_fd(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard route_fd(OpenRouteSocket());
    if (ioctl_fd.Get() < 0 || route_fd.Get() < 0) return 10;

    const auto ifindex = FindIfindex("lo");
    if (!ifindex.has_value()) return 11;
    const auto ioctl_flags = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    uint32_t seq = 1;
    const auto rtnl_flags = QueryLinkFlags(route_fd.Get(), *ifindex, seq++);
    if (!ioctl_flags.has_value() || !rtnl_flags.has_value()) return 12;

    // Existence through both APIs proves that lo is registered. Linux creates
    // a fresh netns with lo administratively down; volatile runtime flags must
    // not leak from construction either.
    constexpr uint32_t kRtnlDownMask = IFF_UP | IFF_RUNNING | IFF_LOWER_UP | IFF_DORMANT;
    if ((static_cast<uint16_t>(*ioctl_flags) & (IFF_UP | IFF_RUNNING)) != 0) return 13;
    if ((*rtnl_flags & kRtnlDownMask) != 0) return 14;
    if ((static_cast<uint16_t>(*ioctl_flags) & IFF_LOOPBACK) == 0 ||
        (*rtnl_flags & IFF_LOOPBACK) == 0) {
        return 15;
    }

    RouteProbe connected_v4 {AF_INET, 8, RT_TABLE_LOCAL, RTN_LOCAL,
                             static_cast<uint32_t>(*ifindex), {}};
    RouteProbe local_v4 {AF_INET, 32, RT_TABLE_LOCAL, RTN_LOCAL,
                         static_cast<uint32_t>(*ifindex), {}};
    RouteProbe broadcast_v4 {AF_INET, 32, RT_TABLE_LOCAL, RTN_BROADCAST,
                             static_cast<uint32_t>(*ifindex), {}};
    RouteProbe local_v6 {AF_INET6, 128, RT_TABLE_LOCAL, RTN_LOCAL,
                         static_cast<uint32_t>(*ifindex), {}};
    if (inet_pton(AF_INET, "127.0.0.0", connected_v4.destination.data()) != 1 ||
        inet_pton(AF_INET, "127.0.0.1", local_v4.destination.data()) != 1 ||
        inet_pton(AF_INET, "127.255.255.255", broadcast_v4.destination.data()) != 1 ||
        inet_pton(AF_INET6, "::1", local_v6.destination.data()) != 1) {
        return 16;
    }

    if (CountRoute(route_fd.Get(), connected_v4, seq++) != 0 ||
        CountRoute(route_fd.Get(), local_v4, seq++) != 0 ||
        CountRoute(route_fd.Get(), broadcast_v4, seq++) != 0 ||
        CountRoute(route_fd.Get(), local_v6, seq++) != 0) {
        return 17;
    }
    if (SendIpv4LoopbackDatagram() != ENETUNREACH) return 18;

    if (SetLinkUp(route_fd.Get(), *ifindex, true, seq++) != 0) return 19;
    if (CountRoute(route_fd.Get(), connected_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), local_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), broadcast_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), local_v6, seq++) != 1) {
        return 20;
    }
    if (SendIpv4LoopbackDatagram() != 0) return 21;

    if (SetLinkUp(route_fd.Get(), *ifindex, false, seq++) != 0) return 22;
    if (CountRoute(route_fd.Get(), connected_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), local_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), broadcast_v4, seq++) != 0 ||
        CountRoute(route_fd.Get(), local_v6, seq++) != 0) {
        return 23;
    }
    if (SendIpv4LoopbackDatagram() != 0) return 24;

    if (SetLinkUp(route_fd.Get(), *ifindex, true, seq++) != 0) return 25;
    if (CountRoute(route_fd.Get(), connected_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), local_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), broadcast_v4, seq++) != 1 ||
        CountRoute(route_fd.Get(), local_v6, seq++) != 1) {
        return 26;
    }
    return 0;
}

int RunVisibleFlagsCase() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;

    FdGuard ioctl_fd(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard route_fd(OpenRouteSocket());
    if (ioctl_fd.Get() < 0 || route_fd.Get() < 0) return 10;

    ifreq index_request {};
    std::strncpy(index_request.ifr_name, "lo", IFNAMSIZ - 1);
    if (ioctl(ioctl_fd.Get(), SIOCGIFINDEX, &index_request) < 0) return 11;
    const int ifindex = index_request.ifr_ifindex;

    if (SetLinkUp(route_fd.Get(), ifindex, true, 1) != 0) return 12;
    const auto ioctl_up = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_up = QueryLinkFlags(route_fd.Get(), ifindex, 2);
    if (!ioctl_up.has_value() || !rtnl_up.has_value()) return 13;
    constexpr uint32_t kRuntimeFlags = IFF_RUNNING | IFF_LOWER_UP | IFF_DORMANT;
    constexpr uint32_t kExpectedUp = IFF_UP | IFF_RUNNING;
    if ((static_cast<uint16_t>(*ioctl_up) & kExpectedUp) != kExpectedUp) return 14;
    if ((*rtnl_up & kExpectedUp) != kExpectedUp) return 15;
    if ((static_cast<uint16_t>(*ioctl_up) & IFF_LOOPBACK) == 0 ||
        (*rtnl_up & IFF_LOOPBACK) == 0) {
        return 16;
    }

    if (SetLinkUp(route_fd.Get(), ifindex, false, 3) != 0) return 17;
    const auto ioctl_down = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_down = QueryLinkFlags(route_fd.Get(), ifindex, 4);
    if (!ioctl_down.has_value() || !rtnl_down.has_value()) return 18;
    if ((static_cast<uint16_t>(*ioctl_down) & (IFF_UP | kRuntimeFlags)) != 0) return 19;
    if ((*rtnl_down & (IFF_UP | kRuntimeFlags)) != 0) return 20;
    if ((static_cast<uint16_t>(*ioctl_down) & IFF_LOOPBACK) == 0 ||
        (*rtnl_down & IFF_LOOPBACK) == 0) {
        return 21;
    }
    if ((static_cast<uint16_t>(*ioctl_down) & 0xffffU) != (*rtnl_down & 0xffffU)) return 22;

    if (SetLinkUp(route_fd.Get(), ifindex, true, 5) != 0) return 23;
    const auto ioctl_restored = QueryIoctlFlags(ioctl_fd.Get(), "lo");
    const auto rtnl_restored = QueryLinkFlags(route_fd.Get(), ifindex, 6);
    if (!ioctl_restored.has_value() || !rtnl_restored.has_value()) return 24;
    if ((static_cast<uint16_t>(*ioctl_restored) & kExpectedUp) != kExpectedUp) return 25;
    if ((*rtnl_restored & kExpectedUp) != kExpectedUp) return 26;
    return 0;
}

TEST(RtnetlinkLinkSemantics, VisibleFlagsFollowAdministrativeAndRuntimeState) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) _exit(RunVisibleFlagsCase());

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    const int result = WEXITSTATUS(status);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

TEST(RtnetlinkLinkSemantics, FreshNetworkNamespaceLoopbackIsPresentButDown) {
    const int result = RunChild(RunFreshNetnsLoopbackLifecycleCase);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

TEST(RtnetlinkLinkSemantics, SharedTransactionCommitsTogetherAndRejectsPartialFailure) {
    const int result = RunChild(RunCombinedMutationCase);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

TEST(RtnetlinkLinkSemantics, ZeroChangeMaskReplacesOnlyConfigurableFlags) {
    const int result = RunChild(RunReplaceFlagsCase);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

TEST(RtnetlinkLinkSemantics, RenameValidatesNamesFormatsAndProjectedIdentity) {
    const int result = RunChild(RunRenameValidationCase);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

TEST(RtnetlinkLinkSemantics, DragonOsInitialNetnsRenamePreservesSysfsInodes) {
    if (!IsDragonOS()) GTEST_SKIP() << "initial-netns sysfs projection is DragonOS coverage";
    EXPECT_EQ(RunChild(RunDragonOsSysfsRenameCase), 0);
}

TEST(RtnetlinkLinkSemantics, MoveUeventIsDeliveredOnlyToOwningNetworkNamespace) {
    const int result = RunChild(RunOwnerNetnsMoveUeventCase);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host cannot create a privileged network namespace";
    }
    EXPECT_EQ(result, 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
