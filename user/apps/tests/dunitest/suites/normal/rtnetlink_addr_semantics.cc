#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <linux/if_addr.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <optional>
#include <string>
#include <vector>

namespace {

constexpr int kHostHasNoFixedRouteCapacity = -2;
constexpr int kNamespaceUnavailable = -3;
constexpr int kMaxRouteFillAttempts = 64;

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }

  private:
    int fd_;
};

struct IpBytes {
    std::array<uint8_t, 16> bytes{};
    size_t length = 0;
};

struct AddressSpec {
    int family;
    IpBytes local;
    uint8_t prefix_len;
    uint32_t ifindex;
};

struct DumpedAddress {
    int family = AF_UNSPEC;
    IpBytes local;
    uint8_t prefix_len = 0;
    uint32_t ifindex = 0;
    bool has_label = false;
    std::string label;
};

struct RouteSpec {
    uint32_t destination;
    uint8_t prefix_len;
    uint32_t ifindex;
};

struct DumpedRoute {
    uint32_t destination = 0;
    uint8_t prefix_len = 0;
    uint32_t ifindex = 0;
    bool has_gateway = false;
};

struct ChildOutcome {
    bool timed_out;
    int wait_status;
    int stage;
};

bool IsDragonOS() {
    utsname name{};
    return uname(&name) == 0 && std::strstr(name.release, "dragonos") != nullptr;
}

IpBytes ParseAddress(int family, const char* text) {
    IpBytes result{};
    result.length = family == AF_INET ? sizeof(in_addr) : sizeof(in6_addr);
    if (inet_pton(family, text, result.bytes.data()) != 1) result.length = 0;
    return result;
}

uint32_t Ipv4(const std::string& text) {
    uint32_t address = 0;
    if (inet_pton(AF_INET, text.c_str(), &address) != 1) return 0;
    return address;
}

bool SameAddress(const IpBytes& left, const IpBytes& right) {
    return left.length == right.length && left.length != 0 &&
           std::memcmp(left.bytes.data(), right.bytes.data(), left.length) == 0;
}

int OpenRouteSocket(uint32_t groups = 0) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout{};
    timeout.tv_sec = 2;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    local.nl_groups = groups;
    if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

template <typename Body>
std::vector<uint8_t> NewRequest(uint16_t type, uint16_t flags, uint32_t sequence) {
    std::vector<uint8_t> request(NLMSG_LENGTH(sizeof(Body)), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = request.size();
    header->nlmsg_type = type;
    header->nlmsg_flags = flags;
    header->nlmsg_seq = sequence;
    return request;
}

void AddAttr(std::vector<uint8_t>* request, uint16_t type, const void* data, size_t length) {
    auto* header = reinterpret_cast<nlmsghdr*>(request->data());
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    const size_t attr_length = RTA_LENGTH(length);
    const size_t end = offset + RTA_ALIGN(attr_length);
    request->resize(end, 0);

    header = reinterpret_cast<nlmsghdr*>(request->data());
    auto* attr = reinterpret_cast<rtattr*>(request->data() + offset);
    attr->rta_type = type;
    attr->rta_len = attr_length;
    std::memcpy(RTA_DATA(attr), data, length);
    header->nlmsg_len = end;
}

int ReceiveAck(int fd, uint32_t sequence) {
    std::array<uint8_t, 8192> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != sequence || header->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int SendAndReceiveAck(int fd, const std::vector<uint8_t>& request) {
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }
    return ReceiveAck(fd, reinterpret_cast<const nlmsghdr*>(request.data())->nlmsg_seq);
}

int SetLinkUp(int fd, uint32_t ifindex, uint32_t sequence) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, sequence);
    auto* message = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    message->ifi_family = AF_UNSPEC;
    message->ifi_index = static_cast<int>(ifindex);
    message->ifi_flags = IFF_UP;
    message->ifi_change = IFF_UP;
    return SendAndReceiveAck(fd, request);
}

int SetLinkMtu(int fd, uint32_t ifindex, uint32_t mtu, uint32_t sequence) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, sequence);
    auto* message = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    message->ifi_family = AF_UNSPEC;
    message->ifi_index = static_cast<int>(ifindex);
    AddAttr(&request, IFLA_MTU, &mtu, sizeof(mtu));
    return SendAndReceiveAck(fd, request);
}

int SetLinkName(int fd, uint32_t ifindex, const char* name, uint32_t sequence) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, sequence);
    auto* message = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    message->ifi_family = AF_UNSPEC;
    message->ifi_index = static_cast<int>(ifindex);
    AddAttr(&request, IFLA_IFNAME, name, std::strlen(name) + 1);
    return SendAndReceiveAck(fd, request);
}

int SetLinkNameAndState(int fd, uint32_t ifindex, const char* name, bool up,
                        uint32_t sequence, std::optional<uint32_t> mtu = std::nullopt) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, sequence);
    auto* message = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    message->ifi_family = AF_UNSPEC;
    message->ifi_index = static_cast<int>(ifindex);
    message->ifi_flags = up ? IFF_UP : 0;
    message->ifi_change = IFF_UP;
    AddAttr(&request, IFLA_IFNAME, name, std::strlen(name) + 1);
    if (mtu.has_value()) AddAttr(&request, IFLA_MTU, &*mtu, sizeof(*mtu));
    return SendAndReceiveAck(fd, request);
}

std::optional<bool> LinkIsUp(const char* name) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return std::nullopt;
    ifreq request{};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd.Get(), SIOCGIFFLAGS, &request) != 0) return std::nullopt;
    return (request.ifr_flags & IFF_UP) != 0;
}

int ChangeAddress(int fd, uint16_t type, uint16_t flags, const AddressSpec& address,
                  bool include_local, bool include_address, uint32_t sequence,
                  const char* label = nullptr,
                  std::optional<size_t> label_length = std::nullopt) {
    auto request = NewRequest<ifaddrmsg>(type, flags, sequence);
    auto* message = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    message->ifa_family = address.family;
    message->ifa_prefixlen = address.prefix_len;
    message->ifa_scope = address.family == AF_INET6 ? RT_SCOPE_UNIVERSE : RT_SCOPE_HOST;
    message->ifa_index = address.ifindex;
    if (address.family == AF_INET6) message->ifa_flags = IFA_F_NODAD;
    if (include_local) {
        AddAttr(&request, IFA_LOCAL, address.local.bytes.data(), address.local.length);
    }
    if (include_address) {
        AddAttr(&request, IFA_ADDRESS, address.local.bytes.data(), address.local.length);
    }
    if (label != nullptr) {
        AddAttr(&request, IFA_LABEL, label, label_length.value_or(std::strlen(label) + 1));
    }
    return SendAndReceiveAck(fd, request);
}

int ChangeRoute(int fd, uint16_t type, uint16_t flags, const RouteSpec& route,
                uint32_t sequence) {
    auto request = NewRequest<rtmsg>(type, flags, sequence);
    auto* message = reinterpret_cast<rtmsg*>(NLMSG_DATA(request.data()));
    message->rtm_family = AF_INET;
    message->rtm_dst_len = route.prefix_len;
    message->rtm_table = RT_TABLE_MAIN;
    message->rtm_protocol = RTPROT_STATIC;
    message->rtm_scope = RT_SCOPE_LINK;
    message->rtm_type = RTN_UNICAST;
    AddAttr(&request, RTA_DST, &route.destination, sizeof(route.destination));
    AddAttr(&request, RTA_OIF, &route.ifindex, sizeof(route.ifindex));
    return SendAndReceiveAck(fd, request);
}

int DumpAddresses(int fd, int family, uint32_t sequence, std::vector<DumpedAddress>* result) {
    auto request = NewRequest<ifaddrmsg>(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, sequence);
    reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()))->ifa_family = family;
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
            if (header->nlmsg_seq != sequence) continue;
            if (header->nlmsg_type == NLMSG_DONE) return 0;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? 0 : -error->error;
            }
            if (header->nlmsg_type != RTM_NEWADDR) continue;

            const auto* message = reinterpret_cast<const ifaddrmsg*>(NLMSG_DATA(header));
            DumpedAddress address{};
            address.family = message->ifa_family;
            address.prefix_len = message->ifa_prefixlen;
            address.ifindex = message->ifa_index;
            address.local.length = message->ifa_family == AF_INET ? sizeof(in_addr)
                                                                  : sizeof(in6_addr);
            std::optional<IpBytes> fallback;
            int attr_length = IFA_PAYLOAD(header);
            for (auto* attr = IFA_RTA(message); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type == IFA_LABEL) {
                    address.has_label = true;
                    if (RTA_PAYLOAD(attr) > 0) {
                        address.label.assign(static_cast<const char*>(RTA_DATA(attr)),
                                             strnlen(static_cast<const char*>(RTA_DATA(attr)),
                                                     RTA_PAYLOAD(attr)));
                    }
                    continue;
                }
                if ((attr->rta_type != IFA_LOCAL && attr->rta_type != IFA_ADDRESS) ||
                    RTA_PAYLOAD(attr) < address.local.length) {
                    continue;
                }
                IpBytes parsed{};
                parsed.length = address.local.length;
                std::memcpy(parsed.bytes.data(), RTA_DATA(attr), parsed.length);
                if (attr->rta_type == IFA_LOCAL) {
                    address.local = parsed;
                } else {
                    fallback = parsed;
                }
            }
            if (address.local.length != 0 &&
                std::all_of(address.local.bytes.begin(), address.local.bytes.end(),
                            [](uint8_t byte) { return byte == 0; }) &&
                fallback.has_value()) {
                address.local = *fallback;
            }
            result->push_back(address);
        }
    }
}

int DumpRoutes(int fd, uint32_t sequence, std::vector<DumpedRoute>* result) {
    auto request = NewRequest<rtmsg>(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, sequence);
    reinterpret_cast<rtmsg*>(NLMSG_DATA(request.data()))->rtm_family = AF_INET;
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
            if (header->nlmsg_seq != sequence) continue;
            if (header->nlmsg_type == NLMSG_DONE) return 0;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? 0 : -error->error;
            }
            if (header->nlmsg_type != RTM_NEWROUTE) continue;

            const auto* message = reinterpret_cast<const rtmsg*>(NLMSG_DATA(header));
            if (message->rtm_family != AF_INET) continue;
            DumpedRoute route{};
            route.prefix_len = message->rtm_dst_len;
            int attr_length = RTM_PAYLOAD(header);
            for (auto* attr = RTM_RTA(message); RTA_OK(attr, attr_length);
                 attr = RTA_NEXT(attr, attr_length)) {
                if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) >= sizeof(route.destination)) {
                    std::memcpy(&route.destination, RTA_DATA(attr), sizeof(route.destination));
                } else if (attr->rta_type == RTA_OIF &&
                           RTA_PAYLOAD(attr) >= sizeof(route.ifindex)) {
                    std::memcpy(&route.ifindex, RTA_DATA(attr), sizeof(route.ifindex));
                } else if (attr->rta_type == RTA_GATEWAY) {
                    route.has_gateway = true;
                }
            }
            result->push_back(route);
        }
    }
}

int CountAddress(int fd, const AddressSpec& expected, uint32_t sequence) {
    std::vector<DumpedAddress> addresses;
    if (const int error = DumpAddresses(fd, expected.family, sequence, &addresses); error != 0) {
        return -error;
    }
    int count = 0;
    for (const auto& address : addresses) {
        if (address.family == expected.family && address.ifindex == expected.ifindex &&
            address.prefix_len == expected.prefix_len &&
            SameAddress(address.local, expected.local)) {
            ++count;
        }
    }
    return count;
}

int CountAddressWithLabel(int fd, const AddressSpec& expected, const char* label,
                          uint32_t sequence) {
    std::vector<DumpedAddress> addresses;
    if (const int error = DumpAddresses(fd, expected.family, sequence, &addresses); error != 0) {
        return -error;
    }
    return static_cast<int>(std::count_if(addresses.begin(), addresses.end(), [&](const auto& item) {
        return item.family == expected.family && item.ifindex == expected.ifindex &&
               item.prefix_len == expected.prefix_len && SameAddress(item.local, expected.local) &&
               item.label == label;
    }));
}

int CountAddressWithoutLabel(int fd, const AddressSpec& expected, uint32_t sequence) {
    std::vector<DumpedAddress> addresses;
    if (const int error = DumpAddresses(fd, expected.family, sequence, &addresses); error != 0) {
        return -error;
    }
    return static_cast<int>(std::count_if(addresses.begin(), addresses.end(), [&](const auto& item) {
        return item.family == expected.family && item.ifindex == expected.ifindex &&
               item.prefix_len == expected.prefix_len && SameAddress(item.local, expected.local) &&
               !item.has_label;
    }));
}

int CountRoute(int fd, const RouteSpec& expected, uint32_t sequence) {
    std::vector<DumpedRoute> routes;
    if (const int error = DumpRoutes(fd, sequence, &routes); error != 0) return -error;
    int count = 0;
    for (const auto& route : routes) {
        if (route.destination == expected.destination && route.prefix_len == expected.prefix_len &&
            route.ifindex == expected.ifindex && !route.has_gateway) {
            ++count;
        }
    }
    return count;
}

bool NotificationMatchesAddress(const nlmsghdr* header, uint16_t type,
                                const AddressSpec& expected, const char* label = nullptr,
                                bool require_label_absent = false) {
    if (header->nlmsg_type != type) return false;
    const auto* message = reinterpret_cast<const ifaddrmsg*>(NLMSG_DATA(header));
    if (message->ifa_family != expected.family || message->ifa_index != expected.ifindex ||
        message->ifa_prefixlen != expected.prefix_len) {
        return false;
    }
    int attr_length = IFA_PAYLOAD(header);
    bool address_matches = false;
    bool label_matches = label == nullptr;
    bool has_label = false;
    for (auto* attr = IFA_RTA(message); RTA_OK(attr, attr_length);
         attr = RTA_NEXT(attr, attr_length)) {
        if ((attr->rta_type == IFA_LOCAL || attr->rta_type == IFA_ADDRESS) &&
            RTA_PAYLOAD(attr) >= expected.local.length &&
            std::memcmp(RTA_DATA(attr), expected.local.bytes.data(), expected.local.length) == 0) {
            address_matches = true;
        } else if (attr->rta_type == IFA_LABEL && label != nullptr && RTA_PAYLOAD(attr) > 0 &&
                   strncmp(static_cast<const char*>(RTA_DATA(attr)), label, RTA_PAYLOAD(attr)) ==
                       0) {
            has_label = true;
            label_matches = true;
        } else if (attr->rta_type == IFA_LABEL) {
            has_label = true;
        }
    }
    return address_matches && label_matches && (!require_label_absent || !has_label);
}

void DrainNotifications(int fd) {
    std::array<uint8_t, 16384> buffer{};
    for (;;) {
        pollfd descriptor{fd, POLLIN, 0};
        const int polled = poll(&descriptor, 1, 20);
        if (polled <= 0) return;
        if (recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT) <= 0) return;
    }
}

int CountAddressNotifications(int fd, uint16_t type, const AddressSpec& address,
                              const char* label = nullptr, bool require_label_absent = false) {
    std::array<uint8_t, 16384> buffer{};
    int count = 0;
    bool received_any = false;
    for (;;) {
        pollfd descriptor{fd, POLLIN, 0};
        const int polled = poll(&descriptor, 1, received_any ? 10 : 100);
        if (polled == 0) return count;
        if (polled < 0) return -errno;
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT);
        if (received < 0) return errno == EAGAIN || errno == EWOULDBLOCK ? count : -errno;
        if (received == 0) return count;
        received_any = true;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (NotificationMatchesAddress(header, type, address, label, require_label_absent)) {
                ++count;
            }
        }
    }
}

int BindUdp(const AddressSpec& address, uint16_t port, int* fd_out = nullptr) {
    int fd = socket(address.family, SOCK_DGRAM, 0);
    if (fd < 0) return errno;
    int error = 0;
    if (address.family == AF_INET) {
        sockaddr_in local{};
        local.sin_family = AF_INET;
        local.sin_port = htons(port);
        std::memcpy(&local.sin_addr, address.local.bytes.data(), sizeof(local.sin_addr));
        if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) error = errno;
    } else {
        sockaddr_in6 local{};
        local.sin6_family = AF_INET6;
        local.sin6_port = htons(port);
        local.sin6_scope_id = address.ifindex;
        std::memcpy(&local.sin6_addr, address.local.bytes.data(), sizeof(local.sin6_addr));
        if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) error = errno;
    }
    if (error != 0 || fd_out == nullptr) {
        close(fd);
    } else {
        *fd_out = fd;
    }
    return error;
}

int VerifyBoundUdpDelivery(const AddressSpec& address) {
    int receiver_raw = -1;
    if (const int error = BindUdp(address, 0, &receiver_raw); error != 0) return 100 + error;
    FdGuard receiver(receiver_raw);
    timeval timeout{};
    timeout.tv_sec = 2;
    if (setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        return 200 + errno;
    }

    int sender_raw = -1;
    if (const int error = BindUdp(address, 0, &sender_raw); error != 0) return 300 + error;
    FdGuard sender(sender_raw);
    const char payload[] = "address-route";
    if (address.family == AF_INET) {
        sockaddr_in destination{};
        socklen_t length = sizeof(destination);
        if (getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&destination), &length) != 0) {
            return 400 + errno;
        }
        if (sendto(sender.Get(), payload, sizeof(payload), 0,
                   reinterpret_cast<sockaddr*>(&destination), length) !=
            static_cast<ssize_t>(sizeof(payload))) {
            return 500 + errno;
        }
    } else {
        sockaddr_in6 destination{};
        socklen_t length = sizeof(destination);
        if (getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&destination), &length) != 0) {
            return 600 + errno;
        }
        if (sendto(sender.Get(), payload, sizeof(payload), 0,
                   reinterpret_cast<sockaddr*>(&destination), length) !=
            static_cast<ssize_t>(sizeof(payload))) {
            return 700 + errno;
        }
    }
    std::array<char, sizeof(payload)> received{};
    if (recv(receiver.Get(), received.data(), received.size(), 0) !=
        static_cast<ssize_t>(sizeof(payload))) {
        return 800 + errno;
    }
    return std::memcmp(payload, received.data(), sizeof(payload)) == 0 ? 0 : EIO;
}

int PrepareNamespace(FdGuard* netlink, uint32_t* ifindex, uint32_t* sequence) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return kNamespaceUnavailable;
    const int fd = OpenRouteSocket();
    if (fd < 0) return 1000 + errno;
    netlink->Reset(fd);
    *ifindex = if_nametoindex("lo");
    if (*ifindex == 0) return 1200 + errno;
    if (const int error = SetLinkUp(netlink->Get(), *ifindex, ++(*sequence)); error != 0) {
        return 1300 + error;
    }
    return 0;
}

AddressSpec MakeAddress(int family, const char* text, uint8_t prefix_len, uint32_t ifindex) {
    return {family, ParseAddress(family, text), prefix_len, ifindex};
}

int RunMultipleAddressConsistency() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 100;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;

    const AddressSpec first = MakeAddress(AF_INET, "192.0.2.1", 24, ifindex);
    const AddressSpec second = MakeAddress(AF_INET, "198.51.100.1", 24, ifindex);
    const AddressSpec ipv6 = MakeAddress(AF_INET6, "2001:db8:1::1", 64, ifindex);
    const std::array<AddressSpec, 3> addresses{first, second, ipv6};
    for (const auto& address : addresses) {
        if (const int error = ChangeAddress(fd.Get(), RTM_NEWADDR,
                                            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE |
                                                NLM_F_EXCL,
                                            address, true, true, ++sequence);
            error != 0) {
            return 2000 + error;
        }
        if (CountAddress(fd.Get(), address, ++sequence) != 1) return 2100;
        if (const int error = BindUdp(address, 0); error != 0) return 2200 + error;
    }

    const RouteSpec first_route{Ipv4("192.0.2.0"), 24, ifindex};
    const RouteSpec second_route{Ipv4("198.51.100.0"), 24, ifindex};
    if (CountRoute(fd.Get(), first_route, ++sequence) < 1 ||
        CountRoute(fd.Get(), second_route, ++sequence) < 1) {
        return 2300;
    }
    if (const int error = ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK,
                                        second, true, true, ++sequence);
        error != 0) {
        return 2500 + error;
    }
    if (CountAddress(fd.Get(), second, ++sequence) != 0 ||
        CountRoute(fd.Get(), second_route, ++sequence) != 0 ||
        CountRoute(fd.Get(), first_route, ++sequence) < 1 ||
        CountAddress(fd.Get(), first, ++sequence) != 1 ||
        CountAddress(fd.Get(), ipv6, ++sequence) != 1) {
        return 2600;
    }
    // Explicitly bind both endpoints to the configured address. This tests
    // address ownership and smoltcp delivery without conflating the result
    // with the separate implicit-source selection policy for non-127/8
    // addresses assigned to a loopback device.
    if (const int error = VerifyBoundUdpDelivery(first); error != 0) return 2700 + error;
    return 0;
}

int RunIdentityFlagsAndErrorPriority() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 1000;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;
    FdGuard notifications(OpenRouteSocket(RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR));
    if (notifications.Get() < 0) return 3000 + errno;

    AddressSpec ipv4 = MakeAddress(AF_INET, "192.0.2.33", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, ipv4, true, true,
                      ++sequence) != 0) {
        return 3100;
    }
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, ipv4, true, true,
                      ++sequence) != EEXIST) {
        return 3200;
    }
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, ipv4,
                      true, true, ++sequence) != 0 ||
        CountAddress(fd.Get(), ipv4, ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, ipv4) != 1) {
        return 3300;
    }
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE | NLM_F_EXCL, ipv4, true, true,
                      ++sequence) != EEXIST) {
        return 3400;
    }

    const AddressSpec ipv6 = MakeAddress(AF_INET6, "2001:db8:2::1", 64, ifindex);
    const AddressSpec ipv6_other_prefix = MakeAddress(AF_INET6, "2001:db8:2::1", 96, ifindex);
    constexpr char kIgnoredIpv6Label[] =
        "1234567890123456123456789012345612345678901234561234567890123456";
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, ipv6, false, true,
                      ++sequence, kIgnoredIpv6Label) != 0 ||
        CountAddressWithoutLabel(fd.Get(), ipv6, ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, ipv6, nullptr, true) != 1 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, ipv6_other_prefix,
                      false, true, ++sequence) != EEXIST) {
        return 3500;
    }
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE,
                      ipv6_other_prefix, false, true, ++sequence, "", 0) != 0) {
        return 3600;
    }
    if (CountAddress(fd.Get(), ipv6, ++sequence) != 1) return 3601;
    if (CountAddress(fd.Get(), ipv6_other_prefix, ++sequence) != 0) return 3602;
    const int ipv6_notifications =
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, ipv6);
    if (ipv6_notifications != 1) return 36030 + ipv6_notifications;
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, ipv6_other_prefix,
                      false, true, ++sequence) != EADDRNOTAVAIL ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, ipv6, false, true,
                      ++sequence, kIgnoredIpv6Label) != 0 ||
        CountAddressNotifications(notifications.Get(), RTM_DELADDR, ipv6, nullptr, true) != 1) {
        return 3700;
    }

    AddressSpec wrong_prefix = ipv4;
    wrong_prefix.prefix_len = 255;
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, wrong_prefix, true, false,
                      ++sequence) != 0 ||
        CountAddress(fd.Get(), ipv4, ++sequence) != 0) {
        return 3800;
    }

    const AddressSpec local_only = MakeAddress(AF_INET, "192.0.2.44", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, local_only, false,
                      true, ++sequence) != EINVAL ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, local_only, true,
                      false, ++sequence) != 0) {
        return 3900;
    }
    AddressSpec network_selector = local_only;
    network_selector.local = ParseAddress(AF_INET, "192.0.2.0");
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, network_selector, false,
                      true, ++sequence) != 0 ||
        CountAddress(fd.Get(), local_only, ++sequence) != 0) {
        return 3950;
    }

    AddressSpec unknown = MakeAddress(AF_INET, "198.51.100.9", 24, 0x7fffffffu);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, unknown, false, false,
                      ++sequence) != EINVAL) {
        return 4000;
    }
    const AddressSpec unsupported{AF_UNSPEC, {}, 0, unknown.ifindex};
    const int unsupported_error = ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK,
                                                unsupported, false, false, ++sequence);
    if ((IsDragonOS() && unsupported_error != EAFNOSUPPORT) ||
        (!IsDragonOS() && unsupported_error != EAFNOSUPPORT && unsupported_error != EOPNOTSUPP)) {
        return 4050;
    }
    AddressSpec invalid_prefix = unknown;
    invalid_prefix.prefix_len = 33;
    AddressSpec zero_index = unknown;
    zero_index.ifindex = 0;
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, invalid_prefix, true,
                      true, ++sequence) != EINVAL ||
        ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, unknown, true, true,
                      ++sequence) != ENODEV ||
        ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, zero_index, true, true,
                      ++sequence) != ENODEV) {
        return 4100;
    }

    const AddressSpec alias = MakeAddress(AF_INET, "198.51.100.44", 24, ifindex);
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, alias, true, true,
                      ++sequence, "lo:1") != 0 ||
        CountAddressWithLabel(fd.Get(), alias, "lo:1", ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, alias, "lo:1") != 1) {
        return 4150;
    }
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, alias,
                      true, true, ++sequence, "lo:9") != 0 ||
        CountAddressWithLabel(fd.Get(), alias, "lo:1", ++sequence) != 1) {
        return 4155;
    }
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, alias, true, true,
                      ++sequence, "lo:2") != EADDRNOTAVAIL ||
        CountAddressWithLabel(fd.Get(), alias, "lo:1", ++sequence) != 1) {
        return 4160;
    }
    DrainNotifications(notifications.Get());
    if (SetLinkName(fd.Get(), ifindex, "ren0", ++sequence) != 0 ||
        CountAddressWithLabel(fd.Get(), alias, "ren0:1", ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, alias, "ren0:1") != 1 ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, alias, true, true,
                      ++sequence, "lo:1") != EADDRNOTAVAIL) {
        return 4165;
    }
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, alias, true, true,
                      ++sequence, "ren0:1") != 0 ||
        CountAddress(fd.Get(), alias, ++sequence) != 0 ||
        CountAddressNotifications(notifications.Get(), RTM_DELADDR, alias, "ren0:1") != 1) {
        return 4170;
    }

    const AddressSpec long_label = MakeAddress(AF_INET, "198.51.100.45", 24, ifindex);
    AddressSpec long_label_unknown = long_label;
    long_label_unknown.ifindex = 0x7fffffffu;
    const AddressSpec zero_address = MakeAddress(AF_INET, "0.0.0.0", 0, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, long_label, true,
                      true, ++sequence, "", 0) != ERANGE ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, long_label, true,
                      true, ++sequence, "1234567890123456") != ERANGE ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                      long_label_unknown, true, true, ++sequence, "1234567890123456") != ERANGE ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, zero_address, true,
                      true, ++sequence, "1234567890123456") != ERANGE) {
        return 4180;
    }
    const AddressSpec empty_label = MakeAddress(AF_INET, "198.51.100.46", 24, ifindex);
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, empty_label, true,
                      true, ++sequence, "") != 0 ||
        CountAddressWithoutLabel(fd.Get(), empty_label, ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, empty_label, nullptr, true) !=
            1 ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, empty_label, true, true,
                      ++sequence, "", 0) != ERANGE ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, empty_label, true, true,
                      ++sequence, "lo") != EADDRNOTAVAIL ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, empty_label, true, true,
                      ++sequence, "") != 0 ||
        CountAddressNotifications(notifications.Get(), RTM_DELADDR, empty_label, nullptr, true) !=
            1) {
        return 4190;
    }

    const AddressSpec absent = MakeAddress(AF_INET, "203.0.113.77", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, absent,
                      true, true, ++sequence) != 0 ||
        CountAddress(fd.Get(), absent, ++sequence) != 1) {
        return 4200;
    }
    const AddressSpec missing = MakeAddress(AF_INET, "203.0.113.78", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, missing, true, true,
                      ++sequence) != EADDRNOTAVAIL) {
        return 4300;
    }
    return 0;
}

int RunCombinedRenameAndLinkState() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 1800;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;
    FdGuard notifications(OpenRouteSocket(RTMGRP_IPV4_IFADDR));
    if (notifications.Get() < 0) return 4400 + errno;

    const AddressSpec primary = MakeAddress(AF_INET, "127.0.0.1", 8, ifindex);
    const AddressSpec preserved = MakeAddress(AF_INET, "198.51.100.51", 24, ifindex);
    const AddressSpec generated = MakeAddress(AF_INET, "198.51.100.52", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, preserved, true,
                      true, ++sequence, "lo:7") != 0 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, generated, true,
                      true, ++sequence, "plain") != 0) {
        return 4410;
    }

    // A later invalid attribute must reject the whole combined request before
    // publishing either the requested name or administrative state.
    if (SetLinkNameAndState(fd.Get(), ifindex, "bad0", false, ++sequence, UINT32_MAX) != EINVAL ||
        if_nametoindex("lo") != ifindex || if_nametoindex("bad0") != 0 ||
        LinkIsUp("lo") != std::optional<bool>(true)) {
        return 4420;
    }

    DrainNotifications(notifications.Get());
    if (SetLinkNameAndState(fd.Get(), ifindex, "txn0", false, ++sequence) != 0 ||
        if_nametoindex("txn0") != ifindex || if_nametoindex("lo") != 0 ||
        LinkIsUp("txn0") != std::optional<bool>(false) ||
        CountAddressWithLabel(fd.Get(), primary, "txn0", ++sequence) != 1 ||
        CountAddressWithLabel(fd.Get(), preserved, "txn0:7", ++sequence) != 1 ||
        CountAddressWithLabel(fd.Get(), generated, "txn0:3", ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, preserved, "txn0:7") != 1) {
        return 4430;
    }

    DrainNotifications(notifications.Get());
    if (SetLinkNameAndState(fd.Get(), ifindex, "txn1", true, ++sequence) != 0 ||
        if_nametoindex("txn1") != ifindex || if_nametoindex("txn0") != 0 ||
        LinkIsUp("txn1") != std::optional<bool>(true) ||
        CountAddressWithLabel(fd.Get(), primary, "txn1", ++sequence) != 1 ||
        CountAddressWithLabel(fd.Get(), preserved, "txn1:7", ++sequence) != 1 ||
        CountAddressWithLabel(fd.Get(), generated, "txn1:3", ++sequence) != 1 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, generated, "txn1:3") != 1) {
        return 4440;
    }
    return 0;
}

int RunLowMtuIpv4Admission() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 1900;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;

    const AddressSpec address = MakeAddress(AF_INET, "192.0.2.61", 24, ifindex);
    const RouteSpec connected{Ipv4("192.0.2.0"), 24, ifindex};
    if (SetLinkMtu(fd.Get(), ifindex, 67, ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true,
                      true, ++sequence) != ENOBUFS ||
        CountAddress(fd.Get(), address, ++sequence) != 0 ||
        CountRoute(fd.Get(), connected, ++sequence) != 0) {
        return 4500;
    }

    if (SetLinkMtu(fd.Get(), ifindex, 68, ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true,
                      true, ++sequence) != 0 ||
        CountAddress(fd.Get(), address, ++sequence) != 1 ||
        CountRoute(fd.Get(), connected, ++sequence) < 1) {
        return 4510;
    }
    if (const int error = VerifyBoundUdpDelivery(address); error != 0) return 4520 + error;
    return 0;
}

int RunInvalidAddressSafety() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 2000;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;
    FdGuard notifications(OpenRouteSocket(RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR));
    if (notifications.Get() < 0) return 5000 + errno;

    const AddressSpec multicast4 = MakeAddress(AF_INET, "224.0.0.123", 24, ifindex);
    const int multicast4_error = ChangeAddress(
        fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        multicast4, true, true, ++sequence);
    if (IsDragonOS() && multicast4_error == 0) return 5100;

    const AddressSpec multicast6 = MakeAddress(AF_INET6, "ff02::123", 64, ifindex);
    const int multicast6_error = ChangeAddress(
        fd.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        multicast6, false, true, ++sequence);
    if (IsDragonOS() && multicast6_error == 0) return 5200;

    DrainNotifications(notifications.Get());
    const AddressSpec unspecified4 = MakeAddress(AF_INET, "0.0.0.0", 0, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, unspecified4, true,
                      true, ++sequence) != 0 ||
        CountAddress(fd.Get(), unspecified4, ++sequence) != 0 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, unspecified4) != 0) {
        return 5300;
    }

    const AddressSpec unspecified6 = MakeAddress(AF_INET6, "::", 0, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, unspecified6,
                      false, true, ++sequence) == 0) {
        return 5400;
    }

    const AddressSpec valid = MakeAddress(AF_INET, "198.51.100.123", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, valid, true, true,
                      ++sequence) != 0 ||
        CountAddress(fd.Get(), valid, ++sequence) != 1) {
        return 5500;
    }
    return 0;
}

RouteSpec FillCandidate(int index, uint32_t ifindex) {
    const std::string destination = "198.18." + std::to_string(index) + ".0";
    return {Ipv4(destination), 24, ifindex};
}

int FillRoutesToCapacity(int fd, uint32_t ifindex, uint32_t* sequence,
                         std::vector<RouteSpec>* added, RouteSpec* failed) {
    for (int index = 0; index < kMaxRouteFillAttempts; ++index) {
        const RouteSpec candidate = FillCandidate(index, ifindex);
        const int error = ChangeRoute(fd, RTM_NEWROUTE,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      candidate, ++(*sequence));
        if (error == 0) {
            added->push_back(candidate);
        } else if (error == ENOSPC) {
            *failed = candidate;
            return 0;
        } else {
            return error;
        }
    }
    return kHostHasNoFixedRouteCapacity;
}

int RunRouteGrowthAndAddressMutation() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 3000;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;
    if (!IsDragonOS()) return kHostHasNoFixedRouteCapacity;
    FdGuard notifications(OpenRouteSocket(RTMGRP_IPV4_IFADDR));
    if (notifications.Get() < 0) return 6000 + errno;

    std::vector<RouteSpec> fillers;
    RouteSpec failed_route{};
    const int fill_error =
        FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers, &failed_route);
    if (fill_error == kHostHasNoFixedRouteCapacity) {
        const AddressSpec scalable_address = MakeAddress(AF_INET, "203.0.113.1", 24, ifindex);
        const RouteSpec scalable_connected{Ipv4("203.0.113.0"), 24, ifindex};
        if (fillers.size() != kMaxRouteFillAttempts ||
            ChangeAddress(fd.Get(), RTM_NEWADDR,
                          NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                          scalable_address, true, true, ++sequence) != 0 ||
            CountAddress(fd.Get(), scalable_address, ++sequence) != 1 ||
            CountRoute(fd.Get(), scalable_connected, ++sequence) < 1) {
            return 6050;
        }
        if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, scalable_address,
                          true, true, ++sequence) != 0) {
            return 6075;
        }
        for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
            if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, *iterator,
                            ++sequence) != 0) {
                return 6090;
            }
        }
        return 0;
    }
    if (fill_error != 0) {
        return 6100 + fill_error;
    }
    if (fillers.empty()) return 6200;
    const size_t route_capacity = fillers.size();

    const AddressSpec address = MakeAddress(AF_INET, "203.0.113.1", 24, ifindex);
    const RouteSpec connected{Ipv4("203.0.113.0"), 24, ifindex};
    DrainNotifications(notifications.Get());
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != ENOSPC) {
        return 6300;
    }
    if (CountAddress(fd.Get(), address, ++sequence) != 0 ||
        CountRoute(fd.Get(), connected, ++sequence) != 0 ||
        CountAddressNotifications(notifications.Get(), RTM_NEWADDR, address) != 0) {
        return 6400;
    }
    if (BindUdp(address, 0) != EADDRNOTAVAIL) return 6500;

    for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
        if (const int error = ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                          *iterator, ++sequence);
            error != 0) {
            return 6600 + error;
        }
    }
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != 0 ||
        CountAddress(fd.Get(), address, ++sequence) != 1 ||
        CountRoute(fd.Get(), connected, ++sequence) < 1) {
        return 6700;
    }

    const RouteSpec explicit_same_value{Ipv4("203.0.113.0"), 24, ifindex};
    fillers.clear();
    if (const int error = FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers,
                                               &failed_route);
        error != 0) {
        return 6800 + error;
    }
    if (fillers.size() + 1 != route_capacity)
        return 6900 + static_cast<int>(fillers.size());
    // The table is full, but the explicit route is a second logical owner of
    // the address's canonical projection and therefore needs no new slot.
    if (ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                    explicit_same_value, ++sequence) != 0) {
        return 6905;
    }
    if (const int error = ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK,
                                        address, true, true, ++sequence);
        error != 0) {
        return 6910 + error;
    }
    if (CountAddress(fd.Get(), address, ++sequence) != 0) return 6920;
    if (CountRoute(fd.Get(), connected, ++sequence) != 1) return 6930;
    if (ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed_route,
                    ++sequence) != ENOSPC) {
        return 6940;
    }
    if (const int error = ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                      explicit_same_value, ++sequence);
        error != 0) {
        return 6950 + error;
    }
    if (ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed_route,
                    ++sequence) != 0) {
        return 6960;
    }

    if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, failed_route,
                    ++sequence) != 0) {
        return 7000;
    }
    for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
        if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, *iterator,
                        ++sequence) != 0) {
            return 7100;
        }
    }

    // Repeat with the opposite creation order and delete the explicit owner
    // while the address still exists. The shared projection must remain until
    // the final owner disappears, then release exactly one physical slot.
    if (ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                    explicit_same_value, ++sequence) != 0) {
        return 7200;
    }
    fillers.clear();
    if (const int error = FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers,
                                               &failed_route);
        error != 0 || fillers.size() + 1 != route_capacity) {
        return 7300 + (error > 0 ? error : 0);
    }
    // Exercise the inverse full-table fast path: the address shares the
    // already projected explicit route.
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != 0) {
        return 7350;
    }
    if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, explicit_same_value,
                    ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed_route,
                    ++sequence) != ENOSPC ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, address, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed_route,
                    ++sequence) != 0) {
        return 7400;
    }
    return 0;
}

int RunSharedAddressAndExplicitRouteProjection() {
    FdGuard fd;
    uint32_t ifindex = 0;
    uint32_t sequence = 4000;
    if (const int error = PrepareNamespace(&fd, &ifindex, &sequence); error != 0) return error;
    if (!IsDragonOS()) return kHostHasNoFixedRouteCapacity;

    const AddressSpec address = MakeAddress(AF_INET, "203.0.113.1", 24, ifindex);
    const RouteSpec route{Ipv4("203.0.113.0"), 24, ifindex};

    std::vector<RouteSpec> fillers;
    RouteSpec failed{};
    const int first_fill_error =
        FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers, &failed);
    const bool dynamic_projection = first_fill_error == kHostHasNoFixedRouteCapacity;
    if ((!dynamic_projection && first_fill_error != 0) || fillers.empty()) {
        return 8350 + (first_fill_error > 0 ? first_fill_error : 0);
    }
    const size_t route_capacity = fillers.size();
    for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
        if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, *iterator,
                        ++sequence) != 0) {
            return 8375;
        }
    }
    fillers.clear();

    if (ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                    ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, address, true, true,
                      ++sequence) != 0) {
        return 8400;
    }

    if (dynamic_projection) return 0;

    if (const int error = FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers, &failed);
        error != 0 || fillers.size() != route_capacity) {
        return 8500 + (error > 0 ? error : static_cast<int>(fillers.size()));
    }
    for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
        if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, *iterator,
                        ++sequence) != 0) {
            return 8600;
        }
    }

    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                    ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, address, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++sequence) != 0) {
        return 8700;
    }

    fillers.clear();
    if (const int error = FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers, &failed);
        error != 0 || fillers.size() != route_capacity) {
        return 8800 + (error > 0 ? error : static_cast<int>(fillers.size()));
    }
    for (auto iterator = fillers.rbegin(); iterator != fillers.rend(); ++iterator) {
        if (ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, *iterator,
                        ++sequence) != 0) {
            return 8900;
        }
    }

    const AddressSpec second = MakeAddress(AF_INET, "203.0.113.2", 24, ifindex);
    if (ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, address, true, true,
                      ++sequence) != 0 ||
        ChangeAddress(fd.Get(), RTM_NEWADDR,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, second, true, true,
                      ++sequence) != 0) {
        return 9000;
    }
    fillers.clear();
    if (const int error = FillRoutesToCapacity(fd.Get(), ifindex, &sequence, &fillers, &failed);
        error != 0 || fillers.size() + 1 != route_capacity) {
        return 9100 + (error > 0 ? error : static_cast<int>(fillers.size()));
    }
    if (ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, address, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed,
                    ++sequence) != ENOSPC ||
        ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, second, true, true,
                      ++sequence) != 0 ||
        ChangeRoute(fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, failed,
                    ++sequence) != 0) {
        return 9200;
    }
    return 0;
}

template <typename Function>
ChildOutcome RunWithWatchdog(Function function) {
    int result_pipe[2];
    if (pipe(result_pipe) != 0) return {false, -1, 8000 + errno};
    const pid_t child = fork();
    if (child < 0) {
        const int saved = errno;
        close(result_pipe[0]);
        close(result_pipe[1]);
        return {false, -1, 8100 + saved};
    }
    if (child == 0) {
        close(result_pipe[0]);
        const int stage = function();
        const ssize_t written = write(result_pipe[1], &stage, sizeof(stage));
        (void)written;
        close(result_pipe[1]);
        _exit(stage == 0 || stage == kHostHasNoFixedRouteCapacity ||
                      stage == kNamespaceUnavailable
                  ? 0
                  : 1);
    }

    close(result_pipe[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    for (;;) {
        const pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) break;
        if (waited < 0) {
            close(result_pipe[0]);
            return {false, -1, 8200 + errno};
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            (void)kill(child, SIGKILL);
            (void)waitpid(child, &status, 0);
            close(result_pipe[0]);
            return {true, status, 0};
        }
        usleep(10 * 1000);
    }

    int stage = -1;
    if (read(result_pipe[0], &stage, sizeof(stage)) != static_cast<ssize_t>(sizeof(stage))) {
        stage = 8300;
    }
    close(result_pipe[0]);
    return {false, status, stage};
}

void ExpectSuccessfulChild(const ChildOutcome& outcome, const char* timeout_message) {
    ASSERT_FALSE(outcome.timed_out) << timeout_message;
    ASSERT_TRUE(WIFEXITED(outcome.wait_status));
    if (outcome.stage == kNamespaceUnavailable) {
        if (!IsDragonOS()) GTEST_SKIP() << "host does not permit a fresh user/network namespace";
        FAIL() << "DragonOS failed to create the required fresh user/network namespace";
    }
    EXPECT_EQ(outcome.stage, 0) << "encoded stage/error=" << outcome.stage;
    EXPECT_EQ(WEXITSTATUS(outcome.wait_status), 0);
}

TEST(RtnetlinkAddressSemantics, MultipleAddressesStayConsistentAcrossAddrRouteAndDataPlane) {
    ExpectSuccessfulChild(RunWithWatchdog(RunMultipleAddressConsistency),
                          "multi-address mutation or UDP data path deadlocked");
}

TEST(RtnetlinkAddressSemantics, DuplicateReplaceDeleteAndParserSemanticsMatchLinux) {
    ExpectSuccessfulChild(RunWithWatchdog(RunIdentityFlagsAndErrorPriority),
                          "address identity or error-priority request deadlocked");
}

TEST(RtnetlinkAddressSemantics, CombinedRenameAndLinkStatePublishesOnePreparedGeneration) {
    ExpectSuccessfulChild(RunWithWatchdog(RunCombinedRenameAndLinkState),
                          "combined link rename/state transaction deadlocked");
}

TEST(RtnetlinkAddressSemantics, LowMtuRejectsIpv4UntilProtocolCanBeRecreated) {
    ExpectSuccessfulChild(RunWithWatchdog(RunLowMtuIpv4Admission),
                          "low-MTU IPv4 admission or recovery deadlocked");
}

TEST(RtnetlinkAddressSemantics, InvalidAddressCannotPanicKernel) {
    ExpectSuccessfulChild(RunWithWatchdog(RunInvalidAddressSafety),
                          "invalid address request hung or panicked the kernel");
}

TEST(RtnetlinkAddressSemantics, RouteGrowthAndAddressMutationRemainTransactional) {
    const ChildOutcome outcome = RunWithWatchdog(RunRouteGrowthAndAddressMutation);
    ASSERT_FALSE(outcome.timed_out) << "address capacity rollback path deadlocked";
    ASSERT_TRUE(WIFEXITED(outcome.wait_status));
    if (outcome.stage == kNamespaceUnavailable) {
        if (!IsDragonOS()) GTEST_SKIP() << "host does not permit a fresh user/network namespace";
        FAIL() << "DragonOS failed to create the required fresh user/network namespace";
    }
    if (outcome.stage == kHostHasNoFixedRouteCapacity) {
        GTEST_SKIP() << "host route scalability is outside this DragonOS projection test";
    }
    EXPECT_EQ(outcome.stage, 0) << "encoded stage/error=" << outcome.stage;
    EXPECT_EQ(WEXITSTATUS(outcome.wait_status), 0);
}

TEST(RtnetlinkAddressSemantics, AddressAndExplicitRouteShareOneDataPlaneProjection) {
    const ChildOutcome outcome = RunWithWatchdog(RunSharedAddressAndExplicitRouteProjection);
    ASSERT_FALSE(outcome.timed_out) << "shared address/route projection path deadlocked";
    ASSERT_TRUE(WIFEXITED(outcome.wait_status));
    if (outcome.stage == kNamespaceUnavailable) {
        if (!IsDragonOS()) GTEST_SKIP() << "host does not permit a fresh user/network namespace";
        FAIL() << "DragonOS failed to create the required fresh user/network namespace";
    }
    if (outcome.stage == kHostHasNoFixedRouteCapacity) {
        GTEST_SKIP() << "host route scalability is outside this DragonOS projection test";
    }
    EXPECT_EQ(outcome.stage, 0) << "encoded stage/error=" << outcome.stage;
    EXPECT_EQ(WEXITSTATUS(outcome.wait_status), 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
